# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Tracking work for v0.2 — see
[`docs/STATEFUL-PROCESSING-EVOLUTION.md`](docs/STATEFUL-PROCESSING-EVOLUTION.md)
for the integrated trajectory.

- **G9.d** — Layer 4 sequence-bounded transitions for processor lifecycle
  (per-partition Raft-committed boundary maps; partition-aware
  `PipelineControl::drain_partitions_at_seq()`; per-pod processor
  instantiation in `cluster_applier`). Foundation for L5/L6.
- **L5** — `WatermarkView` façade exposing existing PoH chain head +
  `AckSeqTracker` + `Event.timestamp` derivations as a uniform
  per-partition watermark API. Thin façade; opens after G9.d lands.
- **L6** — Windowing F1+F2 (tumbling / sliding / session windows;
  per-key state in L1/L2/L3; watermark-based triggers) per
  [`docs/WINDOWING-WATERMARKS-DESIGN.md`](docs/WINDOWING-WATERMARKS-DESIGN.md).
  Keeps original prerequisites (Session B + at least one user demand).
- **Session B (AWS EKS)** — postponed 2026-05-03; resumes when ECR
  bake is operationally scheduled. Throughput ceiling number remains
  the v0.1 → v0.2 substantiation gap.

## [0.1.0] - 2026-05-03

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

### Cluster Correctness — REST follower-routing (added 2026-05-03)

Surfaced and closed a class of bugs where cluster-write REST
endpoints silently no-op'd or partially-applied when called against a
Raft follower. Root cause: the original G9 design (2026-04-19)
returned HTTP 307 Temporary Redirect to point clients at the leader,
but `ureq` (the `aeon-cli` HTTP client) and many off-the-shelf SDK
clients refuse to follow 3xx on POST per RFC 7231 §6.4.7, so the
write silently dropped while the CLI printed success.

- **G9.b — Internal HTTP forwarder middleware** (`cluster_write_forwarder`
  in `crates/aeon-engine/src/rest_api.rs`). Intercepts every
  cluster-write request via the `is_cluster_write_path()` matcher
  (11 unit tests pin the inventory), buffers the body, proxies the
  entire HTTP request to the leader's REST endpoint, returns the
  leader's response verbatim. Loop protection via
  `X-Aeon-Forwarded: 1` header → second hop returns HTTP 421
  Misdirected Request. Pass-through for GET/HEAD, local-only POSTs,
  and on the leader / standalone mode.
- **delete_processor_version Raft replication** —
  `DELETE /api/v1/processors/{name}/versions/{version}` now uses the
  existing `RegistryCommand::DeleteVersion` variant so every node's
  local registry converges (pre-fix the leader's pod deleted locally
  while followers retained the version, risking eventual runtime errors).
- **G9.c — 7 lifecycle endpoints Raft-replicated**: `upgrade/blue-green`,
  `upgrade/canary`, `cutover`, `rollback`, `promote`, `reconfigure/source`,
  `reconfigure/sink`. Adds 7 new `RegistryCommand` variants
  (`BlueGreenStart`, `BlueGreenCutover`, `RollbackUpgrade`,
  `CanaryStart`, `CanaryPromote`, `ReconfigureSource`,
  `ReconfigureSink`) + matching supervisor methods + cluster_applier
  dispatch + REST handler refactor + 7 serde round-trip tests.
  Backfilled the existing `UpgradePipeline` to also trigger the
  supervisor side-effect on followers (closes the pre-existing
  pipeline-wide-only gap).
- **P5.b audit closed as covered-by-existing-design**: per-sink ack-seq
  not carried in `CutoverOffsets` is a non-correctness gap absorbed
  by Aeon's EOS tier semantics (T1-T5 explicitly handle duplicates;
  T6 accepts duplicates by contract).

### Distribution & Documentation (added 2026-05-03)

- **GHCR distribution** — published images at
  `ghcr.io/aeon-rust/aeon` (chosen over Docker Hub which discontinued
  free orgs). OCI labels (`org.opencontainers.image.source`) link the
  image to the source repo on the GitHub Packages tab. Tags published
  per release: `:vNN`, `:<short-sha>`, `:latest`.
- **Connector cookbook** (`docs/CONNECTOR-COOKBOOK.md`) — 14 connector
  type keys (11 sources + 8 sinks) with required + optional config
  keys + copy-pasteable manifest fragments + links to runnable example
  fixtures. 11 new fixtures land in `docs/examples/` (stdout-sink,
  file-source-sink, http-polling, http-sink, http-webhook, websocket,
  redis-streams, nats, postgres-cdc, mysql-cdc, mongodb-cdc); each
  verified via `aeon apply --dry-run` against a live engine.
- **Dev onboarding pass** — README adds three Quickstart paths
  (Quickstart A — pre-built image from GHCR; B — build from source;
  C — multi-node cluster via Helm). `docs/INSTALLATION.md` §3.1 leads
  with the GHCR pull. `docs/CLUSTERING.md` §3.2.1 documents the
  transparent leader-routing contract for cluster-write endpoints.
  `docs/BUILD-FROM-SOURCE.md` updated for GHCR registry. CLI verb
  drift across CLUSTERING / INSTALLATION / BUILD-FROM-SOURCE
  reconciled to the authoritative set in `aeon-cli/src/main.rs`
  (`aeon serve` starts the engine; `aeon apply -f manifest.yaml` is
  the declarative entry point).
- **Stateful-processing evolution doc**
  (`docs/STATEFUL-PROCESSING-EVOLUTION.md`) — integrated trajectory
  for v0.2 broadening: Aeon's existing primitives (PoH chain, L1/L2/L3
  store, UUIDv7, per-source-kind identity, AckSeqTracker, WriteGate)
  ARE the stateful-processing primitives, framed in Aeon's own
  vocabulary not Flink's. Layered progression toward windowing
  (Layer 4 → 5 → 6 → 7) with no parallel uncoordinated stubs. Mapping
  table Flink concept ↔ Aeon vocabulary for readers familiar with
  Flink. Throughput design budget vs Flink (designed-by-structure to
  exceed Flink due to no JVM hop, no serialization between operators,
  inline PoH, native L1).

### Performance Benchmarks

- Per-event overhead: <100ns (Gate 1 target: <100ns)
- Headroom ratio: 130x (Gate 1 target: >=5x)
- CPU utilization: 18.7% when Redpanda saturated (Gate 1 target: <50%)
- Linear partition scaling verified (1-8 partitions)
- Throughput ceiling on AWS EKS premium hardware: **pending Session B
  re-prioritization** (postponed 2026-05-03; ECR pre-bake stays open)

### Test Coverage (post-G9.c)

- **1,717 Rust workspace tests** pass (lib + integration + doc tests
  across 14 crates); 0 failures, 16 ignored (`webtransport-host`
  feature-gated)
- 11 new `cluster_write_matcher` unit tests pinning the
  `is_cluster_write_path` inventory
- 7 new `RegistryCommand` variant serde round-trip tests
- 1 new middleware passthrough test
- 31 Python SDK tests, 20 Go SDK tests, 32 Node.js SDK tests
- 40 C#/.NET SDK tests, 33 PHP SDK tests, 28 Java SDK tests, 22 C/C++ SDK tests
- clippy clean (`-D warnings`); rustfmt clean

[Unreleased]: https://github.com/aeon-rust/aeon/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/aeon-rust/aeon/releases/tag/v0.1.0
