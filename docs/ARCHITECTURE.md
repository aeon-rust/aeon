# Aeon v3 — Architecture Specification

> **Third from-scratch build.** Previous attempts reached ~167K events/sec (blackhole) / ~34K/sec
> end-to-end Redpanda. This version targets **20M events/sec aggregate** through fundamentally
> different design decisions: zero-copy hot path, batch-first APIs, SPSC ring buffers, Raft
> consensus, Proof of History, and QUIC-native inter-node transport.

---

## 1. Project Identity

- **Language**: Rust (stable toolchain, latest edition)
- **License**: Apache 2.0
- **Architecture**: Interface-first (traits defined before implementations, always)
- **Development model**: Feature-by-feature with mandatory tests and benchmarks at each step
- **Primary scenario (Scenario 1)**: Redpanda source -> Processor (Rust native + Wasm multi-language) -> Redpanda sink

---

## 2. Workspace Structure

```
aeon/
├── Cargo.toml                    # Workspace root (resolver = "2")
├── CLAUDE.md                     # AI coding guidelines
├── LICENSE
├── README.md
├── wit/                          # WIT interface definitions (Wasm Component Model)
│   └── aeon-processor.wit        # process() + process-batch() exports
├── docker-compose.yml            # Local dev stack
├── docs/
│   ├── ARCHITECTURE.md           # This file
│   └── ROADMAP.md                # Phase plan
├── benches/                      # Workspace-level benchmarks
└── crates/
    ├── aeon-types/               # Canonical Event envelope, ALL shared traits, error types
    ├── aeon-io/                  # tokio-uring zero-copy I/O abstraction layer
    ├── aeon-state/               # L1 DashMap + L2 mmap + L3 redb/RocksDB multi-tier state
    ├── aeon-wasm/                # Wasmtime host, WIT contracts, fuel metering, multi-language
    ├── aeon-connectors/          # All Source/Sink trait implementations (feature-gated)
    │   └── src/{kafka,redis,nats,mqtt,...}/
    ├── aeon-engine/              # Pipeline orchestrator, router, backpressure, adaptive batching
    ├── aeon-cluster/             # Raft consensus + PoH + Merkle + QUIC inter-node transport
    ├── aeon-crypto/              # EtM encryption, signing, Merkle trees, key management
    ├── aeon-observability/       # Prometheus, Jaeger (OTLP), Loki, Grafana provisioning
    └── aeon-cli/                 # Binary entrypoint, YAML manifest parsing, CLI commands
```

Each connector lives in its own `{technology}/` folder co-locating `mod.rs`, `config.rs`,
`source.rs`, `sink.rs`. Every connector is behind a Cargo feature flag.

---

## 3. Interface-First Architecture

**Rule: Traits are defined BEFORE implementations. Always.**

All connectors, processors, state backends, and transport layers are built on Rust traits.
Code against the trait, not the concrete type.

### Core traits (aeon-types)

**Phase 0 — Gate 1 (implement now):**
```
Source              — event ingestion (next_batch() returns Vec<Event>)
Sink                — event delivery (write_batch(Vec<Output>))
Processor           — event transformation (process / process_batch)
StateOps            — key-value state (get, put, delete, scan)
Seekable            — source rewind capability (seek to offset)
IdempotentSink      — sink deduplication capability
```

**Gate 2 (implement with cluster):**
```
QuicTransport       — QUIC connection lifecycle
KeyProvider         — cryptographic key management
EtmCipher           — encrypt-then-MAC construction
AeonSigner          — digital signature operations
MerkleTreeProvider  — Merkle tree construction and proof generation
PohRecorder         — Proof of History recording
```

**Post-Gate 2 (implement with additional connectors):**
```
StreamConnector     — streaming source abstraction (subscribe, poll, commit)
StreamProducer      — streaming sink abstraction (send, send_batch, flush)
ObjectStorageConnector — blob storage (get, put, list, delete)
DatabaseConnector   — relational DB operations
KeyValueConnector   — KV store operations
QueueConnector      — message queue operations
```

Do not define post-Gate 2 traits prematurely — they will change as connector needs
become concrete. Define them when their phase begins.

### Trait dispatch policy (for 20M/sec target)

| Context | Dispatch | Reason |
|---------|----------|--------|
| Hot-path (Source/Sink) | **Generics (static dispatch)** | Compiler inlining, zero vtable overhead |
| Plug-and-play (Processor) | `dyn Processor` trait objects | Runtime-loaded Wasm guests |
| QUIC transport | `dyn QuicTransport` | Runtime-selectable TLS/congestion config |
| State operations | Generics with trait bounds | L1/L2/L3 tier selection at compile time |

---

## 4. Canonical Event Envelope

The **Event** is the universal data unit. All sources produce Events, all processors
consume/emit Events, all sinks receive Outputs (derived from Events).

```rust
#[repr(align(64))]  // Cache-line aligned — prevents false sharing
pub struct Event {
    pub id: uuid::Uuid,               // UUIDv7 via CoreLocalUuidGenerator
    pub timestamp: i64,                // Unix epoch nanoseconds
    pub source: Arc<str>,              // Source identifier — interned, NOT String (zero alloc)
    pub partition: PartitionId,        // Partition for parallel processing
    pub metadata: SmallVec<[(Arc<str>, Arc<str>); 4]>,  // Inline up to 4 headers, no heap
    pub payload: bytes::Bytes,         // Zero-copy payload — NEVER clone on hot path
    pub source_ts: Option<std::time::Instant>, // For latency tracking
}

#[repr(align(64))]
pub struct Output {
    pub destination: Arc<str>,         // Sink/topic name — interned
    pub key: Option<bytes::Bytes>,     // Optional partition key (zero-copy)
    pub payload: bytes::Bytes,         // Zero-copy output payload
    pub headers: SmallVec<[(Arc<str>, Arc<str>); 4]>,
    pub source_ts: Option<std::time::Instant>, // Propagated from Event for latency tracking
}
```

**Hot-path allocation rules**:
- `source` / `destination`: Use `Arc<str>` — allocated once at pipeline init, shared by reference
- `metadata` / `headers`: `SmallVec` with inline capacity 4 — no heap for typical events
- `payload`: `bytes::Bytes` — reference-counted, zero-copy slice from source buffer
- **NEVER** use `String` or `HashMap` in Event/Output on the hot path
- All structures on the hot path use `#[repr(align(64))]` to prevent false sharing

### Error taxonomy (AeonError)

All error handling uses `Result<T, AeonError>`. Error categories:

| Category | Examples | Retryable? |
|----------|----------|------------|
| `Connection` | Broker unreachable, TLS handshake failure | Yes (with backoff) |
| `Serialization` | Malformed event, bincode decode failure | No (send to DLQ) |
| `State` | L3 (redb/RocksDB) write failure, mmap error | Depends on cause |
| `Processor` | Wasm trap, fuel exhausted, guest panic | No (send to DLQ) |
| `Config` | Invalid manifest, missing field | No (fatal at startup) |
| `Cluster` | Raft error, partition transfer failure, QUIC disconnect | Yes (Raft retry) |
| `Crypto` | Decryption failure, invalid signature, key not found | No (fatal or DLQ) |
| `Resource` | Memory limit, disk full, file descriptor exhaustion | No (fatal, graceful drain) |
| `Timeout` | Source poll timeout, sink flush timeout | Yes (with backoff) |

`thiserror` for all library crates (typed, matchable). `anyhow` only in `aeon-cli`.

### Retry layering (where `BackoffPolicy` applies)

"Retryable" in the table above means *a retry is semantically safe* — it
does **not** mean Aeon automatically retries at that layer. Retries are
layered intentionally:

- **Connector source/sink** — retries with `BackoffPolicy` on
  `Connection` / `Timeout` errors. Resets on first success.
- **Cluster bootstrap/join** — `QuicEndpoint::connect_with_backoff` is
  the opt-in retry path; bounded `max_attempts`.
- **openraft RaftNetwork RPC** — **fail-fast**. openraft drives its own
  protocol-level retry. Adding another retry layer here would block a
  Raft client task long enough to delay heartbeats and trigger spurious
  elections. Plain `QuicEndpoint::connect` is used on this path for
  exactly this reason.
- **Pipeline sink writes** — governed by `BatchFailurePolicy` (retry /
  skip / DLQ), an application concern decoupled from transport retries.

See `docs/FAULT-TOLERANCE-ANALYSIS.md §5 Connection 5` for the full
rationale; `docs/CLUSTERING.md §2 Connection retry and backoff` for the
operator-facing summary.

---

## 5. Performance Architecture (20M Events/Sec Target)

### 5.1 The throughput math

- 20M events/sec at ~256 bytes/event = ~5 GB/sec raw throughput
- Single Redpanda broker: ~1-2M messages/sec (infrastructure ceiling — see Section 5.9)
- **Per-instance target: 1-2M events/sec** (10-20 instances in cluster for 20M aggregate)
- Per-event budget: **50 nanoseconds** (at 20M/sec)
- **Key principle**: Aeon must never be the bottleneck — the external system saturates first

### 5.2 Zero-copy hot path

Events flow as `bytes::Bytes` slices from the source consumer buffer directly through the
processor and into the producer buffer. **No deserialize-reserialize** unless the processor
explicitly mutates the payload.

```
Redpanda consumer buffer
    → Bytes::slice() (zero-copy reference)
        → Event { payload: Bytes }
            → Processor (reads payload, emits Output)
                → Output { payload: Bytes }
                    → Redpanda producer buffer (zero-copy)
```

### 5.3 Batch-first APIs

The `Source` trait returns `Vec<Event>` (batches), not single events.
The `Sink` trait accepts `Vec<Output>` (batches), not single outputs.

```rust
pub trait Source: Send + Sync {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError>;
}

pub trait Sink: Send + Sync {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<(), AeonError>;
    async fn flush(&mut self) -> Result<(), AeonError>;
}
```

### 5.4 SPSC ring buffer (not channels)

At 20M/sec, crossbeam channels add ~50ns/event overhead. Use single-producer-single-consumer
(SPSC) ring buffers (`rtrb` crate) for source→processor and processor→sink paths.
Crossbeam channels are acceptable for control plane only.

### 5.5 SIMD-accelerated parsing

Use `memchr` crate for all hot-path byte scanning. Router destination matching, filter
predicates, and header field extraction must use SIMD byte scanning.

```rust
// GOOD: SIMD byte scan
simd_contains(event.payload.as_ref(), b"\"priority\":\"high\"")

// BAD: Parse entire JSON to check one field
let v: Value = serde_json::from_slice(&event.payload)?;
v["priority"] == "high"
```

### 5.6 Adaptive batching

The engine monitors traffic patterns:
- **During lulls**: Prioritize sub-millisecond latency (small batches)
- **During spikes**: Automatically group events into SIMD "lanes" (large batches)
- Hill-climbing tuner adjusts batch size via `AtomicUsize`

### 5.7 CPU core pinning

Dedicated Tokio runtimes pinned to specific cores via `core_affinity` crate:
- Source runtime (ingestion cores)
- Processor runtime (compute cores)
- Sink runtime (egress cores)

### 5.8 UUIDv7 Generation at 20M/sec

#### The problem

At 20M events/sec = 20,000 UUIDs per millisecond:
- Standard `uuid::Uuid::now_v7()` calls `getrandom()` syscall: 100-200ns per call
- At 50ns/event budget, a single CSPRNG syscall blows the entire per-event budget
- The 12-bit sub-millisecond sequence counter only covers 4,096/ms

#### Solution: Per-Core Monotonic UUIDv7 with Pre-Generation Pool

**Bit layout (128 bits, UUIDv7-compatible):**

```
| 48-bit ms timestamp | 4-bit ver(0x7) | 12-bit counter | 2-bit variant | 6-bit core_id | 56-bit random |
```

- **12-bit monotonic counter**: 0..4095 per millisecond per core
- **6-bit core_id**: 0..63 — eliminates cross-core collisions entirely
- **56-bit random**: 7.2x10^16 unique values per counter slot

**Architecture:**

```
Per-Core Architecture (zero contention between cores):

  Core 0                          Core 1                          Core N
  ┌──────────────┐                ┌──────────────┐                ┌──────────────┐
  │ Background   │                │ Background   │                │ Background   │
  │ Generator    │                │ Generator    │                │ Generator    │
  └──────┬───────┘                └──────┬───────┘                └──────┬───────┘
         │ fill                          │ fill                          │ fill
         ▼                               ▼                               ▼
  ┌──────────────┐                ┌──────────────┐                ┌──────────────┐
  │ SPSC Ring    │                │ SPSC Ring    │                │ SPSC Ring    │
  │ Buffer       │                │ Buffer       │                │ Buffer       │
  │ (64K UUIDs)  │                │ (64K UUIDs)  │                │ (64K UUIDs)  │
  └──────┬───────┘                └──────┬───────┘                └──────┬───────┘
         │ pop (1-2ns)                   │ pop (1-2ns)                   │ pop (1-2ns)
         ▼                               ▼                               ▼
  ┌──────────────┐                ┌──────────────┐                ┌──────────────┐
  │ Hot Path     │                │ Hot Path     │                │ Hot Path     │
  └──────────────┘                └──────────────┘                └──────────────┘
```

**Implementation:**

```rust
pub struct CoreLocalUuidGenerator {
    core_id: u8,                          // 0..63
    pool: rtrb::Consumer<uuid::Uuid>,     // SPSC ring buffer consumer
    fallback_counter: AtomicU16,          // monotonic within ms
    last_ms: AtomicU64,                   // last timestamp (ms)
}

impl CoreLocalUuidGenerator {
    #[inline(always)]
    pub fn next_uuid(&self) -> uuid::Uuid {
        if let Ok(uuid) = self.pool.pop() {
            return uuid;
        }
        self.generate_inline()  // ~20ns fallback, no syscall
    }
}
```

| Metric | Value |
|--------|-------|
| Hot-path UUID cost | **1-2ns** (ring buffer pop) |
| Fallback UUID cost | ~20ns (inline monotonic, no syscall) |
| Memory per core | 1 MB (64K UUIDs x 16 bytes) |
| Cross-core contention | **Zero** |
| Collision probability | **Zero** (core_id + counter + random) |

**Clock drift protection**: Monotonic counter ensures ordering within each core regardless
of clock. If system clock jumps backward, counter continues from last known timestamp.

### 5.9 Infrastructure-Aware Performance Model

Software architecture sets the throughput **ceiling**; infrastructure determines where you
**actually land**. Aeon's performance target is not a single number — it is:

> **Aeon must never be the bottleneck. The external system (Redpanda, network, disk)
> must saturate before Aeon does.**

#### The bottleneck chain at 1M events/sec (~256 bytes/event)

| Resource | Demand at 1M/sec | Typical Limit | Usually the bottleneck? |
|----------|-------------------|---------------|-------------------------|
| Network I/O | ~256 MB/sec (~2 Gbps) | 10 Gbps NIC | No |
| Redpanda broker disk | ~256 MB/sec writes + reads | NVMe: 3+ GB/sec | No |
| Redpanda broker CPU | Serialization, replication | Saturates ~1-2M msg/sec/broker | **Yes — this is the external ceiling** |
| Aeon CPU | Event processing pipeline | 8 cores × ~2.5M/core = 20M | No (if architecture is correct) |
| Aeon memory | SPSC buffers, UUID pools, state | ~2-4 GB working set | No |
| Aeon disk I/O | L2/L3 state persistence | Async, off hot path | No |

#### Infrastructure profiles

Absolute throughput varies by hardware. Gate 1 validates across profiles:

```
Profile A: Development (laptop / CI / WSL2)
  Hardware:  4 cores, 16 GB RAM, SSD, single Redpanda broker
  Expected:  200K-500K events/sec (Redpanda-bound on limited hardware)
  Validates: Correctness, zero-copy path, basic pipeline

Profile B: Dev server (dedicated machine)
  Hardware:  8-16 cores, 32-64 GB RAM, NVMe, single Redpanda broker
  Expected:  1M-2M events/sec (single Redpanda broker ceiling)
  Validates: Performance architecture, core pinning, SPSC buffers

Profile C: Production baseline (multi-broker)
  Hardware:  16+ cores, 64+ GB RAM, NVMe, 3-broker Redpanda cluster
  Expected:  2M-5M events/sec per Aeon instance
  Validates: Horizontal scaling readiness, multi-partition throughput
```

#### Infrastructure-independent success metrics

These metrics prove the architecture regardless of what hardware you run on:

| Metric | Target | How to measure |
|--------|--------|----------------|
| **Per-event overhead** | <100ns | Blackhole benchmark (Aeon ceiling with no external I/O) |
| **Headroom ratio** | Blackhole throughput >= 5x Redpanda throughput | Ratio of blackhole vs end-to-end benchmarks |
| **CPU saturation** | Aeon CPU <50% when Redpanda is at max | `top` / Prometheus metrics during load test |
| **Partition scaling** | Linear: 2x partitions ≈ 2x throughput | Benchmark with 4, 8, 16 partitions |
| **Zero event loss** | source count == sink count | Sustained 10+ minute load test |
| **P99 latency** | <10ms end-to-end | Latency histogram (source_ts → sink_ts) |
| **Backpressure** | No crash, no event loss when sink is slow | Slow-sink load test |

The **headroom ratio** is the key architectural metric. If blackhole = 10M/sec and
Redpanda = 1M/sec, Aeon has 10x headroom — the architecture is sound. If blackhole =
1.2M/sec and Redpanda = 1M/sec, Aeon is dangerously close to being the bottleneck.

#### Benchmark methodology

Every hot-path change requires two benchmarks:

```
1. Blackhole benchmark (measures Aeon's internal ceiling)
   MemorySource → Processor → BlackholeSink
   No external I/O. Pure software throughput.

2. Redpanda benchmark (measures real end-to-end)
   Redpanda → Processor → Redpanda
   I/O-bound. Measures integration overhead.

3. Profile comparison
   If blackhole >> redpanda: Aeon is not the bottleneck (good)
   If blackhole ≈ redpanda: Aeon is the bottleneck (fix Aeon)
```

#### The fix → improve → load test cycle

This is not a one-time phase — it runs continuously within Gate 1:

```
Build/change component
    → Blackhole benchmark (Aeon ceiling)
    → Redpanda benchmark (end-to-end)
    → Profile: where is time spent?
        → If Aeon is bottleneck → fix Aeon, repeat
        → If Redpanda is bottleneck → Aeon is done, move on
    → Record: per-event overhead, CPU %, memory, P99
```

#### Design-validation record for EOS at 20M ev/s

The EO-2 (atomic checkpoint + sink commit) design was validated against this
performance model before any code was written. The validation walks the per-event
cost budget, the per-checkpoint amortisation (L3 redb writes at 1–10 Hz), the
pull-vs-push backpressure split (§10.1), and the multi-node per-partition model
(§6, §11). It concludes that EO-2 adds **zero per-event hot-path cost** and
preserves the <100 ns / ≥5× headroom targets.

See `docs/THROUGHPUT-VALIDATION.md`. Any departure from these numbers re-enters
review before landing.

---

## 6. Multi-Tier State Management

```
L1 (Hot)  — DashMap with #[repr(align(64))] wrapper
            CPU-local shard strategy, lock-free reads
            In-memory only, no persistence

L2 (Warm) — memmap2 (mmap) files
            RAM-speed persistence with OS page cache
            Dirty-page tracking + async flush to L3

L3 (Cold) — Pluggable via L3Store trait (redb default, RocksDB optional)
            redb: Pure Rust B-tree DB, ACID, zero C dependencies
            RocksDB: LSM-tree, pluggable via same L3Store trait (future)
            Backend selected via config: state.l3.backend = redb | rocksdb
```

**Interest-based retention**: Buffer entries purged only after explicit Sink Confirmation.
TTL is a fallback, not the primary eviction path.

**Source-Anchor Recovery**: Persist "Last Safe Offset" to L3 on every successful sink ack.
On node restart, rewind source to last safe offset for exactly-once recovery.

### 6.1 Failure Recovery & Data Durability

#### What survives each failure type

| Component | Abrupt restart (kill -9) | Network partition | Delayed reconnect (hours) |
|-----------|-------------------------|-------------------|---------------------------|
| SPSC ring buffers | **Lost** (volatile) | N/A (local) | N/A |
| L1 DashMap state | **Lost** (volatile) | N/A (local) | N/A |
| L2 mmap state | **Survives** (OS may flush) | N/A (local) | **Survives** |
| L3 state (redb/RocksDB) | **Survives** (ACID transactions) | N/A (local) | **Survives** |
| Source-Anchor offset | **Survives** (in L3) | N/A (local) | **Survives** |
| PoH chain | **Survives** (tip in L3) | Paused (no new events) | **Survives** (resumes on rejoin) |
| Merkle log | **Survives** (in L3) | Paused | **Survives** |

**Data in SPSC ring buffers is volatile** — events that have been read from the source but
not yet acknowledged by the sink are lost on crash. This is safe because Source-Anchor
offset only advances after sink acknowledgement. On restart, these events replay from the
source. The result is **at-least-once delivery** (duplicates possible, no loss).

#### Recovery sequence (single node restart)

```
1. Process restarts
2. Load Source-Anchor offset from L3 (last sink-acknowledged position)
3. Rebuild L1 state from L2/L3 (hot cache refill)
4. Resume PoH chain from tip stored in L3
5. Verify Merkle log consistency (append-only proof)
6. Source seeks to Source-Anchor offset (replay unacknowledged events)
7. Pipeline resumes processing
```

#### Recovery sequence (cluster node rejoin after partition)

```
1. Returning node contacts seed node / known peer
2. Raft snapshot catch-up (current membership, partition assignment, global PoH tip)
3. Node rejoins as learner → promoted to voter
4. Leader may assign partitions back to returning node (rebalance)
5. For each reassigned partition:
   a. Two-phase transfer from current owner (see Section 11.8)
   b. PoH chain continues from transferred tip
   c. Source-Anchor offset transferred with partition
6. Node resumes normal processing
```

#### Source-Anchor atomicity guarantee

The sink acknowledgement → Source-Anchor offset write sequence must be ordered:

```
1. Sink confirms batch delivery (ack from Redpanda/external system)
2. Write new Source-Anchor offset to L3 (redb ACID transaction / RocksDB WAL+fsync)
3. Advance internal offset counter
```

If the process crashes between step 1 and step 2, the offset is NOT advanced. On restart,
the batch replays from the source. This produces **duplicates** (at-least-once) but
**never loss**. For exactly-once semantics, the sink must be idempotent (IdempotentSink
trait) or the external system must deduplicate by event ID (UUIDv7).

#### Role of PoH + Merkle during recovery

- **PoH chain verification**: After restart, verify `hash[n+1] = SHA-512(hash[n] || ...)`.
  Any gap proves events were lost between those hashes — triggers alert.
- **Merkle batch completeness**: After restart, verify every event referenced by a batch
  Merkle root exists in the sink. Missing events are identified for replay.
- **MMR consistency proof**: After cluster rejoin, verify the Merkle log at time T is an
  unmodified prefix of the current log — proves no tampering during the node's absence.
- **Global PoH checkpoint**: After network partition heals, all nodes converge on the same
  global ordering via the Raft-replicated checkpoint.

#### Kafka/Redpanda log retention consideration

If a node is disconnected long enough that Kafka's log retention expires for the events
at the Source-Anchor offset, those events cannot be replayed. Mitigation:
- Set Kafka retention period > maximum expected node downtime
- Monitor Source-Anchor offset lag as a critical alert
- Consider infinite retention or tiered storage for critical topics

---

## 7. Wasm Processor Runtime (Multi-Language)

### 7.1 WIT Contract

```
WIT contract (wit/aeon-processor.wit):
    process(event: event) -> list<output>
    process-batch(events: list<event>) -> list<output>

Host functions ("aeon" namespace):
    state-get(key: string) -> option<list<u8>>
    state-put(key: string, value: list<u8>)
    state-delete(key: string)
    state-scan(prefix: string) -> list<tuple<string, list<u8>>>
    emit(output: output)
    log(level: log-level, message: string)
    metrics-inc(name: string, value: u64)
    metrics-gauge(name: string, value: f64)
    current-time-ms() -> u64
```

### 7.2 Supported Languages

| Language | Toolchain | Compile Target | Notes |
|----------|-----------|----------------|-------|
| **Rust (native)** | Direct `impl Processor` | Host binary | 0% overhead, AVX2/AVX-512 |
| **Rust (Wasm)** | `cargo build --target wasm32-wasip2` | `.wasm` component | ~5% overhead |
| **C / C++** | `wasi-sdk` / `clang --target=wasm32-wasip2` | `.wasm` component | Near-native |
| **Go** | `tinygo` / `GOOS=wasip2` | `.wasm` component | GC overhead |
| **Node.js** | `ComponentizeJS` (jco) | `.wasm` component | V8-free, pure Wasm |
| **Python** | `componentize-py` | `.wasm` component | CPython in Wasm |
| **Java** | `TeaVM` / `GraalWasm` | `.wasm` component | JIT unavailable in Wasm |
| **C# / .NET** | `dotnet-wasi-sdk` / NativeAOT-LLVM | `.wasm` component | NativeAOT preferred |
| **PHP** | `componentize-php` / PTC php-wasm | `.wasm` component | All runtime models (see 7.6) |

### 7.3 Typed State API

The raw WIT host functions (`state-get`, `state-put`) provide byte-level KV access.
Processor SDKs wrap these into typed, ergonomic state primitives:

**ValueState** — single keyed value with serialization:
```
ValueState<T>:
    get() -> Option<T>          // deserialize from bytes
    set(value: T)               // serialize to bytes
    clear()                     // delete key
```

**MapState** — keyed collection (prefix-scan over raw KV):
```
MapState<K, V>:
    get(key: K) -> Option<V>    // state-get("map:{name}:{key}")
    put(key: K, value: V)       // state-put("map:{name}:{key}", serialize(v))
    remove(key: K)              // state-delete("map:{name}:{key}")
    keys() -> Iterator<K>       // state-scan("map:{name}:")
    entries() -> Iterator<(K, V)>
```

**ListState** — append-only ordered list:
```
ListState<T>:
    append(value: T)            // state-put("list:{name}:{counter}", serialize(v))
    get(index: usize) -> Option<T>
    len() -> usize
    clear()
```

**CounterState** — atomic increment/decrement:
```
CounterState:
    increment(delta: i64)       // read-modify-write via state-get/state-put
    get() -> i64
    reset()
```

The host state API (`state-get`/`state-put`) operates on opaque bytes. All typed state
wrappers exist in the processor SDK (guest-side), not in the host. This keeps the WIT
contract minimal and language-agnostic. Each language SDK provides idiomatic wrappers
(e.g., Python uses `dict`-like MapState, Node.js uses `Map`-like API, PHP uses array syntax).

### 7.4 Processor SDK Per Language

Each supported language has an SDK package that wraps the raw WIT imports into idiomatic APIs:

**Rust SDK** (`aeon-processor-sdk` crate):
```rust
use aeon_sdk::{Event, Output, ValueState, MapState, emit, log};

#[aeon_sdk::processor]
fn process(event: Event) -> Vec<Output> {
    let counter: ValueState<u64> = ValueState::new("request_count");
    counter.set(counter.get().unwrap_or(0) + 1);
    vec![Output::new("output-topic", event.payload)]
}
```

**Python SDK** (`aeon-processor` pip package):
```python
from aeon import processor, ValueState, MapState

@processor
def process(event):
    counter = ValueState("request_count", int)
    counter.set(counter.get(0) + 1)
    return [{"destination": "output-topic", "payload": event.payload}]
```

**Node.js SDK** (`@aeon/processor` npm package):
```javascript
import { processor, ValueState } from '@aeon/processor';

export const process = processor((event) => {
    const counter = new ValueState('request_count', Number);
    counter.set((counter.get() ?? 0) + 1);
    return [{ destination: 'output-topic', payload: event.payload }];
});
```

**Go SDK** (`github.com/aeonflow/processor-sdk-go`):
```go
package main

import "github.com/aeonflow/processor-sdk-go/aeon"

func Process(event aeon.Event) []aeon.Output {
    counter := aeon.NewValueState[int64]("request_count")
    counter.Set(counter.Get(0) + 1)
    return []aeon.Output{{Destination: "output-topic", Payload: event.Payload}}
}
```

**Java SDK** (`io.aeonflow:processor-sdk`):
```java
import io.aeonflow.sdk.*;

@AeonProcessor
public class MyProcessor implements Processor {
    private final ValueState<Long> counter = new ValueState<>("request_count", Long.class);

    @Override
    public List<Output> process(Event event) {
        counter.set(counter.get(0L) + 1);
        return List.of(new Output("output-topic", event.getPayload()));
    }
}
```

**C# SDK** (`Aeon.Processor.Sdk` NuGet package):
```csharp
using Aeon.Sdk;

[AeonProcessor]
public class MyProcessor : IProcessor {
    private readonly ValueState<long> _counter = new("request_count");

    public IEnumerable<Output> Process(Event evt) {
        _counter.Set(_counter.Get(0) + 1);
        yield return new Output("output-topic", evt.Payload);
    }
}
```

**PHP SDK** — see Section 7.6 for comprehensive runtime model coverage.

### 7.5 Fuel Metering & Multi-Tenant Isolation

- Per-Wasm-guest configurable **fuel** per execution cycle
- Exhausted fuel **suspends** the task (no panic, graceful yield)
- Per-job **memory limits** enforced at the Wasmtime Store level
- **Namespace isolation**: per-job state partitioning, no cross-tenant leakage
- Per-guest metrics: fuel consumed, memory high water mark, invocation count

### 7.6 PHP Processor Runtime Models

PHP developers work in fundamentally different runtime paradigms. Aeon's Wasm processor
layer supports **all** of them transparently via the same WIT contract.

#### PHP-FPM Model (Request-Response / Stateless)

Developers accustomed to PHP-FPM write **stateless, per-event processor functions**.
Each Wasm invocation is a single request-response cycle — maps directly to the FPM
mental model familiar to most PHP developers.

```php
<?php
// processor.php — FPM-style stateless processor
function process(array $event): array {
    $payload = json_decode($event['payload'], true);
    $payload['processed_at'] = time();
    $payload['source'] = 'aeon-enriched';

    return [[
        'destination' => 'output-topic',
        'payload' => json_encode($payload),
    ]];
}
```

Characteristics:
- No persistent state between invocations (unless explicitly using `aeon_state_get/put`)
- Each `process()` call is independent — same isolation as an FPM worker
- Ideal for: transformations, enrichments, validations, format conversions
- Mental model: "each event is like an HTTP request hitting your PHP endpoint"

#### Async PHP Model (RevoltPHP / AmphP)

Developers using RevoltPHP + AmphP's event loop write **async-aware processors** that
leverage non-blocking patterns. In the Wasm sandbox, the batch processing model replaces
the async event loop — the developer writes batch-oriented logic.

```php
<?php
// processor.php — Async-style batch processor (RevoltPHP/AmphP mental model)
function process_batch(array $events): array {
    $outputs = [];
    $batch_state = []; // Local accumulation within batch

    foreach ($events as $event) {
        $payload = json_decode($event['payload'], true);

        // Aggregate pattern (familiar to AmphP stream consumers)
        $key = $payload['user_id'] ?? 'unknown';
        $batch_state[$key] = ($batch_state[$key] ?? 0) + 1;

        $payload['batch_count'] = $batch_state[$key];
        $outputs[] = [
            'destination' => 'enriched-stream',
            'payload' => json_encode($payload),
        ];
    }

    return $outputs;
}
```

Characteristics:
- Batch-oriented: `process_batch()` receives multiple events (maps to event loop chunk)
- Local state within batch via variables (no external I/O needed for intra-batch aggregation)
- Cross-batch state via host `aeon_state_get/put` functions
- Mental model: "each batch is like one tick of your event loop"

#### Workerman / Swoole / OpenSwoole Model (Long-Running Worker)

Developers using Workerman or Swoole write **long-running worker processes** with persistent
state. In Wasm, this maps to a **stateful processor** that maintains state across invocations
via the host state API.

```php
<?php
// processor.php — Swoole/Workerman-style stateful processor
function process(array $event): array {
    $payload = json_decode($event['payload'], true);
    $user_id = $payload['user_id'] ?? 'unknown';

    // Persistent state (replaces Swoole\Table or shared memory)
    $counter_key = "counter:{$user_id}";
    $current = (int) aeon_state_get($counter_key);
    $current++;
    aeon_state_put($counter_key, (string) $current);

    // Rate limiting pattern
    $window_key = "window:{$user_id}";
    $window_count = (int) aeon_state_get($window_key);
    if ($window_count > 1000) {
        return [['destination' => 'rate-limited-dlq', 'payload' => $event['payload']]];
    }
    aeon_state_put($window_key, (string) ($window_count + 1));

    $payload['request_count'] = $current;
    return [['destination' => 'output-topic', 'payload' => json_encode($payload)]];
}
```

Characteristics:
- Persistent state across invocations (replaces Swoole\Table, APCu, shared memory)
- `aeon_state_get/put` is the equivalent of persistent worker memory
- Fuel metering replaces Swoole's coroutine scheduling
- Mental model: "your Swoole worker's onMessage callback, with shared state via host API"

#### FrankenPHP Model (Embedded PHP / Boot-Once)

Developers using FrankenPHP (PHP embedded in Caddy/Go) work with **worker mode** — PHP
scripts that boot once and handle multiple requests without reinitializing. In Aeon's
Wasm context, this maps to **initialization-once, process-many** semantics.

```php
<?php
// processor.php — FrankenPHP worker-mode style
// === Initialization (runs once on Wasm module instantiation) ===
$config = json_decode(aeon_state_get('processor_config') ?? '{}', true);
$enrichment_rules = [
    'premium' => ['priority' => 'high', 'sla_ms' => 50],
    'standard' => ['priority' => 'normal', 'sla_ms' => 200],
    'free' => ['priority' => 'low', 'sla_ms' => 1000],
];
$lookup_cache = []; // In-memory cache (survives across process() calls)

// === Per-event processing ===
function process(array $event): array {
    global $enrichment_rules, $lookup_cache;

    $payload = json_decode($event['payload'], true);
    $tier = $payload['account_tier'] ?? 'free';
    $rules = $enrichment_rules[$tier] ?? $enrichment_rules['free'];
    $payload['priority'] = $rules['priority'];
    $payload['sla_ms'] = $rules['sla_ms'];

    // In-memory LRU cache (replaces FrankenPHP's persistent worker memory)
    $user_id = $payload['user_id'] ?? 'unknown';
    if (!isset($lookup_cache[$user_id])) {
        $lookup_cache[$user_id] = aeon_state_get("user_profile:{$user_id}") ?? '{}';
        if (count($lookup_cache) > 10000) {
            array_shift($lookup_cache);
        }
    }

    $profile = json_decode($lookup_cache[$user_id], true);
    $payload['user_name'] = $profile['name'] ?? 'Anonymous';

    return [['destination' => 'enriched-events', 'payload' => json_encode($payload)]];
}
```

Characteristics:
- **Boot-once semantics**: Global scope runs once on Wasm instantiation
- **In-memory caching**: PHP variables persist across `process()` calls within same instance
- **No Caddy/Go dependency**: Only the PHP logic is compiled to Wasm
- Mental model: "your FrankenPHP worker script, where the loop body is `process()`"

#### PHP-to-Wasm Compilation

Regardless of runtime model, all PHP processors compile through the same path:

```bash
# Option A: componentize-php (recommended)
componentize-php processor.php -o processor.wasm --wit ../../wit/aeon-processor.wit

# Option B: PTC/php-wasm + wasm-tools
php-wasm compile processor.php -o processor.core.wasm
wasm-tools component new processor.core.wasm -o processor.wasm \
    --adapt wasi_snapshot_preview1.reactor.wasm

# Option C: Pre-built PHP interpreter Wasm + script mount (dev only)
```

**Key principle**: The Wasm sandbox is runtime-agnostic. Whether the developer thinks in
FPM requests, AmphP promises, Workerman callbacks, or Swoole coroutines, the compiled
`.wasm` module implements the same WIT interface.

### 7.7 Shadow Mode

Deploy a "Shadow" Wasm module alongside production. Aeon tees data to both, compares
results at the Sink Buffer level. Mismatches alert to Jaeger/Loki; only the "Live"
output commits to the external Sink.

### 7.8 Deployment Options

| Method | Performance | Best For |
|--------|-------------|----------|
| Mounted Wasm (sidecar/volume) | ~5% overhead | K8s, rapid iteration |
| Bundled Container (baked in) | ~5% overhead | Production stability |
| Native Rust (compiled in) | 0% overhead | 20M+/sec, maximum perf |

### 7.9 Processor Developer Experience Tooling

Aeon provides CLI tools for the full processor development lifecycle:

```bash
# Scaffold a new processor project (generates boilerplate + WIT bindings)
aeon new myprocessor --lang python
aeon new myprocessor --lang php --model fpm
aeon new myprocessor --lang rust-wasm
aeon new myprocessor --lang go

# Local development with hot-reload
aeon dev --processor ./myprocessor --source memory --sink stdout
# Watches for file changes, recompiles Wasm, reloads pipeline automatically

# Build processor to Wasm component
aeon build ./myprocessor
# Detects language, invokes correct toolchain, validates WIT conformance

# Validate processor against WIT contract (without running pipeline)
aeon validate ./myprocessor.wasm

# Deploy processor to running Aeon instance
aeon deploy ./myprocessor.wasm --target localhost:4471
```

`aeon new` generates:
- Language-specific project skeleton (Cargo.toml / package.json / requirements.txt / composer.json / go.mod)
- WIT bindings pre-generated for the target language
- Example `process()` and `process_batch()` implementations
- Makefile / build script for Wasm compilation
- `.aeon/manifest.yaml` for local testing

---

## 8. Pipeline Composition (DAG)

Aeon supports pipeline composition beyond simple linear source→processor→sink chains.
Pipelines are directed acyclic graphs (DAGs) defined in the manifest.

### 8.1 Topology Primitives

**Linear** (default): Source → Processor → Sink
```yaml
pipeline:
  - source: redpanda-in
    processor: enrich
    sink: redpanda-out
```

**Fan-out** (one source, multiple processors/sinks):
```yaml
pipeline:
  - source: redpanda-in
    processor: classify
    routes:
      - match: "payload contains 'priority:high'"
        sink: fast-lane
      - match: "payload contains 'priority:low'"
        sink: batch-lane
      - default: sink: standard-out
```

**Fan-in** (multiple sources, one processor):
```yaml
pipeline:
  - sources:
      - redpanda-orders
      - redpanda-inventory
    processor: join-enricher
    sink: redpanda-enriched
```

**Processor chaining** (sequential processors):
```yaml
pipeline:
  - source: redpanda-in
    processors:
      - validate     # step 1: schema validation
      - enrich       # step 2: add metadata
      - transform    # step 3: format conversion
    sink: redpanda-out
```

**Conditional routing** (content-based):
```yaml
pipeline:
  - source: redpanda-in
    router:
      type: content-based
      field: "$.event_type"
      routes:
        "user.signup": processor-signup
        "user.login": processor-login
        "*": processor-default
    sink: redpanda-out
```

### 8.2 DAG Execution Model

- Each processor stage is connected via its own SPSC ring buffer
- Fan-out duplicates `Bytes` references (zero-copy, not cloned)
- Fan-in merges into a single SPSC buffer with source tagging on each Event
- Router uses SIMD byte scanning for fast field matching (no JSON parse)
- Backpressure propagates through the entire DAG — if any sink is slow,
  upstream stages pause (zero loss guarantee maintained)

### 8.3 DAG Validation

At pipeline startup, Aeon validates:
- No cycles (topological sort must succeed)
- All named processors/sinks exist in the manifest
- Router match expressions are syntactically valid
- Fan-in sources have compatible partition counts (or explicit partition mapping)

---

## 9. Windowing & Temporal Operations

Aeon supports windowing for temporal aggregation use cases. Windows are implemented
as processor-level constructs, not engine-level — keeping the core pipeline simple.

### 9.1 Window Types

**Tumbling window** — fixed-size, non-overlapping:
```yaml
processor:
  name: count-per-minute
  type: wasm
  module: counter.wasm
  window:
    type: tumbling
    size: 60s
```

**Sliding window** — fixed-size, overlapping by slide interval:
```yaml
processor:
  name: moving-average
  window:
    type: sliding
    size: 5m
    slide: 30s
```

**Session window** — gap-based, per key:
```yaml
processor:
  name: session-tracker
  window:
    type: session
    gap: 30m
    key: "$.user_id"
    max_duration: 24h
```

### 9.2 Windowing Internals

- Window state lives in the multi-tier state store (L1 DashMap for active windows,
  L2/L3 for spill and recovery)
- **Watermarks**: Track event-time progress. A watermark at time T means "no events
  with timestamp < T will arrive." Configurable allowed lateness.
- **Late events**: Events arriving after window close are either:
  - Discarded (with metric increment)
  - Sent to a late-events side output
  - Trigger window re-computation (configurable)
- **Triggers**: Windows emit results on:
  - Window close (default)
  - Early firing at interval (e.g., every 10s during a 5m window)
  - Per-element (for running aggregations)

### 9.3 Window Functions (Processor API)

Windowed processors implement additional WIT exports:

```
// Extended WIT for windowed processors
on-window-open(window-id: string, start: u64, end: u64)
on-window-element(window-id: string, event: event)
on-window-close(window-id: string) -> list<output>
```

The engine manages window lifecycle and state. The processor receives events grouped
by window and emits aggregated outputs when windows close.

---

## 10. Connector Architecture

### 10.1 Source delivery models (push vs pull)

Sources fall into two categories based on who controls the data flow:

**Pull sources** — Aeon controls the rate. Backpressure is natural: stop pulling.

| Source | Protocol | Pull Mechanism |
|--------|----------|----------------|
| Kafka / Redpanda | Kafka protocol | `poll()` / `fetch` — consumer controls rate |
| Redis Streams | XREAD | `XREAD BLOCK` — consumer controls rate |
| File System | File I/O | `read()` — consumer controls rate |
| HTTP Polling | HTTP | Aeon initiates requests on its schedule |
| MongoDB Change Streams | MongoDB wire | Cursor iteration — consumer controls rate |

**Push sources** — External system controls the rate. Aeon must apply backpressure.

| Source | Protocol | Push Mechanism | Backpressure Signal |
|--------|----------|----------------|---------------------|
| HTTP Webhook | HTTP | External POSTs to Aeon | HTTP 429 / 503 response |
| WebSocket | WebSocket | Server sends frames | TCP window / close frame |
| MQTT | MQTT | Broker pushes to subscriber | QoS levels / DISCONNECT |
| NATS | NATS | Server pushes to subscriber | Slow consumer signal |
| RabbitMQ | AMQP | Broker pushes to consumer | Prefetch count / channel.flow |
| PostgreSQL CDC | Replication | WAL sender pushes changes | Replication slot backpressure |
| QUIC | QUIC | Remote sends on stream | Stream flow control (RFC 9000) |
| WebTransport Streams | HTTP/3 | Reliable streams over QUIC | QUIC flow control (RFC 9000) |
| WebTransport Datagrams | HTTP/3 | Unreliable datagrams over QUIC | None (fire-and-forget) |
| gRPC stream | HTTP/2 | Server pushes on stream | HTTP/2 flow control frames |

#### Zero data loss guarantee (DEFAULT, MANDATORY OPTION)

**The default behavior is zero data loss at every stage — source, processor, and sink.**

Every event that enters the pipeline must either:
1. Exit through the sink successfully, OR
2. Exit through the DLQ (if processing fails), OR
3. Remain in the source system (backpressure prevents it from entering)

This guarantee is the **design center** of Aeon's architecture. All backpressure,
buffering, spill-to-disk, retry, and DLQ mechanisms exist to enforce it.

**Drop as explicit opt-in**: For use cases where bounded latency matters more than
completeness (e.g., high-volume IoT telemetry, real-time metrics where a missing
data point is acceptable), users can explicitly configure `overflow: drop-oldest`
or `overflow: drop-newest` on a per-source basis. This is never the default —
it requires conscious, per-source configuration and generates a warning log on startup.

```yaml
# Default (zero loss — no overflow config needed)
sources:
  - type: webhook
    bind: "0.0.0.0:8081"
    # overflow defaults to "block" — zero data loss guaranteed

# Explicit opt-in to drop (user accepts data loss)
sources:
  - type: mqtt
    overflow: drop-oldest     # WARNING logged at startup
    buffer_capacity: 131072
```

#### How push sources work with the pull-based Source trait

The `Source` trait's `next_batch()` is pull-based. Push sources bridge this via an
internal receive buffer:

```
External System                    Push Source Connector               Engine
     │                              ┌──────────────────┐                │
     │──── push data ──────────────▶│ Receive Buffer   │                │
     │──── push data ──────────────▶│ (bounded +       │◀── next_batch()│
     │──── push data ──────────────▶│  spill-to-disk)  │───▶ Vec<Event> │
     │                              └──────────────────┘                │
     │◀─── backpressure ────────────       │                            │
     │   (HTTP 429 / TCP window /          │                            │
     │    MQTT disconnect / etc.)    Buffer full? Block.                │
     │                               Never drop.                        │
```

When the engine is pulling fast enough, the buffer stays near-empty. When the engine
is slow (backpressure from slow sink or processor), the buffer fills.

#### Push source buffer behavior

When the receive buffer is full and data keeps arriving:

```
Phase 1: In-memory buffer absorbs burst
  Bounded ring buffer (configurable capacity, default 65536 events)
  Normal operation — sub-microsecond push latency

Phase 2: Spill to disk absorbs sustained pressure
  When in-memory buffer reaches high watermark → spill to append-only file
  Events are read back from spill file as buffer drains
  Disk is slower but prevents backpressure to external system during bursts

Phase 3: Protocol-level backpressure (last resort)
  When spill file reaches configured size limit → signal external system to slow down
  HTTP 429, TCP window shrink, MQTT disconnect, QUIC flow control, etc.
  External system absorbs the slowdown via its own buffering or retry
```

This three-phase approach gives push sources burst absorption (in-memory), sustained
pressure absorption (spill), and graceful degradation (protocol backpressure) — all
without ever dropping a single event.

```yaml
# Manifest configuration for push sources
sources:
  - type: webhook
    bind: "0.0.0.0:8081"
    buffer_capacity: 65536          # In-memory buffer size (events)
    spill_path: /var/aeon/spill     # Disk spill location
    spill_max_bytes: 1073741824     # 1 GB spill limit before protocol backpressure
```

**For Gate 1**: Only pull sources are implemented (Kafka/Redpanda). Backpressure is
trivial — the engine stops calling `next_batch()`, the consumer stops polling, and
Redpanda retains the messages. Push source buffer/spill/backpressure is implemented
in Phase 11 when push-based connectors are built.

#### End-to-end backpressure chain (zero loss guarantee)

Backpressure propagates backward through every stage. No stage ever drops data — each
stage blocks and pressures the stage before it:

```
                    BACKPRESSURE PROPAGATION (zero loss)
                    ====================================

Slow Sink (can't keep up)
  ↓ blocks
Processor→Sink SPSC buffer fills
  ↓ producer blocks (rtrb: spin-wait or yield, never overwrite)
Processor stalls (can't emit output)
  ↓ blocks
Source→Processor SPSC buffer fills
  ↓ producer blocks
Source stalls (can't push to buffer)
  ↓
┌─────────────────────────────────────────┐
│ Pull source:                             │
│   stops calling poll/fetch               │
│   external system retains messages       │
│   (Kafka broker, Redis stream, file)     │
├─────────────────────────────────────────┤
│ Push source:                             │
│   Phase 1: in-memory buffer absorbs     │
│   Phase 2: spill to disk absorbs        │
│   Phase 3: protocol-level backpressure  │
│   external system slows or retries      │
└─────────────────────────────────────────┘
```

**SPSC buffer behavior**: The `rtrb` ring buffer does NOT overwrite on full. The producer
side returns `Err` when full. The source/processor must spin-wait or yield — never skip
the event. This is enforced by design, not by configuration.

**Processor failure**: If a processor fails on an event (Wasm trap, fuel exhaustion,
logic error), the event goes to the **DLQ** — never silently discarded. The DLQ is
itself a Sink and follows the same zero-loss guarantee (DLQ sink backpressure propagates
the same way).

**Sink failure**: If a sink fails (connection lost, timeout), the event is retried with
exponential backoff. After max retries, it goes to the DLQ. If the DLQ is also failing,
the pipeline **pauses** — it does not drop events.

**Sink acknowledgement**: An event is only considered "delivered" when the sink confirms
it. Source-Anchor offsets advance only after sink acknowledgement. On restart, any
unacknowledged events replay from the source (exactly-once via Source-Anchor).

#### Design implication for the Source trait

The `Source` trait does not change. `next_batch()` works for both models:
- **Pull**: `next_batch()` internally polls the external system
- **Push**: `next_batch()` internally drains from the receive buffer

The difference is invisible to the engine. The engine's reactive-pull model (only pull
when processor is ready) provides backpressure regardless of source type. The buffer/spill
behavior is internal to the push source connector — the engine never knows or cares
whether the source is push or pull.

### 10.2 All connectors (feature-gated)

| Connector | Crate Feature | SDK / Driver | Source Model |
|-----------|---------------|-------------|--------------|
| Kafka / Redpanda | `kafka` | rdkafka | Pull |
| RabbitMQ | `rabbitmq` | lapin | Push |
| Redis Streams / Valkey | `redis` | redis | Pull |
| NATS / JetStream | `nats` | async-nats | Push |
| MQTT | `mqtt` | rumqttc | Push |
| WebSocket | `websocket` | tokio-tungstenite | Push |
| WebTransport Streams | `webtransport` | web-transport-quinn | Push |
| WebTransport Datagrams | `webtransport` | web-transport-quinn | Push (lossy opt-in) |
| HTTP Webhook | `http` | hyper | Push |
| HTTP Polling | `http` | hyper + hyper-util | Pull |
| QUIC raw | `quic` | quinn | Push |
| File System | `file` | tokio / tokio-uring | Pull |
| PostgreSQL CDC | `postgres` | tokio-postgres | Push |
| MySQL CDC | `mysql` | mysql_async | Push |
| MongoDB Change Streams | `mongodb` | mongodb | Pull |
| Memory (testing) | (always enabled) | — | Pull |
| Blackhole | (always enabled) | — | — (sink only) |
| Stdout | (always enabled) | — | — (sink only) |

### 10.3 WebTransport connectors (Streams + Datagrams)

WebTransport runs over HTTP/3 (which runs over QUIC). It provides two primitives:

| Primitive | Reliability | Ordering | Zero-loss compatible? |
|-----------|-------------|----------|-----------------------|
| **Streams** | Reliable (retransmitted) | Ordered within stream | **Yes** |
| **Datagrams** | Unreliable (fire-and-forget) | Unordered | **No** (explicit opt-in) |

#### WebTransport Streams (Source + Sink)

Reliable, ordered, multiplexed streams over HTTP/3. Each Aeon partition can map to its
own WebTransport stream — no head-of-line blocking between partitions.

**As source**: Push model. Browser, edge device, or another service sends data to Aeon
via WebTransport streams. Backpressure via QUIC flow control (same as raw QUIC source).
Zero-loss compatible — reliable delivery + flow control.

**As sink**: Aeon sends processed events to a WebTransport client. Sink acknowledgement
via stream-level confirmation. Source-Anchor advances normally.

**Use cases**: Browser→Aeon real-time ingestion, edge→cloud pipelines, CDN-traversable
connections (HTTP/3 understood by CDN proxies, raw QUIC may not be).

**Library**: `web-transport-quinn` — builds on quinn (our chosen QUIC stack). Actively
maintained, pre-1.0 but functional. Same TLS stack (rustls + aws-lc-rs).

#### WebTransport Datagrams (Source only, explicit opt-in)

Unreliable, unordered datagrams over HTTP/3. The network itself may drop datagrams
before Aeon sees them — this is inherent to the transport, not an Aeon decision.

**As source only**: Push model. Accepted data enters the pipeline normally (zero-loss
from that point forward). But the transport itself is lossy — data may never arrive.
This connector requires `overflow: accept-loss` in the manifest, which logs a warning
at startup.

**NOT available as sink**: Unreliable delivery means Aeon cannot confirm the sink
received the data. Source-Anchor offset cannot advance without confirmation. This
breaks the exactly-once recovery chain. **Using datagrams as a sink is architecturally
incompatible with Aeon's data protection guarantees.**

**Use cases**: Game telemetry, live sensor readings where latest-value-wins, real-time
metrics where individual data points are expendable.

```yaml
# WebTransport Streams (zero-loss, reliable)
sources:
  - type: webtransport-stream
    bind: "0.0.0.0:4434"
    tls:
      cert: /etc/aeon/tls/wt.pem
      key: /etc/aeon/tls/wt.key

sinks:
  - type: webtransport-stream
    # ...

# WebTransport Datagrams (explicit opt-in, source only)
sources:
  - type: webtransport-datagram
    bind: "0.0.0.0:4434"
    overflow: accept-loss     # REQUIRED — WARNING logged at startup
```

### 10.4 Connector rules

- Each connector behind a **Cargo feature flag**
- Technology-named folders: `src/kafka/`, `src/redis/`, etc.
- Builder pattern for config: `KafkaSource::new(servers, topic).with_config(k, v)`
- TLS/mTLS configurable per connector via `CertificateStore`
- Redpanda uses the Kafka connector with `is_redpanda: true` flag (for Redpanda-specific
  API optimizations like faster topic creation, not consumer group behavior — Aeon uses
  manual partition assignment regardless)

---

## 11. Cluster Architecture

### 11.1 Design Principle: Always-Raft

Aeon **always runs Raft**, even on a single node. There is no separate "standalone mode" —
a single-node deployment is simply a Raft cluster of size 1 where the node is trivially
the leader, commits are instant (quorum of 1), and there is zero network overhead.

This eliminates the "mode switch" problem. Scaling from 1→3→5 nodes is a Raft membership
change, not a architecture transition. This is the same approach used by etcd, CockroachDB,
and TiKV.

```
┌─────────────────────────────────────────────────────────┐
│                      Aeon Node                           │
│                                                          │
│  ┌───────────┐    ┌────────────┐    ┌────────────────┐  │
│  │ Pipeline   │    │ PoH        │    │ Merkle Log     │  │
│  │ Engine     │───▶│ Recorder   │───▶│ (append-only)  │  │
│  └───────────┘    └────────────┘    └────────────────┘  │
│                                                          │
│  ┌──────────────────────────────────────────────────┐    │
│  │ Cluster Layer (always active)                     │    │
│  │                                                    │    │
│  │  ┌───────────┐  ┌────────────┐  ┌──────────────┐ │    │
│  │  │ Raft      │  │ Partition  │  │ QUIC         │ │    │
│  │  │ (openraft) │  │ Manager   │  │ Transport    │ │    │
│  │  └───────────┘  └────────────┘  └──────────────┘ │    │
│  └──────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

- **Raft**: Always running. Single-node = quorum of 1, zero overhead.
- **PoH + Merkle**: Always running. Cryptographic audit regardless of cluster size.
- **QUIC Transport**: Activates only when peers exist. No listeners on single-node.
- **Partition Manager**: Always running. Single-node = owns all partitions.

### 11.2 Cluster Sizes & Quorum

Aeon enforces **odd-number cluster sizes** for clean Raft majority:

| Nodes | Quorum | Fault Tolerance | Use Case |
|-------|--------|-----------------|----------|
| 1 | 1 | 0 failures | Dev, small workloads, single-instance perf testing |
| 3 | 2 | 1 failure | Minimum production cluster |
| 5 | 3 | 2 failures | Recommended production |
| 7 | 4 | 3 failures | Large-scale / geo-distributed |

Even-number clusters are **rejected at startup** — 4 nodes tolerates only 1 failure (same
as 3), wasting a node. The manifest validator enforces this.

### 11.3 Dynamic Scaling (Upgrade & Downgrade)

Scaling is always a Raft membership change — **one node at a time** (single-server change,
not joint consensus). This is a Raft safety invariant: adding or removing one voter at a
time guarantees no split-brain during transitions. openraft supports this natively.

#### Scale-Up: 1 → 3 → 5

```
Phase 1: Join as Learners
  ┌─────────┐
  │ Node 1  │ (Leader, owns all 16 partitions)
  │ Raft: 1 │
  └─────────┘
       │
       ▼  Node 2 joins as learner (receives Raft log, cannot vote)
  ┌─────────┐     ┌─────────┐
  │ Node 1  │────▶│ Node 2  │
  │ Raft: 1 │     │ Learner │
  └─────────┘     └─────────┘
       │
       ▼  Node 3 joins as learner
  ┌─────────┐     ┌─────────┐     ┌─────────┐
  │ Node 1  │────▶│ Node 2  │     │ Node 3  │
  │ Raft: 1 │────▶│ Learner │     │ Learner │
  └─────────┘     └─────────┘     └─────────┘

Phase 2: Promote to Voters (one at a time)
  Node 2: learner → voter   (cluster is now 2 voters — transitional)
  Node 3: learner → voter   (cluster is now 3 voters — quorum = 2)

Phase 3: Partition Rebalance
  Leader computes new assignment: ~5-6 partitions per node
  Transfers partitions via QUIC (see Section 11.8)
```

#### Scale-Down: 5 → 3 → 1

```
Phase 1: Drain Partitions
  Departing nodes transfer their partitions to remaining nodes via QUIC
  Each partition: pause → transfer state → transfer PoH tip → resume on target

Phase 2: Remove from Raft (one at a time)
  Once a departing node has zero partitions, remove it from Raft voter set
  Node shuts down cleanly after removal is committed

Phase 3: Verify
  Remaining cluster has correct quorum and balanced partitions
```

#### Scaling Safety Rules

1. **One membership change at a time**: Never add/remove two voters simultaneously
2. **Learner-first on join**: New nodes sync Raft log as learners before becoming voters
3. **Drain-first on leave**: Departing nodes transfer all partitions before removal
4. **Odd-number enforcement**: Transitions pass through even-number states briefly during
   promotion/removal, but the **target** cluster size must be odd
5. **No data loss**: Partition transfer uses two-phase sync (see Section 11.8). Source-Anchor
   offsets are transferred with partition ownership.
6. **Pipeline continuity**: Partitions not being transferred continue processing normally.
   Only the partitions being moved experience a brief pause during cutover (milliseconds —
   only the WAL delta + L1 snapshot, not the full L3 state).
7. **Leader failure during scaling**: If the leader dies mid-transfer, the new leader checks
   target acknowledgement state. Bulk sync complete → proceed to cutover. Otherwise → abort,
   revert to `Owned(source)`. Transfer is idempotent and can be retried.
8. **Network partition recovery**: If a cluster splits (e.g., 2|3 in a 5-node cluster), the
   majority side (3) retains quorum and reassigns orphaned partitions from unreachable nodes.
   When the partition heals, returning nodes rejoin as learners and may receive partitions
   back during rebalance.

### 11.4 Raft Consensus

`openraft` crate with:
- Term-based leader election
- Log replication for partition assignment and configuration changes
- Single-server membership changes (scale-up/down safety)
- Learner support (pre-sync before promotion)
- Snapshot for fast catch-up of new nodes

**What goes through Raft log:**
- Partition assignment table (which node owns which partitions)
- Partition transfer state transitions: `Owned(node)` → `Transferring(source, target)` → `Owned(target)`
- Membership changes (add/remove voter/learner)
- Global PoH checkpoints (periodic, default every 1 second)
- Configuration changes

**What does NOT go through Raft log:**
- Event data (flows through pipeline, not consensus)
- Per-partition PoH chains (local to owning node)
- Metrics, health status (gossip or direct query)

**Raft snapshot contents** (for fast new-node catch-up):
- Current partition assignment table
- Current membership (voters + learners)
- Latest global PoH checkpoint
- Per-partition Source-Anchor offsets
- Cluster configuration (partition count, PoH interval)

The snapshot is small (kilobytes) regardless of how long the cluster has been running,
because event data does not flow through Raft.

### 11.5 Proof of History (PoH)

Each node maintains a PoH chain for the partitions it owns:

```
Per-partition PoH (local, no consensus needed):
  hash[n] = SHA-512(hash[n-1] || batch_merkle_root || timestamp)
```

In multi-node clusters, the **leader** periodically produces a global PoH checkpoint
that aggregates per-node PoH tips:

```
Global PoH checkpoint (replicated via Raft):
  global[n] = SHA-512(global[n-1] || node1_poh_tip || node2_poh_tip || ... || nodeN_poh_tip)
```

This gives:
- **Per-partition ordering**: Fast, no coordination, verifiable per-node
- **Global ordering**: Periodic (configurable interval), via Raft leader
- **Audit**: Any party can verify event ordering at either level

On partition transfer during scaling, the PoH chain tip is included in the cutover phase
(after the partition is paused). The receiving node continues the chain from where the
source node left off. No race condition — PoH production is frozen for that partition
during cutover.

### 11.6 Merkle Tree Integrity

**Batch Merkle Tree** (per pipeline flush):
- Binary SHA-512 tree over event commitments: `H(event_id || H(payload))`
- Root signed with Ed25519: `sign(node_key, job_id || seq || timestamp || root)`
- Inclusion proof: O(log N) hashes — prove event E was in batch B

**Append-Only Merkle Log** (Merkle Mountain Range):
- Each batch root becomes a leaf in a persistent growing tree
- Consistency proof: prove log at time T is unmodified prefix of log at T+N
- External witness support (Sigstore Rekor, RFC 3161 timestamping)

### 11.7 QUIC Inter-Node Transport

All inter-node communication over QUIC (RFC 9000).

#### Library choice: quinn (not quiche)

| Dimension | quinn | quiche (Cloudflare) |
|-----------|-------|---------------------|
| Language | Pure Rust | C/Rust hybrid |
| TLS | rustls (Rust) | BoringSSL (C) |
| Async model | Native Tokio async | Sans-I/O (manual socket/timer management) |
| Build deps | `cargo build` | cmake + Go + C compiler (BoringSSL) |
| openraft fit | Async trait → direct | Needs async adapter layer |
| WSL2/cross-platform | Works everywhere | BoringSSL build can fail on WSL2 |
| BBR congestion | Pluggable API (addable) | Built-in, CDN-proven |
| HTTP/3 | Via `h3` + `h3-quinn` crates | Built-in |

**Decision: quinn.** The async-native Tokio integration avoids ~500-1000 LOC of socket/timer
glue code that quiche's sans-I/O API would require. For Aeon's LAN inter-node use case,
the lack of built-in BBR is acceptable (NewReno default, BBR addable via pluggable API).

#### TLS stack: rustls + aws-lc-rs

| Option | Pros | Cons |
|--------|------|------|
| rustls + ring | Pure Rust, audited | ring maintenance concerns, no FIPS |
| **rustls + aws-lc-rs** | **FIPS 140-3 certified, AWS-maintained, rustls default** | C/ASM underneath (ships pre-built) |
| OpenSSL | Battle-tested | C dep, not compatible with quinn |
| BoringSSL | Cloudflare-proven | cmake+Go required, only with quiche |

**Decision: rustls + aws-lc-rs.** FIPS 140-3 path available, actively maintained by AWS,
recently became rustls's default crypto provider. Same TLS stack used for both QUIC
inter-node transport and connector TLS (consistency). Ships pre-built binaries — no
cmake/Go needed. Note: on exotic targets (musl static, some ARM), a C compiler may be
needed; test builds early for those targets.

#### Stream types

| Stream Type | Purpose | Priority |
|-------------|---------|----------|
| Raft RPCs | Vote, AppendEntries, InstallSnapshot | Highest |
| PoH sync | Global checkpoint exchange | High |
| Partition transfer | State + data migration during scaling | Normal |
| Health | Heartbeat, failure detection | Low |

#### Key properties

- **No head-of-line blocking**: Each stream independent — Raft RPCs never blocked by
  partition data transfers
- **0-RTT reconnection**: Sub-millisecond reconnect on transient network drops without
  losing the Raft term
- **NewReno congestion control** (default) / BBR (addable via quinn's pluggable API)
- **mTLS**: Mutual TLS via rustls — all cluster nodes mutually authenticate
- **Bincode framing**: 4-byte LE length prefix + bincode payload

QUIC listeners are **only started when peers exist**. A single-node cluster has no QUIC
overhead — no listening socket, no background tasks.

### 11.8 Partition Management

Partitions are the unit of parallelism and ownership.

#### Partition count (immutable)

Partition count is decided **once at cluster creation** and never changes. Repartitioning
would require rehashing every event's partition assignment and migrating state across all
nodes — prohibitively expensive. Choose a count large enough for maximum expected scale.

| Expected max nodes | Recommended partitions | Partitions per node (min) |
|--------------------|------------------------|---------------------------|
| 1-3 | 16 | 5 |
| 3-7 | 32 | 4 |
| 7+ | 64 or 128 | 9+ |

Rule of thumb: `num_partitions >= max_expected_nodes * 4`.

#### Partition assignment

```
16 partitions across a 3-node cluster:

  Node 1 (Leader):    P0  P1  P2  P3  P4  P15
  Node 2 (Follower):  P5  P6  P7  P8  P9
  Node 3 (Follower):  P10 P11 P12 P13 P14
```

Assignment is determined by the Raft leader and replicated via Raft log. Rebalancing
happens automatically on:
- Node join (after promotion to voter)
- Node departure (after drain)
- Manual rebalance command (`aeon rebalance`)

#### Kafka/Redpanda partition coordination

Aeon partitions map to Kafka/Redpanda partitions. When Aeon transfers a partition between
nodes, the Kafka consumer assignment must also move. Aeon uses **manual partition assignment**
(`assign()`) instead of consumer groups (`subscribe()`):

- Aeon's Partition Manager owns the mapping: Aeon partition → Kafka partition → node
- Each node's Kafka consumer is directly told which Kafka partitions to read
- No Kafka-level consumer group rebalance needed (which would pause all partitions)
- Source-Anchor offsets are managed by Aeon, not Kafka's `__consumer_offsets`

This decouples Aeon's partition management from Kafka's consumer group protocol and
extends cleanly to non-Kafka sources in the future.

#### Two-phase partition transfer protocol (over QUIC)

Naive approach (pause → transfer everything → resume) causes unacceptable downtime for
large L3 state (gigabytes of redb/RocksDB data files). Instead, use two phases:

```
Source Node                              Target Node
     │                                        │
     │  Phase 1: Bulk Sync (partition still running)
     │                                        │
     │──── TransferInit(partition_id) ───────▶│
     │                                        │
     │──── L3 state data files ─────────────▶│  (background, can be GBs)
     │──── (source continues processing,     │
     │      new writes go to WAL delta)      │
     │                                        │
     │◀─── BulkSyncAck ────────────────────  │
     │                                        │
     │  Phase 2: Cutover (brief pause — milliseconds)
     │                                        │
     │  [pause partition processing]          │
     │  [drain SPSC ring buffer to sink]      │
     │                                        │
     │──── WAL delta (small) ───────────────▶│
     │──── L1 snapshot ─────────────────────▶│
     │──── PoH chain tip ──────────────────▶ │
     │──── Source-Anchor offset ───────────▶ │
     │                                        │
     │◀─── CutoverAck ─────────────────────  │
     │                                        │  [resume partition on target]
     │  [Raft commit: Owned(target)]          │
     │  [release partition ownership]         │
     │                                        │
```

**Why two phases**: Phase 1 transfers the bulk state (potentially gigabytes) while the
partition continues processing — no downtime. Phase 2 only transfers the small delta
accumulated during Phase 1 — actual pause is milliseconds.

**In-flight event handling**: Before pausing in Phase 2, the SPSC ring buffer is drained
to the sink. Uncommitted Kafka offsets replay naturally after the target node's consumer
starts (exactly-once via Source-Anchor).

**Raft state machine**: The partition transitions through committed states:
`Owned(source)` → `Transferring(source, target)` → `Owned(target)`.
If either node crashes mid-transfer, the new leader can safely abort or retry.

### 11.9 Node Discovery

A joining node needs to find the existing cluster. Two mechanisms:

**Static peers** (initial cluster bootstrap):
```yaml
cluster:
  peers:
    - "10.0.0.2:4470"
    - "10.0.0.3:4470"
```

**Seed nodes** (dynamic scaling — only need one reachable seed):
```yaml
cluster:
  seed_nodes:
    - "10.0.0.1:4470"
```

The seed node returns the current Raft membership. The new node joins as a learner
through the leader. Once joined, the node discovers all peers via Raft membership log.

For Kubernetes: seed nodes can point to a headless Service DNS name that resolves
to existing pod IPs.

### 11.10 Manifest Configuration

```yaml
# Single-node (minimum config — Raft runs with quorum of 1)
cluster:
  node_id: 1
  bind: "0.0.0.0:4470"
  num_partitions: 16          # Immutable after creation

# 3-node cluster (initial bootstrap — all nodes listed)
cluster:
  node_id: 1
  bind: "0.0.0.0:4470"
  peers:
    - "10.0.0.2:4470"
    - "10.0.0.3:4470"
  num_partitions: 16          # Same as single-node — does NOT change
  tls:
    cert: /etc/aeon/tls/node.pem
    key: /etc/aeon/tls/node.key
    ca: /etc/aeon/tls/ca.pem

# New node joining existing cluster (dynamic scaling)
cluster:
  node_id: 4
  bind: "0.0.0.0:4470"
  seed_nodes:                 # Only need one reachable seed
    - "10.0.0.1:4470"
  tls:
    cert: /etc/aeon/tls/node.pem
    key: /etc/aeon/tls/node.key
    ca: /etc/aeon/tls/ca.pem
```

**Runtime scaling** (no restart needed):
```bash
# Add a node (joins as learner, auto-promotes, auto-rebalances)
aeon cluster add 10.0.0.4:4470

# Remove a node (drains partitions, removes from Raft)
aeon cluster remove 10.0.0.4:4470

# Check cluster state
aeon cluster status

# Manual rebalance
aeon cluster rebalance
```

---

## 12. Non-Functional Requirements

### Security
- Transit: TLS 1.2+ on all connectors; mTLS between cluster nodes
- At-rest: Encrypt-then-MAC (AES-256-CTR + HMAC-SHA-512)
- Key management: KeyProvider trait (env, local, Vault, PKCS#11, cloud KMS)
- FIPS 140-3 mode: aws-lc-rs backend
- PQC hybrid mode: X25519 + ML-KEM-768, Ed25519 + ML-DSA-65
- Event signing: Ed25519 batch signing
- zeroize: All keys zeroed on Drop

### Observability
- Prometheus metrics, Jaeger OTLP tracing, Loki structured logging
- Grafana pre-built dashboards
- PHI/PII masking in logs

### Fault Tolerance
- DLQ, exponential backoff retry, circuit breaker
- Graceful drain on shutdown
- Source-Anchor recovery (exactly-once via offset persistence)

### Backpressure & Data Protection
- **Zero loss by default**: Every event exits via sink or DLQ — never silently dropped.
  Drop is an explicit opt-in per source. See Section 10.1.
- **Engine**: Reactive-pull model — engine pulls from source only when processor is ready
- **Pull sources**: Natural backpressure — stop calling `next_batch()`, source stops polling
- **Push sources**: Three-phase absorption — in-memory buffer → spill-to-disk → protocol
  backpressure (HTTP 429, TCP window, MQTT disconnect, QUIC flow control). See Section 10.1.
- **Slow sinks**: Backpressure propagates backward through entire pipeline. Pipeline
  pauses rather than drops. Source-Anchor offsets advance only after sink acknowledgement.
- **Processor failure**: Failed events → DLQ, never discarded
- **Sink failure**: Retry with exponential backoff → DLQ after max retries → pipeline
  pause if DLQ also failing
- Watermark-based flow control: pause at high watermark, resume at low
- Kafka heartbeat-safe bounded pause (2s max to prevent broker-side session timeout)

### Multi-Tenancy
- Namespace isolation, CPU fuel caps, memory sandboxing, resource quotas

### Health & Admin
- `GET /health`, `/ready`, `/metrics`, `/pipeline`
- `POST /pipeline/pause`, `/pipeline/resume`

---

## 13. Key Architectural Decisions (Locked)

| Concern | Decision | Rationale |
|---------|----------|-----------|
| Async runtime | Tokio (+ tokio-uring on Linux 5.11+) | io_uring where available; standard tokio fallback |
| Wasm runtime | Wasmtime (Component Model) | Best fuel/metering, WIT-based contracts |
| Event serialization | Bincode (hot path) + JSON (config) | Zero-copy, minimal overhead |
| Event IDs | UUIDv7 | Time-ordered -> 6x faster L3 indexing (B-tree/LSM) |
| SIMD | memchr crate | Production-stable, portable (AVX2 + NEON) |
| QUIC | quinn (not quiche) | Async-native Tokio, no C build deps, natural openraft fit |
| Consensus | openraft (always-on, even single-node) | Clean 1→3→5 scaling via membership changes |
| Config format | YAML manifest | K8s/Docker-Compose alignment |
| Error handling | thiserror + anyhow | No panics; typed errors in libs, anyhow in CLI |
| L1 State | DashMap + #[repr(align(64))] | Lock-free, false sharing prevention |
| L2 State | memmap2 | RAM-speed persistence |
| L3 State | redb (default) / RocksDB (pluggable) | Durable, ACID; `L3Store` trait, `L3Backend` config enum |
| Inter-node framing | bincode + 4-byte LE prefix | Compact, zero-copy-friendly |
| TLS | rustls + aws-lc-rs | FIPS 140-3 certified, AWS-maintained, rustls default crypto provider |
| Kafka partition mgmt | Manual assign (not consumer groups) | Decouples Aeon scaling from Kafka rebalance protocol |
| Partition count | Immutable after creation | Repartitioning too expensive; size for max expected scale |
| Data loss | Zero loss by default (drop is explicit opt-in) | Design center: every event exits via sink or DLQ |
| Source models | Push + Pull unified via Source trait | Push sources buffer + spill + protocol backpressure; pull sources stop polling |
| WebTransport | web-transport-quinn (on top of quinn) | Streams = reliable source+sink; Datagrams = lossy source-only opt-in |
| Recovery model | Source-Anchor replay (at-least-once) | Sink ack → L3 offset write; crash replays from last ack'd offset |
| Pipeline topology | DAG via manifest YAML | Fan-in, fan-out, chaining, content-based routing — zero-copy fan-out |
| Windowing | Processor-level, not engine-level | Keeps core pipeline simple; windows use existing state tiers |
| Processor state API | Typed wrappers (ValueState, MapState) over raw KV | Guest-side SDK, not host-side — keeps WIT minimal |
| Processor SDKs | Per-language packages wrapping WIT imports | Idiomatic API per language; same WIT contract underneath |
| Processor DX tooling | `aeon new/dev/build/validate/deploy` CLI | Scaffold, hot-reload, compile, validate, deploy lifecycle |
