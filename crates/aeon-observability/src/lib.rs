//! Vendor-neutral observability for Aeon.
//!
//! Provides a standardized observability interface that decouples Aeon from
//! any specific monitoring backend. Operators choose their stack by configuring
//! the export target — Aeon's instrumentation code never changes.
//!
//! ## Architecture
//!
//! ```text
//! Aeon Pipeline (tracing crate + atomic metrics)
//!     │
//!     ├─ stdout layer (always) ── container runtime ── Loki / CloudWatch / etc.
//!     ├─ OTLP layer (opt-in) ──── OTel Collector ──── SigNoz / Tempo / Jaeger / Elastic
//!     └─ /metrics endpoint ─────── Prometheus scrape ── Mimir / VictoriaMetrics / Thanos
//! ```
//!
//! ## Supported Backends (via OTLP)
//!
//! | Backend | Type | OTLP Support |
//! |---------|------|-------------|
//! | SigNoz | Unified APM | Native (built on OTel) |
//! | Grafana Tempo | Traces | OTLP gRPC/HTTP |
//! | Grafana Loki | Logs | Via OTel Collector |
//! | Jaeger | Traces | OTLP native |
//! | Elastic APM | APM | OTLP native |
//! | OpenObserve | Logs/Traces | OTLP ingestion |
//! | Datadog | Commercial | OTLP ingestion |
//! | Dynatrace | Commercial | OTLP ingestion |
//!
//! ## Quick Start
//!
//! ```ignore
//! use aeon_observability::{LogConfig, init_observability, shutdown_observability};
//!
//! // Stdout-only (development)
//! init_observability(&LogConfig::default())?;
//!
//! // Stdout + OTLP (production — set env var)
//! // OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4317
//! // OTEL_SERVICE_NAME=aeon-pipeline-1
//! init_observability(&LogConfig { json: true, ..Default::default() })?;
//!
//! // At shutdown
//! shutdown_observability();
//! ```
//!
//! ## Components
//!
//! - [`init_observability`]: Unified initialization (stdout + optional OTLP)
//! - [`shutdown_observability`]: Graceful OTLP pipeline flush
//! - [`LatencyHistogram`]: Lock-free histogram with Prometheus exposition
//! - [`PipelineObservability`]: Comprehensive pipeline metrics (per-partition, latency, faults)
//! - [`init_logging`]: Simple stdout-only logging (tests, backwards compat)
//! - [`mask_pii`] / [`mask_email`]: PII/PHI masking utilities
//! - Span helpers: Pre-defined spans for source/processor/sink instrumentation

// FT-10: no-panic policy. Production code in this crate must not use
// `.unwrap()` or `.expect()` except for explicitly-documented startup-time
// invariants, which must carry an `#[allow(...)]` attribute with rationale.
// Test modules and benches are exempt (`cfg(not(test))`).
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod histogram;
pub mod logging;
pub mod metrics;
pub mod otel;
pub mod tracing_spans;

pub use histogram::LatencyHistogram;
pub use logging::{LogConfig, init_logging, mask_email, mask_pii};
pub use metrics::PipelineObservability;
pub use otel::{init_observability, shutdown_observability};
pub use tracing_spans::{
    dlq_span, pipeline_span, processor_batch_span, retry_span, sink_batch_span, source_batch_span,
};
