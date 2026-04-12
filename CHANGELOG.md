# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-04-12

### Added

- **Core Pipeline Engine**: SPSC ring buffer architecture (rtrb), batch-first APIs,
  static dispatch on hot path, `#[repr(align(64))]` event envelope.
- **Connectors**: 16 source types, 13 sink types — Kafka/Redpanda, File, HTTP,
  WebSocket, Redis Streams, NATS JetStream, MQTT, RabbitMQ, QUIC, WebTransport,
  PostgreSQL CDC, MySQL CDC, MongoDB CDC, Memory, Blackhole, Stdout.
- **Four-Tier Processor Runtime**:
  - T1: Native shared library (`.so`/`.dll`/`.dylib`) via `libloading`
  - T2: WebAssembly (Wasmtime Component Model) with fuel metering
  - T3: WebTransport (QUIC/HTTP3) out-of-process with AWPP protocol
  - T4: WebSocket (HTTP/1.1/2) out-of-process with AWPP protocol
- **Processor SDKs**: Rust (T1/T2/T3/T4), C/C++ (T1/T2), C#/.NET (T1/T4),
  Python (T3/T4), Go (T3/T4), Node.js (T4), Java (T4), PHP (T4),
  AssemblyScript (T2).
- **Zero-Downtime Upgrades**:
  - Drain-swap: pause source, drain SPSC rings, swap processor, resume (<1ms)
  - Blue-green: shadow processor + atomic cutover + rollback
  - Canary: probabilistic traffic splitting (configurable steps), auto-promote
  - Source/sink same-type reconfiguration via drain-swap
- **Processor Registry**: Versioned artifact catalog with SHA-512 integrity,
  filesystem storage, Raft-replicable metadata.
- **Pipeline Manager**: Full lifecycle (create/start/stop/upgrade/delete),
  blue-green and canary state machines, history tracking.
- **REST API**: 25+ endpoints for processors, pipelines, identities, delivery,
  integrity verification. Bearer token authentication, OWASP security headers.
- **CLI**: `aeon serve`, `aeon processor register/list/deploy`, `aeon pipeline
  create/start/stop/upgrade/promote/rollback`, `aeon dev watch`, `aeon verify`.
- **Cluster Foundation** (single-node, multi-node tested locally):
  - Raft consensus via `openraft` (always-on, even single-node)
  - QUIC inter-node transport via `quinn` with mTLS (rustls + aws-lc-rs)
  - Dynamic membership: join/leave via QUIC RPC, 1-to-N scaling
  - Auto-TLS with self-signed certificates for development
- **Security & Crypto**:
  - AES-256-CTR + HMAC-SHA-512 (Encrypt-then-MAC) payload encryption
  - Ed25519 signing for Proof-of-History chains and Merkle roots
  - TLS: auto-generated, PEM file, ACME modes with cert expiry metrics
  - Processor identity: Ed25519 keypair challenge-response authentication
- **Delivery Architecture**:
  - Configurable delivery semantics: at-most-once, at-least-once, exactly-once
  - Delivery ledger with event identity tracking and checkpoint WAL
  - Adaptive flush tuning (FlushTuner) for throughput/latency balance
  - Core pinning for predictable latency
- **Fault Tolerance**: DLQ, exponential retry with jitter, circuit breaker,
  graceful shutdown with drain.
- **Observability**: Prometheus metrics, Jaeger tracing, structured logging.
- **Multi-Tier State**: L1 DashMap (hot), L2 MmapStore (warm), L3 redb (cold).
- **Proof-of-History**: SHA-512 chain with Ed25519-signed roots, Merkle tree
  proofs, MMR (Merkle Mountain Range) for efficient verification.
- **Kubernetes Deployment**: Helm chart (Deployment + StatefulSet modes),
  headless Service for cluster discovery, HPA, PVC for artifacts/data.
- **Docker**: Multi-stage production image (~173MB), dev and benchmark images,
  Docker Compose stacks for prod and dev (Redpanda, observability).
- **Performance**: Gate 1 passed — 130x headroom ratio, 18.7% CPU utilization,
  <100ns per-event overhead, zero event loss under sustained load.

### Performance Benchmarks

- Per-event overhead: <100ns (Gate 1 target: <100ns)
- Headroom ratio: 130x (Gate 1 target: >=5x)
- CPU utilization: 18.7% when Redpanda saturated (Gate 1 target: <50%)
- Linear partition scaling verified (1-8 partitions)

### Test Coverage

- 763 Rust lib tests, 17 E2E integration tests
- 10 multi-node cluster lifecycle tests
- 31 Python SDK tests, 20 Go SDK tests, 32 Node.js SDK tests
- 40 C#/.NET SDK tests, 33 PHP SDK tests, 28 Java SDK tests, 22 C/C++ SDK tests

[Unreleased]: https://github.com/aeon-rust/aeon/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/aeon-rust/aeon/releases/tag/v0.1.0
