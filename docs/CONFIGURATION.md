# Aeon Configuration Reference

Complete reference for all Aeon configuration options: environment variables,
pipeline tuning, delivery guarantees, checkpointing, dead-letter queues,
observability, and YAML manifests.

---

## Table of Contents

1. [Environment Variables](#environment-variables)
2. [Pipeline Configuration](#pipeline-configuration)
3. [Delivery Configuration](#delivery-configuration)
4. [Delivery Strategies](#delivery-strategies)
5. [Delivery Semantics](#delivery-semantics)
6. [Flush Strategy](#flush-strategy)
7. [Checkpoint Configuration](#checkpoint-configuration)
8. [Batch Failure Policy](#batch-failure-policy)
9. [Dead-Letter Queue (DLQ)](#dead-letter-queue-dlq)
10. [Adaptive Batch Tuner](#adaptive-batch-tuner)
11. [Observability Configuration](#observability-configuration)
12. [YAML Manifest](#yaml-manifest)

---

## Environment Variables

### Aeon Core

| Variable | Default | Description |
|----------|---------|-------------|
| `AEON_API_TOKEN` | (unset) | Bearer token for REST API authentication. **Required in production.** When unset, authentication is disabled (dev mode). |
| `AEON_API_URL` | `http://localhost:4471` | REST API address used by the CLI (`aeon processor`, `aeon pipeline`, `aeon apply`, etc.). |
| `AEON_API_ADDR` | `0.0.0.0:4471` | Bind address for the REST API server (engine-side). |
| `AEON_CHECKPOINT_DIR` | OS temp dir | Directory for WAL checkpoint files. **Set to a persistent volume in production.** |
| `AEON_LOG_FORMAT` | `pretty` | Log output format. `json` for production (Loki-compatible structured logs), `pretty` for human-readable development output. |
| `AEON_BROKERS` | `redpanda:9092` | Kafka/Redpanda broker addresses for source and sink connectors. |
| `RUST_LOG` | `info` | Standard `tracing` log filter. Supports per-crate granularity, e.g. `info,aeon_engine=debug,aeon_connectors=trace`. |

### OpenTelemetry

| Variable | Default | Description |
|----------|---------|-------------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | (unset) | OTLP gRPC endpoint (e.g. `http://otel-collector:4317`). When unset, only stdout logging is active. When set, Aeon exports traces and structured log spans via OTLP. |
| `OTEL_SERVICE_NAME` | `aeon` | Service name attached to all exported traces and metrics. |
| `OTEL_RESOURCE_ATTRIBUTES` | (unset) | Additional OpenTelemetry resource attributes (key=value pairs). |

### Benchmark Variables

These are used by the benchmark Docker image and Criterion-based benchmarks.

| Variable | Default | Description |
|----------|---------|-------------|
| `AEON_BENCH_BROKERS` | `redpanda:9092` (Docker) / `localhost:19092` (local) | Broker address for benchmark runs. |
| `AEON_BENCH_EVENT_COUNT` | `100000` | Number of events for Redpanda benchmarks. |
| `AEON_BENCH_PAYLOAD_SIZE` | `256` | Event payload size in bytes. |
| `AEON_BENCH_NUM_PARTITIONS` | `16` | Number of Redpanda partitions for key distribution benchmarks. |
| `AEON_BENCH_BATCH_SIZE` | `1024` | Batch size for multi-partition benchmarks. |
| `AEON_BENCH_PARTITION_COUNTS` | `1,2,4,8` | Comma-separated partition counts for scaling benchmarks. |
| `AEON_BENCH_BLACKHOLE_CEILING` | `7500000` | Blackhole throughput ceiling for headroom ratio calculation. |
| `AEON_BENCH_HEADROOM_TARGET` | `5.0` | Minimum headroom ratio for pass/fail determination. |
| `AEON_BENCH_SAMPLE_SIZE` | `10` | Criterion sample size. |
| `AEON_BENCH_WARMUP_TIME` | `1` | Criterion warmup time in seconds. |
| `AEON_BENCH_MEASUREMENT_TIME` | `2` | Criterion measurement time in seconds. |
| `AEON_BENCH_RUN_NUMBER` | `5` | Run number displayed in benchmark entrypoint header. |
| `AEON_BENCH_RUST_WASM` | `/bench/wasm/aeon_sample_rust_wasm.wasm` | Path to Rust Wasm processor artifact for benchmarks. |
| `AEON_BENCH_AS_WASM` | `/bench/wasm/as_processor.wasm` | Path to AssemblyScript Wasm processor artifact for benchmarks. |

### Infrastructure (Docker Compose)

| Variable | Default | Description |
|----------|---------|-------------|
| `POSTGRES_USER` | `aeon` | PostgreSQL username. |
| `POSTGRES_PASSWORD` | (required) | PostgreSQL password. |
| `POSTGRES_DB` | `aeon` | PostgreSQL database name. |
| `MYSQL_ROOT_PASSWORD` | (required) | MySQL root password. |
| `MYSQL_USER` | `aeon` | MySQL username. |
| `MYSQL_PASSWORD` | (required) | MySQL password. |
| `MYSQL_DB` | `aeon` | MySQL database name. |
| `RABBITMQ_USER` | `aeon` | RabbitMQ username. |
| `RABBITMQ_PASSWORD` | (required) | RabbitMQ password. |
| `GRAFANA_ADMIN_USER` | `admin` | Grafana admin username. |
| `GRAFANA_ADMIN_PASSWORD` | (required) | Grafana admin password. |
| `GRAFANA_ANONYMOUS_ENABLED` | `false` | Enable anonymous Grafana access (dev only). |
| `REDPANDA_VERSION` | `v24.2.12` | Redpanda container image version. |
| `REDPANDA_SMP` | `4` | Redpanda CPU cores. |
| `REDPANDA_MEMORY` | `4G` | Redpanda memory limit. |
| `REDPANDA_LOG_LEVEL` | `warn` | Redpanda log verbosity. |

---

## Pipeline Configuration

`PipelineConfig` controls the runtime behavior of a single pipeline instance.

```rust
pub struct PipelineConfig {
    pub source_buffer_capacity: usize,  // Default: 8192
    pub sink_buffer_capacity: usize,    // Default: 8192
    pub max_batch_size: usize,          // Default: 1024
    pub core_pinning: CorePinning,      // Default: Disabled
    pub delivery: DeliveryConfig,       // See below
}
```

### Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `source_buffer_capacity` | `usize` | `8192` | SPSC ring buffer capacity between source and processor. Larger values absorb source burst at the cost of memory. |
| `sink_buffer_capacity` | `usize` | `8192` | SPSC ring buffer capacity between processor and sink. Size relative to sink latency. |
| `max_batch_size` | `usize` | `1024` | Maximum events per `process_batch()` call. Limits work per iteration for latency control. |
| `core_pinning` | `CorePinning` | `Disabled` | CPU core pinning strategy for pipeline tasks (see below). |
| `delivery` | `DeliveryConfig` | See [Delivery Configuration](#delivery-configuration) | Full delivery configuration. |

### Core Pinning

Core pinning eliminates OS-level thread migration, keeping L1/L2 caches warm for each pipeline stage.

| Variant | Description |
|---------|-------------|
| `Disabled` | No core pinning. Let the OS scheduler decide. Best for containers, shared VMs, oversubscribed systems. **Default.** |
| `Auto` | Automatically assign cores. Skips core 0 (OS/runtime), assigns source/processor/sink to consecutive cores. Falls back to `Disabled` if fewer than 3 cores are available. |
| `Manual(PipelineCores)` | Specify exact core IDs per stage. Use for NUMA-aware placement or co-location with specific hardware (NIC, storage controller). |

---

## Delivery Configuration

`DeliveryConfig` controls how events are delivered, what happens on failure, and how
progress is checkpointed. It is nested inside `PipelineConfig`.

```rust
pub struct DeliveryConfig {
    pub strategy: DeliveryStrategy,           // Default: OrderedBatch
    pub semantics: DeliverySemantics,         // Default: AtLeastOnce
    pub failure_policy: BatchFailurePolicy,   // Default: RetryFailed
    pub flush: FlushStrategy,                 // Default: 1s interval, 50K max pending
    pub checkpoint: CheckpointConfig,         // Default: WAL backend, 24h retention
    pub max_retries: u32,                     // Default: 0
    pub retry_backoff: Duration,              // Default: 0ms
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `strategy` | `DeliveryStrategy` | `OrderedBatch` | How events are delivered to the sink. |
| `semantics` | `DeliverySemantics` | `AtLeastOnce` | Delivery guarantee level. |
| `failure_policy` | `BatchFailurePolicy` | `RetryFailed` | What to do when events fail within a batch. |
| `flush` | `FlushStrategy` | See [Flush Strategy](#flush-strategy) | Controls when pending outputs are flushed. |
| `checkpoint` | `CheckpointConfig` | See [Checkpoint Configuration](#checkpoint-configuration) | Where and how progress is persisted. |
| `max_retries` | `u32` | `0` | Maximum retries per event before routing to DLQ. Used with `RetryFailed` and `SkipToDlq` policies. |
| `retry_backoff` | `Duration` | `0ms` | Backoff duration between retries. |

### Benchmark Preset

For benchmarking, use `DeliveryConfig::benchmark(strategy)` which disables checkpoints,
retries, and uses a 100ms flush interval:

```rust
let config = DeliveryConfig::benchmark(DeliveryStrategy::UnorderedBatch);
// checkpoint: None, max_retries: 0, failure_policy: FailBatch
```

---

## Delivery Strategies

The delivery strategy is a per-pipeline setting that controls how the sink connector
delivers outputs to the downstream system.

### PerEvent

Send events one at a time, await confirmation before sending the next.

- **Ordering**: Strict per-event ordering.
- **Throughput**: Lowest (~1.8K events/sec measured with Redpanda in Docker).
- **Blocking**: Yes -- blocks on every event.
- **Use cases**: Regulatory audit trails requiring per-event confirmation.

### OrderedBatch (default)

Send a batch in sequence, collect acks at the batch boundary.

- **Ordering**: Preserved within and across batches.
- **Throughput**: Medium (~30-40K events/sec with Redpanda, ~20-50K/sec with PostgreSQL).
- **Blocking**: Yes -- blocks at batch boundary.
- **Downstream behavior**:
  - Kafka/Redpanda: idempotent producer (`enable.idempotence=true`)
  - PostgreSQL/MySQL: single transaction (`BEGIN` -> batch `INSERT` -> `COMMIT`)
  - Redis/Valkey: `MULTI`/`EXEC` (atomic batch)
  - NATS JetStream: sequential publish, batch ack await
  - File: sequential write, single fsync at batch end
- **Use cases**: Bank transactions, CDC replication, event sourcing, task queues.

### UnorderedBatch

Send a batch concurrently, collect acks at flush intervals.

- **Ordering**: No ordering guarantee. Downstream can sort by UUIDv7 sequence if needed.
- **Throughput**: Highest (~41.6K events/sec measured with Redpanda in Docker).
- **Blocking**: No -- `write_batch()` enqueues and returns immediately.
- **Use cases**: Analytics, bulk loads, search indexing, monitoring, data warehouses.

### Strategy Comparison

| Property | PerEvent | OrderedBatch | UnorderedBatch |
|----------|----------|--------------|----------------|
| Preserves order | Yes | Yes | No |
| Blocks on ack | Per event | Per batch | At flush |
| Throughput | ~1.8K/s | ~30-40K/s | ~41.6K/s |
| Latency | Highest | Medium | Lowest |

---

## Delivery Semantics

Orthogonal to the delivery strategy. Any combination is valid (e.g. `PerEvent` + `ExactlyOnce`,
`UnorderedBatch` + `AtLeastOnce`).

### AtLeastOnce (default)

Checkpoint + source offset replay on failure. On crash recovery, events between the
last checkpoint and the crash point are replayed from the source. Rare duplicates are
possible only during checkpoint-interval failures. Downstream deduplicates via UUIDv7
event IDs or the `IdempotentSink` trait.

- **Overhead**: Minimal. No transaction coordination.
- **When to use**: Most workloads. Acceptable when downstream can deduplicate.

### ExactlyOnce

Uses Kafka transactions, `IdempotentSink`, or UUIDv7-based dedup to guarantee no
duplicates. Requires sink support (e.g. Kafka transactional producer). Higher latency
due to transaction coordination overhead.

- **Overhead**: Transaction coordination per batch/flush.
- **When to use**: Financial systems, billing, anywhere duplicates cause data corruption.

---

## Flush Strategy

Controls when the pipeline flushes pending outputs to the sink. Most relevant for
`UnorderedBatch` strategy where outputs are enqueued without awaiting acks.

```rust
pub struct FlushStrategy {
    pub interval: Duration,              // Default: 1s
    pub max_pending: usize,              // Default: 50,000
    pub adaptive: bool,                  // Default: false
    pub adaptive_min_divisor: u32,       // Default: 10
    pub adaptive_max_multiplier: u32,    // Default: 5
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `interval` | `1s` | How often to trigger a checkpoint flush. |
| `max_pending` | `50,000` | Maximum unacked outputs before forcing a flush. Acts as a backpressure signal -- when exceeded, the pipeline pauses source ingestion until flush completes. |
| `adaptive` | `false` | Enable adaptive flush tuning. Auto-reduces interval when ack failure rate spikes. Uses the `BatchTuner` hill-climbing algorithm. |
| `adaptive_min_divisor` | `10` | Minimum flush interval = `interval / adaptive_min_divisor`. With defaults: 1s / 10 = 100ms minimum. |
| `adaptive_max_multiplier` | `5` | Maximum flush interval = `interval * adaptive_max_multiplier`. With defaults: 1s * 5 = 5s maximum. |

### Tuning Guidelines

- **High-throughput analytics**: `interval: 5s`, `max_pending: 200_000`, `adaptive: true`
- **Low-latency ordered processing**: `interval: 200ms`, `max_pending: 10_000`
- **Benchmarking**: `interval: 100ms`, `max_pending: 50_000`, `adaptive: false`

---

## Checkpoint Configuration

Checkpoints record source offsets and delivery state so the pipeline can resume from
a known-good position after crashes.

```rust
pub struct CheckpointConfig {
    pub backend: CheckpointBackend,   // Default: Wal
    pub retention: Duration,          // Default: 24 hours
    pub dir: Option<PathBuf>,         // Default: None (OS temp dir)
}
```

### Checkpoint Backends

| Backend | Latency | Durability | Best For |
|---------|---------|------------|----------|
| `Wal` | ~100us per write | Survives process crash (append-only WAL with CRC32 integrity) | Single-node, bare-metal, Docker. **Default.** |
| `StateStore` | Varies by tier | Depends on L2/L3 tier config | When L2 (mmap) / L3 (RocksDB) tiers are already active. |
| `Kafka` | ~1-5ms per write | Survives node loss (replicated) | Multi-node clusters. Writes to a Kafka/Redpanda compacted topic. |
| `None` | 0 | Memory only, lost on restart | Dev/test, stateless processors where source replay is acceptable. |

### Configuration

| Field | Default | Description |
|-------|---------|-------------|
| `backend` | `Wal` | Persistence backend (see table above). |
| `retention` | `24h` | How long to retain checkpoint history before rotation/cleanup. |
| `dir` | `None` (OS temp dir) | Directory for WAL checkpoint files. **Set `AEON_CHECKPOINT_DIR` to a persistent volume in production.** |

### Recovery

On startup, the pipeline reads the WAL (or chosen backend) and replays from the last
committed offset. Events between the last checkpoint and the crash point are re-fetched
from the source. Combined with `AtLeastOnce` semantics, this guarantees zero event loss.

---

## Batch Failure Policy

Determines pipeline behavior when individual events fail delivery within a batch.

### RetryFailed (default)

Retry the failed events, continue the batch from the failure point.

- Each connector decides how: Kafka retries via idempotent producer, PostgreSQL uses
  `SAVEPOINT` + retry, Redis retries the command.
- Respects `max_retries` from `DeliveryConfig`. After exhausting retries, behavior
  depends on whether DLQ is configured.
- **Use when**: Transient failures are expected (network blips, temporary downstream load).

### FailBatch

Fail the entire batch. The checkpoint ensures replay from the last committed offset.

- Clean semantics for transactional downstreams (`ROLLBACK` the entire transaction).
- **Use when**: Partial commits are unacceptable (PostgreSQL/MySQL transactional writes).

### SkipToDlq

Skip the failed event, record it in the DLQ, continue the batch.

- Requires a DLQ sink to be configured (see [Dead-Letter Queue](#dead-letter-queue-dlq)).
- **Use when**: Partial delivery is acceptable (analytics, search indexing, monitoring).

### Policy Comparison

| Policy | Partial delivery | Replay on failure | DLQ required |
|--------|-----------------|-------------------|--------------|
| `RetryFailed` | Yes (after retries) | No | No (optional) |
| `FailBatch` | No | Yes (full batch) | No |
| `SkipToDlq` | Yes (immediately) | No | Yes |

---

## Dead-Letter Queue (DLQ)

The DLQ captures events that fail processing for later inspection and reprocessing.
It wraps any `Sink` implementation, so failed events can route to any destination
(Kafka topic, file, memory, etc.).

```rust
pub struct DlqConfig {
    pub destination: Arc<str>,  // Default: "aeon-dlq"
    pub batch_size: usize,      // Default: 64
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `destination` | `aeon-dlq` | Destination name for DLQ outputs (e.g. a Kafka topic name). |
| `batch_size` | `64` | Maximum records to buffer before flushing to the DLQ sink. |

### DLQ Record Headers

Each DLQ record carries the original event payload plus metadata headers:

| Header | Description |
|--------|-------------|
| `dlq.reason` | Error message describing why processing failed. |
| `dlq.source` | Original event source identifier. |
| `dlq.failed_at` | Timestamp (nanos since epoch) when the failure occurred. |
| `dlq.attempts` | Number of processing attempts before routing to DLQ. |
| `dlq.event_id` | Original event UUID. |
| `dlq.event_timestamp` | Original event timestamp. |

The output key is set to the original event's UUID bytes for partition affinity.

---

## Adaptive Batch Tuner

The `BatchTuner` automatically adjusts batch size at runtime to maximize throughput
using a hill-climbing algorithm: measure throughput at the current batch size, try
a larger or smaller size, keep whichever is faster.

```rust
let tuner = BatchTuner::new(
    initial,  // Starting batch size
    min,      // Minimum batch size (never go below)
    max,      // Maximum batch size (never go above)
);
```

### Behavior

- Measures throughput every ~100 batches (relative to current batch size).
- Step size starts at `(max - min) / 4` and shrinks as the tuner converges.
- Minimum step size is `max(1, (max - min) / 64)`.
- Alternates between trying larger and smaller batch sizes.
- Call `tuner.report(events)` after each batch; it returns `true` when the batch size was adjusted.
- Read `tuner.batch_size()` for the current recommended size.

### Tuning Guidelines

- For most workloads, start with `BatchTuner::new(1024, 64, 8192)`.
- The tuner converges within the first few thousand batches.
- Works best when workload characteristics are relatively stable.

---

## Observability Configuration

Aeon uses a unified observability stack built on `tracing` + OpenTelemetry.

### Architecture

```
Aeon (tracing crate)
    |-- fmt layer -> stdout (always present, for container runtime)
    `-- tracing-opentelemetry layer -> OTLP gRPC export
        -> OTel Collector / SigNoz / Tempo / Jaeger / etc.
```

### LogConfig

```rust
pub struct LogConfig {
    pub filter: String,       // Default: "info"
    pub json: bool,           // Default: false
    pub with_file: bool,      // Default: false
    pub with_target: bool,    // Default: true
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `filter` | `info` | Log level filter. Supports per-crate granularity: `info,aeon_engine=debug`. Maps to `RUST_LOG`. |
| `json` | `false` | `true` for JSON output (Loki-compatible), `false` for human-readable. Maps to `AEON_LOG_FORMAT=json`. |
| `with_file` | `false` | Include source file and line number in log output. |
| `with_target` | `true` | Include module path in log output. |

### Supported Backends

Aeon emits OTLP once; the OTel Collector routes to your chosen backend(s):

- **SigNoz** -- unified APM (built on ClickHouse)
- **Grafana Tempo** -- distributed tracing backend
- **Jaeger** -- CNCF tracing (accepts OTLP natively)
- **Elastic APM** -- search-driven APM
- **OpenObserve** -- cost-efficient log/trace storage
- **Datadog / Dynatrace** -- commercial (accept OTLP)
- **OTel Collector** -- routing layer to any combination of backends

### Enabling OTLP Export

1. Compile with the `otlp` feature flag.
2. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to your collector address.
3. Optionally set `OTEL_SERVICE_NAME` (defaults to `aeon`).

When `OTEL_EXPORTER_OTLP_ENDPOINT` is unset, only stdout logging is active.

### Production Docker Compose Example

```yaml
services:
  aeon:
    environment:
      RUST_LOG: info
      AEON_LOG_FORMAT: json
      AEON_API_ADDR: "0.0.0.0:4471"
      AEON_API_TOKEN: ${AEON_API_TOKEN:?Set AEON_API_TOKEN for production}
      AEON_CHECKPOINT_DIR: /app/data/checkpoints
      AEON_BROKERS: "redpanda:9092"
      OTEL_EXPORTER_OTLP_ENDPOINT: ${OTEL_EXPORTER_OTLP_ENDPOINT:-http://otel-collector:4317}
      OTEL_SERVICE_NAME: ${OTEL_SERVICE_NAME:-aeon}
```

---

## YAML Manifest

Aeon supports declarative pipeline management via YAML manifests. Use `aeon apply`
to create pipelines from a manifest, `aeon export` to dump current state, and
`aeon diff` to compare.

### Manifest Structure

```yaml
pipelines:
  - name: my-pipeline
    source:
      type: kafka
      topic: input-events
      partitions: [0, 1, 2, 3]
      config:
        bootstrap.servers: "redpanda:9092"
        group.id: "aeon-my-pipeline"
    processor:
      name: my-processor
      version: "1.2.0"
    sink:
      type: kafka
      topic: output-events
      config:
        bootstrap.servers: "redpanda:9092"
    upgrade_strategy: drain-swap  # or blue-green, canary

  - name: analytics-pipeline
    source:
      type: kafka
      topic: raw-events
      partitions: [0, 1, 2, 3, 4, 5, 6, 7]
    processor:
      name: analytics-enricher
      version: "2.0.0"
    sink:
      type: kafka
      topic: enriched-events
    upgrade_strategy: canary
```

### Pipeline Definition Fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Unique pipeline identifier. Must be alphanumeric with hyphens (path-traversal safe). |
| `source.type` | Yes | Source connector type: `kafka`, `memory`, `blackhole`. |
| `source.topic` | Kafka only | Source topic name. |
| `source.partitions` | No | Partition assignments (list of integers). Empty = all partitions. |
| `source.config` | No | Additional key-value configuration passed to the source connector. |
| `processor.name` | Yes | Processor name in the registry. |
| `processor.version` | Yes | Processor version string (e.g. `1.2.3` or `latest`). |
| `sink.type` | Yes | Sink connector type: `kafka`, `blackhole`, `stdout`. |
| `sink.topic` | Kafka only | Sink topic name. |
| `sink.config` | No | Additional key-value configuration passed to the sink connector. |
| `upgrade_strategy` | No | `drain-swap` (default), `blue-green`, or `canary`. |

### Upgrade Strategies

| Strategy | Behavior | Downtime |
|----------|----------|----------|
| `drain-swap` | Drain in-flight events, swap processor, resume. **Default.** | <100ms pause |
| `blue-green` | Run old + new in parallel, instant cutover after validation. | Zero |
| `canary` | Gradual traffic shift with auto-rollback on threshold breach. | Zero |

### CLI Commands

```bash
# Apply a manifest (create pipelines that don't exist yet)
aeon apply manifest.yaml

# Dry run (preview changes without applying)
aeon apply manifest.yaml --dry-run

# Export current state to YAML
aeon export -o current-state.yaml

# Diff a manifest against current state
aeon diff manifest.yaml

# Override the API address
aeon apply manifest.yaml --api http://aeon-host:4471
```

### Manifest Limits

- Maximum file size: 1 MB (OWASP A04 resource exhaustion prevention).
- Pipeline names are validated against path traversal and injection patterns.

---

## REST API Endpoints

The REST API runs on port `4471` by default (configurable via `AEON_API_ADDR`).

### Authentication

When `AEON_API_TOKEN` is set, all `/api/v1/` endpoints require:

```
Authorization: Bearer <token>
```

Health endpoints (`/health`, `/ready`) bypass authentication.

### Security Features

- Request body limit: 10 MB max.
- Security headers: `X-Content-Type-Options`, `X-Frame-Options`.
- Request logging via `tower-http` TraceLayer.
- Resource name validation for path traversal prevention.

### Endpoint Summary

**Processors:**
- `GET    /api/v1/processors` -- list all
- `GET    /api/v1/processors/:name` -- inspect
- `GET    /api/v1/processors/:name/versions` -- list versions
- `POST   /api/v1/processors` -- register
- `DELETE /api/v1/processors/:name/versions/:ver` -- delete version

**Pipelines:**
- `GET    /api/v1/pipelines` -- list all
- `GET    /api/v1/pipelines/:name` -- inspect
- `POST   /api/v1/pipelines` -- create
- `POST   /api/v1/pipelines/:name/start` -- start
- `POST   /api/v1/pipelines/:name/stop` -- stop
- `POST   /api/v1/pipelines/:name/upgrade` -- upgrade processor
- `GET    /api/v1/pipelines/:name/history` -- lifecycle history
- `DELETE /api/v1/pipelines/:name` -- delete

**System:**
- `GET    /health` -- health check (no auth)
- `GET    /ready` -- readiness check (no auth)
