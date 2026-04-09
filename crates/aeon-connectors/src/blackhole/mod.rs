//! Blackhole sink — discards all outputs. Used for benchmarking Aeon's internal ceiling.

use aeon_types::{AeonError, BatchResult, Output, Sink};
use std::sync::atomic::{AtomicU64, Ordering};

/// A sink that discards all outputs and counts them.
///
/// Used for benchmarking — measures Aeon's throughput without external I/O.
/// The event count is tracked atomically for concurrent access from metrics.
pub struct BlackholeSink {
    count: AtomicU64,
}

impl BlackholeSink {
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
        }
    }

    /// Number of outputs received (discarded).
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Reset counter to zero.
    pub fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
    }
}

impl Default for BlackholeSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for BlackholeSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        self.count
            .fetch_add(outputs.len() as u64, Ordering::Relaxed);
        let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();
        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::Arc;

    #[tokio::test]
    async fn blackhole_counts_outputs() {
        let mut sink = BlackholeSink::new();
        let outputs: Vec<Output> = (0..100)
            .map(|_| Output::new(Arc::from("t"), Bytes::from_static(b"x")))
            .collect();

        sink.write_batch(outputs).await.unwrap();
        assert_eq!(sink.count(), 100);
    }

    #[tokio::test]
    async fn blackhole_accumulates_across_batches() {
        let mut sink = BlackholeSink::new();

        for _ in 0..5 {
            let outputs: Vec<Output> = (0..10)
                .map(|_| Output::new(Arc::from("t"), Bytes::from_static(b"x")))
                .collect();
            sink.write_batch(outputs).await.unwrap();
        }

        assert_eq!(sink.count(), 50);
    }

    #[tokio::test]
    async fn blackhole_reset() {
        let mut sink = BlackholeSink::new();
        sink.write_batch(vec![Output::new(Arc::from("t"), Bytes::from_static(b"x"))])
            .await
            .unwrap();
        assert_eq!(sink.count(), 1);

        sink.reset();
        assert_eq!(sink.count(), 0);
    }
}
