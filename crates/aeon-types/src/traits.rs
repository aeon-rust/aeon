//! Core traits — Gate 1 only.
//!
//! Traits are defined BEFORE implementations. Always.
//! Code against the trait, not the concrete type.
//!
//! Gate 2 and post-Gate 2 traits are defined when their phase begins.

use crate::delivery::BatchResult;
use crate::error::AeonError;
use crate::event::{Event, Output};
use crate::processor_transport::{ProcessorHealth, ProcessorInfo};
use std::future::Future;
use std::pin::Pin;

/// Event ingestion source. Batch-first: returns `Vec<Event>` per poll.
///
/// Pull sources call the external system inside `next_batch()`.
/// Push sources drain an internal receive buffer inside `next_batch()`.
/// The engine does not know or care which model the source uses.
pub trait Source: Send + Sync {
    /// Poll for the next batch of events.
    /// Returns an empty vec during lulls (no events available).
    fn next_batch(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Vec<Event>, AeonError>> + Send;
}

/// Event delivery sink. Batch-first: accepts `Vec<Output>` per flush.
///
/// `write_batch()` returns `BatchResult` which reports per-event delivery
/// status (delivered/pending/failed). The pipeline engine uses this with
/// `BatchFailurePolicy` to drive retry/DLQ/fail decisions.
///
/// How `write_batch()` behaves depends on the `DeliveryStrategy`:
/// - `PerEvent`: send + await each event individually, all returned as delivered
/// - `OrderedBatch`: send all in order, await all at batch end, all delivered
/// - `UnorderedBatch`: enqueue all, return immediately, all returned as pending
pub trait Sink: Send + Sync {
    /// Write a batch of outputs to the external system.
    ///
    /// Returns `BatchResult` indicating which events were delivered, which
    /// are pending (enqueued but not yet acked), and which failed.
    fn write_batch(
        &mut self,
        outputs: Vec<Output>,
    ) -> impl std::future::Future<Output = Result<BatchResult, AeonError>> + Send;

    /// Flush any buffered outputs, ensuring delivery.
    ///
    /// For `UnorderedBatch` mode, this collects pending acks and returns.
    /// For `PerEvent` and `OrderedBatch`, this is typically a no-op since
    /// acks are already collected in `write_batch()`.
    fn flush(&mut self) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;
}

/// Event transformation processor.
///
/// Processors are the only component that may use `dyn Trait` (for Wasm runtime).
/// Native Rust processors implement this trait directly.
pub trait Processor: Send + Sync {
    /// Process a single event, producing zero or more outputs.
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError>;

    /// Process a batch of events. Default implementation calls `process()` per event.
    fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {
        let mut outputs = Vec::with_capacity(events.len());
        for event in events {
            outputs.extend(self.process(event)?);
        }
        Ok(outputs)
    }
}

/// Key-value state operations. Backed by the multi-tier state store.
pub trait StateOps: Send + Sync {
    fn get(
        &self,
        key: &[u8],
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, AeonError>> + Send;

    fn put(
        &self,
        key: &[u8],
        value: &[u8],
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;

    fn delete(&self, key: &[u8])
    -> impl std::future::Future<Output = Result<(), AeonError>> + Send;
}

/// Source that can rewind to a previous offset (e.g., for replay after crash).
pub trait Seekable: Source {
    fn seek(
        &mut self,
        offset: u64,
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;
}

/// Sink that can deduplicate by event ID (for exactly-once delivery).
pub trait IdempotentSink: Sink {
    fn has_seen(
        &self,
        event_id: &uuid::Uuid,
    ) -> impl std::future::Future<Output = Result<bool, AeonError>> + Send;
}

/// Async transport abstraction for all processor tiers (T1–T4).
///
/// The pipeline calls this trait exclusively — it never knows whether the
/// processor is in-process (T1/T2) or out-of-process (T3/T4).
///
/// Unlike `Source`/`Sink`/`Processor` which use APIT (`impl Future`) for
/// static dispatch, this trait uses `Pin<Box<dyn Future>>` to support
/// dynamic dispatch via `&dyn ProcessorTransport` (Decision D2). The
/// per-batch Box allocation (~20ns) is negligible vs processing (240ns+).
pub trait ProcessorTransport: Send + Sync {
    /// Send a batch of events to the processor, receive outputs.
    ///
    /// For T1/T2 (in-process): resolves synchronously, no real await point.
    /// For T3/T4 (network): awaits network round-trip.
    /// Event identity propagation is handled by the implementation.
    fn call_batch(
        &self,
        events: Vec<Event>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Output>, AeonError>> + Send + '_>>;

    /// Check processor health.
    ///
    /// For T1/T2: always returns healthy.
    /// For T3/T4: checks heartbeat and connection state.
    fn health(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessorHealth, AeonError>> + Send + '_>>;

    /// Graceful drain — stop accepting new batches, flush in-flight work.
    ///
    /// For T1/T2: no-op (in-process, nothing to drain).
    /// For T3/T4: sends drain signal on control stream, awaits acknowledgment.
    fn drain(&self) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + '_>>;

    /// Processor metadata (sync — always available without I/O).
    fn info(&self) -> ProcessorInfo;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::PartitionId;
    use bytes::Bytes;
    use std::sync::Arc;

    /// A trivial passthrough processor for testing.
    struct PassthroughProcessor;

    impl Processor for PassthroughProcessor {
        fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
            Ok(vec![
                Output::new(Arc::from("output"), event.payload.clone())
                    .with_source_ts(event.source_ts),
            ])
        }
    }

    #[test]
    fn passthrough_processor_produces_output() {
        let proc = PassthroughProcessor;
        let event = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from_static(b"hello"),
        );
        let outputs = proc.process(event).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].payload.as_ref(), b"hello");
    }

    #[test]
    fn process_batch_default_impl() {
        let proc = PassthroughProcessor;
        let events: Vec<Event> = (0..3)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i,
                    Arc::from("test"),
                    PartitionId::new(0),
                    Bytes::from(format!("event-{i}")),
                )
            })
            .collect();
        let outputs = proc.process_batch(events).unwrap();
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0].payload.as_ref(), b"event-0");
        assert_eq!(outputs[2].payload.as_ref(), b"event-2");
    }

    /// A filter processor that drops events without "keep" in payload.
    struct FilterProcessor;

    impl Processor for FilterProcessor {
        fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
            if event.payload.as_ref().windows(4).any(|w| w == b"keep") {
                Ok(vec![Output::new(
                    Arc::from("output"),
                    event.payload.clone(),
                )])
            } else {
                Ok(vec![])
            }
        }
    }

    /// Compile-time proof that ProcessorTransport is object-safe (dyn-compatible).
    /// If this compiles, `&dyn ProcessorTransport` works — required by Decision D2.
    #[allow(dead_code)]
    fn _assert_processor_transport_object_safe(_: &dyn ProcessorTransport) {}

    #[test]
    fn filter_processor_drops_non_matching() {
        let proc = FilterProcessor;
        let keep = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from_static(b"keep this"),
        );
        let drop = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from_static(b"discard this"),
        );

        assert_eq!(proc.process(keep).unwrap().len(), 1);
        assert_eq!(proc.process(drop).unwrap().len(), 0);
    }
}
