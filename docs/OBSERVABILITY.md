# Aeon Observability Guide

Aeon provides vendor-neutral observability built on OpenTelemetry (OTLP). Operators
choose their monitoring stack by configuring export targets -- Aeon's instrumentation
code never changes regardless of the backend.

---

## Table of Contents

1. [Overview](#overview)
2. [Architecture](#architecture)
3. [Supported Backends](#supported-backends)
4. [Configuration](#configuration)
5. [OTel Collector Setup](#otel-collector-setup)
6. [Traces](#traces)
7. [Metrics](#metrics)
8. [Logs](#logs)
9. [Docker Compose](#docker-compose)
10. [Grafana Dashboards](#grafana-dashboards)
11. [Production Setup](#production-setup)

---

## Overview

Aeon instruments itself across the three pillars of observability:

| Pillar  | Technology                            | Export Path                          |
|---------|---------------------------------------|--------------------------------------|
| Traces  | `tracing` + `tracing-opentelemetry`   | OTLP gRPC to OTel Collector         |
| Metrics | Atomic counters + lock-free histograms | Prometheus scrape on `/metrics`      |
| Logs    | `tracing-subscriber` (JSON or text)    | Stdout to container runtime (Loki)   |

All three are decoupled from any specific vendor. Aeon emits standard OTLP
for traces and logs, and exposes a Prometheus-compatible `/metrics` endpoint
for metrics. The OTel Collector acts as the routing layer, forwarding telemetry
to whichever backends the operator has deployed.

Key design decisions:

- **Zero-allocation metrics on the hot path.** All counters and histograms use
  `AtomicU64` with `Ordering::Relaxed` -- no locks, no heap allocation per event.
- **Feature-gated OTLP.** The `otlp` Cargo feature enables OpenTelemetry export.
  Without it, only stdout logging and the Prometheus endpoint are active.
- **PII/PHI masking.** Built-in `mask_pii()` and `mask_email()` utilities for
  sanitizing sensitive data before it reaches any telemetry backend.

---

## Architecture

```
Aeon Pipeline (tracing crate + atomic metrics)
    |
    |-- stdout layer (always) --> container runtime --> Loki / CloudWatch / etc.
    |-- OTLP layer (opt-in) ---> OTel Collector -----> SigNoz / Tempo / Jaeger / Elastic
    '-- /metrics endpoint ------> Prometheus scrape --> Mimir / VictoriaMetrics / Thanos
```

The OTLP layer is only active when two conditions are met:

1. Aeon is compiled with the `otlp` Cargo feature enabled.
2. The `OTEL_EXPORTER_OTLP_ENDPOINT` environment variable is set at runtime.

When either condition is not met, Aeon operates in stdout-only mode with the
Prometheus metrics endpoint still available.

### Data Flow

```
+------------------+       OTLP gRPC        +------------------+
|                  |  --------------------> |                  |
|   Aeon Engine    |   (traces + logs)      |  OTel Collector  |
|                  |                        |                  |
+------------------+                        +--------+---------+
        |                                            |
        | /metrics (HTTP)                   +--------+---------+
        v                                   |        |         |
+------------------+                   Jaeger  Prometheus  Loki
|   Prometheus     |                   /Tempo  /Mimir     /OpenObserve
+------------------+
        |
        v
+------------------+
|    Grafana       |
+------------------+
```

---

## Supported Backends

Aeon works with any backend that accepts OTLP or Prometheus scrape. The table
below lists tested and documented integrations.

| Backend          | Type           | Traces | Metrics | Logs | OTLP Support                  |
|------------------|----------------|--------|---------|------|-------------------------------|
| SigNoz           | Unified APM    | Yes    | Yes     | Yes  | Native (built on OTel)        |
| Grafana Tempo    | Traces         | Yes    | --      | --   | OTLP gRPC/HTTP                |
| Grafana Loki     | Logs           | --     | --      | Yes  | Via OTel Collector            |
| Grafana Mimir    | Metrics        | --     | Yes     | --   | Prometheus remote write       |
| Jaeger           | Traces         | Yes    | --      | --   | OTLP native (v1.35+)         |
| Elastic APM      | APM            | Yes    | Yes     | Yes  | OTLP native (v8.x+)          |
| OpenObserve      | Logs/Traces    | Yes    | Yes     | Yes  | OTLP ingestion                |
| Datadog          | Commercial APM | Yes    | Yes     | Yes  | OTLP ingestion via Agent      |
| Dynatrace        | Commercial APM | Yes    | Yes     | Yes  | OTLP ingestion                |
| VictoriaMetrics  | Metrics        | --     | Yes     | --   | Prometheus-compatible scrape  |
| Thanos           | Metrics        | --     | Yes     | --   | Prometheus-compatible scrape  |

To switch backends, modify the OTel Collector configuration. Aeon itself never
needs to change.

---

## Configuration

### Environment Variables

| Variable                         | Default  | Description                                           |
|----------------------------------|----------|-------------------------------------------------------|
| `OTEL_EXPORTER_OTLP_ENDPOINT`   | (none)   | OTLP endpoint, e.g. `http://otel-collector:4317`. When unset, OTLP export is disabled. |
| `OTEL_SERVICE_NAME`             | `aeon`   | Service name attached to all traces and spans.        |
| `OTEL_RESOURCE_ATTRIBUTES`      | (none)   | Additional OTel resource attributes (key=value pairs).|
| `RUST_LOG`                       | `info`   | Log level filter. Supports per-crate filters: `aeon_engine=debug,aeon_connectors=trace`. |
| `AEON_LOG_FORMAT`                | `text`   | Set to `json` for structured JSON output (Loki-compatible). |

### LogConfig Struct

The `LogConfig` struct controls logging behavior programmatically:

```rust
use aeon_observability::{LogConfig, init_observability};

let config = LogConfig {
    filter: "info".to_string(),       // RUST_LOG equivalent
    json: true,                        // JSON for Loki, false for human-readable
    with_file: false,                  // Include source file/line in output
    with_target: true,                 // Include module path in output
};

// Returns Ok(true) if OTLP was enabled, Ok(false) for stdout-only
let otlp_active = init_observability(&config)?;
```

### Initialization

```rust
use aeon_observability::{LogConfig, init_observability, shutdown_observability};

// Development: stdout only
init_observability(&LogConfig::default())?;

// Production: stdout + OTLP (set env vars first)
// OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4317
// OTEL_SERVICE_NAME=aeon-pipeline-1
init_observability(&LogConfig { json: true, ..Default::default() })?;

// Graceful shutdown: flush pending spans to OTLP backend
shutdown_observability();
```

---

## OTel Collector Setup

The OTel Collector acts as a vendor-neutral routing layer between Aeon and
observability backends. Aeon ships a ready-to-use configuration at
`docker/otel-collector-config.yaml`.

### Default Configuration

The default config receives OTLP on both gRPC (`:4317`) and HTTP (`:4318`), then
routes telemetry to three backends:

| Pipeline | Receiver | Processors      | Exporter           | Backend         |
|----------|----------|-----------------|--------------------|-----------------|
| Traces   | OTLP     | batch, resource | `otlp/jaeger`      | Jaeger `:4317`  |
| Metrics  | OTLP     | batch, resource | `prometheus`       | Scrape on `:8889` |
| Logs     | OTLP     | batch, resource | `loki`             | Loki `:3100`    |

The `batch` processor aggregates telemetry before export (5s timeout, 1024
batch size) to reduce backend load. The `resource` processor adds a
`deployment.environment=development` attribute to all signals.

### Switching Backends

To change where telemetry goes, edit only the `exporters` section and update
the `service.pipelines` references.

**Example: Replace Jaeger with Grafana Tempo**

```yaml
exporters:
  otlp/tempo:
    endpoint: tempo:4317
    tls:
      insecure: true

service:
  pipelines:
    traces:
      receivers: [otlp]
      processors: [batch, resource]
      exporters: [otlp/tempo]
```

**Example: Replace Loki with Elasticsearch**

```yaml
exporters:
  elasticsearch:
    endpoints: ["https://elasticsearch:9200"]
    logs_index: aeon-logs

service:
  pipelines:
    logs:
      receivers: [otlp]
      processors: [batch, resource]
      exporters: [elasticsearch]
```

**Example: SigNoz (all-in-one)**

```yaml
exporters:
  otlp/signoz:
    endpoint: signoz-otel-collector:4317
    tls:
      insecure: true

service:
  pipelines:
    traces:
      receivers: [otlp]
      processors: [batch, resource]
      exporters: [otlp/signoz]
    metrics:
      receivers: [otlp]
      processors: [batch, resource]
      exporters: [otlp/signoz]
    logs:
      receivers: [otlp]
      processors: [batch, resource]
      exporters: [otlp/signoz]
```

### Debugging the Collector

Uncomment the `debug` exporter in the config to see all telemetry in the
collector's stdout:

```yaml
exporters:
  debug:
    verbosity: detailed
```

---

## Traces

### Span Hierarchy

Aeon emits structured tracing spans for every pipeline stage. When OTLP export
is active, these become distributed traces visible in Jaeger, Tempo, SigNoz,
or any OTLP-compatible trace viewer.

| Span Name          | Level | Attributes                         | Description                       |
|--------------------|-------|------------------------------------|-----------------------------------|
| `pipeline`         | INFO  | `pipeline_name`                    | Root span for the entire pipeline |
| `source_batch`     | DEBUG | `batch_size`, `partition`          | Source polling a batch of events   |
| `processor_batch`  | DEBUG | `batch_size`                       | Processor handling a batch         |
| `sink_batch`       | DEBUG | `batch_size`                       | Sink writing a batch of outputs    |
| `retry`            | WARN  | `attempt`, `max_retries`           | A retry attempt on failure         |
| `dlq`              | WARN  | `reason`                           | Event routed to dead-letter queue  |

### Using Spans in Pipeline Code

```rust
use aeon_observability::{source_batch_span, processor_batch_span, sink_batch_span};

// Source stage
let _span = source_batch_span(batch.len(), partition_id);
// ... poll events ...
// span drops here, recording duration

// Processor stage
let _span = processor_batch_span(events.len());
// ... process events ...

// Sink stage
let _span = sink_batch_span(outputs.len());
// ... write outputs ...
```

### Viewing Traces

**Jaeger UI** (default): Open `http://localhost:16686`, select service `aeon`
(or the value of `OTEL_SERVICE_NAME`), and search for traces. Each trace shows
the full pipeline span tree from source through processor to sink.

**Grafana (via Jaeger datasource)**: Open `http://localhost:3000`, navigate to
Explore, select the Jaeger datasource, and query by service name.

---

## Metrics

### Metrics Endpoint

Aeon exposes a Prometheus-compatible `/metrics` endpoint via a built-in TCP
server (no HTTP framework dependency). By default this listens on port `9091`.

Prometheus is configured to scrape this endpoint every 5 seconds (see
`docker/prometheus.yml`):

```yaml
scrape_configs:
  - job_name: "aeon-engine"
    static_configs:
      - targets: ["host.docker.internal:9091"]
    scrape_interval: 5s
```

### Available Metrics

#### Throughput Counters

| Metric                              | Type    | Description                        |
|-------------------------------------|---------|------------------------------------|
| `aeon_events_received_total`        | Counter | Total events received from sources |
| `aeon_events_processed_total`       | Counter | Total events processed             |
| `aeon_outputs_sent_total`           | Counter | Total outputs sent to sinks        |
| `aeon_partition_received_total`     | Counter | Events received per partition (label: `partition`) |

#### Latency Histograms

All latency histograms use fixed exponential buckets in seconds, covering
sub-microsecond to 100ms range:

| Metric                              | Type      | Description                        |
|-------------------------------------|-----------|------------------------------------|
| `aeon_e2e_latency_seconds`          | Histogram | End-to-end latency (source to sink)|
| `aeon_source_latency_seconds`       | Histogram | Source batch polling latency       |
| `aeon_processor_latency_seconds`    | Histogram | Processor batch latency            |
| `aeon_sink_latency_seconds`         | Histogram | Sink batch write latency           |

Histogram bucket boundaries (in microseconds): 1, 5, 10, 25, 50, 100, 250,
500, 1000, 2500, 5000, 10000, 25000, 50000, 100000, +Inf.

#### Backpressure Gauges

| Metric                    | Type  | Description                                     |
|---------------------------|-------|-------------------------------------------------|
| `aeon_source_buffer_pct`  | Gauge | Source-to-processor SPSC buffer utilization (%)  |
| `aeon_sink_buffer_pct`    | Gauge | Processor-to-sink SPSC buffer utilization (%)    |
| `aeon_batch_size`         | Gauge | Current adaptive batch size                      |

#### Fault Tolerance

| Metric                                | Type    | Description                                     |
|---------------------------------------|---------|-------------------------------------------------|
| `aeon_dlq_total`                      | Counter | Events sent to dead-letter queue                |
| `aeon_retries_total`                  | Counter | Total retry attempts                            |
| `aeon_circuit_breaker_state`          | Gauge   | Circuit breaker state: 0=Closed, 1=Open, 2=Half-Open |
| `aeon_circuit_breaker_trips_total`    | Counter | Total circuit breaker trips                     |

### Implementation Details

All metrics are implemented using `AtomicU64` counters with `Ordering::Relaxed`
-- zero locks, zero allocations on the hot path. The `PipelineObservability`
struct provides the full metrics surface:

- Per-partition tracking for up to 64 partitions (pre-allocated array, no
  dynamic allocation)
- `LatencyHistogram` with lock-free bucket increments and binary search for
  bucket selection
- Prometheus exposition text generated on scrape via `to_prometheus()`

### Useful PromQL Queries

```promql
# Events per second (throughput)
rate(aeon_events_received_total[1m])

# End-to-end P99 latency
histogram_quantile(0.99, rate(aeon_e2e_latency_seconds_bucket[1m]))

# Processing overhead per stage (P95)
histogram_quantile(0.95, rate(aeon_source_latency_seconds_bucket[1m]))
histogram_quantile(0.95, rate(aeon_processor_latency_seconds_bucket[1m]))
histogram_quantile(0.95, rate(aeon_sink_latency_seconds_bucket[1m]))

# Buffer pressure (approaching backpressure when > 80%)
aeon_source_buffer_pct
aeon_sink_buffer_pct

# Event loss indicator (received - sent should be near zero)
aeon_events_received_total - aeon_outputs_sent_total

# DLQ rate
rate(aeon_dlq_total[5m])
```

---

## Logs

### Structured JSON Logging

When `LogConfig.json` is set to `true` (or `AEON_LOG_FORMAT=json`), Aeon emits
structured JSON logs to stdout. Each log line includes:

- `timestamp` -- ISO 8601
- `level` -- trace, debug, info, warn, error
- `target` -- Rust module path (when `with_target` is true)
- `message` -- Log message
- `span` -- Current tracing span context
- `threadId` -- Thread ID (always included in JSON mode)

Example JSON log line:

```json
{
  "timestamp": "2025-01-15T10:30:45.123456Z",
  "level": "INFO",
  "target": "aeon_engine::pipeline",
  "message": "batch processed",
  "span": {"name": "processor_batch", "batch_size": 256},
  "threadId": 7
}
```

### Loki Integration

Loki collects logs from container stdout. The OTel Collector can also forward
OTLP logs to Loki. The shipped Loki configuration (`docker/loki-config.yaml`)
uses:

- **Storage**: Local filesystem (`/loki/chunks`)
- **Schema**: TSDB with v13 schema
- **Retention**: No automatic deletion (configure `limits_config` for production)
- **Auth**: Disabled for local development

Query Aeon logs in Grafana via the Loki datasource:

```logql
{job="aeon-engine"} | json | level="ERROR"
{job="aeon-engine"} |= "batch processed" | json | batch_size > 100
```

### PII/PHI Masking

For environments subject to data protection regulations, use the built-in
masking utilities before logging sensitive values:

```rust
use aeon_observability::{mask_pii, mask_email};

tracing::info!(user = mask_pii("john.doe"), "user action");
// Output: user=j******e

tracing::info!(email = mask_email("user@example.com").as_str(), "notification sent");
// Output: email=u**r@e*********m
```

---

## Docker Compose

The `docker-compose.yml` includes a complete observability stack. Start services
selectively depending on your needs.

### Minimal (No Observability)

```bash
docker compose up -d redpanda redpanda-console
```

### Full Observability Stack

```bash
docker compose up -d redpanda redpanda-console \
  otel-collector prometheus grafana jaeger loki
```

### Service Ports

| Service          | Port    | Description                     |
|------------------|---------|---------------------------------|
| OTel Collector   | 4317    | OTLP gRPC receiver              |
| OTel Collector   | 4318    | OTLP HTTP receiver               |
| OTel Collector   | 8889    | Prometheus metrics from collector |
| Prometheus       | 9090    | Prometheus UI and API            |
| Grafana          | 3000    | Dashboards and exploration       |
| Jaeger           | 16686   | Jaeger trace UI                  |
| Loki             | 3100    | Loki log push/query API         |
| Aeon Engine      | 9091    | Prometheus metrics scrape        |

### Environment Variables for Docker

Set these in a `.env` file alongside `docker-compose.yml`:

```
GRAFANA_ADMIN_PASSWORD=changeme
POSTGRES_PASSWORD=changeme
```

Optional:

```
GRAFANA_ADMIN_USER=admin
GRAFANA_ANONYMOUS_ENABLED=false
```

---

## Grafana Dashboards

### Pre-Provisioned Datasources

Grafana is auto-provisioned with three datasources (see
`docker/grafana/provisioning/datasources/datasources.yaml`):

| Datasource  | Type       | URL                      | Default |
|-------------|------------|--------------------------|---------|
| Prometheus  | prometheus | `http://prometheus:9090`  | Yes     |
| Loki        | loki       | `http://loki:3100`        | No      |
| Jaeger      | jaeger     | `http://jaeger:16686`     | No      |

### Aeon Pipeline Dashboard

A pre-built dashboard is provisioned at
`docker/grafana/provisioning/dashboards/aeon-pipeline.json` and appears under
the **Aeon** folder in Grafana.

The dashboard includes these panels:

| Panel                        | Type           | Key Queries                                      |
|------------------------------|----------------|--------------------------------------------------|
| Events Throughput            | Time series    | `rate(aeon_events_received_total[1m])`, `rate(aeon_events_processed_total[1m])`, `rate(aeon_outputs_sent_total[1m])` |
| Event Latency (P50/P95/P99) | Time series    | `histogram_quantile` over `aeon_e2e_latency_seconds_bucket` |
| Per-Partition Throughput     | Time series    | `rate(aeon_partition_received_total[1m])` by partition |
| Buffer Utilization           | Gauge          | `aeon_source_buffer_pct`, `aeon_sink_buffer_pct` (green/yellow/red thresholds at 0/60/90%) |
| Batch Size                   | Stat           | `aeon_batch_size`                                |
| Fault Tolerance              | Stat           | `aeon_dlq_total`, `aeon_retries_total`, `aeon_circuit_breaker_trips_total` |
| Circuit Breaker State        | State timeline | `aeon_circuit_breaker_state` (0=Closed/green, 1=Open/red, 2=Half-Open/yellow) |
| Stage Latency Breakdown      | Time series    | P95 of `aeon_source_latency_seconds`, `aeon_processor_latency_seconds`, `aeon_sink_latency_seconds` |

### Accessing Grafana

1. Open `http://localhost:3000`.
2. Log in with `admin` / the value of `GRAFANA_ADMIN_PASSWORD`.
3. Navigate to Dashboards > Aeon > Aeon Pipeline.
4. Use the Explore view with the Loki datasource for log queries.
5. Use the Explore view with the Jaeger datasource for trace search.

---

## Production Setup

### Recommendations

**OTLP Export**

- Always compile with the `otlp` Cargo feature in production builds.
- Set `OTEL_EXPORTER_OTLP_ENDPOINT` to point at a dedicated OTel Collector
  instance (not localhost).
- Set `OTEL_SERVICE_NAME` to a unique identifier per pipeline instance
  (e.g., `aeon-orders-pipeline-1`).
- Use `OTEL_RESOURCE_ATTRIBUTES` for environment tagging:
  `deployment.environment=production,service.version=3.1.0`.

**Logging**

- Use JSON log format (`AEON_LOG_FORMAT=json` or `LogConfig { json: true }`).
- Set `RUST_LOG=info` as baseline. Use `aeon_engine=debug` only for
  troubleshooting -- DEBUG spans on the hot path add overhead.
- Ship container stdout to a log aggregator (Loki, CloudWatch, Datadog)
  via the container runtime or a sidecar.

**Metrics**

- Scrape the `/metrics` endpoint at 10-15 second intervals. The 5-second
  interval in the development config is aggressive for production.
- For high-availability metrics, use Prometheus with remote write to Mimir,
  Thanos, or VictoriaMetrics.
- Set up alerts on key signals:
  - `rate(aeon_events_received_total[5m]) == 0` -- pipeline stalled
  - `aeon_circuit_breaker_state == 1` -- circuit breaker open
  - `aeon_source_buffer_pct > 90` -- backpressure, risk of event loss
  - `rate(aeon_dlq_total[5m]) > 0` -- events failing processing

**Traces**

- Use tail-based sampling in the OTel Collector for production to reduce
  storage costs. Sample 100% of error traces, 1-10% of normal traces.
- Ensure `shutdown_observability()` is called before process exit to flush
  pending spans.

**OTel Collector**

- Deploy the OTel Collector as a sidecar or DaemonSet, not as a centralized
  service, to avoid single points of failure.
- Tune the batch processor: increase `send_batch_size` and `timeout` for
  higher-throughput pipelines to reduce export overhead.
- Enable TLS between Aeon and the collector in production (remove
  `tls.insecure: true` from exporter configs).

**Graceful Shutdown**

Always call `shutdown_observability()` before the process exits. This flushes
any buffered spans to the OTLP backend. Without this, the last few seconds of
trace data will be lost.

```rust
// In main() or signal handler
shutdown_observability();
```

### Cargo Feature Flags

The `aeon-observability` crate uses feature flags to minimize dependencies:

| Feature | Dependencies Added                                                        | Purpose              |
|---------|---------------------------------------------------------------------------|----------------------|
| (none)  | `tracing`, `tracing-subscriber`                                           | Stdout logging only  |
| `otlp`  | `tracing-opentelemetry`, `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` | OTLP trace export    |

For production builds:

```toml
[dependencies]
aeon-observability = { path = "../aeon-observability", features = ["otlp"] }
```

For test/CI builds where OTLP is not needed, omit the feature to avoid pulling
in OpenTelemetry dependencies.
