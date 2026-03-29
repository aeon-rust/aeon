//! Tracing span helpers for pipeline instrumentation.
//!
//! Provides pre-defined span constructors for each pipeline stage.
//! These work with any `tracing` subscriber — including the JSON
//! subscriber for Loki and (future) OTLP exporter for Jaeger.
//!
//! Usage in pipeline code:
//! ```ignore
//! let _span = source_batch_span(batch_size, partition);
//! // ... do source work ...
//! // span drops here, recording duration
//! ```

use tracing::{Level, span};

/// Create a tracing span for a source batch operation.
#[inline]
pub fn source_batch_span(batch_size: usize, partition: u16) -> tracing::span::EnteredSpan {
    span!(
        Level::DEBUG,
        "source_batch",
        batch_size = batch_size,
        partition = partition,
    )
    .entered()
}

/// Create a tracing span for a processor batch operation.
#[inline]
pub fn processor_batch_span(batch_size: usize) -> tracing::span::EnteredSpan {
    span!(Level::DEBUG, "processor_batch", batch_size = batch_size,).entered()
}

/// Create a tracing span for a sink write batch operation.
#[inline]
pub fn sink_batch_span(batch_size: usize) -> tracing::span::EnteredSpan {
    span!(Level::DEBUG, "sink_batch", batch_size = batch_size,).entered()
}

/// Create a tracing span for the full pipeline run.
#[inline]
pub fn pipeline_span(name: &str) -> tracing::span::EnteredSpan {
    span!(Level::INFO, "pipeline", pipeline_name = name,).entered()
}

/// Create a tracing span for a retry attempt.
#[inline]
pub fn retry_span(attempt: u32, max_retries: u32) -> tracing::span::EnteredSpan {
    span!(
        Level::WARN,
        "retry",
        attempt = attempt,
        max_retries = max_retries,
    )
    .entered()
}

/// Create a tracing span for a DLQ record.
#[inline]
pub fn dlq_span(reason: &str) -> tracing::span::EnteredSpan {
    span!(Level::WARN, "dlq", reason = reason,).entered()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_batch_span_creates() {
        let _span = source_batch_span(100, 0);
        // Span creation doesn't panic — subscriber-independent
    }

    #[test]
    fn processor_batch_span_creates() {
        let _span = processor_batch_span(256);
    }

    #[test]
    fn sink_batch_span_creates() {
        let _span = sink_batch_span(64);
    }

    #[test]
    fn pipeline_span_creates() {
        let _span = pipeline_span("test-pipeline");
    }

    #[test]
    fn retry_span_creates() {
        let _span = retry_span(2, 5);
    }

    #[test]
    fn dlq_span_creates() {
        let _span = dlq_span("bad format");
    }
}
