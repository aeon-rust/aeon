# Aeon — Build from Source

> How to build, run, and verify Aeon from source on Linux, macOS, and Windows.
> For deployment models (Docker, Helm, systemd), see `docs/INSTALLATION.md`.
> For publishing to crates.io and Docker Hub, see `docs/PUBLISHING.md`.

---

## 1. Prerequisites

### 1.1 Rust Toolchain

Aeon requires **Rust 1.85+** (edition 2024).

```bash
# Install Rust (all platforms)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Or update existing installation
rustup update

# Verify
rustc --version    # Must be >= 1.85
cargo --version
```

### 1.2 System Dependencies

Aeon's Kafka connector (`rdkafka`) requires cmake to compile librdkafka from source.
TLS operations need OpenSSL headers at build time.

**Linux (Debian / Ubuntu)**

```bash
sudo apt-get update
sudo apt-get install -y cmake libssl-dev pkg-config build-essential
```

**Linux (Fedora / RHEL / Amazon Linux)**

```bash
sudo dnf install cmake openssl-devel pkg-config gcc gcc-c++ make
```

**macOS**

```bash
# Xcode Command Line Tools (provides compiler + linker)
xcode-select --install

# Build dependencies
brew install cmake openssl pkg-config
```

**Windows**

1. Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022)
   — select "Desktop development with C++"
2. Install [CMake](https://cmake.org/download/) — add to PATH during install
3. OpenSSL: install via [vcpkg](https://vcpkg.io/) or use pre-built binaries from
   [slproweb.com](https://slproweb.com/products/Win32OpenSSL.html)

### 1.3 Optional: Wasm Processor Development

To build Wasm processors (not required to run Aeon itself):

```bash
rustup target add wasm32-unknown-unknown
```

---

## 2. Clone and Build

### 2.1 Quick Start (Single-Node Server)

```bash
git clone https://github.com/aeon-rust/aeon.git
cd aeon

# Build the CLI with REST API + all engine features
cargo build --release -p aeon-cli --features rest-api
```

The binary is at `target/release/aeon` (Linux/macOS) or `target\release\aeon.exe` (Windows).

Build time: ~3–5 minutes on first build (downloads + compiles ~400 dependencies).
Subsequent builds are incremental (~10–30 seconds).

### 2.2 Build Profiles

| Profile | Command | Binary Size | Use |
|---------|---------|-------------|-----|
| Debug | `cargo build -p aeon-cli --features rest-api` | ~150 MB | Development, fast compile |
| Release | `cargo build --release -p aeon-cli --features rest-api` | ~30 MB | Production, optimized |

Release profile uses thin LTO, single codegen unit, and `-O3` (see workspace `Cargo.toml`).

### 2.3 Feature Flags

Aeon is modular. Build only what you need:

```bash
# Minimal — no connectors, no REST API, just validation
cargo build --release -p aeon-cli

# Standard — REST API + pipeline engine + WebSocket host
cargo build --release -p aeon-cli --features rest-api

# Everything (what the Dockerfile builds)
cargo build --release -p aeon-cli --features rest-api
```

Key feature flags in `aeon-connectors`:

| Feature | What It Enables | External Dependency |
|---------|-----------------|---------------------|
| `memory` | MemorySource/Sink (testing) | None |
| `blackhole` | BlackholeSink (benchmarks) | None |
| `stdout` | StdoutSink (debug) | None |
| `kafka` | KafkaSource/Sink (Redpanda) | cmake (build), librdkafka |
| `websocket` | WebSocket source/sink | None |
| `http` | HTTP webhook/polling/sink | None |
| `redis-streams` | Redis Streams source/sink | Redis server (runtime) |
| `nats` | NATS JetStream source/sink | NATS server (runtime) |
| `mqtt` | MQTT source/sink | MQTT broker (runtime) |
| `rabbitmq` | RabbitMQ source/sink | RabbitMQ server (runtime) |
| `quic` | QUIC source/sink | None (pure Rust via quinn) |
| `webtransport` | WebTransport source/sink | None (pure Rust via wtransport) |
| `postgres-cdc` | PostgreSQL CDC source | PostgreSQL server (runtime) |
| `mysql-cdc` | MySQL CDC source | MySQL server (runtime) |
| `mongodb-cdc` | MongoDB CDC source | MongoDB server (runtime) |

The `rest-api` feature on `aeon-cli` automatically pulls in the engine defaults
(native-loader, processor-auth, websocket-host) plus all default connectors.

### 2.4 Build All Workspace Crates

```bash
# Build everything (13 crates + 2 samples)
cargo build --workspace --release

# Check compilation without producing binaries (faster)
cargo check --workspace
```

### 2.5 Build a Wasm Processor

```bash
# From the repo root — build the sample Rust Wasm processor
cd samples/processors/rust-wasm
cargo build --release --target wasm32-unknown-unknown

# Output: target/wasm32-unknown-unknown/release/aeon_sample_rust_wasm.wasm
```

---

## 3. Run (Single-Node)

### 3.1 Start the Server

```bash
# Start Aeon with default settings (binds to 0.0.0.0:4471)
./target/release/aeon serve

# Custom address
./target/release/aeon serve --addr 127.0.0.1:4471

# Custom artifact directory (where .wasm/.so processors are stored)
./target/release/aeon serve --artifact-dir ./my-processors

# With debug logging
RUST_LOG=debug ./target/release/aeon serve
```

**Environment variable overrides:**

| Variable | Default | Purpose |
|----------|---------|---------|
| `AEON_API_ADDR` | `0.0.0.0:4471` | REST API bind address |
| `AEON_ARTIFACT_DIR` | `/app/artifacts` | Processor artifact storage |
| `RUST_LOG` | `info` | Log level (`error`, `warn`, `info`, `debug`, `trace`) |

### 3.2 Verify It Works

```bash
# Health check
curl http://localhost:4471/health
# → 200 OK

# List pipelines (empty on fresh start)
curl http://localhost:4471/api/v1/pipelines
# → []

# List registered processors
curl http://localhost:4471/api/v1/processors
# → []
```

### 3.3 Default Ports

| Port | Protocol | Purpose | When Active |
|------|----------|---------|-------------|
| 4470 | UDP | QUIC inter-node (Raft consensus) | Multi-node cluster only |
| 4471 | TCP | REST API + WebSocket processor host | Always (`aeon serve`) |
| 4472 | UDP | QUIC external connectors (WebTransport) | When WT connectors configured |

All ports are configurable. See `docs/INSTALLATION.md` for full manifest configuration.

---

## 4. Verify the Build

### 4.1 Run Tests

```bash
# All workspace tests (no external infra needed for most)
cargo test --workspace

# Just the engine (263 tests)
cargo test -p aeon-engine

# Sustained load test (30 seconds, zero event loss)
cargo test -p aeon-engine --test sustained_load -- --nocapture

# Backpressure validation
cargo test -p aeon-engine --test backpressure -- --nocapture

# Chaos / fault injection
cargo test -p aeon-engine --test chaos_test -- --nocapture
```

### 4.2 Run Benchmarks

```bash
# Blackhole ceiling — Aeon's internal maximum throughput
cargo bench -p aeon-engine --bench blackhole_bench

# Multi-partition scaling (1, 2, 4, 8 partitions)
cargo bench -p aeon-engine --bench multi_partition_blackhole_bench

# Large message overhead (256B → 1MB payloads)
cargo bench -p aeon-engine --bench large_message_bench

# Pipeline component micro-benchmarks (SPSC ring buffer, processor batch)
cargo bench -p aeon-engine --bench pipeline_components_bench
```

All benchmarks above run in-memory — no Redpanda or Docker needed.

### 4.3 Lint

```bash
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

---

## 5. Docker Build

> **Just want to run a published image?** Skip this section and use the
> pre-built image: `docker pull ghcr.io/aeon-rust/aeon:latest`. See
> [INSTALLATION.md §3.1](INSTALLATION.md) for the published-image quickstart.
> The instructions below are for building your own image from source.

### 5.1 Build the Image

```bash
# Standard build (tag with whatever name you want — local-only)
docker build -t aeon:dev .

# With custom profile
docker build -t aeon:dev --build-arg PROFILE=release .
```

The Dockerfile is a multi-stage build:
1. **rust-builder**: compiles `aeon` CLI with `--features rest-api`
2. **wasm-builder**: compiles sample Rust Wasm processor
3. **runtime**: minimal Debian image (~160 MB) with binary + Wasm artifacts

### 5.2 Run the Container

```bash
# Basic run
docker run -p 4471:4471 aeon:dev

# With artifact volume
docker run -p 4471:4471 \
  -v ./my-processors:/app/artifacts \
  aeon:dev

# Verify
curl http://localhost:4471/health
```

### 5.3 Multi-Platform Build (for Publishing)

The published image lives at `ghcr.io/aeon-rust/aeon` (GitHub Container
Registry — chosen 2026-05-02; see ROADMAP.md §P4a). To publish a
multi-platform release from source:

```bash
# Create builder (one-time)
docker buildx create --name aeon-builder --use

# Authenticate to GHCR (PAT with write:packages scope)
echo "$GHCR_PAT" | docker login ghcr.io -u <gh-username> --password-stdin

# Build and push both architectures
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  -t ghcr.io/aeon-rust/aeon:latest \
  -t ghcr.io/aeon-rust/aeon:v<NN> \
  --push .
```

---

## 6. Development Workflow

### 6.1 Run from Source (Without Building Release)

```bash
# Run directly via cargo (debug build, slower but faster compile)
cargo run -p aeon-cli --features rest-api -- serve

# With logging
RUST_LOG=debug cargo run -p aeon-cli --features rest-api -- serve
```

### 6.2 Dev Mode with File Watcher

Watch a Wasm or native processor file and auto-reload on changes:

```bash
# Build release first
cargo build --release -p aeon-cli --features rest-api

# Watch a .wasm file — reloads processor on save
./target/release/aeon dev watch --artifact ./my-processor.wasm

# Watch a native .so/.dll
./target/release/aeon dev watch --artifact ./my-processor.so
```

This starts a dev pipeline (TickSource → Processor → StdoutSink) that reloads
the processor within ~500ms of file change, with zero event loss during swap.

### 6.3 Validate a Processor Artifact

```bash
# Validate a .wasm file (checks exports, ABI compatibility)
./target/release/aeon validate ./my-processor.wasm

# Validate a native .so/.dll
./target/release/aeon validate ./my-processor.so
```

---

## 7. Workspace Structure

```
aeon/
├── Cargo.toml                    # Workspace root (13 crates + 2 samples)
├── Dockerfile                    # Multi-stage production build
├── docker-compose.yml            # Dev stack (Redpanda, Prometheus, Grafana, etc.)
├── helm/aeon/                    # Helm chart (Deployment + StatefulSet)
├── crates/
│   ├── aeon-types/               # Event, Output, AeonError, core traits
│   ├── aeon-io/                  # Tokio I/O abstraction
│   ├── aeon-state/               # L1 DashMap + L2 mmap + L3 redb
│   ├── aeon-wasm/                # Wasmtime processor runtime
│   ├── aeon-connectors/          # 16 sources + 13 sinks (feature-gated)
│   ├── aeon-engine/              # Pipeline orchestrator, SPSC, REST API
│   ├── aeon-cluster/             # Raft consensus + QUIC transport
│   ├── aeon-crypto/              # Ed25519, PoH, Merkle, encryption
│   ├── aeon-observability/       # OpenTelemetry OTLP, Prometheus
│   ├── aeon-native-sdk/          # SDK for native .so/.dll processors
│   ├── aeon-wasm-sdk/            # SDK for Wasm processors (no_std)
│   ├── aeon-processor-client/    # SDK for T3/T4 network processors
│   └── aeon-cli/                 # Binary entrypoint (`aeon` command)
├── sdks/
│   ├── python/                   # Python SDK (T3 + T4)
│   ├── go/                       # Go SDK (T3 + T4)
│   ├── nodejs/                   # Node.js SDK (T4)
│   ├── java/                     # Java SDK (T4)
│   ├── dotnet/                   # C#/.NET SDK (T1 + T4)
│   ├── php/                      # PHP SDK (T4, 6 deployment models)
│   └── c/                        # C/C++ SDK (T1 + T2)
├── samples/
│   ├── processors/rust-native/   # Sample native Rust processor
│   ├── processors/rust-wasm/     # Sample Wasm processor
│   └── e2e-pipeline/             # E2E benchmark pipeline
└── docs/                         # Architecture, roadmap, guides
```

---

## 8. Troubleshooting

### cmake not found

```
error: failed to run custom build command for `librdkafka-sys`
  cmake not found
```

Install cmake: `apt install cmake` / `brew install cmake` / download from cmake.org.

### OpenSSL not found

```
error: failed to run custom build command for `openssl-sys`
  Could not find directory of OpenSSL installation
```

- Linux: `apt install libssl-dev` or `dnf install openssl-devel`
- macOS: `brew install openssl` then `export OPENSSL_DIR=$(brew --prefix openssl)`
- Windows: install via vcpkg or set `OPENSSL_DIR` environment variable

### Linker errors on Windows

```
LINK : fatal error LNK1181: cannot open input file 'librdkafka.lib'
```

Ensure Visual Studio Build Tools are installed with the "Desktop development with C++"
workload. CMake must be on PATH.

### Rust version too old

```
error: package `aeon-types v0.1.0` cannot be built because it requires
  rustc 1.85 or newer
```

Update Rust: `rustup update`

### Binary locked (Windows, during tests)

```
LINK : fatal error LNK1104: cannot open file 'target\debug\deps\...'
```

A previous test run may still hold the binary. Wait a few seconds or delete the
locked file: `rm -f target/debug/deps/<locked-file>.exe`
