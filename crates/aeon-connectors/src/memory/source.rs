//! In-memory source that serves pre-loaded events.

use std::sync::{Arc, Mutex};

use aeon_types::{AeonError, CoreLocalUuidGenerator, Event, PartitionId, Source};
use bytes::Bytes;

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

/// A source that synthesizes events on demand inside `next_batch`,
/// never pre-allocating the full event vec.
///
/// Sized for sustained load tests where the pre-loaded `MemorySource`
/// would OOM the process — e.g., a 10 M event run at 256 B payload
/// would pin 2.5 GiB of `Vec<Event>` up front. `StreamingMemorySource`
/// keeps only a single `Bytes` payload alive (refcounted; each emitted
/// event clones the `Arc`).
///
/// Set `count = 0` to run unbounded — every `next_batch` call produces
/// exactly `batch_size` events until paused or stopped.
///
/// Added 2026-04-18 to unblock the Session A 3-minute sustained sweep
/// called out in `docs/GATE2-ACCEPTANCE-PLAN.md § 11.5`.
pub struct StreamingMemorySource {
    /// Event count limit. `0` means unbounded.
    limit: usize,
    /// Number of events emitted so far.
    emitted: usize,
    batch_size: usize,
    paused: bool,
    payload: Bytes,
    source_name: Arc<str>,
    partition: PartitionId,
    /// Per-source UUIDv7 generator (SPSC pool, ~1-2ns per UUID).
    uuid_gen: Mutex<CoreLocalUuidGenerator>,
}

impl StreamingMemorySource {
    /// Create a streaming memory source.
    ///
    /// - `count = 0`: unbounded (runs until paused or task cancelled).
    /// - `count > 0`: bounded — emits exactly `count` events then
    ///   returns empty batches.
    pub fn new(count: usize, payload_size: usize, batch_size: usize) -> Self {
        Self {
            limit: count,
            emitted: 0,
            batch_size,
            paused: false,
            payload: Bytes::from(vec![0u8; payload_size]),
            source_name: Arc::from("memory"),
            partition: PartitionId::new(0),
            uuid_gen: Mutex::new(CoreLocalUuidGenerator::new(0)),
        }
    }

    /// Whether the bounded limit has been hit. Always `false` when
    /// `count == 0` (unbounded).
    pub fn is_exhausted(&self) -> bool {
        self.limit != 0 && self.emitted >= self.limit
    }
}

impl Source for StreamingMemorySource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        if self.paused || self.is_exhausted() {
            return Ok(Vec::new());
        }
        let want = if self.limit == 0 {
            self.batch_size
        } else {
            self.batch_size.min(self.limit - self.emitted)
        };
        let mut batch = Vec::with_capacity(want);
        let mut id_gen = self
            .uuid_gen
            .lock()
            .map_err(|_| AeonError::connection("UUID generator mutex poisoned"))?;
        for _ in 0..want {
            batch.push(Event::new(
                id_gen.next_uuid(),
                self.emitted as i64,
                Arc::clone(&self.source_name),
                self.partition,
                self.payload.clone(),
            ));
            self.emitted += 1;
        }
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

    // ── StreamingMemorySource ───────────────────────────────────────

    #[tokio::test]
    async fn streaming_bounded_emits_exactly_count_events() {
        let mut source = StreamingMemorySource::new(10, 32, 4);
        let mut total = 0usize;
        loop {
            let batch = source.next_batch().await.unwrap();
            if batch.is_empty() {
                break;
            }
            total += batch.len();
        }
        assert_eq!(total, 10);
        assert!(source.is_exhausted());
    }

    #[tokio::test]
    async fn streaming_bounded_respects_batch_size_and_tail() {
        let mut source = StreamingMemorySource::new(10, 16, 4);
        let b1 = source.next_batch().await.unwrap();
        assert_eq!(b1.len(), 4);
        let b2 = source.next_batch().await.unwrap();
        assert_eq!(b2.len(), 4);
        let b3 = source.next_batch().await.unwrap();
        assert_eq!(b3.len(), 2, "tail batch clamped to remaining");
        let b4 = source.next_batch().await.unwrap();
        assert!(b4.is_empty());
    }

    #[tokio::test]
    async fn streaming_unbounded_never_exhausts() {
        // count=0 → unbounded. Drain N batches and verify full size each time.
        let mut source = StreamingMemorySource::new(0, 8, 16);
        for _ in 0..1000 {
            let batch = source.next_batch().await.unwrap();
            assert_eq!(batch.len(), 16);
            assert!(!source.is_exhausted());
        }
    }

    #[tokio::test]
    async fn streaming_pause_returns_empty_batch() {
        let mut source = StreamingMemorySource::new(0, 8, 4);
        source.pause().await;
        let b = source.next_batch().await.unwrap();
        assert!(b.is_empty());
        source.resume().await;
        let b = source.next_batch().await.unwrap();
        assert_eq!(b.len(), 4);
    }

    #[tokio::test]
    async fn streaming_uses_payload_size() {
        let mut source = StreamingMemorySource::new(1, 128, 1);
        let batch = source.next_batch().await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].payload.len(), 128);
    }
}
