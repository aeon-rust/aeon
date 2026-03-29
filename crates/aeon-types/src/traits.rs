//! Core traits — Gate 1 only.
//!
//! Traits are defined BEFORE implementations. Always.
//! Code against the trait, not the concrete type.
//!
//! Gate 2 and post-Gate 2 traits are defined when their phase begins.

use crate::error::AeonError;
use crate::event::{Event, Output};

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
pub trait Sink: Send + Sync {
    /// Write a batch of outputs to the external system.
    fn write_batch(
        &mut self,
        outputs: Vec<Output>,
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;

    /// Flush any buffered outputs, ensuring delivery.
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
