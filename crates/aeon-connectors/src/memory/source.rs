//! In-memory source that serves pre-loaded events.

use aeon_types::{AeonError, Event, Source};

/// A source that yields events from a pre-loaded `Vec<Event>`.
///
/// Returns events in configurable batch sizes. Once exhausted, returns
/// empty batches. Used for testing and benchmarking.
pub struct MemorySource {
    events: Vec<Event>,
    position: usize,
    batch_size: usize,
    paused: bool,
}

impl MemorySource {
    /// Create a new `MemorySource` from a vec of events.
    pub fn new(events: Vec<Event>, batch_size: usize) -> Self {
        Self {
            events,
            position: 0,
            batch_size,
            paused: false,
        }
    }

    /// Number of events remaining.
    pub fn remaining(&self) -> usize {
        self.events.len().saturating_sub(self.position)
    }

    /// Whether all events have been consumed.
    pub fn is_exhausted(&self) -> bool {
        self.position >= self.events.len()
    }

    /// Reset position to beginning (for repeated benchmarks).
    pub fn reset(&mut self) {
        self.position = 0;
    }
}

impl Source for MemorySource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        if self.paused || self.position >= self.events.len() {
            return Ok(Vec::new());
        }

        let end = (self.position + self.batch_size).min(self.events.len());
        let batch = self.events[self.position..end].to_vec();
        self.position = end;
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
        let mut source = MemorySource::new(events, 10);

        let _ = source.next_batch().await.unwrap();
        assert!(source.is_exhausted());

        source.reset();
        assert_eq!(source.remaining(), 5);

        let batch = source.next_batch().await.unwrap();
        assert_eq!(batch.len(), 5);
    }
}
