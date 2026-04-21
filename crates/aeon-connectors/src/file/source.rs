//! File source — reads newline-delimited records from a file.
//!
//! Each line becomes an Event payload. Suitable for log files, JSONL, CSV, etc.
//! Uses tokio::io::BufReader for async, non-blocking reads.

use aeon_types::{AeonError, CoreLocalUuidGenerator, Event, PartitionId, Source};
use bytes::Bytes;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

/// Configuration for `FileSource`.
pub struct FileSourceConfig {
    /// Path to the file to read.
    pub path: PathBuf,
    /// Maximum lines per `next_batch()` call.
    pub batch_size: usize,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Partition ID to assign to all events from this file.
    pub partition: PartitionId,
}

impl FileSourceConfig {
    /// Create a config for reading from a single file.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            batch_size: 1024,
            source_name: Arc::from("file"),
            partition: PartitionId::new(0),
        }
    }

    /// Set maximum lines per batch.
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Set the source name used in Event.source.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Set the partition ID for events.
    pub fn with_partition(mut self, partition: PartitionId) -> Self {
        self.partition = partition;
        self
    }
}

/// File event source — reads lines as events.
///
/// Opens the file on first `next_batch()` call (lazy initialization).
/// Returns empty batches once the file is fully read (exhausted).
pub struct FileSource {
    config: FileSourceConfig,
    reader: Option<BufReader<tokio::fs::File>>,
    exhausted: bool,
    line_buf: String,
    uuid_gen: Mutex<CoreLocalUuidGenerator>,
}

impl FileSource {
    /// Create a new FileSource. The file is opened lazily on first read.
    pub fn new(config: FileSourceConfig) -> Self {
        Self {
            config,
            reader: None,
            exhausted: false,
            line_buf: String::with_capacity(4096),
            uuid_gen: Mutex::new(CoreLocalUuidGenerator::new(0)),
        }
    }

    /// Open the file if not already open.
    async fn ensure_open(&mut self) -> Result<(), AeonError> {
        if self.reader.is_none() {
            let file = tokio::fs::File::open(&self.config.path)
                .await
                .map_err(|e| {
                    AeonError::connection(format!(
                        "file open failed: {}: {}",
                        self.config.path.display(),
                        e
                    ))
                })?;
            self.reader = Some(BufReader::new(file));
            tracing::info!(path = %self.config.path.display(), "FileSource opened");
        }
        Ok(())
    }
}

impl Source for FileSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        if self.exhausted {
            return Ok(Vec::new());
        }

        self.ensure_open().await?;
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| AeonError::state("FileSource reader not initialized"))?;

        let mut events = Vec::with_capacity(self.config.batch_size);
        let now = std::time::Instant::now();

        for _ in 0..self.config.batch_size {
            self.line_buf.clear();
            let bytes_read = reader
                .read_line(&mut self.line_buf)
                .await
                .map_err(|e| AeonError::connection(format!("file read error: {e}")))?;

            if bytes_read == 0 {
                self.exhausted = true;
                break;
            }

            // Trim trailing newline
            let line = self.line_buf.trim_end_matches('\n').trim_end_matches('\r');
            if line.is_empty() {
                continue; // Skip empty lines
            }

            let event_id = self
                .uuid_gen
                .lock()
                .map_err(|_| AeonError::connection("UUID generator mutex poisoned"))?
                .next_uuid();
            let payload = Bytes::copy_from_slice(line.as_bytes());
            let mut event = Event::new(
                event_id,
                0,
                Arc::clone(&self.config.source_name),
                self.config.partition,
                payload,
            );
            event = event.with_source_ts(now);
            events.push(event);
        }

        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn test_file_source_reads_lines() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "line1").unwrap();
        writeln!(tmp, "line2").unwrap();
        writeln!(tmp, "line3").unwrap();
        tmp.flush().unwrap();

        let config = FileSourceConfig::new(tmp.path()).with_batch_size(2);
        let mut source = FileSource::new(config);

        let batch1 = source.next_batch().await.unwrap();
        assert_eq!(batch1.len(), 2);
        assert_eq!(batch1[0].payload.as_ref(), b"line1");
        assert_eq!(batch1[1].payload.as_ref(), b"line2");

        let batch2 = source.next_batch().await.unwrap();
        assert_eq!(batch2.len(), 1);
        assert_eq!(batch2[0].payload.as_ref(), b"line3");

        // Exhausted
        let batch3 = source.next_batch().await.unwrap();
        assert!(batch3.is_empty());
    }

    #[tokio::test]
    async fn test_file_source_skips_empty_lines() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "hello").unwrap();
        writeln!(tmp).unwrap();
        writeln!(tmp, "world").unwrap();
        tmp.flush().unwrap();

        let config = FileSourceConfig::new(tmp.path());
        let mut source = FileSource::new(config);

        let batch = source.next_batch().await.unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].payload.as_ref(), b"hello");
        assert_eq!(batch[1].payload.as_ref(), b"world");
    }

    #[tokio::test]
    async fn test_file_source_missing_file() {
        let config = FileSourceConfig::new("/nonexistent/path/file.txt");
        let mut source = FileSource::new(config);

        let result = source.next_batch().await;
        assert!(result.is_err());
    }
}
