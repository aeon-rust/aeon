//! Canonical Event and Output envelopes.
//!
//! **All** sources produce `Event`s. **All** sinks receive `Output`s.
//! Never generate custom event structures.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use smallvec::SmallVec;

use crate::partition::PartitionId;

/// The universal data unit flowing through the Aeon pipeline.
///
/// Cache-line aligned to prevent false sharing between CPU cores.
/// All fields are designed for zero-copy on the hot path:
/// - `source`: `Arc<str>` — interned at pipeline init, pointer-copy clone
/// - `metadata`: `SmallVec` — inline up to 4 headers, no heap allocation
/// - `payload`: `Bytes` — reference-counted, zero-copy slice from source buffer
#[repr(align(64))]
#[derive(Debug, Clone)]
pub struct Event {
    /// UUIDv7 event identifier — time-ordered, per-core monotonic.
    pub id: uuid::Uuid,
    /// Unix epoch nanoseconds.
    pub timestamp: i64,
    /// Source identifier — interned `Arc<str>`, NOT `String`.
    pub source: Arc<str>,
    /// Partition for parallel processing.
    pub partition: PartitionId,
    /// Key-value metadata headers. Inline up to 4 without heap allocation.
    pub metadata: SmallVec<[(Arc<str>, Arc<str>); 4]>,
    /// Zero-copy payload bytes from the source buffer.
    pub payload: Bytes,
    /// Source-side timestamp for end-to-end latency tracking.
    pub source_ts: Option<Instant>,
    /// Source-system offset (e.g., Kafka message offset) for checkpoint resume.
    /// Set by source connectors; propagated to Output via `with_event_identity()`.
    pub source_offset: Option<i64>,
}

/// The output envelope emitted by processors and consumed by sinks.
///
/// Cache-line aligned to prevent false sharing between CPU cores.
///
/// **Event identity propagation**: `source_event_id`, `source_partition`, and
/// `source_offset` trace every output back to the originating source event.
/// This enables per-event delivery tracking in the `DeliveryLedger`, checkpoint
/// offset persistence, DLQ correlation, and end-to-end traceability without
/// header-based workarounds.
#[repr(align(64))]
#[derive(Debug, Clone)]
pub struct Output {
    /// Destination sink/topic name — interned `Arc<str>`.
    pub destination: Arc<str>,
    /// Optional partition key for key-based routing (zero-copy).
    pub key: Option<Bytes>,
    /// Zero-copy output payload.
    pub payload: Bytes,
    /// Key-value headers. Inline up to 4 without heap allocation.
    pub headers: SmallVec<[(Arc<str>, Arc<str>); 4]>,
    /// Propagated from source Event for latency tracking.
    pub source_ts: Option<Instant>,

    // ── Event identity (propagated from source Event) ────────────────
    /// UUIDv7 of the source event that produced this output.
    /// Set by processors; used by DeliveryLedger for per-event ack tracking.
    /// `None` only for synthetic outputs (e.g., DLQ records, test fixtures).
    pub source_event_id: Option<uuid::Uuid>,
    /// Source partition of the originating event.
    /// Used by checkpoint to persist per-partition offsets.
    pub source_partition: Option<PartitionId>,
    /// Source offset (e.g., Kafka offset) of the originating event.
    /// Used by checkpoint to persist resume position per partition.
    pub source_offset: Option<i64>,
}

impl Event {
    /// Create a new Event with the given fields. Metadata and source_ts default to empty/None.
    pub fn new(
        id: uuid::Uuid,
        timestamp: i64,
        source: Arc<str>,
        partition: PartitionId,
        payload: Bytes,
    ) -> Self {
        Self {
            id,
            timestamp,
            source,
            partition,
            metadata: SmallVec::new(),
            payload,
            source_ts: None,
            source_offset: None,
        }
    }

    /// Add a metadata header.
    #[inline]
    pub fn with_metadata(mut self, key: Arc<str>, value: Arc<str>) -> Self {
        self.metadata.push((key, value));
        self
    }

    /// Set the source timestamp for latency tracking.
    #[inline]
    pub fn with_source_ts(mut self, ts: Instant) -> Self {
        self.source_ts = Some(ts);
        self
    }

    /// Set the source-system offset (e.g., Kafka message offset) for checkpoint.
    #[inline]
    pub fn with_source_offset(mut self, offset: i64) -> Self {
        self.source_offset = Some(offset);
        self
    }
}

impl Output {
    /// Create a new Output with the given destination and payload.
    pub fn new(destination: Arc<str>, payload: Bytes) -> Self {
        Self {
            destination,
            key: None,
            payload,
            headers: SmallVec::new(),
            source_ts: None,
            source_event_id: None,
            source_partition: None,
            source_offset: None,
        }
    }

    /// Set the partition key.
    #[inline]
    pub fn with_key(mut self, key: Bytes) -> Self {
        self.key = Some(key);
        self
    }

    /// Add a header.
    #[inline]
    pub fn with_header(mut self, key: Arc<str>, value: Arc<str>) -> Self {
        self.headers.push((key, value));
        self
    }

    /// Propagate source timestamp for latency tracking.
    #[inline]
    pub fn with_source_ts(mut self, ts: Option<Instant>) -> Self {
        self.source_ts = ts;
        self
    }

    /// Set the source event identity fields from the originating Event.
    /// This is the primary method processors should use to propagate traceability.
    /// Copies: event.id, event.partition, event.source_ts, event.source_offset.
    #[inline]
    pub fn with_event_identity(mut self, event: &Event) -> Self {
        self.source_event_id = Some(event.id);
        self.source_partition = Some(event.partition);
        self.source_ts = event.source_ts;
        self.source_offset = event.source_offset;
        self
    }

    /// Set the source event ID (UUIDv7) for delivery tracking.
    #[inline]
    pub fn with_source_event_id(mut self, id: uuid::Uuid) -> Self {
        self.source_event_id = Some(id);
        self
    }

    /// Set the source partition for checkpoint offset tracking.
    #[inline]
    pub fn with_source_partition(mut self, partition: PartitionId) -> Self {
        self.source_partition = Some(partition);
        self
    }

    /// Set the source offset for checkpoint resume position.
    #[inline]
    pub fn with_source_offset(mut self, offset: i64) -> Self {
        self.source_offset = Some(offset);
        self
    }

    /// Convert this output back into an event for processor chaining.
    ///
    /// Used in DAG topologies where one processor's output feeds another processor's input.
    /// The caller provides identity fields (id, timestamp, source, partition).
    /// Headers are mapped to metadata. Payload is moved zero-copy (Bytes).
    pub fn into_event(
        self,
        id: uuid::Uuid,
        timestamp: i64,
        source: Arc<str>,
        partition: PartitionId,
    ) -> Event {
        Event {
            id,
            timestamp,
            source,
            partition,
            metadata: self.headers,
            payload: self.payload,
            source_ts: self.source_ts,
            source_offset: self.source_offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_alignment_is_64_bytes() {
        assert_eq!(std::mem::align_of::<Event>(), 64);
    }

    #[test]
    fn output_alignment_is_64_bytes() {
        assert_eq!(std::mem::align_of::<Output>(), 64);
    }

    #[test]
    fn event_construction() {
        let source: Arc<str> = Arc::from("test-source");
        let event = Event::new(
            uuid::Uuid::nil(),
            1234567890,
            source,
            PartitionId::new(0),
            Bytes::from_static(b"hello"),
        );
        assert_eq!(event.timestamp, 1234567890);
        assert_eq!(&*event.source, "test-source");
        assert_eq!(event.payload.as_ref(), b"hello");
        assert!(event.metadata.is_empty());
        assert!(event.source_ts.is_none());
    }

    #[test]
    fn event_with_metadata() {
        let source: Arc<str> = Arc::from("src");
        let key: Arc<str> = Arc::from("content-type");
        let value: Arc<str> = Arc::from("application/json");
        let event = Event::new(
            uuid::Uuid::nil(),
            0,
            source,
            PartitionId::new(0),
            Bytes::new(),
        )
        .with_metadata(key, value);
        assert_eq!(event.metadata.len(), 1);
        assert_eq!(&*event.metadata[0].0, "content-type");
    }

    #[test]
    fn event_metadata_stays_inline_up_to_4() {
        let source: Arc<str> = Arc::from("src");
        let mut event = Event::new(
            uuid::Uuid::nil(),
            0,
            source,
            PartitionId::new(0),
            Bytes::new(),
        );
        for i in 0..4 {
            let k: Arc<str> = Arc::from(format!("k{i}"));
            let v: Arc<str> = Arc::from(format!("v{i}"));
            event.metadata.push((k, v));
        }
        // SmallVec with capacity 4 stays inline (on stack) for <= 4 elements
        assert!(!event.metadata.spilled());
    }

    #[test]
    fn output_construction() {
        let dest: Arc<str> = Arc::from("output-topic");
        let output = Output::new(dest, Bytes::from_static(b"result"))
            .with_key(Bytes::from_static(b"user-123"));
        assert_eq!(&*output.destination, "output-topic");
        assert_eq!(output.payload.as_ref(), b"result");
        assert_eq!(
            output.key.as_ref().map(|k| k.as_ref()),
            Some(b"user-123".as_ref())
        );
    }

    #[test]
    fn output_into_event_roundtrip() {
        let output = Output::new(Arc::from("dest"), Bytes::from_static(b"payload"))
            .with_header(Arc::from("k1"), Arc::from("v1"));
        let source: Arc<str> = Arc::from("chained");
        let event = output.into_event(uuid::Uuid::nil(), 42, source, PartitionId::new(3));
        assert_eq!(event.payload.as_ref(), b"payload");
        assert_eq!(event.timestamp, 42);
        assert_eq!(&*event.source, "chained");
        assert_eq!(event.partition, PartitionId::new(3));
        assert_eq!(event.metadata.len(), 1);
        assert_eq!(&*event.metadata[0].0, "k1");
    }

    #[test]
    fn event_source_is_zero_copy_clone() {
        let source: Arc<str> = Arc::from("shared-source");
        let event1 = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::clone(&source),
            PartitionId::new(0),
            Bytes::new(),
        );
        let event2 = event1.clone();
        // Both events share the same Arc<str> allocation
        assert!(Arc::ptr_eq(&event1.source, &event2.source));
    }

    #[test]
    fn output_new_has_no_event_identity() {
        let output = Output::new(Arc::from("dest"), Bytes::from_static(b"data"));
        assert!(output.source_event_id.is_none());
        assert!(output.source_partition.is_none());
        assert!(output.source_offset.is_none());
    }

    #[test]
    fn output_with_event_identity() {
        let event_id = uuid::Uuid::from_bytes([1u8; 16]);
        let event = Event::new(
            event_id,
            999,
            Arc::from("src"),
            PartitionId::new(7),
            Bytes::from_static(b"payload"),
        );
        let output = Output::new(Arc::from("dest"), Bytes::from_static(b"result"))
            .with_event_identity(&event);

        assert_eq!(output.source_event_id, Some(event_id));
        assert_eq!(output.source_partition, Some(PartitionId::new(7)));
        assert!(output.source_ts.is_none()); // event had no source_ts
    }

    #[test]
    fn output_with_source_event_id_builder() {
        let id = uuid::Uuid::from_bytes([42u8; 16]);
        let output = Output::new(Arc::from("dest"), Bytes::from_static(b"data"))
            .with_source_event_id(id)
            .with_source_partition(PartitionId::new(3))
            .with_source_offset(12345);

        assert_eq!(output.source_event_id, Some(id));
        assert_eq!(output.source_partition, Some(PartitionId::new(3)));
        assert_eq!(output.source_offset, Some(12345));
    }
}
