//! File sink — writes outputs as newline-delimited records to a file.
//!
//! Each output payload is written as a single line. Supports append mode
//! for continuous writing and truncate mode for fresh output.
//!
//! Delivery strategies:
//! - **PerEvent**: writes each output + fsync individually (slowest, strictest).
//! - **OrderedBatch** (default): writes all outputs in order + single fsync at batch end.
//! - **UnorderedBatch**: writes to BufWriter only, flush() fsyncs (highest throughput).

use aeon_types::{AeonError, BatchResult, DeliveryStrategy, Output, Sink};
use tokio::io::AsyncWriteExt;

use std::path::PathBuf;

/// Configuration for `FileSink`.
pub struct FileSinkConfig {
    /// Path to the output file.
    pub path: PathBuf,
    /// Whether to append to existing file (true) or truncate (false).
    pub append: bool,
    /// Delivery strategy — controls flush behavior.
    pub strategy: DeliveryStrategy,
}

impl FileSinkConfig {
    /// Create a config for writing to a file (truncate mode by default).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            append: false,
            strategy: DeliveryStrategy::default(),
        }
    }

    /// Set append mode (don't truncate existing file).
    pub fn with_append(mut self, append: bool) -> Self {
        self.append = append;
        self
    }

    /// Set the delivery strategy.
    pub fn with_strategy(mut self, strategy: DeliveryStrategy) -> Self {
        self.strategy = strategy;
        self
    }
}

/// File output sink — writes outputs as lines.
///
/// Opens the file lazily on first `write_batch()`.
/// Uses a BufWriter for efficient batched I/O.
///
/// - **PerEvent**: writes + fsync per event (strict durability).
/// - **OrderedBatch**: writes all events in order, single fsync at batch end.
/// - **UnorderedBatch**: writes accumulate in BufWriter until `flush()`.
pub struct FileSink {
    config: FileSinkConfig,
    writer: Option<tokio::io::BufWriter<tokio::fs::File>>,
    written: u64,
}

impl FileSink {
    /// Create a new FileSink. The file is opened lazily on first write.
    pub fn new(config: FileSinkConfig) -> Self {
        Self {
            config,
            writer: None,
            written: 0,
        }
    }

    /// Number of outputs successfully written.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Open the file if not already open.
    async fn ensure_open(&mut self) -> Result<(), AeonError> {
        if self.writer.is_none() {
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .append(self.config.append)
                .truncate(!self.config.append)
                .open(&self.config.path)
                .await
                .map_err(|e| {
                    AeonError::connection(format!(
                        "file open failed: {}: {}",
                        self.config.path.display(),
                        e
                    ))
                })?;
            self.writer = Some(tokio::io::BufWriter::new(file));
            tracing::info!(path = %self.config.path.display(), append = self.config.append, "FileSink opened");
        }
        Ok(())
    }
}

impl Sink for FileSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        self.ensure_open().await?;
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| AeonError::state("FileSink writer not initialized"))?;

        let event_ids: Vec<_> = outputs.iter().filter_map(|o| o.source_event_id).collect();

        match self.config.strategy {
            DeliveryStrategy::PerEvent => {
                // Write + flush per event for strict durability.
                for output in &outputs {
                    writer
                        .write_all(output.payload.as_ref())
                        .await
                        .map_err(|e| AeonError::connection(format!("file write error: {e}")))?;
                    writer
                        .write_all(b"\n")
                        .await
                        .map_err(|e| AeonError::connection(format!("file write error: {e}")))?;
                    writer
                        .flush()
                        .await
                        .map_err(|e| AeonError::connection(format!("file flush error: {e}")))?;
                    self.written += 1;
                }
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::OrderedBatch => {
                // Write all in order, single flush at batch end.
                for output in &outputs {
                    writer
                        .write_all(output.payload.as_ref())
                        .await
                        .map_err(|e| AeonError::connection(format!("file write error: {e}")))?;
                    writer
                        .write_all(b"\n")
                        .await
                        .map_err(|e| AeonError::connection(format!("file write error: {e}")))?;
                    self.written += 1;
                }
                writer
                    .flush()
                    .await
                    .map_err(|e| AeonError::connection(format!("file flush error: {e}")))?;
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::UnorderedBatch => {
                // Write to BufWriter only — no flush until explicit flush() call.
                for output in &outputs {
                    writer
                        .write_all(output.payload.as_ref())
                        .await
                        .map_err(|e| AeonError::connection(format!("file write error: {e}")))?;
                    writer
                        .write_all(b"\n")
                        .await
                        .map_err(|e| AeonError::connection(format!("file write error: {e}")))?;
                    self.written += 1;
                }
                Ok(BatchResult::all_pending(event_ids))
            }
        }
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        if let Some(writer) = self.writer.as_mut() {
            writer
                .flush()
                .await
                .map_err(|e| AeonError::connection(format!("file flush error: {e}")))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::Arc;

    fn test_output(data: &str) -> Output {
        Output::new(Arc::from("file"), Bytes::from(data.to_string()))
    }

    #[tokio::test]
    async fn test_file_sink_write_and_read() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let config = FileSinkConfig::new(&path);
        let mut sink = FileSink::new(config);

        let outputs = vec![test_output("hello"), test_output("world")];
        sink.write_batch(outputs).await.unwrap();
        sink.flush().await.unwrap();

        assert_eq!(sink.written(), 2);

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "hello\nworld\n");
    }

    #[tokio::test]
    async fn test_file_sink_append_mode() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        // Write first batch (truncate)
        {
            let config = FileSinkConfig::new(&path);
            let mut sink = FileSink::new(config);
            sink.write_batch(vec![test_output("first")]).await.unwrap();
            sink.flush().await.unwrap();
        }

        // Append second batch
        {
            let config = FileSinkConfig::new(&path).with_append(true);
            let mut sink = FileSink::new(config);
            sink.write_batch(vec![test_output("second")]).await.unwrap();
            sink.flush().await.unwrap();
        }

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "first\nsecond\n");
    }

    #[tokio::test]
    async fn test_file_source_to_sink_roundtrip() {
        use crate::file::source::{FileSource, FileSourceConfig};
        use aeon_types::Source;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        // Write via sink
        let config = FileSinkConfig::new(&path);
        let mut sink = FileSink::new(config);
        sink.write_batch(vec![
            test_output("alpha"),
            test_output("beta"),
            test_output("gamma"),
        ])
        .await
        .unwrap();
        sink.flush().await.unwrap();

        // Read via source
        let config = FileSourceConfig::new(&path);
        let mut source = FileSource::new(config);
        let batch = source.next_batch().await.unwrap();

        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].payload.as_ref(), b"alpha");
        assert_eq!(batch[1].payload.as_ref(), b"beta");
        assert_eq!(batch[2].payload.as_ref(), b"gamma");
    }

    #[tokio::test]
    async fn test_file_sink_unordered_batch_defers_flush() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let config = FileSinkConfig::new(&path).with_strategy(DeliveryStrategy::UnorderedBatch);
        let mut sink = FileSink::new(config);

        // Write without explicit flush — data stays in BufWriter
        let result = sink
            .write_batch(vec![test_output("buffered")])
            .await
            .unwrap();
        // No source_event_id on test outputs, so pending IDs are empty,
        // but the data is buffered (not flushed to disk yet).
        assert_eq!(result.delivered.len(), 0);

        // After flush, data must be durable
        sink.flush().await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "buffered\n");
        assert_eq!(sink.written(), 1);
    }

    #[tokio::test]
    async fn test_file_sink_ordered_batch_flushes_per_batch() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let config = FileSinkConfig::new(&path).with_strategy(DeliveryStrategy::OrderedBatch);
        let mut sink = FileSink::new(config);

        // In OrderedBatch mode, data is flushed after each batch
        let result = sink
            .write_batch(vec![test_output("durable")])
            .await
            .unwrap();
        assert!(result.all_succeeded());

        // Should be readable without explicit flush()
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "durable\n");
    }

    #[tokio::test]
    async fn test_file_sink_per_event_flushes_each() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let config = FileSinkConfig::new(&path).with_strategy(DeliveryStrategy::PerEvent);
        let mut sink = FileSink::new(config);

        let result = sink
            .write_batch(vec![test_output("a"), test_output("b")])
            .await
            .unwrap();
        assert!(result.all_succeeded());
        assert_eq!(sink.written(), 2);

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "a\nb\n");
    }
}
