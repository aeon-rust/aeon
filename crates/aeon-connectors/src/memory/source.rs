//! In-memory source that serves pre-loaded events.

use aeon_types::{AeonError, Event, Source};

/// A source that yields events from a pre-loaded `Vec<Event>`.
///
/// Returns events in configurable batch sizes. Once exhausted, returns
/// empty batches. Used for testing and benchmarking.
///
/// **FT-12 (Gate 1 perf)**: events are consumed by move — `next_batch()`
/// drains them out of the internal iterator rather than cloning a slice.
/// This removes an `Event::clone` per event (Arc/Bytes refcount bumps +
/// 128 B memcpy) from the pipeline's hot path, so benchmarks measure real
/// pipeline overhead rather than test-fixture cloning. The original event
/// vec is retained only when `reset()` is enabled.
pub struct MemorySource {
    /// Active iterator — events are moved out by `next_batch`.
    iter: std::vec::IntoIter<Event>,
    /// Original vec kept only if `reset()` may be called later.
    /// `None` in the hot benchmark path (no reset ⇒ no retained copy).
    original: Option<Vec<Event>>,
    batch_size: usize,
    paused: bool,
    remaining_hint: usize,
}

impl MemorySource {
    /// Create a new `MemorySource` from a vec of events. The source consumes
    /// the events by move; `reset()` is disabled (the iterator, once drained,
    /// cannot be rewound).
    pub fn new(events: Vec<Event>, batch_size: usize) -> Self {
        let remaining_hint = events.len();
        Self {
            iter: events.into_iter(),
            original: None,
            batch_size,
            paused: false,
            remaining_hint,
        }
    }

    /// Create a `MemorySource` that supports `reset()` — the events are kept
    /// alive via an internal `Clone` and re-seeded on reset. Prefer `new()`
    /// in benchmarks to avoid the extra `Vec<Event>` clone on construction.
    pub fn new_resetable(events: Vec<Event>, batch_size: usize) -> Self {
        let remaining_hint = events.len();
        Self {
            iter: events.clone().into_iter(),
            original: Some(events),
            batch_size,
            paused: false,
            remaining_hint,
        }
    }

    /// Number of events remaining.
    pub fn remaining(&self) -> usize {
        self.remaining_hint
    }

    /// Whether all events have been consumed.
    pub fn is_exhausted(&self) -> bool {
        self.remaining_hint == 0
    }

    /// Reset position to beginning. Only supported on sources created via
    /// `new_resetable()`; a no-op otherwise.
    pub fn reset(&mut self) {
        if let Some(ref original) = self.original {
            self.iter = original.clone().into_iter();
            self.remaining_hint = original.len();
        }
    }
}

impl Source for MemorySource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        if self.paused || self.remaining_hint == 0 {
            return Ok(Vec::new());
        }
        // Moves events out of the iterator — no Event::clone, no shift.
        let batch: Vec<Event> = self.iter.by_ref().take(self.batch_size).collect();
        self.remaining_hint = self.remaining_hint.saturating_sub(batch.len());
        Ok(batch)
    }

    async fn pause(&mut self) {
        self.paused = true;
    }

    async fn resume(&mut self) {
        self.paused = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::PartitionId;
    use bytes::Bytes;
    use std::sync::Arc;

    fn make_events(count: usize) -> Vec<Event> {
        (0..count)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i as i64,
                    Arc::from("test"),
                    PartitionId::new(0),
                    Bytes::from(format!("event-{i}")),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn memory_source_yields_all_events() {
        let events = make_events(10);
        let mut source = MemorySource::new(events, 3);

        let mut total = Vec::new();
        loop {
            let batch = source.next_batch().await.unwrap();
            if batch.is_empty() {
                break;
            }
            total.extend(batch);
        }
        assert_eq!(total.len(), 10);
        assert!(source.is_exhausted());
    }

    #[tokio::test]
    async fn memory_source_respects_batch_size() {
        let events = make_events(10);
        let mut source = MemorySource::new(events, 4);

        let batch1 = source.next_batch().await.unwrap();
        assert_eq!(batch1.len(), 4);

        let batch2 = source.next_batch().await.unwrap();
        assert_eq!(batch2.len(), 4);

        let batch3 = source.next_batch().await.unwrap();
        assert_eq!(batch3.len(), 2); // remaining

        let batch4 = source.next_batch().await.unwrap();
        assert!(batch4.is_empty()); // exhausted
    }

    #[tokio::test]
    async fn memory_source_reset() {
        let events = make_events(5);
        let mut source = MemorySource::new_resetable(events, 10);

        let _ = source.next_batch().await.unwrap();
        assert!(source.is_exhausted());

        source.reset();
        assert_eq!(source.remaining(), 5);

        let batch = source.next_batch().await.unwrap();
        assert_eq!(batch.len(), 5);
    }

    #[tokio::test]
    async fn memory_source_new_is_not_resetable() {
        let events = make_events(3);
        let mut source = MemorySource::new(events, 10);
        let _ = source.next_batch().await.unwrap();
        assert!(source.is_exhausted());
        source.reset(); // No-op for non-resetable source.
        assert!(source.is_exhausted());
    }
}
