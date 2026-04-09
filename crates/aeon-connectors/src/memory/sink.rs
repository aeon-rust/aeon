//! In-memory sink that collects outputs for testing.

use aeon_types::{AeonError, BatchResult, Output, Sink};

/// A sink that collects all outputs into a `Vec<Output>` for assertion.
///
/// Used for testing pipeline correctness — inspect outputs after pipeline completes.
pub struct MemorySink {
    outputs: Vec<Output>,
}

impl MemorySink {
    pub fn new() -> Self {
        Self {
            outputs: Vec::new(),
        }
    }

    /// Get all collected outputs.
    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }

    /// Take all collected outputs, leaving the sink empty.
    pub fn take_outputs(&mut self) -> Vec<Output> {
        std::mem::take(&mut self.outputs)
    }

    /// Number of collected outputs.
    pub fn len(&self) -> usize {
        self.outputs.len()
    }

    /// Whether no outputs have been collected.
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }
}

impl Default for MemorySink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for MemorySink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();
        self.outputs.extend(outputs);
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
    async fn memory_sink_collects_outputs() {
        let mut sink = MemorySink::new();
        let outputs = vec![
            Output::new(Arc::from("topic"), Bytes::from_static(b"a")),
            Output::new(Arc::from("topic"), Bytes::from_static(b"b")),
        ];

        sink.write_batch(outputs).await.unwrap();
        assert_eq!(sink.len(), 2);
        assert_eq!(sink.outputs()[0].payload.as_ref(), b"a");
        assert_eq!(sink.outputs()[1].payload.as_ref(), b"b");
    }

    #[tokio::test]
    async fn memory_sink_take_outputs() {
        let mut sink = MemorySink::new();
        sink.write_batch(vec![Output::new(Arc::from("t"), Bytes::from_static(b"x"))])
            .await
            .unwrap();

        let taken = sink.take_outputs();
        assert_eq!(taken.len(), 1);
        assert!(sink.is_empty());
    }
}
