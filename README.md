# Aeon

**Real-time data processing engine targeting 20M events/sec aggregate.**

Aeon is a high-performance stream processing engine written in Rust. It ingests events from sources (Redpanda/Kafka), transforms them through processors (Rust-native or WebAssembly), and delivers results to sinks — with zero-copy data paths, SPSC ring buffers, and SIMD-accelerated parsing.

## Architecture

```
Redpanda (source topic)
  -> Aeon Source (batch polling, manual partition assign)
    -> SPSC Ring Buffer
      -> Processor (Rust-native OR Wasm guest)
        -> SPSC Ring Buffer
          -> Aeon Sink (batch produce)
            -> Redpanda (sink topic)
```

**Processors can be written in any language that compiles to WebAssembly** — Rust, AssemblyScript/TypeScript, Go, C/C++, and more. Native Rust processors are also supported for maximum performance.

## Performance

| Metric | Result |
|--------|--------|
| Blackhole ceiling (in-memory) | **6.4-9.6M events/sec** |
| Redpanda E2E (source+sink) | **1,459-1,957 events/sec** |
| Headroom ratio | **5,142x** (target >=5x) |
| Rust-native processor | **240ns/event (4.2M/sec)** |
| Rust-Wasm processor | **1.2us/event (820K/sec)** |
| AssemblyScript-Wasm processor | **1.1us/event (940K/sec)** |
| Wasm binary size | **1.4-2.8KB** |

Aeon is never the bottleneck. Infrastructure (Redpanda, network, disk) determines absolute throughput; Aeon's architecture ensures it always has orders-of-magnitude headroom.

## Quick Start

### Prerequisites

- Rust 1.85+ (`rustup update`)
- Docker / Rancher Desktop (for Redpanda)
- Node.js 18+ (only if building AssemblyScript processors)
- `wasm32-unknown-unknown` target (only if building Rust-Wasm processors):
  ```bash
  rustup target add wasm32-unknown-unknown
  ```

### 1. Start Infrastructure

```bash
# Start Redpanda + observability stack
docker compose up -d redpanda redpanda-console redpanda-init

# Verify topics are created
docker exec aeon-redpanda rpk topic list
```

### 2. Run Tests

```bash
# All workspace tests (excludes Kafka-dependent tests without Redpanda)
cargo test --workspace

# Specific crate
cargo test -p aeon-types
cargo test -p aeon-engine
cargo test -p aeon-wasm
cargo test -p aeon-state

# With Redpanda running — integration tests
cargo test -p aeon-connectors
```

### 3. Run Benchmarks

```bash
# Blackhole benchmark (in-memory pipeline ceiling)
cargo bench -p aeon-engine --bench blackhole_bench

# Redpanda E2E benchmark (requires Redpanda running)
cargo bench -p aeon-engine --bench redpanda_bench

# Wasm runtime benchmarks
cargo bench -p aeon-wasm

# Multi-runtime processor comparison (Rust-native vs Rust-Wasm vs AssemblyScript-Wasm)
# First build the Wasm processors:
cd samples/processors/rust-wasm && cargo build --target wasm32-unknown-unknown --release && cd ../../..
cd samples/processors/assemblyscript-wasm && npm install && npx asc assembly/index.ts --outFile build/processor.wasm --optimize --exportStart '' --runtime stub --noAssert --initialMemory 4 --maximumMemory 8 && cd ../../..
# Then run the comparison:
cargo bench -p aeon-sample-rust-native --bench multi_runtime
```

### 4. Run an E2E Pipeline

The `samples/e2e-pipeline/` package provides two ready-to-use binaries:

**`aeon-pipeline`** — Runs Redpanda → WasmProcessor → Redpanda:

```bash
# Build a Wasm processor first (e.g., the Rust-Wasm sample)
cd samples/processors/rust-wasm
cargo build --target wasm32-unknown-unknown --release
cd ../../..

# Start the pipeline
cargo run --release --bin aeon-pipeline -- \
  --wasm samples/processors/rust-wasm/target/wasm32-unknown-unknown/release/aeon_sample_rust_wasm.wasm \
  --source-topic aeon-source \
  --sink-topic aeon-sink \
  --brokers localhost:19092 \
  --partitions 0
```

**`aeon-producer`** — Generates JSON test events to Redpanda:

```bash
# Produce 10,000 events at 1,000/sec
cargo run --release --bin aeon-producer -- \
  --topic aeon-source \
  --brokers localhost:19092 \
  --count 10000 \
  --rate 1000

# Unlimited rate (flood)
cargo run --release --bin aeon-producer -- \
  --topic aeon-source \
  --count 100000
```

Run both in separate terminals to see events flow through the pipeline. Use Redpanda Console at `http://localhost:8080` to inspect messages on the sink topic.

### 5. Develop a Processor

See [docs/PROCESSOR-GUIDE.md](docs/PROCESSOR-GUIDE.md) for the complete guide covering:
- **Rust-native** processors (fastest, direct trait implementation)
- **Rust-Wasm** processors (sandboxed, compile to wasm32-unknown-unknown)
- **AssemblyScript-Wasm** processors (TypeScript-like, compiles to Wasm)

The Wasm wire format is documented in [docs/WIRE-FORMAT.md](docs/WIRE-FORMAT.md).

**Tip:** Once you've built a `.wasm` processor, use `aeon-pipeline` (step 4) to test it end-to-end against Redpanda without writing any Rust.

## Workspace Structure

```
crates/
  aeon-types/          # Event, Output, AeonError, traits (Source, Sink, Processor, StateOps)
  aeon-io/             # Tokio I/O abstraction layer
  aeon-state/          # L1 DashMap state store, typed wrappers, windowing
  aeon-wasm/           # Wasmtime host runtime, WasmProcessor, fuel metering
  aeon-connectors/     # KafkaSource, KafkaSink, MemorySource, BlackholeSink
  aeon-engine/         # Pipeline orchestrator, SPSC wiring, backpressure, DLQ, retry, circuit breaker
  aeon-observability/  # Histograms, logging, metrics, tracing spans
  aeon-cluster/        # (future) Raft + QUIC
  aeon-crypto/         # (future) Encryption, signing
  aeon-cli/            # (future) Binary entrypoint

samples/
  e2e-pipeline/              # Runnable binaries: aeon-pipeline + aeon-producer
  processors/
    rust-native/             # Rust-native JSON enrichment processor
    rust-wasm/               # Rust -> wasm32-unknown-unknown processor
    assemblyscript-wasm/     # AssemblyScript -> Wasm processor

docs/
  ARCHITECTURE.md      # Full product specification
  ROADMAP.md           # Phase-based implementation plan
  PROCESSOR-GUIDE.md   # Processor development guide
  WIRE-FORMAT.md       # Wasm processor ABI specification
```

## Docker Services

| Service | Port | Purpose |
|---------|------|---------|
| Redpanda | 19092 | Kafka-compatible broker |
| Redpanda Console | 8080 | Web UI for topics/messages |
| Prometheus | 9090 | Metrics collection |
| Grafana | 3000 | Dashboards (admin/aeon_dev) |
| Jaeger | 16686 | Distributed tracing |
| Loki | 3100 | Log aggregation |

```bash
# Minimal (just Redpanda)
docker compose up -d redpanda redpanda-console redpanda-init

# Full observability stack
docker compose up -d redpanda redpanda-console redpanda-init prometheus grafana jaeger loki
```

Pre-created topics: `aeon-source` (16p), `aeon-sink` (16p), `aeon-dlq` (4p), `aeon-bench-source` (16p), `aeon-bench-sink` (16p).

## Current Status

**Phases 0-7 complete.** The core pipeline, connectors, state management, fault tolerance, observability, and Wasm runtime are all implemented and benchmarked. See [docs/ROADMAP.md](docs/ROADMAP.md) for full details.

**236 tests passing** across the workspace. Clippy clean, rustfmt clean.

## License

Apache-2.0
