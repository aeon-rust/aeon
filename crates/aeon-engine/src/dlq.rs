//! Dead-Letter Queue — captures events that fail processing.
//!
//! When a processor returns an error for an event, the DLQ captures the
//! original event along with error context (reason, timestamp) for later
//! inspection and reprocessing.
//!
//! The DLQ wraps any `Sink` implementation, allowing failed events to be
//! routed to any destination (memory, Kafka topic, file, etc.).

use aeon_types::{AeonError, Event, Output, Sink};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A failed event record sent to the DLQ sink.
#[derive(Debug, Clone)]
pub struct DlqRecord {
    /// The original event that failed processing.
    pub event: Event,
    /// Error message describing why processing failed.
    pub reason: String,
    /// Timestamp (nanos since epoch) when the failure occurred.
    pub failed_at: i64,
    /// Number of attempts made before sending to DLQ.
    pub attempts: u32,
}

impl DlqRecord {
    /// Serialize this record into an Output suitable for a DLQ sink.
    pub fn to_output(&self, destination: &Arc<str>) -> Output {
        // Encode error context as a simple header-enriched output.
        // The payload carries the original event payload; error info goes in headers.
        // Event identity is carried structurally via source_event_id/source_partition.
        let mut output = Output::new(Arc::clone(destination), self.event.payload.clone())
            .with_event_identity(&self.event);

        // Add the original event key as the output key for partition affinity
        output.key = Some(Bytes::copy_from_slice(self.event.id.as_bytes()));

        output = output.with_header(Arc::from("dlq.reason"), Arc::from(self.reason.as_str()));
        output = output.with_header(Arc::from("dlq.source"), Arc::clone(&self.event.source));
        output = output.with_header(
            Arc::from("dlq.failed_at"),
            Arc::from(self.failed_at.to_string().as_str()),
        );
        output = output.with_header(
            Arc::from("dlq.attempts"),
            Arc::from(self.attempts.to_string().as_str()),
        );
        output = output.with_header(
            Arc::from("dlq.event_id"),
            Arc::from(self.event.id.to_string().as_str()),
        );
        output = output.with_header(
            Arc::from("dlq.event_timestamp"),
            Arc::from(self.event.timestamp.to_string().as_str()),
        );

        output
    }
}

/// DLQ configuration.
#[derive(Debug, Clone)]
pub struct DlqConfig {
    /// Destination name for DLQ outputs (e.g., topic name).
    pub destination: Arc<str>,
    /// Maximum number of records to buffer before flushing.
    pub batch_size: usize,
}

impl Default for DlqConfig {
    fn default() -> Self {
        Self {
            destination: Arc::from("aeon-dlq"),
            batch_size: 64,
        }
    }
}

/// Dead-Letter Queue handler that routes failed events to a sink.
pub struct DeadLetterQueue<K: Sink> {
    sink: K,
    config: DlqConfig,
    buffer: Vec<Output>,
    /// Total records sent to DLQ.
    records_sent: AtomicU64,
}

impl<K: Sink> DeadLetterQueue<K> {
    /// Create a new DLQ with the given sink and config.
    pub fn new(sink: K, config: DlqConfig) -> Self {
        let batch_size = config.batch_size;
        Self {
            sink,
            config,
            buffer: Vec::with_capacity(batch_size),
            records_sent: AtomicU64::new(0),
        }
    }

    /// Record a failed event. Flushes when the buffer reaches batch_size.
    pub async fn record(&mut self, record: DlqRecord) -> Result<(), AeonError> {
        let output = record.to_output(&self.config.destination);
        self.buffer.push(output);

        if self.buffer.len() >= self.config.batch_size {
            self.flush().await?;
        }

        Ok(())
    }

    /// Flush all buffered DLQ records to the sink.
    pub async fn flush(&mut self) -> Result<(), AeonError> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        let batch: Vec<Output> = self.buffer.drain(..).collect();
        let count = batch.len() as u64;
        let _batch_result = self.sink.write_batch(batch).await?;
        self.sink.flush().await?;
        self.records_sent.fetch_add(count, Ordering::Relaxed);
        Ok(())
    }

    /// Total number of records sent to the DLQ.
    pub fn records_sent(&self) -> u64 {
        self.records_sent.load(Ordering::Relaxed)
    }

    /// Current number of buffered (unflushed) records.
    pub fn buffered_count(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_connectors::MemorySink;
    use aeon_types::PartitionId;

    fn make_event(id: u64) -> Event {
        Event::new(
            uuid::Uuid::nil(),
            id as i64,
            Arc::from("test-source"),
            PartitionId::new(0),
            Bytes::from(format!("payload-{id}")),
        )
    }

    #[tokio::test]
    async fn dlq_records_failed_events() {
        let sink = MemorySink::new();
        let config = DlqConfig {
            destination: Arc::from("test-dlq"),
            batch_size: 10,
        };
        let mut dlq = DeadLetterQueue::new(sink, config);

        let record = DlqRecord {
            event: make_event(1),
            reason: "serialization error".to_string(),
            failed_at: 1000,
            attempts: 3,
        };

        dlq.record(record).await.unwrap();
        dlq.flush().await.unwrap();

        assert_eq!(dlq.records_sent(), 1);
    }

    #[tokio::test]
    async fn dlq_auto_flushes_at_batch_size() {
        let sink = MemorySink::new();
        let config = DlqConfig {
            destination: Arc::from("test-dlq"),
            batch_size: 3,
        };
        let mut dlq = DeadLetterQueue::new(sink, config);

        // Send 3 records — should auto-flush
        for i in 0..3 {
            let record = DlqRecord {
                event: make_event(i),
                reason: format!("error-{i}"),
                failed_at: i as i64,
                attempts: 1,
            };
            dlq.record(record).await.unwrap();
        }

        assert_eq!(dlq.records_sent(), 3);
        assert_eq!(dlq.buffered_count(), 0);
    }

    #[tokio::test]
    async fn dlq_record_contains_error_context() {
        let sink = MemorySink::new();
        let config = DlqConfig {
            destination: Arc::from("my-dlq"),
            batch_size: 10,
        };
        let mut dlq = DeadLetterQueue::new(sink, config);

        let event = make_event(42);
        let record = DlqRecord {
            event: event.clone(),
            reason: "bad format".to_string(),
            failed_at: 9999,
            attempts: 2,
        };

        let output = record.to_output(&Arc::from("my-dlq"));

        assert_eq!(output.destination.as_ref(), "my-dlq");
        assert_eq!(output.payload.as_ref(), b"payload-42");

        // Verify headers contain error context
        let headers: Vec<(&str, &str)> = output
            .headers
            .iter()
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
            .collect();

        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "dlq.reason" && *v == "bad format")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "dlq.attempts" && *v == "2")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "dlq.failed_at" && *v == "9999")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "dlq.source" && *v == "test-source")
        );

        // Also make sure it can be recorded
        let record2 = DlqRecord {
            event,
            reason: "bad format".to_string(),
            failed_at: 9999,
            attempts: 2,
        };
        dlq.record(record2).await.unwrap();
        dlq.flush().await.unwrap();
        assert_eq!(dlq.records_sent(), 1);
    }

    #[tokio::test]
    async fn dlq_flush_empty_is_noop() {
        let sink = MemorySink::new();
        let config = DlqConfig::default();
        let mut dlq = DeadLetterQueue::new(sink, config);

        dlq.flush().await.unwrap();
        assert_eq!(dlq.records_sent(), 0);
    }
}
