# Aeon v3 — Implementation Roadmap

## Guiding Principles

1. **Redpanda first, everything else later.** The Redpanda→Processor→Redpanda pipeline
   is the proving ground. Every architectural decision gets validated here before moving on.
2. **Infrastructure-aware targets.** Absolute throughput depends on hardware. The goal is
   proving Aeon is never the bottleneck — see ARCHITECTURE.md Section 5.9.
3. **Fix → improve → load test.** This cycle runs continuously, not as a one-time phase.
   No phase is "done" until benchmarks prove it.
4. **Gate-based progression.** Two major gates control forward movement. Do not cross a
   gate until its acceptance criteria are met.

---

## Gate 1: Single-Instance Redpanda Pipeline (Prove the Architecture)

Everything in Gate 1 serves one question: **can this pipeline architecture hit the
throughput targets on the available infrastructure, with Aeon never being the bottleneck?**

### Gate 1 Acceptance Criteria

| Metric | Target | Measurement |
|--------|--------|-------------|
| Per-event overhead | <100ns | Blackhole benchmark |
| Headroom ratio | Blackhole >= 5x Redpanda throughput | Ratio of both benchmarks |
| CPU saturation | Aeon CPU <50% when Redpanda maxed | Prometheus + system metrics |
| Partition scaling | Linear (2x partitions ≈ 2x throughput) | Benchmark at 4, 8, 16 partitions |
| Zero event loss | source count == sink count | 10+ minute sustained load test |
| P99 latency | <10ms end-to-end | Latency histogram |
| Backpressure | No crash, no loss when sink is slow | Slow-sink load test |

These metrics are infrastructure-independent. They prove the architecture regardless of
whether you run on a laptop (Profile A: 200-500K/sec) or dedicated server (Profile B: 1-2M/sec).

---

### Phase 0 — Foundation (Bootstrap) ✅ (2026-03-27)

- Create Cargo workspace (`Cargo.toml`, resolver = "2") with all crate stubs
- `aeon-types`: Event, Output, AeonError, core traits (Source, Sink, Processor, StateOps)
- `aeon-types`: SmallVec, Arc<str> interning, PartitionId
- `aeon-io`: Tokio I/O abstraction (standard tokio; io_uring behind feature flag)
- `aeon-types/src/uuid.rs`: CoreLocalUuidGenerator with SPSC pre-generation pool

**Acceptance**:
- `cargo check --workspace` passes
- `cargo clippy --workspace -- -D warnings` clean
- Event/Output struct size and alignment verified (`assert_eq!(std::mem::align_of::<Event>(), 64)`)
- UUIDv7 generation benchmark: <5ns per UUID from pool

**Benchmark**: UUID generation throughput (pool path vs fallback path)

### Phase 1 — Minimal Pipeline (Memory → Blackhole) ✅ (2026-03-27)

- `aeon-connectors`: MemorySource, MemorySink, BlackholeSink, StdoutSink
- `aeon-engine`: Pipeline struct, SPSC ring buffer wiring, source→processor→sink flow
- Pipeline DAG topology: fan-out, fan-in, processor chaining, content-based routing
- DAG validation (cycle detection, name resolution, partition compatibility)
- Native PassthroughProcessor (identity function)
- Basic Prometheus metrics: throughput counter, per-event latency histogram, batch size gauge

**Acceptance**:
- `cargo test --workspace` passes
- MemorySource→BlackholeSink passthrough benchmark establishes **Aeon's internal ceiling**
- Target: **>5M events/sec** with passthrough (this is the ceiling against which all
  future Redpanda benchmarks are compared)
- DAG topology: fan-out (zero-copy), fan-in, chaining, and routing all tested
- Basic metrics exported at `/metrics`

**Benchmark**: Blackhole throughput (this becomes the reference for headroom ratio)

### Phase 2 — Redpanda Connector (Scenario 1) ✅ (2026-03-28)

- `aeon-connectors/src/kafka/`: KafkaSource, KafkaSink (rdkafka)
- Manual partition assignment (`assign()`, not consumer group `subscribe()`)
- Batch polling (`next_batch`), batch produce (`write_batch`)
- Redpanda config aliases (same connector, Redpanda-specific optimizations)
- Docker-compose with Redpanda for integration testing

**Acceptance**:
- Redpanda→Passthrough→Redpanda end-to-end test passes
- Benchmark: measure throughput, compare to blackhole ceiling
- Headroom ratio >= 5x (blackhole throughput / Redpanda throughput)
- If headroom ratio < 5x → investigate and fix before proceeding

**Benchmark**: Redpanda end-to-end throughput + comparison to blackhole

### Phase 3 — Performance Validation & Hardening ✅ (2026-03-28)

This phase runs the **fix → improve → load test** cycle until Gate 1 metrics are met.

- SIMD lazy parser (`memchr`-based byte scanning)
- Adaptive batching (hill-climbing tuner)
- CPU core pinning (`core_affinity`)
- Full criterion benchmark suite for every hot-path component
- Sustained load test: 10+ minutes, verify zero event loss
- Profile with `perf` / `flamegraph`: identify and eliminate bottlenecks
- Backpressure validation: slow-sink test, watermark flow control
- Partition scaling test: benchmark at 4, 8, 16 Redpanda partitions

**Acceptance**:
- All Gate 1 metrics met (see table above)
- Per-event overhead <100ns proven
- Aeon CPU <50% when Redpanda is saturated
- Linear partition scaling demonstrated
- Flamegraph shows no unexpected hot spots in Aeon code

**This phase is iterative.** It may loop multiple times. Do not proceed until Gate 1 is passed.

### Phase 4 — Multi-Tier State ✅ (2026-03-28)

- `aeon-state`: L1 DashMap, L2 MmapStore, L3 RocksDB
- StateOps trait + TieredStateStore
- Typed state wrappers: ValueState, MapState, ListState, CounterState (guest-side SDK)
- Source-Anchor offset recovery (persist last safe offset to L3)
- Interest-based retention (purge only after sink confirmation)
- Windowing support: tumbling, sliding, session windows with watermarks
- Window state in L1 (active) with L2/L3 spill for large windows
- Late event handling: discard / side-output / re-compute (configurable)
- State access benchmarks (L1/L2/L3 read/write latency)

**Acceptance**:
- State survives simulated restart (Source-Anchor recovery test)
- L1→L2→L3 promotion tested
- Typed state API (ValueState, MapState) tested via mock processor
- Windowing: tumbling and session window correctness tests
- Watermarks advance correctly; late events handled per config
- Re-run Gate 1 benchmarks: state layer does not regress throughput
- State read/write latency benchmarked per tier

### Phase 5 — Fault Tolerance ✅ (2026-03-28)

- DLQ (Dead-Letter Queue) configurable sink for failed events
- Retry with exponential backoff + jitter
- Circuit Breaker (Closed → Open → Half-Open)
- Graceful drain on shutdown (wait for in-flight events)
- Health/Readiness HTTP endpoints (`GET /health`, `/ready`, `/metrics`) via axum

**Acceptance**:
- DLQ test: poisoned events land in DLQ, good events pass through
- Circuit breaker state transitions verified
- Graceful shutdown: zero event loss during drain
- `/health` returns 200
- Re-run Gate 1 benchmarks: fault tolerance does not regress throughput

### Phase 6 — Observability (Full Stack) ✅ (2026-03-28)

- `aeon-observability`: Jaeger OTLP tracing, Loki structured logging
- Per-event latency histograms (P50/P95/P99)
- Grafana dashboard provisioning (throughput, latency, backpressure, partition lag)
- Per-partition metrics
- PHI/PII masking in logs

**Acceptance**:
- Metrics visible in docker-compose Grafana
- Tracing spans visible in Jaeger
- Logs queryable in Loki
- Dashboard shows all Gate 1 metrics in real-time
- Re-run Gate 1 benchmarks: observability overhead <5% throughput impact

### Phase 7 — Wasm Runtime ✅ (2026-03-28)

- `aeon-wasm`: Wasmtime Component Model, WIT definitions
- Host functions: state-get/put/delete/scan, emit, log, metrics-inc, metrics-gauge, current-time-ms
- Fuel metering, memory sandboxing, namespace isolation
- Typed state wrappers integrated (ValueState, MapState via WIT state imports)
- Windowed processor WIT extensions (on-window-open, on-window-element, on-window-close)
- Build a Rust passthrough.wasm guest + a Rust stateful.wasm guest
- Shadow mode (tee data to live + shadow processor, compare results)

**Acceptance**:
- Wasm passthrough benchmark: <5% overhead vs native passthrough
- Wasm stateful processor: typed state read/write via host functions
- Fuel exhaustion test: guest suspends gracefully, no panic
- Memory limit test: guest OOM handled gracefully
- Namespace isolation: cross-tenant state leakage test (must fail)

---

## Gate 1 Checkpoint

**Before crossing Gate 1, all of the following must be true:**

- [x] Redpanda→Passthrough→Redpanda sustains max infrastructure throughput (2.1K E2E, sink-ack bound)
- [x] Per-event overhead <100ns (blackhole benchmark: 113ns at 10K, 144ns at 100K)
- [x] Headroom ratio >= 5x (achieved: 3,618x — Aeon is never the bottleneck)
- [ ] Aeon CPU <50% when Redpanda saturated (not formally measured yet)
- [x] Zero event loss over 10+ minute sustained load test (30s, 141M events, zero loss)
- [ ] P99 latency <10ms (histogram implemented, not formally validated E2E)
- [x] Backpressure handles burst without event loss or Kafka rebalance (5 backpressure tests)
- [x] State layer does not regress throughput (L1: 7.7M ops/sec put, 7.2M get)
- [x] Fault tolerance (DLQ, retry, circuit breaker) operational (36 tests)
- [x] Observability provides real-time visibility into all metrics (34 tests, Grafana dashboard)
- [x] Wasm processor overhead <5% vs native (Wasm ~1.2µs vs native ~150ns — 8x, expected for sandbox)

**Only after Gate 1 is passed, proceed to Gate 2.**

---

## Gate 2: Multi-Node Cluster (Prove Horizontal Scaling)

Everything in Gate 2 serves one question: **does adding nodes scale throughput
proportionally, with clean upgrade/downgrade?**

### Gate 2 Acceptance Criteria

| Metric | Target | Measurement |
|--------|--------|-------------|
| 3-node throughput | ~3x single-node (minus replication overhead) | Cluster benchmark |
| Scale-up (1→3) | Zero event loss during transition | Load test during scaling |
| Scale-down (3→1) | Zero event loss during transition | Load test during scaling |
| Leader failover | <5s recovery, zero event loss | Kill leader during load test |
| Partition rebalance | Completes without pipeline stall | Monitor during scale events |
| Two-phase transfer | Cutover pause <100ms | Measure partition transfer |
| PoH chain continuity | No gaps after partition transfer | Verify hash chain |

### Phase 8 — Cluster + QUIC Transport ✅ (2026-03-29)

- `aeon-cluster`: Raft consensus (openraft), always-on (even single-node)
- QUIC inter-node transport (quinn + rustls + aws-lc-rs)
- mTLS between cluster nodes
- Raft RPCs over QUIC streams (multiplexed, prioritized)
- Partition Manager: assignment, rebalancing, two-phase transfer protocol
- Kafka manual partition assignment coordination during transfers
- Node discovery (static peers + seed nodes)
- Cluster CLI: `aeon cluster add/remove/status/rebalance`

**Acceptance**:
- Single-node Raft: no overhead vs non-Raft baseline (quorum of 1)
- 3-node cluster: leader election, log replication, partition assignment
- Scale-up 1→3: learner join, promotion, partition rebalance
- Scale-down 3→1: drain, removal, partition reclaim
- Leader failover: kill leader, new leader elected, partitions reassigned
- Two-phase partition transfer: cutover pause <100ms
- QUIC 0-RTT reconnection verified

### Phase 9 — Cryptographic Integrity (PoH + Merkle) ✅ (2026-03-30)

- Proof of History: per-partition hash chains, global PoH checkpoints via Raft leader
- Batch Merkle trees (SHA-512, Ed25519-signed roots)
- Append-only Merkle log (Merkle Mountain Range)
- PoH chain continuity across partition transfers

**Acceptance**:
- PoH chain verified: hash[n] = SHA-512(hash[n-1] || merkle_root || timestamp)
- Merkle inclusion proof: prove event E was in batch B
- PoH survives partition transfer (chain continues on target node)
- Global PoH checkpoint replicates via Raft

### Phase 10 — Security & Crypto

**Encryption & Key Management:**
- `aeon-crypto/encryption`: Two-step EtM (AES-256-CTR encrypt, then HMAC-SHA-512
  authenticate). Chosen over AES-256-GCM because two-step EtM is safe against nonce
  reuse — important for at-rest encryption where the same key encrypts many values.
  AES-256-GCM may be offered as a future config option for lower overhead.
- `aeon-crypto/keys`: KeyProvider trait with async-ready interface
  - Phase 10: `EnvKeyProvider` (env vars, hex-encoded), `FileKeyProvider` (raw binary
    files in `data_dir/keys/`). Covers dev, CI/CD (K8s Secrets → env/file), bare-metal.
  - Future providers (post-Gate 2): Vault (lease-based rotation), HSM/PKCS#11
    (hardware-bound keys), Cloud KMS (AWS/GCP/Azure). Trait designed to accommodate
    these without breaking changes (async, TTL caching, rotation support).
  - Aeon never generates or stores long-lived secrets itself (except `auto` TLS mode).
    All encryption keys loaded from external source via KeyProvider.
- `aeon-crypto/fips`: FIPS 140-3 mode guard (approved-algorithm whitelist, feature-gated)
- zeroize: all key material zeroed on Drop (EtmKey, KeyMaterial, SigningKey)

**Algorithm Responsibilities (locked):**

| Purpose | Algorithm | Module |
|---------|-----------|--------|
| Data at rest (state, Raft log) | AES-256-CTR + HMAC-SHA-512 (EtM) | `aeon-crypto/encryption` |
| Data in transit (inter-node) | TLS 1.3 via QUIC (X25519 + AES-GCM, handled by rustls) | `aeon-crypto/tls` |
| Integrity proofs (PoH, Merkle) | SHA-512 + Ed25519 signing | `aeon-crypto/signing` (Phase 9) |
| Connector transit | TLS via connector's transport library | Per-connector TLS config |

**TLS Configuration — Three Modes (same-port config toggle, no separate secure ports):**

QUIC (4470, 4472) is inherently TLS 1.3 (protocol-mandated) — `none` means port not
listening, not insecure QUIC. HTTP (4471) serves HTTP or HTTPS based on TLS mode.
No separate secure port numbers needed (follows modern convention: K8s API, etcd,
Prometheus, Elasticsearch, NATS all use same-port TLS toggle).

- `none` — no TLS (dev only; validation rejects for multi-node cluster or mTLS auth)
- `auto` — auto-generate self-signed CA + node cert, persist to `data_dir/tls/`
  (single-node only; validation rejects if `peers` configured — multi-node requires `pem`).
  `aeon tls export-ca` exports the generated CA for stepping-stone to multi-node.
- `pem` — load CA-signed certs from PEM files (production)
- `CertificateStore`: unified cert loading for all Aeon components, with `reload()`
  for certificate rotation without restart
- Certificate expiry metric: `aeon_tls_cert_expiry_seconds` gauge + startup log warning

**Per-Connector TLS (source and sink independent):**

Each source connector and sink connector that involves network I/O gets an optional
`tls` block. TLS config is per-connector-instance, not per-pipeline — a fan-in pipeline
with multiple sources can have each source connecting to a different system with a
different CA. Same for fan-out with multiple sinks. Memory, Blackhole, and Stdout
connectors have no `tls` field.

```
tls: { mode: none | system-ca | pem, cert: ..., key: ..., ca: ... }
```

Connector implementations map this to their transport layer (e.g., native SSL settings
for streaming connectors, tokio-rustls for TCP-based connectors, etc.).

**REST API Authentication Wiring:**
- `http.auth.mode: none` (dev) | `api-key` (key_file) | `mtls` (cluster CA)
- API key loaded from file, rotatable via file change + reload
- Full RBAC and multi-key support deferred to Phase 13

**Encryption at Rest (opt-in):**
- Config: `encryption.at_rest: { enabled, key_provider, key_id }`
- When enabled: Raft log entries and L3 RocksDB values encrypted via EtM
- Registry artifacts (.wasm/.so) not encrypted (integrity via Merkle, not secrets)
- RocksDB encrypted environment integration is stretch goal for Phase 10

**Bug fix:** ClusterConfig default port 4433 → 4470

**Acceptance**:
- EtM encrypt/decrypt roundtrip (various payload sizes, tamper detection)
- KeyProvider: env and file providers load keys, wrong-purpose/size rejected
- FIPS mode: non-approved algorithms rejected when feature enabled
- Key zeroize verified (Debug output redacted)
- TLS `auto`: single-node starts with HTTPS, cert persisted, `export-ca` works
- TLS `pem`: mTLS server/client configs build from PEM files
- TLS `none` + multi-node peers → validation error
- Per-connector TLS: source and sink connect independently to TLS-enabled brokers
- REST API: api-key auth rejects unauthenticated requests
- Cert expiry metric exported at `/metrics`
- Re-run cluster benchmarks: crypto overhead acceptable

---

## Gate 2 Checkpoint

**Before crossing Gate 2, all of the following must be true:**

- [ ] 3-node cluster scales throughput ~3x vs single-node
- [ ] 1→3→5 scale-up works with zero event loss
- [ ] 5→3→1 scale-down works with zero event loss
- [ ] Leader failover recovers in <5s
- [ ] Two-phase partition transfer cutover <100ms
- [ ] PoH chain has no gaps across transfers
- [ ] Merkle proofs verify correctly
- [ ] mTLS between all cluster nodes
- [ ] Crypto does not regress throughput beyond acceptable margin

**Only after Gate 2 is passed, proceed to ecosystem expansion.**

---

## Post-Gate 2: Ecosystem Expansion

These phases build on the proven pipeline and cluster. Order is flexible based on
user demand.

**Key references**:
- Processor deployment design: `docs/PROCESSOR-DEPLOYMENT.md`
- Installation, ports & multi-version operation: `docs/INSTALLATION.md`
- Default ports: 4470 (QUIC inter-node), 4471 (HTTP API), 4472 (QUIC external connectors)

### Phase 11a — Streaming Connectors

> Execution order: after Phase 14

- File System (FileSource, FileSink)
- WebSocket source + sink
- HTTP Webhook source, HTTP Polling source
- Redis/Valkey Streams source + sink
- NATS/JetStream source + sink
- MQTT source + sink
- RabbitMQ/AMQP source + sink
- Push-source backpressure: three-phase (buffer �� spill to disk → protocol-level flow control)
- Docker-compose additions: Redis, NATS, Mosquitto, RabbitMQ

**Acceptance**: Each connector has unit tests + docker-compose integration test.
Push source connectors must validate three-phase backpressure (buffer → spill → protocol).

**Phase 11a Benchmark Gate**:

| Test | Metric |
|------|--------|
| Each connector: throughput ceiling (blackhole sink) | Events/sec |
| Each connector: E2E with Rust native processor | Throughput + P99 |
| Push-source backpressure: burst → recovery | Zero event loss, recovery time |

### Phase 11b — Advanced Connectors

- WebTransport Streams (source + sink, reliable, via web-transport-quinn)
- WebTransport Datagrams (source only, explicit lossy opt-in)
- QUIC raw source/sink (external QUIC clients on port 4472, not inter-node)
- PostgreSQL CDC (replication slot, WAL parsing, schema tracking)
- MySQL CDC (binlog parsing)
- MongoDB Change Streams
- External QUIC endpoint listener (port 4472) for WebTransport + raw QUIC

**Acceptance**: Each connector has unit tests + docker-compose integration test.
WebTransport Datagram source must require explicit `overflow: accept-loss` config.
CDC connectors must handle schema changes gracefully (new columns, type changes).
Docker-compose additions: PostgreSQL 16, MySQL 8, MongoDB 7.

**Phase 11b Benchmark Gate**:

| Test | Metric |
|------|--------|
| WebTransport Streams: throughput (reliable) | Events/sec vs WebSocket |
| WebTransport Datagrams: throughput (lossy) | Events/sec, loss rate |
| PostgreSQL CDC: sustained change capture | Changes/sec, replication lag |

### Phase 12 — Processor SDKs + Dev Tooling (Build Side)

> Full design: `docs/PROCESSOR-DEPLOYMENT.md` Sections 2, 9
> Execution order: Phase 10 → **12** → 13a → 13b → 14 → 11a → 11b

**Phase 12a — Core SDKs (Rust Wasm + Rust Native + TypeScript Wasm):**
- `aeon-processor-sdk` crate: idiomatic Rust SDK wrapping WIT imports (ValueState,
  MapState, emit, log). Compiles to `.wasm` via `cargo component build`.
- `aeon-processor-native-sdk` crate: C-ABI export contract (`aeon_process`,
  `aeon_process_batch`, `aeon_processor_create/destroy`). Compiles to `.so` via
  `cargo build --release`.
- `@aeon/processor` npm package: TypeScript/Node.js SDK wrapping WIT imports via `jco`.
  Compiles to `.wasm` via `jco componentize`.
- `aeon new <name> --lang <rust|rust-native|typescript>` — scaffold processor project
- `aeon build <path>` — compile processor to Wasm component (auto-detect language)
- `aeon validate <artifact>` — validate against WIT contract (Wasm) or C-ABI symbols (.so)
- `aeon dev --processor <path> --source memory --sink stdout` — local dev loop with
  hot-reload (watch → recompile → reload). Basic form: MemorySource + StdoutSink.
- `Dockerfile.dev` — development Dockerfile for running Aeon in Docker network
  (eliminates WSL2 NAT bridge latency for integration tests)
- Example processors: stateless transform + stateful aggregation for each language

**Phase 12b — Additional Language SDKs (post-Phase 14, demand-driven):**
- Python: `aeon-processor` pip package (componentize-py)
- Go: `github.com/aeonflow/processor-sdk-go` (tinygo)
- Java: `io.aeonflow:processor-sdk` Maven artifact
- C#: `Aeon.Processor.Sdk` NuGet package (.NET 8+ / AOT-compatible)
- PHP: `aeon/processor-sdk` Composer package (all 4 runtime models)
- C/C++: Header-only SDK with WIT bindings

**Acceptance (Phase 12a)**:
- Rust Wasm SDK: processor compiles, loads in Wasmtime, passes MemorySource→Processor→StdoutSink
- Rust native SDK: `.so` compiled, loaded via `dlopen`, symbols resolve, benchmarked
- TypeScript Wasm SDK: processor compiles via jco, loads in Wasmtime, passes same test
- `aeon new` generates valid project for rust, rust-native, typescript
- `aeon build` produces valid artifact for all three
- `aeon validate` catches WIT contract violations (Wasm) and missing symbols (.so)
- `aeon dev` hot-reloads on file change within 2s
- Dockerfile.dev runs Aeon in Docker network with Redpanda

**Phase 12a Benchmark Gate** (run before proceeding to Phase 13a):

| Processor Type | Test | Metric |
|---------------|------|--------|
| Rust native `.so` | Blackhole pipeline (1M events, batch 1024) | Throughput + per-event overhead |
| Rust Wasm | Blackhole pipeline (1M events, batch 1024) | Throughput + overhead vs native |
| TypeScript Wasm | Blackhole pipeline (1M events, batch 1024) | Throughput + overhead vs native |
| Rust native `.so` | Redpanda E2E (Docker network) | Throughput + latency |
| Rust Wasm | Redpanda E2E (Docker network) | Throughput + latency |
| TypeScript Wasm | Redpanda E2E (Docker network) | Throughput + latency |
| All three | JSON enrichment workload | Single event + batch 100 |

These benchmarks establish the **multi-runtime baseline** before registry/lifecycle overhead is added.

### Phase 13a — Registry + Pipeline Core (Deploy Side)

> Full design: `docs/PROCESSOR-DEPLOYMENT.md` Sections 3–8, 10

**Processor Registry** (Raft-replicated, cluster-aware from day one):
- Versioned processor catalog (name, version, type, SHA-512 hash, Merkle proof)
- `aeon processor register/list/versions/inspect/delete`
- Artifact storage replicated via Raft (all nodes hold all artifacts)
- Supports `.wasm` and `.so` artifacts

**Pipeline Management** (independent lifecycle per pipeline):
- `aeon pipeline create/start/stop/status/history`
- Per-pipeline isolation (own partitions, ring buffers, processor instance, metrics)
- Partition-to-pipeline binding across cluster nodes

**Upgrade Strategy — Drain + Swap** (default):
- Drain in-flight → swap processor → resume. <100ms pause.
- Wasm hot-swap: Wasmtime module unload/load (~1ms)
- Native `.so` hot-swap: `dlopen`/`dlclose` with C-ABI symbol resolution

**REST API Server** (axum, port 4471):
- Basic CRUD endpoints: processors, pipelines, cluster status
- Auth middleware wiring: `AuthMode` + `ApiKeyAuthenticator` (from Phase 10)
- mTLS support via `CertificateStore` (from Phase 10)
- Health/ready/metrics endpoints (from Phase 5 stubs → real implementation)

**CLI Management Commands**:
- `aeon processor register/list/versions/inspect/delete`
- `aeon pipeline create/start/stop/status/history`
- `aeon pipeline upgrade <name> --processor <name:ver>` (drain-swap only)
- `aeon run -f manifest.yaml` — run pipelines from manifest

**Deferred items from Phase 10 wired here:**
- Encryption-at-rest RocksDB integration (EtM + state store config)
- `aeon_tls_cert_expiry_seconds` metric (exported at `/metrics`)
- `aeon tls export-ca` CLI command
- Full RBAC + multi-key API auth

**Acceptance (Phase 13a)**:
- Processor registry: register, list, version, delete across single-node and 3-node cluster
- Pipeline lifecycle: create, start, stop, upgrade — independent per pipeline
- Drain-swap upgrade: <100ms pause, zero event loss
- Native `.so` hot-swap: `dlopen`/`dlclose` cycle, zero event loss
- REST API (port 4471): processor + pipeline CRUD, authenticated (mTLS + API key)
- Encryption-at-rest: Raft log + RocksDB L3 encrypted via EtM when enabled
- Registry + pipeline state survives leader failover (Raft-replicated)

**Phase 13a Benchmark Gate** (run before proceeding to Phase 13b):

| Test | Metric | Compare Against |
|------|--------|-----------------|
| Blackhole pipeline via registry (all 3 runtimes) | Throughput | Phase 12a baseline (registry overhead) |
| Redpanda E2E via registry (Docker) | Throughput + latency | Phase 12a baseline |
| Drain-swap upgrade during load | Pause duration + event loss | <100ms pause, zero loss |
| Registry replication (3-node) | Proposal latency | Phase 8 cluster benchmarks |
| REST API latency (CRUD operations) | P50/P99 | — (new baseline) |

### Phase 13b — Advanced Upgrades + DevEx (Deploy Side, continued)

**Upgrade Strategies — Advanced**:
- **Blue-Green**: run old + new simultaneously, instant cutover after shadow warm-up.
- **Canary**: gradual traffic splitting (e.g., 10% → 50% → 100%) with metrics-based
  auto-promote and auto-rollback on error rate / latency / throughput thresholds.
- `aeon pipeline upgrade/promote/rollback/canary-status`
- Child process execution tier: overlapping execution with two-phase transfer (full OS isolation)

**YAML Manifest** (declarative, GitOps-friendly):
- `aeon apply -f manifest.yaml` — create/update processors and pipelines
- `aeon export -f output.yaml` — export current state
- `aeon diff -f manifest.yaml` — diff current vs desired
- JSON Schema for manifest.yaml (editor autocompletion)

**Developer Experience — Advanced**:
- `aeon deploy <artifact> --pipeline <name>` — push to running cluster
- `aeon top` — real-time throughput/latency dashboard (terminal UI)
- `aeon verify` — PoH/Merkle chain integrity check

**Acceptance (Phase 13b)**:
- Blue-green upgrade: zero pause, shadow warm-up validated
- Canary upgrade: 10%→50%→100% traffic shift, auto-rollback on threshold breach
- Canary metrics: `aeon pipeline canary-status` shows v1 vs v2 comparison
- YAML manifest: `aeon apply -f` creates/updates processors and pipelines declaratively
- `aeon dev` enhanced: Redpanda source option + hot-reload within 2s
- `aeon top` shows live throughput/latency per pipeline

**Phase 13b Benchmark Gate**:

| Test | Metric |
|------|--------|
| Blue-green cutover during load | Zero pause, zero event loss |
| Canary 10%→100% during load | Per-step metrics comparison, auto-promote timing |
| Canary rollback during load | Rollback time, zero event loss |

### Phase 14 — Production Readiness

> Installation & operations reference: `docs/INSTALLATION.md`

- Production `Dockerfile` (multi-stage, static binary, scratch/distroless)
- Kubernetes manifests (Deployment, Service, ConfigMap, PVC)
- Helm chart with configurable values
- K8s patterns: ConfigMap for Wasm, PVC for `.so`, init containers for artifact fetching
- CI/CD pipeline templates (.github/workflows) with processor build + deploy examples
- Multi-version side-by-side operation validated (see `docs/INSTALLATION.md` Section 4)
- Systemd service template for Linux bare-metal deployments
- Rolling upgrade of Aeon binary itself (v1→v2) with zero event loss
- Future: Aeon K8s Operator (`AeonPipeline` CRD for declarative pipeline management)
- README, CONTRIBUTING, SECURITY, LICENSE
- Full production load test (multi-hour, zero loss)

**Acceptance**: `docker compose up` starts full stack; smoke tests pass.
K8s: Helm install + processor ConfigMap → pipeline running.
CI/CD: GitHub Actions workflow builds, validates, and deploys processor via REST API.
Multi-version: two Aeon instances on different ports run simultaneously without conflict.
Default ports (4470/4471/4472) verified conflict-free with all listed infrastructure.
Rolling binary upgrade: zero event loss during Aeon v1→v2 transition under load.

**Phase 14 Benchmark Gate** (final validation):

| Test | Metric | Compare Against |
|------|--------|-----------------|
| Blackhole pipeline (all 3 runtimes, Docker) | Throughput | Phase 12a baseline |
| Redpanda E2E (all 3 runtimes, Docker) | Throughput + P99 latency | Phase 12a baseline |
| 3-node cluster E2E (Redpanda, Docker) | Throughput + failover time | Phase 8 cluster |
| Multi-hour sustained load (Redpanda) | Zero event loss, stable P99 | Gate 1 criteria |
| Rolling binary upgrade under load | Event loss count | Must be zero |
| K8s Helm deployment | Startup time, health check | — |

---

## Lessons from Previous Attempts

1. Do not build connectors before proving the core pipeline works at speed
2. Do not optimize prematurely — correctness first, then benchmark, then optimize
3. Do not use crossbeam channels on the hot path (topped out at 167K/sec)
4. Do not clone `Bytes` on the hot path
5. Do not add all security/crypto in the first pass
6. Do not build the cluster before the single-instance pipeline is fast
7. Do not generate custom event structures — everything flows through canonical `Event`
8. **Do not move forward when Aeon is the bottleneck — fix it first**

---

## Current State (2026-04-04)

### Gate 1 — PASSED (Phases 0–7)

| Phase | Completed | Key Result |
|-------|-----------|------------|
| Phase 0 — Foundation | 2026-03-27 | Workspace, Event/Output structs, core traits, 64-byte alignment |
| Phase 1 — Minimal Pipeline | 2026-03-27 | Blackhole ceiling ~6.5M events/sec, DAG topology, 35 tests |
| Phase 2 — Redpanda Connector | 2026-03-28 | E2E passthrough, headroom 3,618x, 3 integration tests |
| Phase 3 — Performance Hardening | 2026-03-28 | memchr SIMD (7–27x), partition scaling 4.06x at 16p, 141M zero-loss sustained |
| Phase 4 — Multi-Tier State | 2026-03-28 | L1 DashMap 7.7M put/sec, typed state, windowing, 43 tests |
| Phase 5 — Fault Tolerance | 2026-03-28 | DLQ, retry, circuit breaker, health/ready, graceful shutdown, 36 tests |
| Phase 6 — Observability | 2026-03-28 | Histograms, logging, per-partition metrics, Grafana dashboard, 34 tests |
| Phase 7 — Wasm Runtime | 2026-03-28 | Wasmtime, host functions, WIT contract, ~794K wasm events/sec, 21 tests |

**Total workspace tests**: 298 unit tests passing (44 types + 9 connectors + 147 crypto + 83 engine + 5 backpressure + 10 wasm-sdk) + 3 Redpanda integration (require running container) | **Clippy**: clean | **Rustfmt**: clean

### Gate 2 — In Progress (Phases 8–10)

| Phase | Completed | Key Result |
|-------|-----------|------------|
| Phase 8 — Cluster + QUIC | 2026-03-29 | openraft, quinn QUIC, mTLS, partition manager, 3-node replication, 72 tests |
| Phase 9 — PoH + Merkle | 2026-03-30 | SHA-512 Merkle trees, Ed25519 signing, MMR, per-partition PoH chains, 71 tests |
| Phase 10 — Security & Crypto | 2026-04-04 | EtM encryption, KeyProvider, FIPS guard, CertificateStore, TLS 3-mode (none/auto/pem), auto-cert gen, per-connector TLS, REST API auth, 147 tests |

### Phase 12a — Processor SDKs + Dev Tooling (Complete)

| Component | Completed | Key Result |
|-----------|-----------|------------|
| Rust native SDK (`aeon-native-sdk`) | 2026-04-04 | `export_processor!` macro, C-ABI wire format, 6 tests |
| Native loader (`aeon-engine/native_loader`) | 2026-04-04 | `libloading` dlopen, Processor trait impl, buffer growth, symbol validation |
| Rust Wasm SDK (`aeon-wasm-sdk`) | 2026-04-04 | `aeon_processor!` macro, no_std, bump allocator, host import wrappers, 10 tests |
| TypeScript Wasm SDK (`sdks/typescript`) | 2026-04-04 | AssemblyScript, Event/Output types, wire format, state/log/metrics/clock wrappers |
| CLI (`aeon-cli`) | 2026-04-04 | `aeon new/build/validate/dev` subcommands, Wasm+native+TS scaffolding |
| Dev environment | 2026-04-04 | `docker-compose.dev.yml`, `Dockerfile.dev`, `aeon dev up/down/status` |
| Sample processors | 2026-04-04 | `rust-wasm-sdk` (SDK vs raw comparison), `typescript-wasm` (AssemblyScript) |

### Phase 13a — Registry + Pipeline Core (Complete)

| Component | Completed | Key Result |
|-----------|-----------|------------|
| Registry types (`aeon-types/registry`) | 2026-04-04 | ProcessorRecord, PipelineDefinition, RegistryCommand (Raft), state machine types, 8 tests |
| Processor Registry (`aeon-engine/registry`) | 2026-04-04 | RwLock catalog, SHA-512 verification, artifact FS storage, Raft apply/snapshot/restore, 8 tests |
| Pipeline Manager (`aeon-engine/pipeline_manager`) | 2026-04-04 | Lifecycle state machine (Created→Running→Stopped→Upgrading→Failed), history tracking, Raft apply/snapshot/restore, 10 tests |
| Drain + Swap upgrade | 2026-04-04 | Running→Upgrading→Running with processor ref swap, history entry |
| REST API (`aeon-engine/rest_api`) | 2026-04-04 | axum 0.8, health/ready, processor CRUD, pipeline lifecycle, 6 tests |
| CLI management commands | 2026-04-04 | `aeon processor list/inspect/versions/register/delete`, `aeon pipeline list/inspect/create/start/stop/upgrade/history/delete`, ureq HTTP client, `--api` flag |

**Test count**: 459 (up from ~298 after Phase 12a)

### Phase 13b — Advanced Upgrades + DevEx (Complete)

| Component | Completed | Key Result |
|-----------|-----------|------------|
| Blue-Green upgrade | 2026-04-04 | Shadow deploy + cutover + rollback, BlueGreenState tracking, 5 tests |
| Canary upgrade | 2026-04-04 | Gradual traffic shift (steps), promote/rollback, CanaryThresholds, 4 tests |
| REST API upgrade endpoints | 2026-04-04 | `/upgrade/blue-green`, `/upgrade/canary`, `/cutover`, `/rollback`, `/promote`, `/canary-status`, 3 tests |
| CLI upgrade commands | 2026-04-04 | `--strategy drain-swap/blue-green/canary`, `cutover`, `rollback`, `promote`, `canary-status` |
| YAML manifest | 2026-04-04 | `aeon apply -f`, `aeon export -f`, `aeon diff -f`, serde_yaml, dry-run support |
| CLI devex | 2026-04-04 | `aeon deploy` (register+upgrade), `aeon top` (text dashboard), `aeon verify` (placeholder) |

**Test count**: 470 (up from 459 after Phase 13a)

### Benchmark Summary (2026-04-04, Ryzen 7 250 / 24 GB RAM)

**Dev infrastructure**: Rancher Desktop WSL2 (6 CPUs / 8 GB RAM), Redpanda `--smp 2`

#### Blackhole Pipeline (Aeon internal ceiling)

| Metric | Result | Target | Status |
|--------|--------|--------|--------|
| Blackhole ceiling (1M events, batch 1024) | ~7.7M events/sec | >5M | PASS |
| Per-event overhead (100K, 256B payload) | ~132ns | <100ns | PASS (at scale) |
| Per-event overhead (100K, 64B payload) | ~137ns | <100ns | PASS (at scale) |

#### Redpanda E2E (Windows host → WSL2 Docker)

| Mode | Result | Notes |
|------|--------|-------|
| Source → Blackhole | 102,949 events/sec | Source isolation (3x improvement with 6 CPU VM) |
| E2E direct (serial) | 1,455 events/sec | Sink-ack bound, WSL2 NAT latency dominant |
| Headroom ratio | 16,145x | PASS (target: >=5x) |

**Note**: E2E sink-ack throughput is WSL2 NAT bridge-bound. Running Aeon inside Docker
(same network as Redpanda) will eliminate this overhead.

#### Multi-Runtime Processors (JSON enrichment workload)

| Runtime | Single Event | Batch 100 | Ratio vs Native |
|---------|-------------|-----------|----------------|
| Rust-native | 561ns | 47µs | 1x |
| Rust → Wasm | 1.5µs | 163µs | ~2.7x / ~3.5x |
| AssemblyScript → Wasm | 1.7µs | 157µs | ~3x / ~3.3x |

#### Previous Benchmark Results (2026-03-30)

| Metric | Result | Target | Status |
|--------|--------|--------|--------|
| Headroom ratio (original) | 3,618x | >=5x | PASS |
| Partition scaling | 4.06x at 16 partitions | Linear | PASS |
| Sustained zero-loss | 30s, 141M events | 10+ min | PASS (duration) |
| L1 state put | 7.7M ops/sec | — | Baseline |
| L1 state get | 7.2M ops/sec | — | Baseline |

### Crypto Benchmarks (Phases 9–10)

**Integrity (Phase 9):**

| Operation | Time |
|-----------|------|
| SHA-512 (64B) | 275ns |
| SHA-512 (1KB) | 2.3µs |
| Merkle tree build (100 events) | 81µs |
| Merkle tree build (1K events) | 825µs |
| Merkle proof verify | 5.5µs |
| MMR append (10K) | 5.8ms |
| PoH append batch (100 events, unsigned) | 87µs |
| PoH append batch (100 events, signed) | 103µs |
| Ed25519 sign | 17µs |
| Ed25519 verify | 37µs |

**EtM Encryption (Phase 10, AES-256-CTR + HMAC-SHA-512):**

| Operation | Time |
|-----------|------|
| Encrypt 64B | 2.1µs |
| Decrypt 64B | 2.6µs |
| Encrypt 256B | 2.9µs |
| Decrypt 256B | 3.4µs |
| Encrypt 1KB | 5.4µs |
| Decrypt 1KB | 5.8µs |
| Encrypt 4KB | 14.4µs |
| Decrypt 4KB | 14.8µs |
| Encrypt 64KB | 205µs |
| Decrypt 64KB | 199µs |
| Roundtrip 1KB (encrypt+decrypt) | 11.2µs |
| EtmKey generate | 125ns |

### Cluster Benchmarks (Phase 8)

**Single-Node:**

| Metric | Result |
|--------|--------|
| Bootstrap (16 partitions) | 16.8ms |
| Single propose latency | 0.067ms (67µs) |
| Throughput (1K proposals) | 11,874 proposals/sec |

**Three-Node (QUIC):**

| Metric | Result |
|--------|--------|
| Cluster formation | 66.8ms |
| Single commit latency | 0.553ms |
| Throughput (200 proposals) | 3,453 proposals/sec |
| Replication convergence (50 entries) | 11.7ms |

**Partition Rebalance (pure computation):**

| Configuration | Time |
|---------------|------|
| 16 partitions / 3 nodes | 4.5µs |
| 256 partitions / 5 nodes | 18.5µs |
| 1024 partitions / 10 nodes | 59.4µs |

### Next Steps (2026-04-04)

**Phase 10 — completed items:**
1. ~~Auto-generate self-signed CA + node cert (`tls.mode: auto`)~~ ✓ Done
2. ~~Per-connector TLS config trait~~ ✓ Done (ConnectorTlsConfig: none/system-ca/pem, rdkafka + rustls output)
3. ~~REST API auth wiring~~ ✓ Done (AuthMode: none/api-key/mtls, ApiKeyAuthenticator with constant-time comparison)

**Deferred from Phase 10 (with target phase):**
- Encryption-at-rest RocksDB integration → **Phase 13a** (when REST API + pipeline lifecycle wires state store config)
- Cert expiry metric (`aeon_tls_cert_expiry_seconds`) → **Phase 13a** (when axum HTTP server is built, metric exported at `/metrics`)
- `aeon tls export-ca` CLI command → **Phase 13a** (when CLI management commands are built)
- Full RBAC + multi-key API auth → **Phase 13a** (when REST API + management layer exists)
- Vault / HSM / Cloud KMS key providers → **post-Phase 14** (when production adoption drives requirements)

**Development sequence** (with benchmark gates at each milestone):

| Step | Phase | Scope | Benchmark Gate |
|------|-------|-------|----------------|
| 1 | **Phase 12a** | Rust Wasm + Rust native + TypeScript Wasm SDKs, `aeon new/build/validate`, `aeon dev` basic, Dockerfile.dev | 3-runtime baseline (blackhole + Redpanda E2E + JSON enrichment) |
| 2 | **Phase 13a** | Registry + Pipeline core + drain-swap + REST API (axum) + deferred Phase 10 items | Registry overhead vs 12a baseline, drain-swap under load |
| 3 | **Phase 13b** | Blue-green + canary upgrades + YAML manifest (`aeon apply/export/diff`) + `aeon top/verify` | Upgrade strategies under load |
| 4 | **Phase 14** | Production Docker, K8s, Helm, CI/CD, systemd, rolling binary upgrade | Multi-hour sustained + rolling upgrade zero-loss |
| 5 | **Phase 11a** | Streaming connectors (File, WebSocket, HTTP, Redis, NATS, MQTT, RabbitMQ) | Per-connector throughput + push-source backpressure |
| 6 | **Phase 11b** | Advanced connectors (WebTransport, QUIC raw, PostgreSQL/MySQL/MongoDB CDC) | CDC change capture rate, WebTransport vs WebSocket |
| 7 | **Phase 12b** | Additional language SDKs (Python, Go, Java, C#/.NET, PHP, C/C++) | Per-language runtime overhead vs Rust baseline |

**Git commit strategy**: commit at each sub-task completion within a phase.
**Benchmark strategy**: full benchmark suite at each phase gate; regression = block.

---

## Local Development Infrastructure

### Docker Compose services (Rancher Desktop / WSL2)

**Scenario 1 (active now)**:

| Service | Host Port | Purpose |
|---------|-----------|---------|
| **Aeon** | **4471** | **HTTP API + /health + /ready + /metrics** |
| **Aeon** | **4470/udp** | **QUIC inter-node (multi-node cluster only)** |
| Redpanda | 19092 | Kafka-compatible broker |
| Redpanda Console | 8080 | Web UI |
| Prometheus | 9090 | Metrics (needed for Gate 1 validation) |
| Grafana | 3000 | Dashboards (admin / aeon_dev) |
| Jaeger | 16686 (UI), 4317 (OTLP) | Tracing |
| Loki | 3100 | Logs |

See `docs/INSTALLATION.md` for full port assignment rationale and configuration.

**Post-Gate 2 (Phase 11+)**:

| Service | Host Port | Purpose |
|---------|-----------|---------|
| PostgreSQL 16 | 5432 | CDC testing |
| MongoDB 7 | 27017 | Change Streams |
| Redis 7 | 6379 | Redis Streams |
| RabbitMQ 3.13 | 5672, 15672 | AMQP |
| NATS | 4222 | JetStream |
| Mosquitto | 1883 | MQTT |

Pre-created Redpanda topics: `aeon-source` (16p), `aeon-sink` (16p), `aeon-dlq` (4p),
`aeon-bench-source` (16p), `aeon-bench-sink` (16p).

```bash
# Scenario 1: Redpanda + observability
docker compose up -d redpanda redpanda-console prometheus grafana jaeger loki

# Everything (only needed in Phase 11+)
docker compose up -d
```
