# CLAUDE.md — Aeon v3 AI Coding Guidelines

> Aeon is a real-time data processing engine targeting 20M events/sec aggregate.
> Full architecture: `docs/ARCHITECTURE.md` | Phase plan: `docs/ROADMAP.md`

---

## Current Focus: Scenario 1 (Redpanda → Processor → Redpanda)

The **only** scenario that matters until Gate 1 is passed. Everything else is future work.
Fix → improve → load test cycle until Aeon is proven to never be the bottleneck.

```
Redpanda (source topic)
    → Aeon Source (rdkafka consumer, batch polling, manual partition assign)
        → SPSC Ring Buffer
            → Processor (Rust native OR Wasm guest)
                → SPSC Ring Buffer
                    → Aeon Sink (rdkafka producer, batch send)
                        → Redpanda (sink topic)
```

**Active connectors**: Memory (testing), Blackhole (benchmarks), Stdout (debug), Kafka/Redpanda
**Everything else** (Redis, NATS, MQTT, RabbitMQ, PostgreSQL CDC, etc.) is post-Gate 2.

**Current phase**: Phase 0 — Foundation (see `docs/ROADMAP.md`)

**Gate 1 target**: Per-event overhead <100ns, headroom ratio >=5x, zero event loss,
Aeon CPU <50% when Redpanda is saturated. Infrastructure determines absolute throughput;
architecture ensures Aeon is never the bottleneck.

---

## Workspace Structure

```
crates/
├── aeon-types/        # Event, Output, AeonError, ALL shared traits
├── aeon-io/           # Tokio I/O abstraction layer
├── aeon-state/        # L1 DashMap + L2 mmap + L3 RocksDB
├── aeon-wasm/         # Wasmtime host, WIT contracts, fuel metering
├── aeon-connectors/   # Source/Sink implementations (feature-gated)
├── aeon-engine/       # Pipeline orchestrator, SPSC wiring, backpressure
├── aeon-cluster/      # Raft + PoH + QUIC (future)
├── aeon-crypto/       # Encryption, signing, Merkle (future)
├── aeon-observability/# Prometheus, Jaeger, Loki (future)
└── aeon-cli/          # Binary entrypoint, YAML manifest
```

---

## Mandatory Coding Rules

### 1. Interface-first

Traits are defined BEFORE implementations. Always. Code against the trait, not the concrete type.

### 2. No panics in production

```rust
// FORBIDDEN on hot path:
.unwrap()  .expect()  panic!()  unreachable!()

// REQUIRED:
.map_err(AeonError::from)?
.ok_or(AeonError::NotFound)?
```

`Result<T, AeonError>` everywhere. `thiserror` in library crates, `anyhow` in CLI only.

### 3. Zero-copy first

Prioritize `Bytes` slices and `&[u8]` over cloning. Any `.clone()` on the hot path requires
a justification comment explaining why zero-copy is not possible.

```rust
// GOOD: Zero-copy slice
let payload = raw_bytes.slice(offset..offset + len);

// BAD: Unnecessary allocation
let payload = raw_bytes[offset..offset + len].to_vec();
```

### 4. No std I/O on hot path

**Forbidden**: `std::fs`, `std::net`, `std::io::Read/Write` on hot path.
All I/O via **tokio** async interfaces. Use `aeon_io::read()` / `aeon_io::write()`,
never call tokio-uring directly. Standard I/O acceptable only in CLI and test setup.

### 5. SIMD over iterators on hot path

Use `memchr` crate for hot-path byte scanning. Never use `.contains()`, `.find()`, or
iterator chains on raw byte data in the hot path. Never parse JSON just to route.

### 6. Memory alignment

All core hot-path structures use `#[repr(align(64))]`: Event, Output, state store wrappers,
ring buffer entries. Prevents false sharing between CPU cores.

### 7. Batch-first APIs

`Source::next_batch()` returns `Vec<Event>`, `Sink::write_batch()` accepts `Vec<Output>`.
No per-event channel overhead.

### 8. SPSC ring buffers on hot path

Use `rtrb` crate for source→processor and processor→sink paths.
Crossbeam channels acceptable for control plane only.

### 9. Static dispatch on hot path

Generics (static dispatch) for Source/Sink/StateOps on hot path.
`dyn Trait` objects only for Processor (Wasm runtime) and QuicTransport.

### 10. Feature-flag everything

Every connector, every optional capability behind a Cargo feature flag:
```toml
[features]
default = ["memory", "blackhole", "stdout"]
kafka = ["rdkafka"]
wasm = ["wasmtime"]
```

### 11. Test-driven development

Every feature must have tests before moving on. Unit tests in-module, integration tests
in `tests/`, benchmarks via criterion for every hot-path component.

### 12. No unnecessary dependencies

Every dependency must be justified. Prefer standard library, minimal crates, feature-gated
optional deps.

### 13. Idiomatic Rust

- `clippy` clean: `cargo clippy --workspace -- -D warnings`
- `rustfmt`: `cargo fmt --all`
- No `unsafe` without `// SAFETY:` comment
- Use `Arc` only when shared ownership is genuinely needed

---

## Canonical Event Envelope

**All** sources produce Events, all processors consume/emit Events, all sinks receive Outputs.
Never generate custom event structures.

```rust
#[repr(align(64))]
pub struct Event {
    pub id: uuid::Uuid,                                    // UUIDv7
    pub timestamp: i64,                                     // Unix epoch nanos
    pub source: Arc<str>,                                   // Interned, NOT String
    pub partition: PartitionId,
    pub metadata: SmallVec<[(Arc<str>, Arc<str>); 4]>,     // Inline ≤4 headers
    pub payload: bytes::Bytes,                              // Zero-copy
    pub source_ts: Option<std::time::Instant>,
}

#[repr(align(64))]
pub struct Output {
    pub destination: Arc<str>,
    pub key: Option<bytes::Bytes>,
    pub payload: bytes::Bytes,
    pub headers: SmallVec<[(Arc<str>, Arc<str>); 4]>,
    pub source_ts: Option<std::time::Instant>,
}
```

---

## Core Traits — Gate 1 Only (aeon-types)

Only define these traits now. Gate 2 and post-Gate 2 traits are defined when their
phase begins (see `docs/ARCHITECTURE.md` Section 3).

```rust
pub trait Source: Send + Sync {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError>;
}

pub trait Sink: Send + Sync {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<(), AeonError>;
    async fn flush(&mut self) -> Result<(), AeonError>;
}

pub trait Processor: Send + Sync {
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError>;
    fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError>;
}

pub trait StateOps: Send + Sync {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, AeonError>;
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), AeonError>;
    async fn delete(&self, key: &[u8]) -> Result<(), AeonError>;
}

pub trait Seekable: Source {
    async fn seek(&mut self, offset: u64) -> Result<(), AeonError>;
}

pub trait IdempotentSink: Sink {
    async fn has_seen(&self, event_id: &uuid::Uuid) -> Result<bool, AeonError>;
}
```

---

## Testing Commands

```bash
cargo test -p aeon-types           # Unit tests for a crate
cargo test --workspace             # All workspace tests
cargo bench -p aeon-engine         # Benchmarks
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

---

## Key Decisions (Locked)

| Concern | Decision |
|---------|----------|
| Async runtime | Tokio (+ tokio-uring behind feature flag) |
| Wasm runtime | Wasmtime (Component Model) |
| Hot-path serialization | Bincode (cluster inter-node) |
| AWPP transport codec | MessagePack (default) / JSON (fallback), per-pipeline config |
| Event IDs | UUIDv7 (per-core pre-generation pool) |
| SIMD | memchr crate |
| QUIC | quinn (not quiche) — async-native Tokio |
| TLS | rustls + aws-lc-rs (FIPS 140-3 path) |
| Consensus | openraft (always-on, even single-node) |
| Kafka partition mgmt | Manual assign (not consumer groups) |
| Partition count | Immutable after creation |
| Config | YAML manifest |
| Errors | thiserror (lib) + anyhow (CLI) |
| Hot-path buffers | SPSC ring buffers (rtrb) |
| Performance target | Aeon never the bottleneck (see ARCHITECTURE.md 5.9) |



The following
rules apply to every response unless explicitly overridden by the route configuration.

1. CITE OR REFUSE
   Every concrete factual claim must cite a source span (file path with line range,
   or a quoted phrase from the input). Claims you cannot cite must be marked
   "(unsourced inference)". If you cannot verify a claim against the source and
   cannot mark it as unsourced inference, refuse rather than guess.

2. PRESERVE SOURCE TENSE AND VOICE
   When summarizing or paraphrasing, preserve the verb tense, voice, register,
   and perspective of the source. If the source is in future tense, your output
   remains in future tense. If the source is formal, your output is formal.
   If you cannot determine the source's tense or voice, ask before summarizing.

3. REFUSE ON EMPTY INPUT
   If no source content is provided for a content-generation task, return the
   exact token NO_SOURCE_PROVIDED and stop. Do not generate plausible-looking
   filler from nothing.

4. CALIBRATED UNCERTAINTY
   "I cannot determine this from the source" is a correct answer. You are
   evaluated on calibrated uncertainty, not on confident-sounding answers.
   When uncertain, say so and qualify your response.

5. PASTED-CONTENT PROVENANCE
   On any turn where the user has pasted content longer than 500 characters,
   list the three most important factual claims in the pasted content and rate
   your confidence in each (high / medium / low / unverifiable) BEFORE responding
   to the user's question. Do not skip this step on long pasted content.

6. QUOTATION DISCIPLINE
   Use quotation marks ONLY for verbatim source spans you can locate. Paraphrase
   without quotes. If you cannot retrieve the verbatim span, paraphrase.

7. VERIFY IDENTIFIERS VIA TOOLS
   When tools are available (read_file, grep, search), use them to verify
   concrete identifiers (function names, file paths, ADR numbers, library
   versions) before referencing them. Do not rely on recall for identifiers.

8. REFUSAL TOKENS
   Reserved tokens that, when present, terminate generation:
   - NO_SOURCE_PROVIDED — empty/whitespace input on a generation task
   - INSUFFICIENT_CONTEXT — input present but inadequate to answer
   - VERIFICATION_REQUIRED — answer requires tool use that is unavailable
   Use these tokens precisely. Do not paraphrase them.