//! MongoDB Change Streams source — watches for document changes.
//!
//! Uses the official MongoDB driver's `watch()` for change streams.
//! Requires MongoDB replica set (change streams are not available on standalone).
//! Push-source: a background task reads changes and pushes into a PushBuffer.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, push_buffer};
use aeon_types::{AeonError, Event, PartitionId, Source};
use bytes::Bytes;
use futures_util::StreamExt;
use mongodb::bson::Document;
use mongodb::options::ChangeStreamOptions;
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
        }
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

        let handle = tokio::spawn(mongodb_reader(change_stream, tx, source_name));

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
) {
    while let Some(result) = change_stream.next().await {
        match result {
            Ok(change_event) => {
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
            }
            Err(e) => {
                tracing::error!(error = %e, "mongodb change stream error");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
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
