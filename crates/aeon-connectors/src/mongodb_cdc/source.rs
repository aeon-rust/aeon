//! MongoDB Change Streams source — watches for document changes.
//!
//! Uses the official MongoDB driver's `watch()` for change streams.
//! Requires MongoDB replica set (change streams are not available on standalone).
//! Push-source: a background task reads changes and pushes into a PushBuffer.
//!
//! ## Resume token persistence
//!
//! MongoDB change streams are resumable: each `ChangeStreamEvent` carries a
//! `ResumeToken` (its `_id` field) that can be passed to
//! `ChangeStreamOptions::resume_after` on restart to continue from that point.
//! Without persisting this token, a crash restarts the stream from "now",
//! missing any events that happened while the process was down.
//!
//! If `resume_token_path` is set on the config, the source:
//! 1. On startup, loads the token from that file (if it exists) and passes
//!    it to `resume_after`.
//! 2. During streaming, keeps the latest observed token in memory and
//!    atomically flushes it to the file every `resume_token_flush_every_n`
//!    events (via write-to-temp-then-rename).
//! 3. On stream shutdown, performs a final flush so a clean shutdown never
//!    loses the token.
//!
//! The token is serialized as JSON (ResumeToken derives serde). This is a
//! tiny opaque BSON document — ~100 bytes on the wire.
//!
//! **Delivery semantics**: at-least-once. The token is flushed after events
//! are pushed to the internal buffer, not after sinks ack them, so a crash
//! between "buffered" and "delivered" will re-process the affected batch.
//! Exactly-once would require integrating resume tokens into the pipeline
//! delivery-ledger checkpoint, which is future work (see CONNECTOR-AUDIT
//! §4.3 notes).

use crate::push_buffer::{PushBufferConfig, PushBufferRx, push_buffer};
use aeon_types::{AeonError, Event, PartitionId, Source};
use bytes::Bytes;
use futures_util::StreamExt;
use mongodb::bson::Document;
use mongodb::change_stream::event::ResumeToken;
use mongodb::options::ChangeStreamOptions;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Configuration for `MongoDbCdcSource`.
pub struct MongoDbCdcSourceConfig {
    /// MongoDB connection URI (e.g., "mongodb://localhost:27017").
    pub uri: String,
    /// Database name.
    pub database: String,
    /// Collection name (None = watch entire database).
    pub collection: Option<String>,
    /// Push buffer configuration.
    pub buffer_config: PushBufferConfig,
    /// Timeout for first event in `next_batch()`.
    pub poll_timeout: Duration,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Whether to include full document on update events.
    pub full_document: bool,
    /// Aggregation pipeline stages for filtering changes.
    pub pipeline: Vec<Document>,
    /// File path for persisting the latest resume token. If `Some`, the
    /// source loads the token on startup and flushes updates to it during
    /// streaming. If `None`, the stream always starts from "now" on restart.
    pub resume_token_path: Option<PathBuf>,
    /// Flush the resume token to disk every N pushed events. Smaller values
    /// narrow the re-replay window on crash at the cost of extra fsync.
    /// Defaults to 100. Ignored if `resume_token_path` is `None`.
    pub resume_token_flush_every_n: usize,
    /// Reconnect backoff policy — applied when the change stream returns an
    /// error. Exponential + jitter prevents reconnect storms against a flaky
    /// MongoDB replica set (TR-3).
    pub backoff: aeon_types::BackoffPolicy,
}

impl MongoDbCdcSourceConfig {
    /// Create a config for MongoDB change streams.
    pub fn new(uri: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            database: database.into(),
            collection: None,
            buffer_config: PushBufferConfig::default(),
            poll_timeout: Duration::from_secs(1),
            source_name: Arc::from("mongodb-cdc"),
            full_document: true,
            pipeline: Vec::new(),
            resume_token_path: None,
            resume_token_flush_every_n: 100,
            backoff: aeon_types::BackoffPolicy::default(),
        }
    }

    /// Override the reconnect backoff policy.
    pub fn with_backoff(mut self, backoff: aeon_types::BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Enable resume token persistence at the given file path.
    pub fn with_resume_token_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.resume_token_path = Some(path.into());
        self
    }

    /// Override the resume-token flush cadence (every N pushed events).
    pub fn with_resume_token_flush_every_n(mut self, every_n: usize) -> Self {
        self.resume_token_flush_every_n = every_n.max(1);
        self
    }

    /// Watch a specific collection (instead of the whole database).
    pub fn with_collection(mut self, collection: impl Into<String>) -> Self {
        self.collection = Some(collection.into());
        self
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Add a pipeline stage for filtering.
    pub fn with_pipeline_stage(mut self, stage: Document) -> Self {
        self.pipeline.push(stage);
        self
    }

    /// Disable full document lookup on update events.
    pub fn without_full_document(mut self) -> Self {
        self.full_document = false;
        self
    }
}

/// MongoDB Change Streams event source.
///
/// Watches for changes on a collection or database. A background task reads
/// from the change stream and pushes events into a PushBuffer.
/// `next_batch()` drains events from the buffer.
pub struct MongoDbCdcSource {
    rx: PushBufferRx,
    poll_timeout: Duration,
    _reader_handle: tokio::task::JoinHandle<()>,
}

impl MongoDbCdcSource {
    /// Connect to MongoDB and open a change stream.
    pub async fn new(config: MongoDbCdcSourceConfig) -> Result<Self, AeonError> {
        let client = mongodb::Client::with_uri_str(&config.uri)
            .await
            .map_err(|e| AeonError::connection(format!("mongodb connect failed: {e}")))?;

        let db = client.database(&config.database);

        let mut options = ChangeStreamOptions::default();
        options.batch_size = Some(config.buffer_config.batch_size as u32);

        if config.full_document {
            options.full_document = Some(mongodb::options::FullDocumentType::UpdateLookup);
        }

        // Resume token recovery: if a persisted token exists, resume from it.
        if let Some(ref path) = config.resume_token_path {
            match load_resume_token(path) {
                Ok(Some(token)) => {
                    tracing::info!(
                        path = %path.display(),
                        "MongoDbCdcSource resuming from persisted token"
                    );
                    options.resume_after = Some(token);
                }
                Ok(None) => {
                    tracing::info!(
                        path = %path.display(),
                        "MongoDbCdcSource no persisted token — starting from current position"
                    );
                }
                Err(e) => {
                    // Do not fail startup on a corrupted token file — log
                    // and start from "now". The operator can inspect the
                    // file and decide whether to delete it.
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "MongoDbCdcSource failed to load resume token — starting from current position"
                    );
                }
            }
        }

        let change_stream = if let Some(ref collection) = config.collection {
            let coll = db.collection::<Document>(collection);
            coll.watch()
                .pipeline(config.pipeline.clone())
                .with_options(options)
                .await
                .map_err(|e| AeonError::connection(format!("mongodb change stream failed: {e}")))?
        } else {
            db.watch()
                .pipeline(config.pipeline.clone())
                .with_options(options)
                .await
                .map_err(|e| AeonError::connection(format!("mongodb change stream failed: {e}")))?
        };

        tracing::info!(
            database = %config.database,
            collection = ?config.collection,
            "MongoDbCdcSource watching for changes"
        );

        let (tx, rx) = push_buffer(config.buffer_config);
        let source_name = config.source_name;

        let handle = tokio::spawn(mongodb_reader(
            change_stream,
            tx,
            source_name,
            config.resume_token_path,
            config.resume_token_flush_every_n.max(1),
            config.backoff,
        ));

        Ok(Self {
            rx,
            poll_timeout: config.poll_timeout,
            _reader_handle: handle,
        })
    }
}

async fn mongodb_reader(
    mut change_stream: mongodb::change_stream::ChangeStream<
        mongodb::change_stream::event::ChangeStreamEvent<Document>,
    >,
    tx: crate::push_buffer::PushBufferTx,
    source_name: Arc<str>,
    resume_token_path: Option<PathBuf>,
    flush_every_n: usize,
    backoff_policy: aeon_types::BackoffPolicy,
) {
    let mut latest_token: Option<ResumeToken> = None;
    let mut unflushed_since: usize = 0;
    let mut backoff = backoff_policy.iter();

    while let Some(result) = change_stream.next().await {
        match result {
            Ok(change_event) => {
                // Successful event — reset backoff so the next outage restarts
                // at initial_ms rather than wherever we left off (TR-3).
                backoff.reset();

                // Capture the per-event resume token. `change_event.id` is
                // the authoritative resume point for this event.
                latest_token = Some(change_event.id.clone());

                // Serialize the full change event as BSON → bytes
                let mut doc = Document::new();
                doc.insert(
                    "operationType",
                    format!("{:?}", change_event.operation_type),
                );

                if let Some(ns) = &change_event.ns {
                    let mut ns_doc = Document::new();
                    ns_doc.insert("db", &ns.db);
                    if let Some(coll) = &ns.coll {
                        ns_doc.insert("coll", coll.as_str());
                    }
                    doc.insert("ns", ns_doc);
                }

                if let Some(full_doc) = &change_event.full_document {
                    doc.insert("fullDocument", full_doc.clone());
                }

                if let Some(key) = &change_event.document_key {
                    doc.insert("documentKey", key.clone());
                }

                let payload_bytes = doc_to_json_bytes(&doc);
                let mut event = Event::new(
                    uuid::Uuid::nil(),
                    0,
                    Arc::clone(&source_name),
                    PartitionId::new(0),
                    payload_bytes,
                );

                // Add operation type as metadata
                event.metadata.push((
                    Arc::from("mongodb.op"),
                    Arc::from(format!("{:?}", change_event.operation_type).as_str()),
                ));

                if let Some(ns) = &change_event.ns {
                    if let Some(coll) = &ns.coll {
                        event
                            .metadata
                            .push((Arc::from("mongodb.collection"), Arc::from(coll.as_str())));
                    }
                }

                if tx.send(event).await.is_err() {
                    break; // Buffer closed
                }

                // Flush the resume token every N pushed events. We flush
                // AFTER the event is handed to the buffer so the token
                // always leads "delivered" by at most N-1 items, never
                // lags it.
                unflushed_since += 1;
                if let Some(ref path) = resume_token_path {
                    if unflushed_since >= flush_every_n {
                        if let Some(ref token) = latest_token {
                            if let Err(e) = save_resume_token(path, token) {
                                tracing::warn!(
                                    path = %path.display(),
                                    error = %e,
                                    "MongoDbCdcSource failed to persist resume token"
                                );
                            }
                        }
                        unflushed_since = 0;
                    }
                }
            }
            Err(e) => {
                let delay = backoff.next_delay();
                tracing::error!(error = %e, delay_ms = delay.as_millis() as u64,
                    "mongodb change stream error; backing off before retry");
                tokio::time::sleep(delay).await;
            }
        }
    }

    // Final flush on shutdown so a clean stop never loses the latest token.
    if let (Some(path), Some(token)) = (resume_token_path.as_ref(), latest_token.as_ref()) {
        if let Err(e) = save_resume_token(path, token) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "MongoDbCdcSource final resume token flush failed"
            );
        }
    }
}

/// Load a persisted resume token from disk. Returns `Ok(None)` if the file
/// does not exist, `Err` if it exists but is unreadable or corrupt.
fn load_resume_token(path: &std::path::Path) -> Result<Option<ResumeToken>, AeonError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)
        .map_err(|e| AeonError::state(format!("mongodb resume token read: {e}")))?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let token: ResumeToken = serde_json::from_slice(&bytes)
        .map_err(|e| AeonError::state(format!("mongodb resume token parse: {e}")))?;
    Ok(Some(token))
}

/// Atomically persist a resume token to disk via write-to-temp-then-rename.
fn save_resume_token(path: &std::path::Path, token: &ResumeToken) -> Result<(), AeonError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AeonError::state(format!("mongodb resume token dir: {e}")))?;
        }
    }
    let json = serde_json::to_vec(token)
        .map_err(|e| AeonError::state(format!("mongodb resume token serialize: {e}")))?;

    // Write to a sibling temp file then rename for atomicity. On Windows,
    // `rename` over an existing file is only atomic on NTFS via ReplaceFile,
    // which `std::fs::rename` uses on MoveFileEx fallback. Good enough for
    // single-writer semantics.
    let tmp_path = path.with_extension("token.tmp");
    std::fs::write(&tmp_path, &json)
        .map_err(|e| AeonError::state(format!("mongodb resume token tmp write: {e}")))?;
    // On Windows, rename fails if the destination exists; remove first.
    #[cfg(windows)]
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp_path, path)
        .map_err(|e| AeonError::state(format!("mongodb resume token rename: {e}")))?;
    Ok(())
}

fn doc_to_json_bytes(doc: &Document) -> Bytes {
    let json_bytes = mongodb::bson::to_vec(doc).unwrap_or_default();
    Bytes::from(json_bytes)
}

impl Source for MongoDbCdcSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic ResumeToken by round-tripping through serde.
    /// `ResumeToken` has a private constructor; `serde_json::from_value`
    /// is the only public construction path, and it's what we'd use on
    /// a real persisted token anyway.
    fn sample_token(data: &str) -> ResumeToken {
        let json = serde_json::json!({ "_data": data });
        serde_json::from_value(json).expect("valid ResumeToken json")
    }

    #[test]
    fn load_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.token");
        let got = load_resume_token(&path).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn load_empty_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.token");
        std::fs::write(&path, b"").unwrap();
        let got = load_resume_token(&path).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn save_then_load_round_trips_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round.token");

        let token = sample_token("82abcdef0102030405");
        save_resume_token(&path, &token).unwrap();

        assert!(path.exists());
        let loaded = load_resume_token(&path).unwrap().expect("token present");
        assert_eq!(loaded, token);
    }

    #[test]
    fn save_overwrites_existing_token_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overwrite.token");

        let first = sample_token("8200000000000000aa");
        save_resume_token(&path, &first).unwrap();

        let second = sample_token("8211111111111111bb");
        save_resume_token(&path, &second).unwrap();

        let loaded = load_resume_token(&path).unwrap().unwrap();
        assert_eq!(loaded, second);

        // Temp file should not linger after the rename.
        let tmp = path.with_extension("token.tmp");
        assert!(
            !tmp.exists(),
            "temp file should be renamed, not left behind"
        );
    }

    #[test]
    fn save_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("x.token");
        assert!(!path.parent().unwrap().exists());

        let token = sample_token("820000deadbeef");
        save_resume_token(&path, &token).unwrap();

        assert!(path.exists());
        let loaded = load_resume_token(&path).unwrap().unwrap();
        assert_eq!(loaded, token);
    }

    #[test]
    fn load_corrupt_file_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.token");
        std::fs::write(&path, b"this is not json at all").unwrap();

        let result = load_resume_token(&path);
        assert!(result.is_err(), "corrupt json should surface an error");
    }
}
