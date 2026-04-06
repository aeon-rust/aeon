//! OpenTelemetry OTLP integration — vendor-neutral observability export.
//!
//! When the `otlp` feature is enabled and `OTEL_EXPORTER_OTLP_ENDPOINT` is set,
//! Aeon exports traces (and structured log spans) via OTLP gRPC to any
//! OpenTelemetry-compatible backend:
//!
//! - **SigNoz** — unified APM (built on ClickHouse)
//! - **Grafana Tempo** — distributed tracing backend
//! - **Jaeger** — CNCF tracing (accepts OTLP natively)
//! - **Elastic APM** — search-driven APM
//! - **OpenObserve** — cost-efficient log/trace storage
//! - **Datadog / Dynatrace** — commercial (accept OTLP)
//! - **OTel Collector** — routing layer to any combination of backends
//!
//! ## Architecture
//!
//! ```text
//! Aeon (tracing crate)
//!     ├─ fmt layer → stdout (always present, for container runtime)
//!     └─ tracing-opentelemetry layer → OTLP gRPC export
//!         → OTel Collector / SigNoz / Tempo / Jaeger / etc.
//! ```
//!
//! ## Configuration (environment variables)
//!
//! | Variable | Default | Description |
//! |----------|---------|-------------|
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | (none) | OTLP endpoint (e.g., `http://localhost:4317`) |
//! | `OTEL_SERVICE_NAME` | `aeon` | Service name in traces |
//! | `OTEL_RESOURCE_ATTRIBUTES` | (none) | Additional resource attributes |
//!
//! When `OTEL_EXPORTER_OTLP_ENDPOINT` is unset, only stdout logging is active.

#[cfg(feature = "otlp")]
use opentelemetry::KeyValue;
#[cfg(feature = "otlp")]
use opentelemetry::trace::TracerProvider as _;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::Resource;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::runtime::Tokio;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::trace::TracerProvider;
#[cfg(feature = "otlp")]
use tracing_opentelemetry::OpenTelemetryLayer;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::LogConfig;

/// Initialize the unified observability stack.
///
/// Always configures a stdout logging layer (JSON or human-readable).
/// When the `otlp` feature is enabled AND `OTEL_EXPORTER_OTLP_ENDPOINT` is set,
/// also configures an OTLP trace export layer.
///
/// This replaces `init_logging()` for production use. The old `init_logging()`
/// remains available for tests and simple setups.
///
/// # Returns
///
/// `Ok(true)` if OTLP export was enabled, `Ok(false)` if stdout-only.
pub fn init_observability(config: &LogConfig) -> Result<bool, String> {
    let filter = EnvFilter::try_new(&config.filter)
        .map_err(|e| format!("invalid log filter '{}': {e}", config.filter))?;

    #[cfg(feature = "otlp")]
    {
        if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
            return init_with_otlp(config, filter);
        }
    }

    // Stdout-only fallback (no OTLP endpoint configured)
    init_stdout_only(config, filter)?;
    Ok(false)
}

/// Initialize stdout-only logging (no OTLP).
fn init_stdout_only(config: &LogConfig, filter: EnvFilter) -> Result<(), String> {
    if config.json {
        let layer = fmt::layer()
            .json()
            .with_file(config.with_file)
            .with_target(config.with_target)
            .with_thread_ids(true);

        tracing_subscriber::registry()
            .with(filter)
            .with(layer)
            .try_init()
            .map_err(|e| format!("failed to init logging: {e}"))
    } else {
        let layer = fmt::layer()
            .with_file(config.with_file)
            .with_target(config.with_target);

        tracing_subscriber::registry()
            .with(filter)
            .with(layer)
            .try_init()
            .map_err(|e| format!("failed to init logging: {e}"))
    }
}

/// Initialize stdout + OTLP dual export.
///
/// The OTLP exporter reads `OTEL_EXPORTER_OTLP_ENDPOINT` automatically
/// (standard OTel SDK behavior). Service name comes from `OTEL_SERVICE_NAME`
/// or defaults to "aeon".
#[cfg(feature = "otlp")]
fn init_with_otlp(config: &LogConfig, filter: EnvFilter) -> Result<bool, String> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_string());

    // The OTLP exporter reads OTEL_EXPORTER_OTLP_ENDPOINT from env automatically
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .map_err(|e| format!("failed to create OTLP exporter: {e}"))?;

    let service_name = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "aeon".to_string());

    let resource = Resource::new(vec![KeyValue::new("service.name", service_name.clone())]);

    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, Tokio)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("aeon");

    // Register globally so shutdown_tracer_provider() works
    let _prev = opentelemetry::global::set_tracer_provider(provider);

    // Build the OTLP tracing layer
    let otel_layer = OpenTelemetryLayer::new(tracer);

    // Build stdout layer (always present — container runtime captures it)
    // The OTel layer wraps the fmt+filter subscriber, adding trace context propagation.
    if config.json {
        let fmt_layer = fmt::layer()
            .json()
            .with_file(config.with_file)
            .with_target(config.with_target)
            .with_thread_ids(true);

        tracing_subscriber::registry()
            .with(otel_layer)
            .with(filter)
            .with(fmt_layer)
            .try_init()
            .map_err(|e| format!("failed to init observability: {e}"))?;
    } else {
        let fmt_layer = fmt::layer()
            .with_file(config.with_file)
            .with_target(config.with_target);

        tracing_subscriber::registry()
            .with(otel_layer)
            .with(filter)
            .with(fmt_layer)
            .try_init()
            .map_err(|e| format!("failed to init observability: {e}"))?;
    }

    tracing::info!(
        endpoint = endpoint.as_str(),
        service_name = service_name.as_str(),
        "OTLP trace export enabled"
    );

    Ok(true)
}

/// Gracefully shut down the OpenTelemetry pipeline.
///
/// Call this before process exit to flush pending trace spans to the
/// configured OTLP backend (SigNoz, Tempo, Jaeger, etc.).
/// No-op when the `otlp` feature is disabled or OTLP was not initialized.
pub fn shutdown_observability() {
    #[cfg(feature = "otlp")]
    {
        opentelemetry::global::shutdown_tracer_provider();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_observability_stdout_only() {
        // In test context, subscriber may already be set — that's OK.
        let config = LogConfig::default();
        let result = init_observability(&config);
        // Either succeeds (first test) or fails (subscriber already set)
        match result {
            Ok(otlp_enabled) => assert!(!otlp_enabled, "no OTLP endpoint set"),
            Err(e) => assert!(e.contains("already"), "expected 'already set' error: {e}"),
        }
    }

    #[test]
    fn invalid_filter_returns_error() {
        let config = LogConfig {
            filter: "[invalid".to_string(),
            ..Default::default()
        };
        assert!(init_observability(&config).is_err());
    }

    #[test]
    fn shutdown_is_safe_without_init() {
        // Should not panic even if OTLP was never initialized
        shutdown_observability();
    }
}
