# Aeon

**Real-time data processing engine targeting 20M events/sec aggregate.**

Aeon is a high-performance stream processing engine written in Rust. It ingests events from sources (Kafka/Redpanda, HTTP, WebSocket, NATS, MQTT, RabbitMQ, Redis Streams, PostgreSQL/MySQL/MongoDB CDC, QUIC, WebTransport), transforms them through processors (Rust-native or WebAssembly), and delivers results to sinks -- with zero-copy data paths, SPSC ring buffers, and SIMD-accelerated parsing.

## Architecture

```
Source (Kafka, HTTP, NATS, MQTT, CDC, ...)
  -> Aeon Source (batch polling, manual partition assign)
    -> SPSC Ring Buffer
      -> Processor (Rust-native OR Wasm guest)
        -> SPSC Ring Buffer
          -> Aeon Sink (batch produce)
            -> Sink (Kafka, Redis, File, WebSocket, ...)
```

**Processors can be written in any language** across four tiers: T1 Native (Rust, C/C++, .NET), T2 Wasm (Rust, AssemblyScript), T3 WebTransport, and T4 WebSocket (Python, Go, Node.js, Java, PHP, C#, Rust). SDKs available for 8 languages.

## Key Features

- **22 connectors** across 16 connector types (Kafka, HTTP, WebSocket, Redis Streams, NATS, MQTT, RabbitMQ, PostgreSQL CDC, MySQL CDC, MongoDB CDC, QUIC, WebTransport, File, Memory, Blackhole, Stdout)
- **4 processor tiers**: T1 Native (Rust/C/.NET — 4.2M events/sec), T2 Wasm (820K/sec), T3 WebTransport, T4 WebSocket — with SDKs in 8 languages
- **3 delivery strategies**: PerEvent (strictest ordering), OrderedBatch (ordered + high throughput), UnorderedBatch (maximum throughput)
- **3 upgrade strategies**: Drain-swap (<100ms pause), blue-green (zero-downtime), canary (gradual rollout)
- **Vendor-neutral observability**: OpenTelemetry OTLP export to SigNoz, Grafana, Jaeger, Elastic APM, Datadog, or any OTLP-compatible backend
- **REST API + CLI**: Full pipeline and processor management with declarative YAML manifests
- **Production security**: OWASP Top 10 compliant, Bearer token auth, SHA-256 processor integrity verification, Wasm sandboxing
- **12-Factor compliant**: Environment-driven configuration, stateless processes, externalized storage

## Performance

| Metric | Result |
|--------|--------|
| Blackhole ceiling (in-memory) | **6.4-9.6M events/sec** |
| Redpanda E2E — UnorderedBatch | **41,500+ events/sec** |
| Redpanda E2E — PerEvent | **1,800+ events/sec** |
| Headroom ratio | **5,142x** (target >=5x) |
| Rust-native processor | **240ns/event (4.2M/sec)** |
| Rust-Wasm processor | **1.2us/event (820K/sec)** |
| AssemblyScript-Wasm processor | **1.1us/event (940K/sec)** |
| Wasm binary size | **1.4-2.8KB** |
| Per-event overhead | **<100ns** (Gate 1 target) |

Aeon is never the bottleneck. Infrastructure (Redpanda, network, disk) determines absolute throughput; Aeon's architecture ensures it always has orders-of-magnitude headroom.

## Quick Start

### Prerequisites

- Rust 1.85+ (`rustup update`)
- Docker / Rancher Desktop (for infrastructure services)
- Node.js 18+ (only if building AssemblyScript processors)
- `wasm32-unknown-unknown` target (only if building Rust-Wasm processors):
  ```bash
  rustup target add wasm32-unknown-unknown
  ```

### 1. Configure Environment

```bash
# Copy the environment template
cp .env.example .env

# Edit .env — set required passwords:
#   POSTGRES_PASSWORD, MYSQL_ROOT_PASSWORD, MYSQL_PASSWORD,
#   RABBITMQ_PASSWORD, GRAFANA_ADMIN_PASSWORD
#
# For production, also set:
#   AEON_API_TOKEN (enables REST API authentication)
```

### 2. Start Infrastructure

```bash
# Minimal: just Redpanda (for Kafka-compatible streaming)
docker compose up -d redpanda redpanda-console redpanda-init

# Full stack with observability
docker compose up -d

# Verify topics are created
docker exec aeon-redpanda rpk topic list
```

### 3. Run Tests

```bash
# All workspace tests
cargo test --workspace

# Specific crate
cargo test -p aeon-types
cargo test -p aeon-engine
cargo test -p aeon-wasm
cargo test -p aeon-connectors   # Requires Redpanda running
```

### 4. Run Benchmarks

```bash
# Blackhole benchmark (in-memory pipeline ceiling)
cargo bench -p aeon-engine --bench blackhole_bench

# E2E delivery benchmark — all 3 strategies x 3 sink types (requires Redpanda)
cargo bench -p aeon-engine --bench e2e_delivery_bench

# Redpanda E2E benchmark
cargo bench -p aeon-engine --bench redpanda_bench

# Multi-runtime processor comparison
cargo bench -p aeon-sample-rust-native --bench multi_runtime
```

### 5. Run an E2E Pipeline

```bash
# Build a Wasm processor first
cd samples/processors/rust-wasm
cargo build --target wasm32-unknown-unknown --release
cd ../../..

# Start the pipeline (Redpanda -> Wasm Processor -> Redpanda)
cargo run --release --bin aeon-pipeline -- \
  --wasm samples/processors/rust-wasm/target/wasm32-unknown-unknown/release/aeon_sample_rust_wasm.wasm \
  --source-topic aeon-source \
  --sink-topic aeon-sink \
  --brokers localhost:19092 \
  --partitions 0

# In another terminal: produce test events
cargo run --release --bin aeon-producer -- \
  --topic aeon-source \
  --brokers localhost:19092 \
  --count 10000 \
  --rate 1000
```

Use Redpanda Console at `http://localhost:8080` to inspect messages.

### 6. Manage via CLI

```bash
# Register a processor
aeon processor register my-processor --version 1.0.0 \
  --artifact path/to/processor.wasm --runtime wasm

# Create a pipeline
aeon pipeline create my-pipeline \
  --source kafka --source-topic input-events \
  --sink kafka --sink-topic output-events \
  --processor my-processor --processor-version 1.0.0

# Start the pipeline
aeon pipeline start my-pipeline

# Apply a declarative manifest
aeon apply -f pipeline.yaml

# Real-time monitoring
aeon top
```

### 7. Develop a Processor

Aeon supports **4 processor tiers** — choose based on your performance and language needs:

| Tier | Transport | Languages | Latency |
|------|-----------|-----------|---------|
| **T1 Native** | In-process (C-ABI) | Rust, C/C++, .NET NativeAOT | ~240ns |
| **T2 Wasm** | In-process (Wasmtime) | Rust, AssemblyScript | ~1.1us |
| **T3 WebTransport** | QUIC/UDP | Any with WT client | ~5-15us |
| **T4 WebSocket** | TCP/WS | Python, Go, Node.js, Java, PHP, C#, Rust | ~30-80us |

See [docs/PROCESSOR-GUIDE.md](docs/PROCESSOR-GUIDE.md) for the Rust processor guide, [docs/FOUR-TIER-PROCESSORS.md](docs/FOUR-TIER-PROCESSORS.md) for the four-tier architecture, and [docs/WIRE-FORMAT.md](docs/WIRE-FORMAT.md) for the Wasm ABI specification.

## Connectors

| Type | Source | Sink | Feature Flag |
|------|--------|------|-------------|
| Kafka / Redpanda | KafkaSource | KafkaSink | `kafka` |
| HTTP | HttpPollingSource, HttpWebhookSource | -- | `http` |
| WebSocket | WebSocketSource | WebSocketSink | `websocket` |
| Redis Streams | RedisSource | RedisSink | `redis-streams` |
| NATS JetStream | NatsSource | NatsSink | `nats` |
| MQTT | MqttSource | MqttSink | `mqtt` |
| RabbitMQ | RabbitMqSource | RabbitMqSink | `rabbitmq` |
| PostgreSQL CDC | PostgresCdcSource | -- | `postgres-cdc` |
| MySQL CDC | MysqlCdcSource | -- | `mysql-cdc` |
| MongoDB CDC | MongoDbCdcSource | -- | `mongodb-cdc` |
| QUIC | QuicSource | QuicSink | `quic` |
| WebTransport | WebTransportSource | WebTransportSink | `webtransport` |
| File | FileSource | FileSink | `file` |
| Memory | MemorySource | MemorySink | `memory` (default) |
| Blackhole | -- | BlackholeSink | `blackhole` (default) |
| Stdout | -- | StdoutSink | `stdout` (default) |

See [docs/CONNECTORS.md](docs/CONNECTORS.md) for detailed connector documentation.

## Delivery Strategies

| Strategy | Ordering | Throughput | Use Case |
|----------|----------|------------|----------|
| **PerEvent** | Strict per-event ack | ~1,800/s | Regulatory audit trails, financial transactions |
| **OrderedBatch** (default) | Batch-ordered acks | ~30-40K/s | CDC, event sourcing, bank transactions |
| **UnorderedBatch** | No ordering | ~41,500/s | Analytics, logging, metrics |

See [docs/CONFIGURATION.md](docs/CONFIGURATION.md) for delivery configuration details.

## Workspace Structure

```
crates/
  aeon-types/          # Event, Output, AeonError, traits (Source, Sink, Processor, StateOps)
  aeon-io/             # Tokio I/O abstraction layer
  aeon-state/          # L1 DashMap + L2 mmap + L3 RocksDB state store
  aeon-wasm/           # Wasmtime host runtime, WasmProcessor, fuel metering
  aeon-connectors/     # 22 Source/Sink implementations (feature-gated)
  aeon-engine/         # Pipeline orchestrator, SPSC wiring, delivery, DLQ, REST API
  aeon-cluster/        # Raft consensus + QUIC transport
  aeon-crypto/         # Encryption, signing, Merkle trees
  aeon-observability/  # OpenTelemetry OTLP, Prometheus, structured logging
  aeon-native-sdk/     # Native processor SDK (export_processor! macro, C-ABI)
  aeon-wasm-sdk/       # Wasm processor SDK (aeon_processor! macro, no_std)
  aeon-processor-client/ # T3/T4 network processor SDK (WebSocket, WebTransport)
  aeon-cli/            # CLI binary (new, build, validate, deploy, apply, top)

sdks/
  python/              # Python T4 processor SDK (WebSocket + AWPP)
  go/                  # Go T4 processor SDK (WebSocket + AWPP)
  nodejs/              # Node.js T4 processor SDK (WebSocket + AWPP)
  dotnet/              # C#/.NET T4 SDK + NativeAOT T1 processor
  java/                # Java T4 processor SDK (WebSocket + AWPP)
  php/                 # PHP T4 processor SDK (6 deployment models)
  c/                   # C/C++ T1 native processor SDK (C-ABI)

samples/
  e2e-pipeline/              # Runnable binaries: aeon-pipeline + aeon-producer
  processors/
    rust-native/             # Rust-native JSON enrichment processor
    rust-wasm/               # Rust -> wasm32-unknown-unknown processor
    rust-wasm-sdk/           # Rust-Wasm using aeon-wasm-sdk convenience macros
    assemblyscript-wasm/     # AssemblyScript -> Wasm processor

docs/
  ARCHITECTURE.md      # Full product specification
  ROADMAP.md           # Phase-based implementation plan
  PROCESSOR-GUIDE.md   # Processor development guide
  WIRE-FORMAT.md       # Wasm processor ABI specification
  INSTALLATION.md      # Installation, ports, multi-version operation
  PROCESSOR-DEPLOYMENT.md  # Upgrade strategies, registry, K8s patterns
  CONNECTORS.md        # Source and sink connector reference
  CONFIGURATION.md     # Pipeline, delivery, and environment configuration
  REST-API.md          # REST API reference
  OBSERVABILITY.md     # OpenTelemetry, metrics, traces, logs
  SECURITY.md          # Security model, OWASP compliance, hardening
  CLUSTERING.md        # Single-node, multi-node, Raft consensus guide
  DEPLOYMENT-ARCHITECTURE.md  # Topology options, Redpanda segregation, latency
```

## Docker Services

| Service | Port | Purpose |
|---------|------|---------|
| Redpanda | 19092 | Kafka-compatible streaming broker |
| Redpanda Console | 8080 | Web UI for topics/messages |
| PostgreSQL | 5432 | Database (for PostgreSQL CDC source) |
| MySQL | 3306 | Database (for MySQL CDC source) |
| MongoDB | 27017 | Database (for MongoDB CDC source) |
| Redis | 6379 | Redis Streams source/sink |
| RabbitMQ | 5672 / 15672 | AMQP broker / Management UI |
| NATS | 4222 / 8222 | JetStream messaging / Monitoring |
| Mosquitto | 1883 | MQTT broker |
| OTel Collector | 4317 / 4318 | OpenTelemetry gRPC/HTTP receiver |
| Prometheus | 9090 | Metrics collection |
| Grafana | 3000 | Dashboards |
| Jaeger | 16686 | Distributed tracing |
| Loki | 3100 | Log aggregation |

```bash
# Minimal (just Redpanda)
docker compose up -d redpanda redpanda-console redpanda-init

# Full observability stack
docker compose up -d
```

Pre-created topics: `aeon-source` (16p), `aeon-sink` (16p), `aeon-dlq` (4p), `aeon-bench-source` (16p), `aeon-bench-sink` (16p).

## Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 4470 | QUIC/UDP | Cluster inter-node transport (Raft, mTLS) |
| 4471 | HTTP/TCP | REST API, health checks, metrics |
| 4472 | QUIC/UDP | External QUIC/WebTransport connectors |

See [docs/INSTALLATION.md](docs/INSTALLATION.md) for port configuration and conflict avoidance.

## Observability

Aeon uses **OpenTelemetry (OTLP)** for vendor-neutral observability. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to enable trace/metric/log export to any backend:

- SigNoz, Grafana Tempo, Jaeger, Elastic APM, OpenObserve, Datadog, Dynatrace

See [docs/OBSERVABILITY.md](docs/OBSERVABILITY.md) for setup details.

## Security

- OWASP Top 10 (2021) compliant
- Bearer token API authentication
- SHA-256 processor integrity verification
- Wasm fuel metering and memory sandboxing
- Non-root Docker containers
- 12-Factor credential management

See [docs/SECURITY.md](docs/SECURITY.md) for the full security model.

## Current Status

**Phase 0 — Foundation** is the current focus: achieving Gate 1 performance targets
(per-event overhead <100ns, headroom ratio >=5x, Aeon never the bottleneck).

**What's implemented and tested:**
- Core pipeline engine with SPSC ring buffers and backpressure
- T1 Native processors (Rust, C/C++, .NET NativeAOT) with C-ABI loading
- T2 Wasm processors (Wasmtime, fuel metering, memory sandboxing)
- T4 WebSocket processor host with AWPP protocol
- Kafka/Redpanda source and sink (batch polling, manual partition assign)
- Memory, File, Blackhole, Stdout, HTTP, WebSocket connectors
- 3 delivery strategies (PerEvent, OrderedBatch, UnorderedBatch)
- Checkpoint WAL and state store (L1 DashMap + L2 mmap + L3 redb)
- REST API with Bearer token auth and OWASP-compliant security
- SDKs: Python, Go, Node.js, C#/.NET, Java, PHP, C/C++, Rust
- OpenTelemetry OTLP observability (traces, metrics, logs)

**Designed, code exists, not yet production-validated:**
- T3 WebTransport processor host (implemented, needs TLS test harness)
- NATS, Redis Streams, MQTT, RabbitMQ connectors (implemented, E2E tests pending)
- PostgreSQL/MySQL/MongoDB CDC connectors (stubs, post-Gate 2)
- QUIC/WebTransport source and sink connectors
- Multi-node Raft cluster (openraft + quinn, single-node tested)
- Processor upgrade strategies (drain-swap, blue-green, canary)

**820 tests passing** (776 Rust + 24 Python + 20 Go), 0 failures. Clippy clean, rustfmt clean.

See [docs/ROADMAP.md](docs/ROADMAP.md) for full phase details.

## Documentation

| Document | Description |
|----------|-------------|
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | Full product specification and design |
| [ROADMAP.md](docs/ROADMAP.md) | Phase-based implementation plan |
| [INSTALLATION.md](docs/INSTALLATION.md) | Installation, ports, deployment models |
| [CONFIGURATION.md](docs/CONFIGURATION.md) | Pipeline, delivery, and environment config |
| [CONNECTORS.md](docs/CONNECTORS.md) | Source and sink connector reference |
| [CONNECTOR-DEV-GUIDE.md](docs/CONNECTOR-DEV-GUIDE.md) | Custom source/sink connector development |
| [REST-API.md](docs/REST-API.md) | REST API endpoint reference |
| [PROCESSOR-GUIDE.md](docs/PROCESSOR-GUIDE.md) | Processor development guide (4 runtimes) |
| [PROCESSOR-DEPLOYMENT.md](docs/PROCESSOR-DEPLOYMENT.md) | Upgrade strategies, registry, K8s patterns |
| [WIRE-FORMAT.md](docs/WIRE-FORMAT.md) | Wasm processor ABI specification |
| [OBSERVABILITY.md](docs/OBSERVABILITY.md) | OpenTelemetry, metrics, traces, logs |
| [SECURITY.md](docs/SECURITY.md) | Security model, OWASP compliance |
| [CLUSTERING.md](docs/CLUSTERING.md) | Single-node, multi-node, Raft consensus |
| [DEPLOYMENT-ARCHITECTURE.md](docs/DEPLOYMENT-ARCHITECTURE.md) | Topology options, Redpanda segregation, network latency |
| [FOUR-TIER-PROCESSORS.md](docs/FOUR-TIER-PROCESSORS.md) | Four-tier processor architecture (T1-T4) |
| [PUBLISHING.md](docs/PUBLISHING.md) | crates.io, Docker Hub, GitHub Releases, CI/CD |
| [E2E-TEST-PLAN.md](docs/E2E-TEST-PLAN.md) | End-to-end test matrix (8 tiers, 63 tests) |

## Built With

Aeon is developed using [Claude](https://claude.ai) (Anthropic) as an AI coding partner via [Claude Code](https://claude.ai/claude-code). The `.claude/` directory and `CLAUDE.md` contain the project instructions and coding guidelines that shape how Claude assists with development.

## License

Apache-2.0
