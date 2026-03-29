//! Prometheus metrics, latency histograms, structured logging, and PII masking.
//!
//! Provides:
//! - `LatencyHistogram`: lock-free histogram with fixed exponential buckets
//! - `PipelineObservability`: comprehensive metrics (per-partition, latency, fault tolerance)
//! - `init_logging`: structured JSON logging for Loki integration
//! - `mask_pii` / `mask_email`: PII/PHI masking utilities

pub mod histogram;
pub mod logging;
pub mod metrics;
pub mod tracing_spans;

pub use histogram::LatencyHistogram;
pub use logging::{LogConfig, init_logging, mask_email, mask_pii};
pub use metrics::PipelineObservability;
pub use tracing_spans::{
    dlq_span, pipeline_span, processor_batch_span, retry_span, sink_batch_span, source_batch_span,
};
