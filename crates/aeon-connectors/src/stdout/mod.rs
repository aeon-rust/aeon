//! Stdout sink — prints outputs to stdout for debugging.

use aeon_types::{AeonError, BatchResult, Output, Sink};

/// A sink that prints each output to stdout.
///
/// Used for debugging pipelines. Not suitable for hot-path benchmarking.
pub struct StdoutSink {
    count: u64,
}

impl StdoutSink {
    pub fn new() -> Self {
        Self { count: 0 }
    }

    pub fn count(&self) -> u64 {
        self.count
    }
}

impl Default for StdoutSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for StdoutSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();
        for output in &outputs {
            let payload = String::from_utf8_lossy(&output.payload);
            println!("[{}] {}", output.destination, payload);
        }
        self.count += outputs.len() as u64;
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
    async fn stdout_sink_counts() {
        let mut sink = StdoutSink::new();
        let outputs = vec![
            Output::new(Arc::from("topic"), Bytes::from_static(b"hello")),
            Output::new(Arc::from("topic"), Bytes::from_static(b"world")),
        ];
        sink.write_batch(outputs).await.unwrap();
        assert_eq!(sink.count(), 2);
    }
}
