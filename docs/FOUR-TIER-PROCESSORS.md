# Four-Tier Processor Runtime Architecture — Evaluation & Design Document

> **Status**: Partially implemented  
> **Date**: 2026-04-05 (design), 2026-04-09 (status update)  
> **Scope**: Phase 12b — Universal processor development model  
> **Goal**: Enable processor development in 26+ programming languages via four transport tiers
>
> **What's implemented**: T1 Native (Rust, C/C++, .NET NativeAOT via C-ABI loading),
> T2 Wasm (Wasmtime host, fuel metering, state isolation), T4 WebSocket host (AWPP
> protocol, ED25519 identity, JSON/MsgPack codecs) with SDKs in Python, Go, Node.js,
> C#/.NET, Java, PHP, C/C++, and Rust. 43 E2E tests passing across 8 tiers.
> **Not yet implemented**: T3 WebTransport host (code exists, needs TLS test harness),
> child process isolation tier, auto-scaling, RBAC per-processor.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Analysis](#2-current-state-analysis)
3. [Proposed Four-Tier Architecture](#3-proposed-four-tier-architecture)
4. [Gap Analysis](#4-gap-analysis)
5. [Reusable Infrastructure](#5-reusable-infrastructure)
6. [Architectural Changes Required](#6-architectural-changes-required) — types, traits, binding, ED25519 identity
7. [Trait Hierarchy Design](#7-trait-hierarchy-design)
8. [Wire Format & Protocol Design](#8-wire-format--protocol-design) — AWPP, challenge-response, per-batch signing
9. [Per-Tier Implementation Details](#9-per-tier-implementation-details)
10. [Pipeline Integration](#10-pipeline-integration)
11. [SDK Strategy](#11-sdk-strategy)
12. [Registry & CLI Changes](#12-registry--cli-changes) — identity management, pipeline binding YAML
13. [REST API Changes](#13-rest-api-changes) — identity CRUD, instance status, WS upgrade
14. [Observability & Monitoring](#14-observability--monitoring)
15. [Security Model](#15-security-model) — RBAC, ED25519, pipeline isolation, dedicated vs shared
16. [Testing Strategy](#16-testing-strategy)
17. [Implementation Plan](#17-implementation-plan) — 8 phases
18. [Risk Assessment](#18-risk-assessment) — technical, compatibility, security
19. [Decision Log](#19-decision-log) — 15 decisions

---

## 1. Executive Summary

Aeon currently supports two processor runtimes: **Rust-native (.so/.dll)** and **WebAssembly (.wasm)**. Both are in-process, requiring either Rust compilation or a Wasm-capable toolchain.

This document proposes extending to a **four-tier architecture** that enables processor development in **any programming language** while preserving Aeon's core performance characteristics:

| Tier | Transport | Throughput (batched) | Throughput Scope | Languages |
|------|-----------|---------------------|-----------------|-----------|
| **T1 — Native** | In-process (.so/.dll) | **4.2M evt/s** | Per partition, vertical only | Rust, C, C++ |
| **T2 — Wasm** | In-process (wasmtime) | **820K-940K evt/s** | Per partition, vertical only | Rust, C, C++, AssemblyScript, Go (TinyGo), Zig, Kotlin/Native |
| **T3 — WebTransport** | Out-of-process (QUIC/HTTP3) | **~1.2M evt/s** | Per stream per instance, horizontal scaling | Go, Python, Java, C#/.NET, Rust, C/C++, Kotlin/JVM, Swift |
| **T4 — WebSocket** | Out-of-process (HTTP/2 or HTTP/1.1) | **~400K evt/s** | Per connection per instance, horizontal scaling | **All languages** — PHP, Ruby, Perl, R, Lua, Erlang/Elixir, Haskell, Dart, Julia, and everything above |

**Key principle**: Interface-first. One trait, four transports. The pipeline never knows which tier is behind it.

---

## 2. Current State Analysis

### 2.1 Existing Processor Trait

**File**: `crates/aeon-types/src/traits.rs` (lines 57-69)

```rust
pub trait Processor: Send + Sync {
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError>;
    fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {
        // Default: calls process() per event
    }
}
```

**Characteristics**:
- Synchronous — `fn process(&self, ...)`, no async
- Takes `&self` — safe for concurrent use
- Returns owned `Vec<Output>` — no streaming/async yield
- `process_batch()` has default per-event implementation

**Implication for T3/T4**: The `Processor` trait is synchronous. Out-of-process transports are inherently async (network I/O). We need either:
- (A) Change `Processor` to async — **breaking change**, affects all existing processors
- (B) Introduce a new `ProcessorTransport` trait that is async, and adapt in-process processors to it — **non-breaking**, preferred

### 2.2 T1 — Native Processor (Implemented)

**Files**:
- `crates/aeon-engine/src/native_loader.rs` (358 lines)
- `crates/aeon-native-sdk/src/lib.rs` (182 lines)
- `crates/aeon-native-sdk/src/wire.rs` (313 lines)

**Architecture**:
```
Host (aeon-engine)                     Guest (.so/.dll)
──────────────────                     ────────────────
serialize_event(Event) → bytes    →    aeon_process(ctx, ptr, len, out, cap, &out_len) → i32
                                       │  deserialize_event(bytes) → Event
                                       │  user_fn.process(event) → Vec<Output>
                                       │  serialize_outputs(outputs) → bytes
deserialize_outputs(bytes) ← outputs ←  return 0 (success)
stamp event identity on outputs
```

**C-ABI Contract**:
- `aeon_processor_create(config_ptr, config_len) → *mut c_void`
- `aeon_processor_destroy(ctx)`
- `aeon_process(ctx, event_ptr, event_len, out_buf, out_capacity, &out_len) → i32`
- `aeon_process_batch(ctx, events_ptr, events_len, out_buf, out_capacity, &out_len) → i32`

**Wire Format**: Binary, little-endian, length-prefixed fields (same as Wasm).

**Key Details**:
- Reusable 1MB output buffer with auto-growth (doubles on `-4` return code)
- SHA-256 integrity verification before loading
- Host-side event identity stamping after each `process()` call
- `process_batch()` currently iterates per-event for identity propagation
- `Mutex<Vec<u8>>` on output buffer (allows `&self` on `process()`)

### 2.3 T2 — Wasm Processor (Implemented)

**Files**:
- `crates/aeon-wasm/src/processor.rs` (577 lines)
- `crates/aeon-wasm/src/runtime.rs` (668 lines)
- `crates/aeon-wasm-sdk/src/lib.rs` (264 lines)
- `crates/aeon-wasm-sdk/src/wire.rs` (436 lines)

**Architecture**:
```
Host (aeon-wasm)                       Guest (.wasm)
────────────────                       ──────────────
serialize_event(Event) → bytes    →    [guest memory via alloc()]
call "process"(ptr, len)          →    process(ptr, len) → result_ptr
                                       │  deserialize_event(ptr..len)
                                       │  user_fn(event) → Vec<Output>
                                       │  serialize_outputs(outputs)
                                       │  alloc(4 + len), write [len][bytes]
read_memory(result_ptr, len)      ←    return result_ptr
call "dealloc"(ptr, len) × 2
deserialize_outputs(bytes)
stamp event identity on outputs
```

**Wasm ABI Contract (guest exports)**:
- `alloc(size: i32) → i32` — bump allocator
- `dealloc(ptr: i32, size: i32)` — no-op (bump, resets per call)
- `process(ptr: i32, len: i32) → i32` — returns pointer to length-prefixed output

**Host Imports (env namespace)**:
- `state_get`, `state_put`, `state_delete` — namespaced KV store
- `log_info`, `log_warn`, `log_error` — forwarded to tracing
- `metrics_counter_inc`, `metrics_gauge_set` — metrics collection
- `clock_now_ms` — wall clock access
- `abort` — AssemblyScript compatibility

**Runtime Config**:
- `max_fuel: Option<u64>` — fuel metering per `process()` call (default: 1,000,000)
- `max_memory_bytes: usize` — memory limit (default: 64 MiB)
- `enable_simd: bool` — SIMD instructions (default: true)
- `namespace: String` — state isolation prefix

**SDK Features**:
- `aeon_processor!` macro generates all ABI boilerplate
- Guest `Event`/`Output` types (owned, not zero-copy — Wasm boundary)
- `no_std` compatible, bump allocator (256KB heap, resets per call)
- Host function wrappers: `state::get/put/delete`, `log::info/warn/error`, `metrics::*`, `clock::*`

### 2.4 Pipeline Wiring (How Processors Connect)

**File**: `crates/aeon-engine/src/pipeline.rs`

**Direct mode** (`run()`):
```rust
pub async fn run<S, P, K>(source, processor, sink, metrics, shutdown)
where S: Source, P: Processor, K: Sink
```
Loop: `source.next_batch()` → `processor.process_batch()` → `sink.write_batch()`

**Buffered mode** (`run_buffered()`):
```
Source Task ──[SPSC RingBuffer<Vec<Event>>]──▶ Processor Task ──[SPSC RingBuffer<Vec<Output>>]──▶ Sink Task
```
Three concurrent async tasks. Processor task:
```rust
match src_cons.pop() {
    Ok(events) => {
        let outputs = processor.process_batch(events)?;
        // Push to sink buffer
    }
}
```

**Critical observation**: The pipeline is **generic over `P: Processor`** with static dispatch. This means:
1. T1/T2 work because `NativeProcessor` and `WasmProcessor` implement `Processor`
2. T3/T4 cannot plug in directly — they need async transport, not sync `fn process()`
3. We need a new abstraction layer that wraps both sync processors and async transports

### 2.5 ProcessorType Enum

**File**: `crates/aeon-types/src/registry.rs` (lines 11-28)

```rust
pub enum ProcessorType {
    Wasm,
    NativeSo,
}
```

Only two variants. Matched in:
- `Display` impl (same file)
- CLI file extension detection (`crates/aeon-cli/src/main.rs` lines 794-798)
- Tests

### 2.6 Event Identity Propagation

**Critical pattern** — every processor must propagate event identity:

```rust
// After processing, host stamps outputs:
output.source_event_id = Some(event.id);
output.source_partition = Some(event.partition);
output.source_offset = event.source_offset;
output.source_ts = event.source_ts;  // if not set by processor
```

This enables:
- **DeliveryLedger** tracking per-event delivery status
- **Checkpoint** persistence (per-partition minimum pending offset)
- **DLQ** correlation (failed outputs trace back to source event)

For T1/T2: Host stamps after each `process()` call (line-by-line in native_loader.rs).  
For T3/T4: Must be handled in the transport adapter, since the remote processor doesn't have access to `Arc<str>`, `Instant`, etc.

### 2.7 Wire Format (Already Universal)

The binary wire format is **identical** across T1 and T2:

**Event (host → processor)**:
```
[16B: UUID][8B: timestamp i64 LE][4B: source_len][source bytes]
[2B: partition u16 LE][4B: meta_count]
  per meta: [4B: key_len][key][4B: val_len][val]
[4B: payload_len][payload]
```

**Output (processor → host)**:
```
[4B: output_count]
per output:
  [4B: dest_len][dest][1B: has_key]
  if has_key: [4B: key_len][key]
  [4B: payload_len][payload][4B: header_count]
  per header: [4B: key_len][key][4B: val_len][val]
```

**This wire format is the universal contract.** T3/T4 send the same bytes over WebTransport streams or WebSocket frames. No format translation needed.

### 2.8 Existing WebTransport Connector Infrastructure

**Files**: `crates/aeon-connectors/src/webtransport/`

Already implemented:
- `WebTransportSource` — server accepting WebTransport sessions, bidirectional streams, length-prefixed framing
- `WebTransportSink` — client connecting to WebTransport server, opens stream per batch
- `WebTransportDatagramSource` — unreliable datagrams with explicit loss acceptance
- Uses `wtransport` crate, `PushBuffer` for backpressure

**Length-prefixed framing already matches our wire format pattern:**
```rust
// Read 4-byte length prefix
stream.read_exact(&mut len_buf).await?;
let len = u32::from_le_bytes(len_buf) as usize;
// Read payload
stream.read_exact(&mut payload_buf[..len]).await?;
```

### 2.9 Existing WebSocket Connector Infrastructure

**Files**: `crates/aeon-connectors/src/websocket/`

Already implemented:
- `WebSocketSource` — connects to WS server, reads messages via `PushBuffer`
- `WebSocketSink` — maintains persistent `SplitSink`, sends binary messages
- Uses `tokio-tungstenite`, `futures-util`

**Frame-per-message pattern:**
```rust
// Send binary message
self.writer.send(Message::Binary(payload.to_vec().into())).await?;
```

### 2.10 Existing Python SDK

**Files**: `sdks/python/aeon_processor.py` (308 lines)

Already implements:
- `Event` and `Output` dataclasses
- `deserialize_event()` — reads binary wire format
- `serialize_outputs()` — writes binary wire format
- Host function stubs (state, log, metrics, clock)
- `register_processor()` decorator pattern

Currently Wasm-oriented (imports from `env` module). For T3/T4, the wire format logic is **directly reusable** — only the transport layer changes (network I/O instead of Wasm memory).

---

## 3. Proposed Four-Tier Architecture

### 3.1 Tier Definitions

| Tier | Name | Transport | Process Model | Scaling Model |
|------|------|-----------|---------------|---------------|
| **T1** | Native | In-process, C-ABI (`libloading`) | Synchronous, direct function call | Vertical only (single node) |
| **T2** | Wasm | In-process, Wasm ABI (wasmtime) | Synchronous, shared memory | Vertical only (single node) |
| **T3** | WebTransport | Out-of-process, QUIC/HTTP3 | Asynchronous, bidirectional streams | Horizontal + vertical |
| **T4** | WebSocket | Out-of-process, WS/HTTP2/HTTP1.1 | Asynchronous, WebSocket frames | Horizontal + vertical |

### 3.2 Language Support Matrix

| Language | T1 | T2 | T3 | T4 | Recommended |
|----------|:--:|:--:|:--:|:--:|-------------|
| **Rust** | ✓ | ✓ | ✓ | ✓ | T1 (max perf) or T2 (sandboxed) |
| **C / C++** | ✓ | ✓ | ✓ | ✓ | T1 or T2 |
| **AssemblyScript** | — | ✓ | — | ✓ | T2 |
| **Zig** | — | ✓ | ✓ | ✓ | T2 |
| **Go** | — | ✓ (TinyGo) | ✓ | ✓ | T3 (`quic-go`) |
| **Kotlin/Native** | — | ✓ | ✓ | ✓ | T2 or T3 |
| **Python** | — | — | ✓ | ✓ | T3 (`aioquic`) or T4 |
| **Java** | — | — | ✓ | ✓ | T3 (Netty QUIC) |
| **Kotlin/JVM** | — | — | ✓ | ✓ | T3 |
| **C# / .NET** | — | — | ✓ | ✓ | T3 (`System.Net.Quic`) |
| **Swift** | — | — | ✓ | ✓ | T3 (Network.framework) |
| **Node.js / TypeScript** | — | ✓ (AS) | experimental | ✓ | T2 (via AS) or T4 |
| **PHP** | — | — | — | ✓ | T4 (Ratchet, Swoole) |
| **Ruby** | — | — | — | ✓ | T4 |
| **Perl** | — | — | — | ✓ | T4 |
| **Erlang / Elixir** | — | — | — | ✓ | T4 |
| **R** | — | — | — | ✓ | T4 |
| **Lua** | — | — | — | ✓ | T4 |
| **Haskell** | — | — | — | ✓ | T4 |
| **Dart** | — | — | — | ✓ | T4 |
| **Julia** | — | — | — | ✓ | T4 |
| **Delphi / Pascal** | — | — | — | ✓ | T4 |
| **COBOL** (via FFI) | — | — | — | ✓ | T4 |

### 3.3 Performance Comparison

| Metric | T1 Native | T2 Wasm | T3 WebTransport | T4 WebSocket |
|--------|-----------|---------|-----------------|--------------|
| Per-event latency | 240ns | 1.1-1.2μs | ~5-15μs | ~30-80μs |
| Batched throughput (single partition) | 4.2M evt/s | 940K evt/s | ~1.2M evt/s | ~400K evt/s |
| 8-partition, 1 node | 33.6M | 7.5M | 9.6M | 3.2M |
| 8-partition, 3-node cluster | 100M+ | 22.5M | 28.8M | 9.6M |
| 3-node, 4 instances each | — | — | 115M | 38.4M |
| Isolation | None (same process) | Wasm sandbox | OS process | OS process |
| Hot-reload | dlclose/dlopen | Wasm re-instantiate | Reconnect stream | Reconnect socket |
| Debugging | GDB/lldb | Limited | Full native debugger | Full native debugger |
| Horizontal scaling | No | No | Yes (N instances) | Yes (N instances) |

### 3.4 Scaling to 20M Events/sec

| Target | T1 | T2 | T3 | T4 |
|--------|----|----|----|----|
| **20M evt/s** | 1 node, 5 partitions | 3 nodes | 3 nodes, 2 instances/node | 3 nodes, 6 instances/node |

---

## 4. Gap Analysis

### 4.1 What Exists vs What's Needed

| Component | Current State | Required for Four-Tier | Gap |
|-----------|--------------|----------------------|-----|
| **Processor trait** | Sync `fn process(&self, Event)` | Async transport abstraction | New `ProcessorTransport` trait |
| **ProcessorType enum** | `Wasm`, `NativeSo` | + `WebTransport`, `WebSocket` | 2 new variants |
| **Pipeline wiring** | Generic `P: Processor` (static dispatch) | `Box<dyn ProcessorTransport>` or enum dispatch | Adapter layer |
| **Wire format** | Binary, identical in T1/T2 | Same format over network | Batch framing header |
| **WebTransport infra** | Source/Sink connectors exist | Processor host (server side) | New `WebTransportProcessorHost` |
| **WebSocket infra** | Source/Sink connectors exist | Processor host (server side) | New `WebSocketProcessorHost` |
| **Control protocol** | None | Registration, heartbeat, config negotiation | New AWPP protocol |
| **Event identity** | Host stamps after sync `process()` | Same stamping after async `call_batch()` | Handled in transport adapter |
| **Host imports** | Wasm host functions (state, log, metrics, clock) | Network-accessible equivalents | Control stream RPCs or separate API |
| **Registry** | Stores artifacts (files) | T3/T4 have no artifact — they are endpoints | New registration model |
| **CLI** | Infers type from file extension | T3/T4 need endpoint URL, not file | New registration flow |
| **REST API** | Processor CRUD with artifact upload | T3/T4 processor registration with endpoint | New endpoint fields |
| **Backpressure** | SPSC ring buffer (in-process) | Network-level flow control | QUIC flow control (T3), WS pause (T4) |
| **Health monitoring** | None (in-process assumed healthy) | Heartbeat, connection loss detection | Control stream |
| **Multi-instance** | N/A (single in-process instance) | Load balancing, failover | Connection pool |
| **SDKs** | Rust native SDK, Rust Wasm SDK, Python (Wasm) | Python T3/T4, Go, Java, C#, Node.js, PHP | New SDKs |

### 4.2 Critical Gaps (Must Solve Before Implementation)

1. **Sync-to-Async Bridge**: Current `Processor` trait is sync. T3/T4 are inherently async. The `ProcessorTransport` trait must be async, with sync-to-async adapters for T1/T2.

2. **Batch Correlation**: When sending a batch over the network, we need to correlate returned outputs with source events. T1/T2 handle this per-event in a loop. T3/T4 need batch-level correlation (batch_id or event-index mapping).

3. **Host Services Over Network**: Wasm processors get `state_get/put/delete`, `log_*`, `metrics_*` via host imports. T3/T4 processors need these same capabilities via network calls. Options:
   - (A) Separate gRPC/HTTP API for host services
   - (B) Multiplex on the control stream
   - (C) Don't provide — T3/T4 processors manage their own state
   - **Recommendation**: Option (C) initially, with (B) as future enhancement. T3/T4 processors are full processes — they can use their own databases, caches, metrics libraries.

4. **Artifact vs Endpoint Registration**: T1/T2 processors are artifacts (files uploaded to registry). T3/T4 processors are services (endpoints that connect to Aeon). The registry model needs to support both.

---

## 5. Reusable Infrastructure

### 5.1 Direct Reuse (No Changes Needed)

| Component | Location | Reuse in |
|-----------|----------|----------|
| Wire format (Event serialization) | `aeon-native-sdk/src/wire.rs` | T3/T4 (same bytes over network) |
| Wire format (Output deserialization) | `aeon-native-sdk/src/wire.rs` | T3/T4 (same bytes from network) |
| `Event` / `Output` structs | `aeon-types/src/event.rs` | All tiers (canonical types) |
| `BatchResult` | `aeon-types/src/delivery.rs` | All tiers (delivery tracking) |
| `DeliveryLedger` | `aeon-engine/src/delivery_ledger.rs` | All tiers (per-event tracking) |
| `PipelineMetrics` | `aeon-engine/src/pipeline.rs` | All tiers (same metrics) |
| `PushBuffer` (backpressure) | `aeon-connectors/src/push_buffer.rs` | T3/T4 (receive-side buffering) |
| Event identity propagation | Pattern in `native_loader.rs` | T3/T4 transport adapters |
| Python wire format | `sdks/python/aeon_processor.py` | T3/T4 Python SDK |

### 5.2 Reuse With Adaptation

| Component | Location | Adaptation Needed |
|-----------|----------|-------------------|
| WebTransport server | `aeon-connectors/src/webtransport/source.rs` | Accept processor sessions instead of data events |
| WebSocket server | `aeon-connectors/src/websocket/source.rs` | Currently a client; need server-side listener |
| Length-prefixed framing | WebTransport source (lines 153-160) | Add batch framing header |
| `wtransport` dependency | `aeon-connectors/Cargo.toml` | Share with `aeon-engine` (or extract to shared crate) |
| `tokio-tungstenite` dependency | `aeon-connectors/Cargo.toml` | Share with `aeon-engine` |
| `ProcessorRegistry` | `aeon-engine/src/registry.rs` | Support endpoint-based registration |

### 5.3 New Components Required

| Component | Crate | Purpose |
|-----------|-------|---------|
| `ProcessorTransport` trait | `aeon-types` | Async transport abstraction |
| `ProcessorHost` trait | `aeon-engine` | Lifecycle management for processor connections |
| `InProcessTransport` | `aeon-engine` | Wraps sync `Processor` → async `ProcessorTransport` |
| `WebTransportProcessorHost` | `aeon-engine` | Server accepting T3 processor connections |
| `WebSocketProcessorHost` | `aeon-engine` | Server accepting T4 processor connections |
| `WebTransportProcessorClient` | New SDK crate | Client-side transport for T3 processors |
| `WebSocketProcessorClient` | New SDK crate | Client-side transport for T4 processors |
| AWPP protocol definition | `aeon-types` | Control stream messages |
| Batch framing | `aeon-types` or shared wire module | Batch header for network transport |

---

## 6. Architectural Changes Required

### 6.1 Changes to `aeon-types`

#### 6.1.1 New `ProcessorTransport` trait

```rust
// aeon-types/src/traits.rs (NEW)

/// Async transport abstraction for all processor tiers.
///
/// The pipeline calls this trait — it never knows whether the processor
/// is in-process (T1/T2) or out-of-process (T3/T4).
pub trait ProcessorTransport: Send + Sync {
    /// Send a batch of events to the processor, receive outputs.
    /// Event identity propagation is handled by the implementation.
    fn call_batch(
        &self,
        events: Vec<Event>,
    ) -> impl std::future::Future<Output = Result<Vec<Output>, AeonError>> + Send;

    /// Check processor health.
    fn health(
        &self,
    ) -> impl std::future::Future<Output = Result<ProcessorHealth, AeonError>> + Send;

    /// Graceful drain — stop accepting new batches, flush in-flight.
    fn drain(
        &self,
    ) -> impl std::future::Future<Output = Result<(), AeonError>> + Send;

    /// Processor metadata.
    fn info(&self) -> ProcessorInfo;
}
```

#### 6.1.2 Supporting types

```rust
// aeon-types/src/processor_transport.rs (NEW)

#[derive(Debug, Clone)]
pub struct ProcessorHealth {
    pub healthy: bool,
    pub latency_us: Option<u64>,      // Last call latency in microseconds
    pub pending_batches: Option<u32>,  // Batches in flight (T3/T4)
    pub uptime_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ProcessorInfo {
    pub name: String,
    pub version: String,
    pub tier: ProcessorTier,
    pub capabilities: Vec<String>,     // e.g., ["batch", "stateful"]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProcessorTier {
    Native,
    Wasm,
    WebTransport,
    WebSocket,
}

/// Processor binding mode within a pipeline definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessorBinding {
    /// Processor instance serves only this pipeline (default, recommended).
    #[default]
    Dedicated,
    /// Processor instance may be shared across pipelines in the same group.
    /// Pipeline isolation is enforced via separate data streams.
    Shared { group: String },
}

/// Per-pipeline processor connection config override.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessorConnectionConfig {
    /// Override endpoint (if different from registry default).
    pub endpoint: Option<String>,
    /// Max batch size for this pipeline.
    pub batch_size: Option<u32>,
    /// Timeout for processor call_batch (ms).
    pub timeout_ms: Option<u64>,
    /// Number of processor instances to require before starting.
    pub min_instances: Option<u32>,
}
```

#### 6.1.3 ProcessorType enum expansion

```rust
// aeon-types/src/registry.rs (MODIFIED)

pub enum ProcessorType {
    Wasm,           // Existing
    NativeSo,       // Existing
    WebTransport,   // NEW — out-of-process QUIC/HTTP3
    WebSocket,      // NEW — out-of-process WS/HTTP2/HTTP1.1
}
```

#### 6.1.4 ProcessorVersion expansion for endpoint-based processors

```rust
// aeon-types/src/registry.rs (MODIFIED)

pub struct ProcessorVersion {
    pub version: String,
    pub sha512: String,               // Hash of artifact (T1/T2) or empty (T3/T4)
    pub size_bytes: u64,              // Artifact size (T1/T2) or 0 (T3/T4)
    pub processor_type: ProcessorType,
    pub platform: String,
    pub status: VersionStatus,
    pub registered_at: i64,
    pub registered_by: String,
    // NEW fields for T3/T4:
    pub endpoint: Option<String>,     // e.g., "https://processor.example.com:4472"
    pub max_batch_size: Option<u32>,  // Negotiated or configured
}
```

#### 6.1.5 ProcessorRef expansion (binding and connection config)

```rust
// aeon-types/src/registry.rs (MODIFIED)

pub struct ProcessorRef {
    pub name: String,
    pub version: String,
    /// Binding mode: dedicated (default) or shared with group.
    #[serde(default)]
    pub binding: ProcessorBinding,
    /// Optional per-pipeline connection config override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ProcessorConnectionConfig>,
}
```

#### 6.1.6 Processor identity types (ED25519-based, T3/T4)

```rust
// aeon-types/src/processor_identity.rs (NEW)

/// ED25519 identity for a T3/T4 processor.
///
/// Each processor instance holds a private key; the corresponding public key
/// is registered in Aeon. Authentication uses challenge-response — the
/// processor signs a server-provided nonce to prove key ownership.
/// Batch responses are also signed for non-repudiation and audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorIdentity {
    /// ED25519 public key (base64-encoded, 32 bytes raw).
    pub public_key: String,
    /// Key fingerprint: SHA-256 of the raw public key bytes (hex, for display/audit).
    pub fingerprint: String,
    /// Processor name this key authenticates as.
    pub processor_name: String,
    /// Pipelines this key is authorized to serve.
    pub allowed_pipelines: PipelineScope,
    /// Maximum concurrent connections from this key.
    pub max_instances: u32,
    /// Registered at (Unix epoch millis).
    pub registered_at: i64,
    /// Registered by (admin identity).
    pub registered_by: String,
    /// Revoked at (None = active).
    pub revoked_at: Option<i64>,
}

/// Which pipelines a processor identity is authorized to serve.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PipelineScope {
    /// Can serve any pipeline that references this processor name.
    AllMatchingPipelines,
    /// Can only serve specifically named pipelines.
    Named(Vec<String>),
}
```

### 6.2 Changes to `aeon-engine`

#### 6.2.1 In-process transport adapter

Wraps existing sync `Processor` implementations (T1/T2) into async `ProcessorTransport`:

```rust
// aeon-engine/src/transport/in_process.rs (NEW)

pub struct InProcessTransport<P: Processor> {
    processor: P,
    info: ProcessorInfo,
}

impl<P: Processor> ProcessorTransport for InProcessTransport<P> {
    async fn call_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {
        // Synchronous call — no await needed, but trait requires async
        self.processor.process_batch(events)
    }

    async fn health(&self) -> Result<ProcessorHealth, AeonError> {
        Ok(ProcessorHealth { healthy: true, ..Default::default() })
    }

    async fn drain(&self) -> Result<(), AeonError> {
        Ok(()) // In-process: nothing to drain
    }

    fn info(&self) -> ProcessorInfo { self.info.clone() }
}
```

#### 6.2.2 Pipeline signature change

```rust
// BEFORE (current):
pub async fn run_buffered<S, P, K>(source, processor, sink, ...)
where S: Source, P: Processor, K: Sink

// AFTER (proposed):
pub async fn run_buffered<S, K>(source, transport: &dyn ProcessorTransport, sink, ...)
where S: Source, K: Sink
```

**Note**: This changes `P: Processor` (static dispatch) to `&dyn ProcessorTransport` (dynamic dispatch). The performance impact is negligible — the overhead of a vtable lookup (~1ns) is dwarfed by the actual processing time (240ns+ for T1). The SPSC ring buffer boundaries are the real hot path, not the trait dispatch.

However, if we want to preserve zero-cost for T1/T2, we can use an enum dispatch instead:

```rust
pub enum AnyTransport {
    InProcess(InProcessTransport<Box<dyn Processor>>),
    WebTransport(WebTransportTransport),
    WebSocket(WebSocketTransport),
}
```

**Recommendation**: Start with `&dyn ProcessorTransport` for simplicity. Optimize to enum dispatch only if benchmarks show measurable impact (unlikely).

#### 6.2.3 New engine modules

```
crates/aeon-engine/src/
  transport/
    mod.rs                          # Transport abstraction
    in_process.rs                   # T1/T2 adapter
    webtransport_host.rs            # T3 server
    websocket_host.rs               # T4 server
```

### 6.3 Changes to `aeon-cli`

#### 6.3.1 New processor registration flow

```bash
# T1/T2 (artifact-based, existing):
aeon processor register my-proc --version 1.0.0 \
  --artifact path/to/processor.wasm --runtime wasm

# T3 (endpoint-based, NEW):
aeon processor register my-proc --version 1.0.0 \
  --runtime webtransport --endpoint https://processor:4472

# T4 (endpoint-based, NEW):
aeon processor register my-proc --version 1.0.0 \
  --runtime websocket --endpoint ws://processor:8080/process
```

### 6.4 Feature Flags

```toml
# aeon-engine/Cargo.toml (MODIFIED)
[features]
default = ["native-loader", "rest-api"]
native-loader = ["libloading", "aeon-native-sdk", "sha2", "hex"]
rest-api = ["axum", "serde_json", "tower-http"]
# NEW:
webtransport-processor = ["wtransport"]
websocket-processor = ["tokio-tungstenite", "futures-util"]
```

---

## 7. Trait Hierarchy Design

### 7.1 Complete Trait Hierarchy

```
                         ProcessorTransport (async, all tiers)
                         ├── call_batch(Vec<Event>) → Vec<Output>
                         ├── health() → ProcessorHealth
                         ├── drain()
                         └── info() → ProcessorInfo
                              │
              ┌───────────────┼────────────────┬──────────────────┐
              │               │                │                  │
    InProcessTransport   WebTransportTransport  WebSocketTransport
    (wraps Processor)    (QUIC streams)         (WS frames)
              │
    ┌─────────┴─────────┐
    │                   │
NativeProcessor    WasmProcessor
(libloading)       (wasmtime)
    │                   │
    └─────────┬─────────┘
              │
        Processor (sync, T1/T2 only)
        ├── process(Event) → Vec<Output>
        └── process_batch(Vec<Event>) → Vec<Output>
```

### 7.2 Trait Relationships

- `Processor` — existing sync trait, unchanged. Implemented by `NativeProcessor`, `WasmProcessor`, `PassthroughProcessor`.
- `ProcessorTransport` — new async trait. The pipeline uses this exclusively.
- `InProcessTransport<P: Processor>` — bridges sync `Processor` to async `ProcessorTransport`.
- `WebTransportTransport` — connects to remote T3 processor via QUIC.
- `WebSocketTransport` — connects to remote T4 processor via WebSocket.

**No existing code breaks.** The `Processor` trait remains unchanged. We add a new layer on top.

---

## 8. Wire Format & Protocol Design

### 8.1 Batch Framing (T3/T4 Network Extension)

T1/T2 use the existing wire format directly (shared memory / Wasm memory). T3/T4 add a thin batch framing header for network transport:

```
Batch Request (Aeon → Processor):
┌─────────────────────────────────────────────┐
│ [8B: batch_id u64 LE]                       │  Correlation ID
│ [4B: event_count u32 LE]                    │  Number of events
│ for each event:                             │
│   [4B: event_len u32 LE]                    │  Per-event length prefix
│   [event_len bytes: serialized Event]       │  Codec-encoded (see §8.4)
│ [4B: checksum CRC32]                        │  Integrity check
└─────────────────────────────────────────────┘

Batch Response (Processor → Aeon):
┌─────────────────────────────────────────────┐
│ [8B: batch_id u64 LE]                       │  Must match request
│ [4B: output_count u32 LE]                   │  Total outputs
│ [4B: event_map_count u32 LE]                │  Event→output mapping
│ for each event_map:                         │
│   [4B: event_index u32 LE]                  │  Index in request batch
│   [4B: output_start u32 LE]                 │  First output index
│   [4B: output_count u32 LE]                 │  Number of outputs
│ for each output:                            │
│   [4B: output_len u32 LE]                   │  Per-output length prefix
│   [output_len bytes: serialized Output]     │  Standard wire format
│ [4B: checksum CRC32]                        │  Integrity check
└─────────────────────────────────────────────┘
```

**Why event_map?** Event identity propagation. The host needs to know which outputs came from which event, so it can stamp `source_event_id`, `source_partition`, `source_offset` correctly. Without this mapping, the host cannot correlate outputs to source events.

**Alternative (simpler)**: Require processors to return outputs in the same order as input events, with a per-event output count:

```
Simplified Batch Response:
┌─────────────────────────────────────────────┐
│ [8B: batch_id u64 LE]                       │
│ [4B: event_count u32 LE]                    │  Must match request
│ for each event (in order):                  │
│   [4B: output_count u32 LE]                 │  Outputs for this event
│   for each output:                          │
│     [4B: output_len u32 LE]                 │
│     [output_len bytes: serialized Output]   │  Codec-encoded (see §8.4)
│ [4B: checksum CRC32]                        │
└─────────────────────────────────────────────┘
```

**Recommendation**: Use the simplified format. It's easier to implement in SDKs and preserves the event→output mapping naturally (sequential, same count as input).

### 8.2 Control Stream Protocol (AWPP — Aeon Wire Processor Protocol)

T3/T4 processors establish a control stream before processing begins. **No event data
flows until authentication and authorization complete.**

```
Connection Lifecycle:
1. Processor obtains OAuth token from IdP (if OAuth enabled)
2. Processor connects to Aeon over TLS (T3: QUIC, T4: WSS)
3. Control stream established (stream 0 or first message)
4. Challenge-response authentication (ED25519 + optional OAuth token)
5. Authorization check (pipeline scope, instance limits)
6. Data streams opened per pipeline per partition
7. Heartbeat loop on control stream (includes token expiry monitoring)
8. Token refresh on control stream (if OAuth enabled, before expiry)
9. Per-batch ED25519 signing on data streams
10. Drain signal → graceful shutdown
```

#### Step 1 — Challenge (Aeon → Processor):
```json
{
    "type": "challenge",
    "protocol": "awpp/1",
    "nonce": "a1b2c3d4e5f6...",
    "oauth_required": true
}
```

Aeon generates a cryptographically random 32-byte nonce (hex-encoded). This prevents
replay attacks — even if a previous handshake is captured, it cannot be reused.
The `oauth_required` field tells the processor whether it must include an OAuth token
in the registration message. This is `false` (or absent) when OAuth is not configured.

#### Step 2 — Registration with Proof (Processor → Aeon):
```json
{
    "type": "register",
    "protocol": "awpp/1",
    "transport": "webtransport",
    "name": "my-enricher",
    "version": "1.2.0",
    "public_key": "ed25519:MCowBQYDK2VwAyEA...",
    "challenge_signature": "<ED25519_Sign(private_key, nonce_bytes)>",
    "oauth_token": "eyJhbGciOiJSUzI1NiIs...",
    "capabilities": ["batch"],
    "max_batch_size": 1024,
    "transport_codec": "msgpack",
    "requested_pipelines": ["orders-pipeline", "payments-pipeline"],
    "binding": "dedicated"
}
```

The processor signs the nonce with its ED25519 private key. Aeon verifies the signature
against the registered public key for this processor name. The `oauth_token` field is
included when OAuth is required (omitted or null when OAuth is disabled).

The `transport_codec` field specifies how Event/Output envelopes are serialized within
batch frames on data streams (see §8.4). Valid values: `"msgpack"` (default), `"json"`.
If omitted, defaults to `"msgpack"`. The control stream always uses JSON regardless.

#### Step 3 — Aeon Validates:

```
Layer 2 — ED25519 (mandatory):
1. ✓ public_key matches a registered ProcessorIdentity for "my-enricher"
2. ✓ challenge_signature is valid ED25519 signature of nonce
3. ✓ identity is not revoked (revoked_at is None)

Layer 2.5 — OAuth (when enabled):
4. ✓ oauth_token JWT signature valid (verified via JWKS from configured issuer)
5. ✓ token not expired (exp > now - grace_period)
6. ✓ issuer matches configured issuer
7. ✓ audience matches configured audience ("aeon-processors")
8. ✓ required scopes present (e.g., "aeon:process")
9. ✓ claim-mapped processor name matches registration name

Layer 3 — Authorization:
10. ✓ version "1.2.0" exists in registry for "my-enricher"
11. ✓ requested_pipelines ⊆ identity.allowed_pipelines
12. ✓ current connected instances < identity.max_instances
13. ✓ each requested pipeline references processor "my-enricher"
```

If any check fails → reject with specific error code.

#### Step 4 — Acceptance (Aeon → Processor):
```json
{
    "type": "accepted",
    "session_id": "sess-abc123",
    "pipelines": [
        {
            "name": "orders-pipeline",
            "partitions": [0, 1, 2, 3],
            "batch_size": 512
        },
        {
            "name": "payments-pipeline",
            "partitions": [0, 1],
            "batch_size": 256
        }
    ],
    "wire_format": "binary/v1",
    "transport_codec": "msgpack",
    "heartbeat_interval_ms": 5000,
    "batch_signing": true
}
```

Data streams are opened per pipeline per partition after acceptance.

#### Rejection (Aeon → Processor):
```json
{
    "type": "rejected",
    "code": "AUTH_FAILED",
    "message": "ED25519 signature verification failed for public key SHA256:abc123..."
}
```

Error codes: `AUTH_FAILED`, `KEY_REVOKED`, `KEY_NOT_FOUND`, `PIPELINE_NOT_AUTHORIZED`,
`MAX_INSTANCES_REACHED`, `VERSION_NOT_FOUND`, `PROCESSOR_NOT_REGISTERED`.

#### Heartbeat (bidirectional, every `heartbeat_interval_ms`):
```json
{"type": "heartbeat", "timestamp_ms": 1712345678000}
```

#### Drain (Aeon → Processor):
```json
{"type": "drain", "reason": "upgrade", "deadline_ms": 30000}
```

#### Error (either direction):
```json
{"type": "error", "code": "BATCH_FAILED", "message": "...", "batch_id": 12345}
```

**Control stream encoding**: JSON for simplicity and debuggability. Only exchanged at
connection setup and every few seconds (heartbeat), so JSON parsing overhead is
irrelevant. Data streams use binary wire format.

### 8.3 Per-Batch Signing (Data Streams)

Every batch response on data streams includes an ED25519 signature appended after the
CRC32 checksum:

```
Signed Batch Response (Processor → Aeon):
┌─────────────────────────────────────────────┐
│ [8B: batch_id u64 LE]                       │
│ [4B: event_count u32 LE]                    │
│ for each event (in order):                  │
│   [4B: output_count u32 LE]                 │
│   for each output:                          │
│     [4B: output_len u32 LE]                 │
│     [output_len bytes: serialized Output]   │
│ [4B: checksum CRC32]                        │  Integrity
│ [64B: ED25519 signature]                    │  Non-repudiation
└─────────────────────────────────────────────┘
```

The signature covers all bytes from `batch_id` through `checksum` (inclusive).
Aeon verifies the signature against the processor's registered public key before
accepting outputs into the pipeline.

**Performance impact**: ED25519 sign (~60μs) + verify (~150μs) per batch.
At batch size 1024: ~0.21μs per event amortized, <4% overhead on T3 throughput.

**Audit record** (logged per batch):
```json
{
    "timestamp": "2026-04-05T14:30:12.456Z",
    "processor": "my-enricher",
    "key_fingerprint": "SHA256:abc123...",
    "session_id": "sess-abc123",
    "pipeline": "orders-pipeline",
    "partition": 2,
    "batch_id": 98765,
    "events_in": 512,
    "outputs_out": 512,
    "signature_valid": true,
    "latency_us": 4200,
    "binding": "dedicated"
}
```

### 8.4 Transport-Specific Framing

#### T3 — WebTransport:
```
Session
├── Stream 0 (bidirectional) — Control stream (JSON, length-prefixed)
├── Stream 1 (bidirectional) — Pipeline "orders", Partition 0 (binary batches + signature)
├── Stream 2 (bidirectional) — Pipeline "orders", Partition 1 (binary batches + signature)
├── Stream 3 (bidirectional) — Pipeline "payments", Partition 0 (binary batches + signature)
└── ...
```

Each data stream carries one pipeline+partition combination. No head-of-line blocking
between partitions or pipelines (QUIC stream independence). When a processor is shared
across pipelines, each pipeline gets its own set of streams — **complete data isolation**.

#### T4 — WebSocket:
```
Connection
├── Text frames   — Control messages (JSON)
└── Binary frames — Data batches (tagged by pipeline + partition)
```

WebSocket doesn't have native stream multiplexing. Each binary frame is tagged:

```
WebSocket Binary Frame:
┌──────────────────────────────────────────────┐
│ [4B: pipeline_name_len u32 LE]               │  Pipeline identifier
│ [N bytes: pipeline_name]                     │
│ [2B: partition u16 LE]                       │  Partition identifier
│ [remaining: batch data + signature]          │  Standard batch format
└──────────────────────────────────────────────┘
```

This ensures pipeline isolation even on a single WebSocket connection. The processor
must include the correct pipeline name and partition in responses; Aeon validates that
outputs are routed only to the correct pipeline's sink.

### 8.5 Transport Codec (Event/Output Envelope Serialization)

The batch frames in §8.1 carry length-prefixed `serialized Event` and `serialized Output`
blobs. The **transport codec** determines how those Event/Output envelopes are serialized
within the frames.

**Important distinction**: The transport codec serializes the Event *envelope* (id,
timestamp, source, partition, metadata, payload). The `Event.payload` field is the user's
data — it passes through as opaque bytes regardless of codec. Aeon never interprets
payload content.

#### Supported Codecs

| Codec | Wire ID | Description | Use Case |
|-------|---------|-------------|----------|
| **MessagePack** | `msgpack` | Binary, compact, schema-less | Default — best balance of speed, size, cross-language support |
| **JSON** | `json` | Text, human-readable | Debugging, prototyping, languages with weak MsgPack support |

#### Codec Negotiation

The codec is configured **per-pipeline** and communicated during AWPP handshake:

1. Processor declares preferred codec in Registration message (`transport_codec` field)
2. Aeon confirms (or overrides to pipeline's configured codec) in Accepted response
3. All subsequent data stream batches use the confirmed codec

If the processor omits `transport_codec`, it defaults to `msgpack`. If the processor
requests a codec that doesn't match the pipeline's configuration, Aeon responds with
the pipeline's configured codec in the Accepted message — the processor must comply.

#### Pipeline Configuration

```yaml
pipeline:
  name: orders
  transport_codec: msgpack   # default, can be omitted
  source: ...
  processor: ...
  sink: ...
```

#### Performance Characteristics

| Codec | Encode (per event) | Decode (per event) | Envelope Size (typical) |
|-------|-------------------|-------------------|------------------------|
| MessagePack | ~200ns | ~180ns | ~200-400B |
| JSON | ~800ns | ~1μs | ~400-800B |

At batch size 1024, the difference is ~600μs per batch — negligible compared to T3/T4
network RTT (500μs-10ms). The codec choice is primarily about debuggability vs compactness,
not performance.

#### Scope

- **Applies to**: T3 (WebTransport) and T4 (WebSocket) data streams only
- **Does NOT apply to**: T1 (native, in-process), T2 (Wasm, uses its own binary wire format),
  AWPP control stream (always JSON), cluster inter-node transport (uses Bincode)

---

## 9. Per-Tier Implementation Details

### 9.1 T1 — Native (No Changes)

**Status**: Fully implemented. No changes required.

The only addition is wrapping `NativeProcessor` in `InProcessTransport`:

```rust
let native = NativeProcessor::load_verified(path, config, hash)?;
let transport = InProcessTransport::new(native, info);
// transport implements ProcessorTransport
```

### 9.2 T2 — Wasm (No Changes)

**Status**: Fully implemented. No changes required.

Same wrapping:

```rust
let wasm = WasmProcessor::new(module, config)?;
let transport = InProcessTransport::new(wasm, info);
```

### 9.3 T3 — WebTransport Processor Host (New)

**Module**: `crates/aeon-engine/src/transport/webtransport_host.rs`

**Architecture**:
```
Aeon Engine
  └── WebTransportProcessorHost
        ├── QUIC Listener (:4472)
        ├── Session Manager (accepts processor connections)
        ├── Control Stream Handler (registration, heartbeat)
        └── Per-Partition Data Streams
              ├── Partition 0: send batch → recv outputs
              ├── Partition 1: send batch → recv outputs
              └── ...
```

**Key implementation details**:

1. **Server setup**: Reuse `wtransport::Endpoint<Server>` pattern from existing WebTransport connector
2. **Session acceptance**: Background task accepts sessions, reads control stream registration
3. **Partition assignment**: Engine assigns partitions based on registration request and current load
4. **Data stream lifecycle**: One bidirectional stream per partition, persistent (not per-batch)
5. **Batch flow**: 
   - Engine serializes events using existing `wire::serialize_event()`
   - Wraps in batch framing (batch_id + event_count + events + checksum)
   - Writes to partition stream
   - Reads batch response from same stream
   - Deserializes outputs using existing `wire::deserialize_outputs()`
   - Stamps event identity
6. **Backpressure**: QUIC per-stream flow control (native, no custom implementation needed)
7. **Failure handling**: Stream reset on error, reconnect with 0-RTT
8. **Connection pool**: Multiple processor instances can connect; Aeon distributes partitions

**Estimated implementation**: ~400-600 lines (server + transport adapter)

### 9.4 T4 — WebSocket Processor Host (New)

**Module**: `crates/aeon-engine/src/transport/websocket_host.rs`

**Architecture**:
```
Aeon Engine
  └── WebSocketProcessorHost
        ├── TCP/TLS Listener (:4473 or shared with REST API)
        ├── WebSocket Upgrade Handler
        ├── Control Message Handler (text frames)
        └── Data Frame Handler (binary frames)
              └── Tagged by partition: [2B: partition][batch data]
```

**Key implementation details**:

1. **Server setup**: Use `axum` WebSocket upgrade (already a dependency for REST API) or standalone `tokio-tungstenite` server
2. **Integration option**: Add WebSocket endpoint to existing REST API router:
   ```
   GET /api/v1/processors/connect → WebSocket upgrade
   ```
   This is elegant — reuses existing HTTP server, auth, TLS.
3. **Frame tagging**: Binary frames prefixed with 2-byte partition ID
4. **Control messages**: Text frames for registration, heartbeat, drain (JSON)
5. **Batch flow**: Same as T3 but over WebSocket binary frames
6. **Backpressure**: Application-level — track in-flight batches, pause sending when limit reached

**Estimated implementation**: ~300-500 lines (server + transport adapter)

---

## 10. Pipeline Integration

### 10.1 Current Pipeline Flow

```rust
// Source task
let events = source.next_batch().await?;
src_prod.push(events)?;

// Processor task
let events = src_cons.pop()?;
let outputs = processor.process_batch(events)?;  // SYNC call
sink_prod.push(outputs)?;

// Sink task
let outputs = sink_cons.pop()?;
sink.write_batch(outputs).await?;
```

### 10.2 Modified Pipeline Flow

```rust
// Source task — UNCHANGED
let events = source.next_batch().await?;
src_prod.push(events)?;

// Processor task — NOW ASYNC
let events = src_cons.pop()?;
let outputs = transport.call_batch(events).await?;  // ASYNC call
sink_prod.push(outputs)?;

// Sink task — UNCHANGED
let outputs = sink_cons.pop()?;
sink.write_batch(outputs).await?;
```

**The only change is in the processor task**: `processor.process_batch(events)?` becomes `transport.call_batch(events).await?`.

For T1/T2: `InProcessTransport::call_batch()` is async in signature but synchronous in execution (no actual await point). The compiler optimizes this to the same code as before.

For T3/T4: The `.await` point is real — it waits for network round-trip. The SPSC ring buffers on both sides provide natural pipelining (source can fill buffer while processor awaits response).

### 10.3 Multi-Instance Fan-Out (T3/T4 Only)

For T3/T4, multiple processor instances can connect for the same processor name. The transport adapter can distribute partitions across instances:

```
WebTransportTransport
  ├── Instance A (partitions 0, 1, 2, 3)
  ├── Instance B (partitions 4, 5, 6, 7)
  └── Instance C (partitions 8, 9, 10, 11)
```

This is transparent to the pipeline — `call_batch()` routes to the correct instance based on event partition.

---

## 11. SDK Strategy

### 11.1 SDK Architecture (Per Language)

Each SDK provides:
1. **Wire format library**: Serialize/deserialize events and outputs (binary format)
2. **Transport client**: Connect to Aeon via T3 or T4
3. **Processor abstraction**: User implements a function/interface
4. **Registration**: Auto-register with Aeon on startup

### 11.2 SDK Template (Pseudocode)

```python
# Python SDK (T3/T4)
import aeon_sdk

@aeon_sdk.processor(name="my-enricher", version="1.0.0")
def process(event: aeon_sdk.Event) -> list[aeon_sdk.Output]:
    enriched = enrich(event.payload)
    return [aeon_sdk.Output(destination="enriched-topic", payload=enriched)]

if __name__ == "__main__":
    aeon_sdk.run(
        endpoint="https://aeon-host:4472",
        transport="webtransport",  # or "websocket"
        partitions=[0, 1, 2, 3],
    )
```

### 11.3 Per-Language Processor Development Approaches

Each language has different deployment models and tier options. This section details the
recommended approach per language, considering ecosystem maturity, deployment patterns,
and performance characteristics.

#### Rust
- **T1 Native (.so/.dll)**: `aeon-native-sdk`, `export_processor!` macro → `cargo build --release`. Maximum performance (240ns/event). Direct function call, zero serialization overhead. **Already implemented.**
- **T2 Wasm**: `aeon-wasm-sdk`, `aeon_processor!` macro → `cargo build --target wasm32-unknown-unknown`. Sandboxed, fuel-metered. **Already implemented.**
- **T3/T4**: Possible but rarely needed (T1/T2 are strictly better for Rust).

#### C / C++
- **T1 Native (.so/.dll)**: Compile to shared library with C-ABI exports (`aeon_processor_create`, `aeon_process`, etc.). Link against header-only SDK. Matches native Rust performance.
- **T2 Wasm**: Compile via Emscripten or `wasi-sdk` → `.wasm`. Sandboxed execution. Good for untrusted C/C++ code.
- **T3 WebTransport**: `libquiche` (Cloudflare) or `ngtcp2` + `nghttp3`. Full HTTP/3 stack. Useful for existing C/C++ services that need to integrate as processors.
- **T4 WebSocket**: `libwebsockets` or any WS library. Universal fallback.
- **Recommended**: T1 for maximum performance, T2 for sandboxing, T3/T4 for existing services.

#### Go
- **T2 Wasm (TinyGo)**: `tinygo build -target wasm` → `.wasm`. Limited stdlib, smaller binary. Good for simple processors. GC pauses possible.
- **T3 WebTransport (recommended)**: `quic-go` library. Full HTTP/3 support. Native Go concurrency model. Best for Go developers — full stdlib, goroutines, channels.
- **T4 WebSocket**: `gorilla/websocket` or `nhooyr.io/websocket`. Fallback option.
- **Recommended**: T3 for production Go processors.

#### Java / Kotlin (JVM)
- **T3 WebTransport (recommended)**: Netty QUIC (`netty-incubator-codec-quic`). High-performance NIO. Mature ecosystem. Spring Boot integration possible.
- **T4 WebSocket**: `javax.websocket` (Jakarta EE), Jetty WebSocket, or Spring WebSocket. Well-supported fallback.
- **Deployment models**: Fat JAR (`java -jar processor.jar`), Docker container, K8s pod. Long-running JVM process connects to Aeon.
- **Recommended**: T3 for new projects, T4 for environments with HTTP/3 restrictions.

#### C# / .NET
- **T1 Native (NativeAOT, .NET 8+)**: `dotnet publish -r linux-x64 --self-contained -p:PublishAot=true` produces a native shared library. Exposes C-ABI exports via `[UnmanagedCallersOnly]`. **Unique capability** — C# can target T1 for near-native performance.
- **T3 WebTransport (recommended)**: `System.Net.Quic` (.NET 7+, built on `msquic`). First-class HTTP/3 support in the runtime. `HttpClient` with `HTTP/3` version.
- **T4 WebSocket**: `System.Net.WebSockets.ClientWebSocket`. Built into the runtime, no external dependencies.
- **Deployment models**: Self-contained executable, Docker container, Azure Container Apps, K8s pod. `dotnet run` or published binary.
- **Recommended**: T3 for most .NET projects. T1 (NativeAOT) for performance-critical paths.

#### Node.js / TypeScript / JavaScript
- **T2 Wasm (AssemblyScript)**: AssemblyScript compiles to Wasm directly. TypeScript-like syntax. **Already implemented** (Phase 12a).
- **T4 WebSocket (recommended for runtime Node.js)**: `ws` package (de facto standard). Native `WebSocket` in browsers. Event-loop friendly. Best for processors that leverage npm ecosystem (ML libraries, data transformation, API integrations).
- **T3 WebTransport**: Experimental in Node.js (`--experimental-quic` flag). Not production-ready. Browser `WebTransport` API is stable but runs in browser context, not server.
- **Deployment models**: `node processor.js`, Docker container, serverless adapter possible.
- **Recommended**: T2 (AssemblyScript) for Wasm path, T4 for runtime Node.js with npm ecosystem.

#### PHP
- **T4 WebSocket (only viable tier)**: PHP has no production HTTP/3 client library (T3 not available).
- **Deployment model 1 — Swoole/OpenSwoole (recommended)**: Coroutine-based async PHP runtime. Long-running process with built-in WebSocket client. Best performance. `swoole_websocket` connects to Aeon, processes events in coroutines.
- **Deployment model 2 — ReactPHP**: Event-loop based async PHP. `ratchet/pawl` WebSocket client. Good for developers familiar with ReactPHP ecosystem.
- **Deployment model 3 — Amphp**: Fiber-based async PHP (PHP 8.1+). `amphp/websocket-client`. Modern async patterns.
- **Deployment model 4 — Laravel Octane (Swoole/RoadRunner)**: Laravel application running on Swoole. Processor logic in Laravel service class. Familiar framework for Laravel developers.
- **NOT suitable**: Traditional PHP-FPM (request-response model, no persistent connections). Processor requires long-running process.
- **Recommended**: Swoole for maximum PHP performance, ReactPHP/Amphp for async-native, Laravel Octane for Laravel shops.

#### Ruby
- **T4 WebSocket**: `faye-websocket` (EventMachine), `websocket-client-simple`, or `async-websocket` (Socketry).
- **Deployment models**: Long-running Ruby process (`ruby processor.rb`), Docker container.
- **Recommended**: T4 with `faye-websocket` or `async-websocket`.

#### Swift
- **T3 WebTransport (recommended)**: `Network.framework` (Apple, built-in QUIC support on macOS/iOS). First-party support.
- **T4 WebSocket**: `URLSessionWebSocketTask` (Foundation) or `Starscream`.
- **Deployment**: Linux server (Swift on Linux) or macOS. Docker container with Swift runtime.

#### Erlang / Elixir
- **T4 WebSocket (recommended)**: `:gun` (Erlang HTTP client with WS), `WebSockex` (Elixir). BEAM VM concurrency model handles multiple pipeline streams naturally.
- **T3**: `quicer` (Erlang NIF wrapping `msquic`). Exists but less mature.
- **Deployment**: OTP release, Docker container. BEAM VM is naturally long-running.

#### Scala
- **T3 WebTransport**: Via Netty QUIC (same as Java). `http4s` integration possible.
- **T4 WebSocket**: `http4s-client`, Akka HTTP, or Netty WebSocket.
- **Recommended**: T3 via Netty QUIC for JVM Scala.

#### Haskell
- **T4 WebSocket**: `websockets` package (Hackage). Well-maintained.
- **T3**: No mature QUIC client library yet.
- **Recommended**: T4.

### 11.4 SDK Delivery Plan

| Priority | Language | Tier(s) | SDK Package | Key Dependencies | Deployment Model |
|----------|----------|---------|-------------|-----------------|-----------------|
| P0 | Rust | T1/T2 | `aeon-native-sdk`, `aeon-wasm-sdk` | — | Already implemented |
| P0 | AssemblyScript | T2 | `sdks/typescript/` | `assemblyscript` | Already implemented |
| P1 | Python | T3 + T4 | `aeon-sdk-python` (PyPI) | `aioquic`, `websockets`, `ed25519` | `python processor.py`, Docker |
| P1 | Go | T3 | `aeon-sdk-go` (Go module) | `quic-go`, `ed25519` | `go run`, Docker |
| P2 | Java/Kotlin | T3 + T4 | `aeon-sdk-java` (Maven) | Netty QUIC, `java.security` (EdDSA) | Fat JAR, Docker, K8s |
| P2 | C# / .NET | T1 + T3 + T4 | `Aeon.Sdk` (NuGet) | `System.Net.Quic`, NativeAOT | `dotnet run`, Docker, NativeAOT .so |
| P2 | Node.js / TypeScript | T4 | `@aeon/sdk` (npm) | `ws`, `tweetnacl` (ED25519) | `node processor.js`, Docker |
| P2 | C / C++ | T1 + T3 | `aeon-sdk-c` (header-only) | `libquiche` or `ngtcp2` | Shared .so (T1), Docker (T3) |
| P3 | PHP | T4 | `aeon/sdk` (Composer) | Swoole / ReactPHP / Amphp | Swoole, Docker |
| P3 | Ruby | T4 | `aeon-sdk` (RubyGems) | `faye-websocket` | `ruby processor.rb`, Docker |
| P3 | Swift | T3 | `AeonSDK` (SPM) | `Network.framework` | Linux binary, Docker |
| P3 | Elixir | T4 | `aeon_sdk` (Hex) | `WebSockex`, `:gun` | OTP release, Docker |
| P3 | Scala | T3 | `aeon-sdk-scala` (Maven) | Netty QUIC | Fat JAR, Docker |
| P4 | Haskell | T4 | `aeon-sdk` (Hackage) | `websockets` | Binary, Docker |
| P4 | Others | T4 | Community-driven | Any WebSocket client | Varies |

### 11.5 SDK Directory Structure

```
sdks/
├── python/          # Existing (wire format) + T3/T4 transport
├── go/              # NEW
├── java/            # NEW (Java + Kotlin JVM)
├── dotnet/          # NEW (C# + F#, NativeAOT support)
├── nodejs/          # NEW (Node.js + TypeScript runtime)
├── c/               # NEW (header-only, T1 + T3)
├── php/             # NEW (Swoole + ReactPHP + Amphp examples)
├── ruby/            # NEW
├── swift/           # NEW
├── elixir/          # NEW
└── typescript/      # Existing (AssemblyScript Wasm SDK)
```

---

## 12. Registry & CLI Changes

### 12.1 Registry Model Extension

Current model: **Artifact-centric** (upload .wasm or .so, store on disk, load on demand).

New model: **Dual-mode** — artifact-centric for T1/T2, endpoint-centric for T3/T4.

```rust
pub struct ProcessorVersion {
    // Existing fields (T1/T2):
    pub version: String,
    pub sha512: String,           // Hash of artifact (T1/T2) or empty (T3/T4)
    pub size_bytes: u64,          // Artifact size (T1/T2) or 0 (T3/T4)
    pub processor_type: ProcessorType,
    pub platform: String,
    pub status: VersionStatus,
    pub registered_at: i64,
    pub registered_by: String,

    // New fields (T3/T4):
    pub endpoint: Option<String>,       // Connection URL
    pub max_batch_size: Option<u32>,    // Negotiated batch size
    pub instance_count: Option<u32>,    // Current connected instances
}
```

### 12.2 CLI Registration Commands

```bash
# T1 — Native (existing)
aeon processor register my-proc --version 1.0.0 \
  --artifact my-proc.so --runtime native

# T2 — Wasm (existing)
aeon processor register my-proc --version 1.0.0 \
  --artifact my-proc.wasm --runtime wasm

# T3 — WebTransport (new)
aeon processor register my-proc --version 1.0.0 \
  --runtime webtransport --endpoint https://processor-host:4472

# T4 — WebSocket (new)
aeon processor register my-proc --version 1.0.0 \
  --runtime websocket --endpoint ws://processor-host:8080/process

# Auto-detect (existing behavior for T1/T2, new flag for T3/T4)
aeon processor register my-proc --version 1.0.0 --artifact my-proc.wasm
# → auto-detects Wasm from extension

aeon processor register my-proc --version 1.0.0 --endpoint wss://...
# → auto-detects WebSocket from URL scheme (ws:// or wss://)

aeon processor register my-proc --version 1.0.0 --endpoint https://...
# → auto-detects WebTransport from URL scheme (https://)
```

### 12.3 CLI Identity Management (ED25519)

```bash
# Register a processor's ED25519 public key
aeon processor identity add my-enricher \
  --public-key ed25519:MCowBQYDK2VwAyEA... \
  --pipelines orders-pipeline,payments-pipeline \
  --max-instances 4
# → Registered identity for my-enricher (fingerprint: SHA256:abc123...)

# List registered identities
aeon processor identity list my-enricher
# → KEY FINGERPRINT           PIPELINES                    MAX  STATUS   REGISTERED
# → SHA256:abc123...          orders,payments              4    active   2026-04-05
# → SHA256:def456...          analytics                    2    active   2026-04-01

# Revoke an identity (immediate, all connections using this key are terminated)
aeon processor identity revoke my-enricher --fingerprint SHA256:abc123...
# → Revoked identity SHA256:abc123... for my-enricher (2 active connections terminated)

# Show connected instances for an identity
aeon processor instances my-enricher
# → INSTANCE   TRANSPORT      KEY FINGERPRINT     PIPELINE          PARTITIONS  UPTIME
# → inst-001   webtransport   SHA256:abc123...    orders-pipeline   [0,1,2,3]   2h 15m
# → inst-002   websocket      SHA256:def456...    analytics         [0,1]        45m
```

### 12.4 CLI Pipeline Configuration with Binding

```yaml
# pipeline.yaml — dedicated binding (default)
apiVersion: aeon/v1
kind: Pipeline
metadata:
  name: orders-pipeline
spec:
  source:
    type: kafka
    topic: orders
    partitions: [0, 1, 2, 3]
  processor:
    name: enricher
    version: "1.0.0"
    binding: dedicated            # One processor instance for this pipeline only
  sink:
    type: kafka
    topic: enriched-orders

---
# Shared binding — reuse processor across pipelines
apiVersion: aeon/v1
kind: Pipeline
metadata:
  name: payments-pipeline
spec:
  source:
    type: kafka
    topic: payments
  processor:
    name: enricher
    version: "1.0.0"
    binding: shared
    group: finance-enrichers      # Can share with other pipelines in this group
    connection:
      batch_size: 256
      timeout_ms: 5000
  sink:
    type: kafka
    topic: enriched-payments

---
apiVersion: aeon/v1
kind: Pipeline
metadata:
  name: invoices-pipeline
spec:
  source:
    type: kafka
    topic: invoices
  processor:
    name: enricher
    version: "1.0.0"
    binding: shared
    group: finance-enrichers      # Same group → can share instance with payments
  sink:
    type: kafka
    topic: enriched-invoices
```

```bash
# Apply all pipelines
aeon apply -f pipeline.yaml
```

---

## 13. REST API Changes

### 13.1 New/Modified Endpoints

#### Register T3/T4 Processor
```
POST /api/v1/processors
Content-Type: application/json

{
    "name": "my-enricher",
    "description": "JSON enrichment processor",
    "version": {
        "version": "1.0.0",
        "processor_type": "webtransport",
        "endpoint": "https://processor-host:4472",
        "max_batch_size": 1024,
        "platform": "any",
        "status": "Available",
        "registered_by": "cli"
    }
}
```

#### Register Processor Identity (ED25519 Public Key)
```
POST /api/v1/processors/:name/identities
Content-Type: application/json
Authorization: Bearer <admin_token>

{
    "public_key": "ed25519:MCowBQYDK2VwAyEA...",
    "allowed_pipelines": { "named": ["orders-pipeline", "payments-pipeline"] },
    "max_instances": 4
}

→ 201 Created
{
    "fingerprint": "SHA256:abc123...",
    "processor_name": "my-enricher",
    "registered_at": 1712345678000,
    "status": "active"
}
```

#### List Processor Identities
```
GET /api/v1/processors/:name/identities
Authorization: Bearer <admin_token>

→ 200 OK
{
    "processor": "my-enricher",
    "identities": [
        {
            "fingerprint": "SHA256:abc123...",
            "public_key": "ed25519:MCowBQYDK2VwAyEA...",
            "allowed_pipelines": { "named": ["orders-pipeline", "payments-pipeline"] },
            "max_instances": 4,
            "registered_at": 1712345678000,
            "registered_by": "admin",
            "revoked_at": null,
            "active_connections": 2
        }
    ]
}
```

#### Revoke Processor Identity
```
DELETE /api/v1/processors/:name/identities/:fingerprint
Authorization: Bearer <admin_token>

→ 200 OK
{
    "fingerprint": "SHA256:abc123...",
    "revoked_at": 1712345999000,
    "connections_terminated": 2
}
```

Active connections using the revoked key are terminated immediately with a
`KEY_REVOKED` error on the control stream.

#### Processor Instance Status (Connected T3/T4 Instances)
```
GET /api/v1/processors/:name/instances
Authorization: Bearer <admin_token>

→ 200 OK
{
    "name": "my-enricher",
    "instances": [
        {
            "id": "inst-001",
            "session_id": "sess-abc123",
            "transport": "webtransport",
            "remote_addr": "10.0.1.5:38291",
            "key_fingerprint": "SHA256:abc123...",
            "binding": "dedicated",
            "pipelines": [
                {
                    "name": "orders-pipeline",
                    "assigned_partitions": [0, 1, 2, 3]
                }
            ],
            "connected_at": "2026-04-05T10:30:00Z",
            "batches_processed": 152847,
            "last_heartbeat": "2026-04-05T10:35:12Z",
            "health": "healthy",
            "batch_signing": true
        }
    ]
}
```

#### WebSocket Processor Connection Endpoint (T4, on existing REST API port)
```
GET /api/v1/processors/connect
Upgrade: websocket
Connection: Upgrade

→ 101 Switching Protocols
→ AWPP challenge-response handshake over WebSocket (see Section 8.2)
```

Note: This endpoint does NOT use Bearer token auth — authentication happens via
ED25519 challenge-response within the AWPP protocol after the WebSocket upgrade.
The upgrade itself is unauthenticated; the AWPP handshake provides authentication.

---

## 14. Observability & Monitoring

### 14.1 New Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `aeon_processor_transport_call_duration_us` | Histogram | `tier`, `processor`, `partition` | Time for `call_batch()` round-trip |
| `aeon_processor_transport_batch_size` | Histogram | `tier`, `processor` | Events per batch sent to processor |
| `aeon_processor_instances_connected` | Gauge | `processor`, `tier` | Current connected T3/T4 instances |
| `aeon_processor_transport_errors_total` | Counter | `tier`, `processor`, `error_type` | Transport-level errors |
| `aeon_processor_heartbeat_age_ms` | Gauge | `processor`, `instance` | Time since last heartbeat |
| `aeon_processor_transport_bytes_sent` | Counter | `tier`, `processor` | Bytes sent to processor |
| `aeon_processor_transport_bytes_received` | Counter | `tier`, `processor` | Bytes received from processor |

### 14.2 New Trace Spans

- `processor.transport.call_batch` — wraps the entire batch call
- `processor.transport.serialize` — event serialization time
- `processor.transport.network` — network round-trip (T3/T4 only)
- `processor.transport.deserialize` — output deserialization time
- `processor.transport.identity_stamp` — event identity propagation

### 14.3 Log Events

- `processor.connected` — T3/T4 processor instance connected
- `processor.disconnected` — T3/T4 processor instance disconnected (with reason)
- `processor.heartbeat_missed` — heartbeat timeout exceeded
- `processor.drain_started` — drain signal sent
- `processor.drain_completed` — processor confirmed drain

---

## 15. Security Model

### 15.1 Why Processors Need Aeon-Managed RBAC

Source and sink connectors delegate access control to external systems — Kafka ACLs,
Redis AUTH, NATS permissions, etc. Aeon does not manage these; the external system does.

**Processors are fundamentally different.** For T3/T4, the processor connects directly
to Aeon over the network. Aeon is the server, the trust boundary, and the data custodian.
There is no external system to delegate to. Without RBAC:

- Any process on the network could connect to port 4472 and claim to be any processor
- A rogue processor could receive events from any pipeline (potentially PII, financial data)
- A compromised processor could inject malicious outputs into any sink
- No audit trail of which processor instance produced which outputs

**Aeon must own processor RBAC** for T3/T4 because it is the only entity in the data path
that can enforce it.

### 15.2 Four-Layer Security Model

```
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 1: Transport Security                   │
│  TLS 1.3 (mandatory for T3/T4). No plaintext processor data.   │
│  T3: QUIC mandates TLS. T4: WSS required in production.        │
├─────────────────────────────────────────────────────────────────┤
│                    Layer 2: ED25519 Authentication               │
│  Challenge-response. Each processor instance holds a private    │
│  key; public key registered in Aeon. Proves identity without    │
│  shared secrets. Non-repudiable. MANDATORY for T3/T4.          │
├─────────────────────────────────────────────────────────────────┤
│                Layer 2.5: OAuth 2.0 Token (Optional)            │
│  Client Credentials Grant (M2M). Processor obtains JWT from    │
│  organization's IdP (Keycloak, Auth0, Okta, Azure AD).         │
│  Aeon verifies JWT signature via JWKS. Adds defense-in-depth:  │
│  ED25519 key theft alone is insufficient without valid token.   │
├─────────────────────────────────────────────────────────────────┤
│                    Layer 3: Authorization                        │
│  Per-processor pipeline scoping. ProcessorIdentity defines      │
│  which pipelines and how many instances are allowed. Enforced   │
│  at connection time and continuously via heartbeat.             │
└─────────────────────────────────────────────────────────────────┘
```

When OAuth is enabled, **both** Layer 2 and Layer 2.5 must pass. Either alone is insufficient.
ED25519 provides per-batch non-repudiation (OAuth cannot do this — tokens authenticate
sessions, not data). OAuth provides identity federation with the organization's existing IAM,
auto-expiring tokens, and an independent audit stream via the IdP.

### 15.3 Per-Tier Security Comparison

| Concern | T1 Native | T2 Wasm | T3 WebTransport | T4 WebSocket |
|---------|-----------|---------|-----------------|--------------|
| **Code execution** | Full process access | Wasm sandbox (memory, fuel) | OS process isolation | OS process isolation |
| **Memory safety** | Rust guarantees (unsafe possible) | Wasm memory model | Separate address space | Separate address space |
| **Resource limits** | None (same process) | Fuel metering, memory cap | OS-level (cgroups, ulimits) | OS-level |
| **Authentication** | SHA-256 artifact hash | SHA-256 artifact hash | **ED25519 challenge-response** | **ED25519 challenge-response** |
| **Identity federation** | N/A | N/A | **OAuth 2.0 Client Credentials (optional)** | **OAuth 2.0 Client Credentials (optional)** |
| **Encryption** | N/A (in-process) | N/A (in-process) | QUIC TLS 1.3 (mandatory) | WSS (TLS required in production) |
| **Authorization** | Artifact registry ACL | Artifact registry ACL | **Pipeline-scoped identity** | **Pipeline-scoped identity** |
| **Non-repudiation** | N/A | N/A | **ED25519 per-batch signing** | **ED25519 per-batch signing** |
| **Audit trail** | Load/unload events | Load/unload events | **Per-batch signed + IdP audit** | **Per-batch signed + IdP audit** |

### 15.4 ED25519 Processor Identity

#### Why ED25519 Over Bearer Tokens

| Property | Bearer Token | ED25519 Keypair |
|----------|-------------|-----------------|
| Identity proof | Shared secret (copyable) | Cryptographic (only key holder can sign) |
| Non-repudiation | No — token can be shared | Yes — signed outputs prove which processor produced them |
| Revocation | Rotate token → all instances affected | Revoke one key → only that instance affected |
| Per-batch integrity | Not practical | Every batch response signed (~60μs overhead) |
| Multi-instance | All share same token | Each instance can have its own keypair |
| Credential theft | Token = full impersonation | Private key theft = one instance, revoke immediately |

ED25519 was chosen for:
- **Speed**: Sign ~60μs, verify ~150μs — negligible per-batch overhead
- **Small keys**: 32-byte public key, 64-byte signature
- **Wide language support**: Libraries exist in Go, Python, Java, C#, Node.js, PHP, Ruby, etc.
- **Existing infrastructure**: `aeon-crypto` crate already provides signing primitives

#### Keypair Lifecycle

```
1. GENERATE (processor-side, one-time per instance)
   ┌─────────────────────────────────────────────────────────────┐
   │  Processor generates ED25519 keypair.                       │
   │  Private key → stored securely (env var, secrets manager,   │
   │                Vault, K8s Secret, HSM)                      │
   │  Public key → exported as base64 for registration           │
   └─────────────────────────────────────────────────────────────┘

2. REGISTER (admin action via CLI or REST API)
   ┌─────────────────────────────────────────────────────────────┐
   │  aeon processor identity add my-enricher \                  │
   │    --public-key ed25519:MCowBQYDK2VwAyEA... \              │
   │    --pipelines orders-pipeline,payments-pipeline \           │
   │    --max-instances 4                                        │
   │                                                             │
   │  Aeon stores: ProcessorIdentity {                           │
   │    public_key, fingerprint, processor_name,                 │
   │    allowed_pipelines, max_instances, ...                    │
   │  }                                                          │
   │  Replicated via Raft to all cluster nodes.                  │
   └─────────────────────────────────────────────────────────────┘

3. AUTHENTICATE (per connection, challenge-response)
   ┌─────────────────────────────────────────────────────────────┐
   │  Aeon → Processor: { "nonce": "<random_32_bytes_hex>" }     │
   │  Processor → Aeon: { "public_key": "...",                   │
   │                       "challenge_signature":                 │
   │                         ED25519_Sign(privkey, nonce) }       │
   │  Aeon: ED25519_Verify(registered_pubkey, nonce, sig)        │
   │  → Accept or reject                                         │
   └─────────────────────────────────────────────────────────────┘

4. SIGN (per batch, continuous during processing)
   ┌─────────────────────────────────────────────────────────────┐
   │  Processor signs every batch response:                      │
   │  signature = ED25519_Sign(privkey, batch_bytes)             │
   │  Aeon verifies before accepting outputs into pipeline.      │
   │                                                             │
   │  Cost: ~0.21μs per event at batch size 1024 (<4% overhead) │
   └─────────────────────────────────────────────────────────────┘

5. REVOKE (admin action, immediate effect)
   ┌─────────────────────────────────────────────────────────────┐
   │  aeon processor identity revoke my-enricher \               │
   │    --fingerprint SHA256:abc123...                            │
   │                                                             │
   │  All active connections using this key are terminated with   │
   │  KEY_REVOKED error. The key cannot be used for new          │
   │  connections. Revocation is Raft-replicated.                 │
   └─────────────────────────────────────────────────────────────┘
```

### 15.5 Pipeline Isolation (Dedicated vs Shared Binding)

#### Dedicated Binding (Default)

```
Pipeline A ──▶ Processor Instance 1 (exclusive) ──▶ Pipeline A Sink
Pipeline B ──▶ Processor Instance 2 (exclusive) ──▶ Pipeline B Sink
```

- One processor instance serves exactly one pipeline
- No cross-pipeline data exposure by design
- Failure in one pipeline cannot affect another
- Independent scaling, upgrades, and monitoring per pipeline
- **Recommended for**: sensitive data, regulatory compliance, independent teams

#### Shared Binding (Opt-in, Group-Based)

```
Pipeline A ──┐                                 ┌──▶ Pipeline A Sink
             ├──▶ Processor Instance 1 ────────┤
Pipeline B ──┘    (shared, group: "finance")   └──▶ Pipeline B Sink
```

Even in shared mode, strict isolation holds:

1. **Separate data streams per pipeline**: Events from pipeline A never appear on
   pipeline B's data stream (T3: separate QUIC streams; T4: tagged frames with
   pipeline name verified by Aeon)
2. **Output routing enforcement**: Aeon validates that outputs from a pipeline's
   stream are routed only to that pipeline's sink. The processor cannot redirect outputs.
3. **Per-pipeline metrics**: `events_processed`, `latency`, `errors` tracked per pipeline
   even in shared mode
4. **Per-pipeline backpressure**: Slow processing on pipeline A does not block pipeline B
   (T3: independent QUIC streams; T4: application-level per-pipeline tracking)
5. **Per-batch audit**: Every batch carries `pipeline_name` + `session_id` + ED25519
   signature — fully traceable to specific pipeline and processor instance

**Shared binding is suitable for**: same-team pipelines with similar data sensitivity,
cost optimization, stateless enrichment processors that benefit from shared warm caches.

#### Comparison

| Aspect | Dedicated | Shared |
|--------|-----------|--------|
| Data isolation | Physical (separate process) | Logical (separate streams, Aeon-enforced) |
| Failure blast radius | One pipeline | All pipelines in group |
| Resource efficiency | Higher (one process per pipeline) | Lower (one process serves N pipelines) |
| Upgrade impact | One pipeline | All pipelines in group |
| Operational tracking | Simple 1:1 mapping | Group-aware monitoring |
| Max ROI | Lower | Higher (processor reuse) |
| Compliance suitability | Strong (physical separation) | Acceptable (logical separation with audit) |

### 15.6 Network Security Requirements

- **T3 (WebTransport)**: QUIC mandates TLS 1.3 — encryption is always on. No configuration needed.
- **T4 (WebSocket)**: Production deployments MUST use `wss://` (TLS). Plain `ws://` is rejected
  unless `AEON_ALLOW_INSECURE_WS=true` is set (dev mode only, logs warning at startup).
- **Bind interface**: Processor listener binds to a configurable interface
  (default: `127.0.0.1` for local-only). Set `AEON_PROCESSOR_BIND=0.0.0.0` for network access.
- **Firewall**: Port 4472 (T3 QUIC/UDP) should be restricted to the processor network.
  T4 shares port 4471 with the REST API.
- **Rate limiting**: Maximum 10 connection attempts per second per source IP (configurable).
  Prevents connection storms from misconfigured or malicious clients.

### 15.7 Complete Authentication Flow

```
Processor                     Identity Provider          Aeon
─────────                     ─────────────────          ────
   │                                │                      │
   │── POST /token ────────────────▶│                      │  (OAuth, if enabled)
   │   grant_type=client_credentials│                      │  M2M token request
   │   client_id=enricher-prod-01   │                      │
   │   client_secret=<secret>       │                      │
   │◀── { access_token: "eyJ..." }─│                      │
   │                                │                      │
   │──── TLS Handshake (HTTPS) ────────────────────────────▶│  Layer 1: Transport
   │     (T3: QUIC TLS 1.3, T4: WSS)                       │  Encryption established
   │◀─── TLS Established ──────────────────────────────────│
   │                                                        │
   │◀─── Challenge ────────────────────────────────────────│  Layer 2: ED25519
   │     { "type": "challenge",                             │  Random 32-byte nonce
   │       "nonce": "a1b2c3...",                            │
   │       "oauth_required": true }                         │
   │                                                        │
   │──── Register + Proof ─────────────────────────────────▶│
   │     { "name": "enricher",                              │
   │       "public_key": "ed25519:...",                     │
   │       "challenge_signature": "<sig>",                  │
   │       "oauth_token": "eyJ...",                         │  Layer 2.5: OAuth
   │       "requested_pipelines": ["orders"] }              │  (if enabled)
   │                                                        │
   │                                │                       │  Aeon Validates:
   │                                │◀── JWKS fetch ───────│  (cached, refresh
   │                                │──── public keys ─────▶│   on kid miss)
   │                                │                       │
   │                                                        │  Layer 2: ED25519
   │                                                        │  ✓ Key registered for "enricher"
   │                                                        │  ✓ ED25519 signature valid
   │                                                        │  ✓ Key not revoked
   │                                                        │
   │                                                        │  Layer 2.5: OAuth
   │                                                        │  ✓ JWT signature valid (JWKS)
   │                                                        │  ✓ Token not expired
   │                                                        │  ✓ Issuer matches config
   │                                                        │  ✓ Audience = "aeon-processors"
   │                                                        │  ✓ client_id ↔ processor name
   │                                                        │
   │                                                        │  Layer 3: Authorization
   │                                                        │  ✓ "orders" ∈ allowed_pipelines
   │                                                        │  ✓ instances < max_instances
   │                                                        │
   │◀─── Accepted ─────────────────────────────────────────│
   │     { "session_id": "sess-abc",                        │
   │       "pipelines": [{ "name": "orders",               │
   │         "partitions": [0,1,2,3] }] }                   │
   │                                                        │
   │═══════ Data streams open ═════════════════════════════│  Processing begins
   │                                                        │
   │     Per batch:                                         │
   │◀─── Batch Request ────────────────────────────────────│
   │──── Signed Batch Response ────────────────────────────▶│  ED25519 signature
   │     [outputs + 64B signature]                          │  verified before accept
   │                                                        │
   │◀─── Heartbeat (every 5s) ─────────────────────────────│  Health monitoring
   │──── Heartbeat ────────────────────────────────────────▶│  (includes token
   │                                                        │   expiry check)
   │                                                        │
   │     On token expiry:                                   │
   │── POST /token (refresh) ──────▶│                       │
   │◀── { access_token: "new..." }─│                       │
   │──── Token Refresh ────────────────────────────────────▶│  New JWT on
   │     { "type": "token_refresh",                         │  control stream
   │       "oauth_token": "new..." }                        │
   │◀─── Token Accepted ──────────────────────────────────│
   │                                                        │
   │◀─── Drain ────────────────────────────────────────────│  Graceful shutdown
   │──── Drain ACK ────────────────────────────────────────▶│
   │     Connection closes                                  │
```

### 15.8 OAuth 2.0 Client Credentials (Optional Layer 2.5)

OAuth 2.0 is an **optional, configurable** addition to the mandatory ED25519 authentication.
It is designed for organizations that want to integrate processor identity with their
existing Identity Provider (IdP) infrastructure.

#### Why OAuth Complements ED25519 (Not Replaces)

| Concern | ED25519 Alone | ED25519 + OAuth |
|---------|--------------|-----------------|
| Key compromise | Attacker can connect until key is manually revoked | Attacker also needs valid OAuth token from IdP |
| Identity federation | Aeon-only identity silo | Integrates with org's existing IAM (SSO, audit logs) |
| Credential rotation | Manual (CLI revoke + register) | OAuth tokens auto-expire (15min-1hr); key rotation is secondary defense |
| Audit trail | Aeon-internal only | IdP logs + Aeon logs (two independent audit streams) |
| Compliance | Self-managed PKI | Leverages org's certified IdP (SOC 2, ISO 27001) |
| Non-repudiation | ED25519 per-batch signing | Unchanged — OAuth authenticates sessions, ED25519 signs data |

**OAuth alone is NOT sufficient** — bearer tokens are copyable, have no per-batch signing,
and provide no cryptographic binding between identity and produced data. ED25519 remains
mandatory for non-repudiation.

#### M2M Flow: OAuth 2.0 Client Credentials Grant

This is machine-to-machine authentication. There is no human in the loop — no MFA, no
device binding, no browser redirects. The processor is a service that authenticates
programmatically.

```
POST /token HTTP/1.1
Host: auth.company.com
Content-Type: application/x-www-form-urlencoded

grant_type=client_credentials
&client_id=enricher-prod-01
&client_secret=<secret>
&scope=aeon:process
```

The IdP returns a JWT with claims that Aeon validates:

```json
{
    "iss": "https://auth.company.com/realms/aeon",
    "sub": "enricher-prod-01",
    "aud": "aeon-processors",
    "exp": 1712349278,
    "iat": 1712345678,
    "scope": "aeon:process",
    "aeon_processor": "my-enricher",
    "aeon_pipelines": ["orders-pipeline", "payments-pipeline"]
}
```

#### Aeon-Side Configuration

```yaml
# aeon-config.yaml
processor_auth:
  ed25519: required                    # Always required for T3/T4 (not configurable)
  oauth:
    enabled: false                     # Default: disabled (ED25519-only)
    issuer: "https://auth.company.com/realms/aeon"
    audience: "aeon-processors"
    jwks_uri: "https://auth.company.com/realms/aeon/protocol/openid-connect/certs"
    jwks_refresh_interval_secs: 300    # Re-fetch JWKS every 5 minutes
    token_expiry_grace_secs: 30        # Allow 30s grace period for token refresh
    claim_mapping:
      processor_name: "aeon_processor"     # JWT claim → processor name
      allowed_pipelines: "aeon_pipelines"  # JWT claim → pipeline scope (optional)
    required_scopes: ["aeon:process"]      # JWT must contain these scopes
```

#### What the IdP Can Enforce (M2M-Applicable)

- **IP restrictions**: Only allow token requests from specific CIDR ranges (processor network)
- **Client secret rotation**: Periodic rotation enforced by IdP policy
- **Scope restrictions**: Different client_ids get different scopes (e.g., `aeon:process` vs `aeon:admin`)
- **Token lifetime**: Short-lived tokens (15min-1hr) limit exposure window
- **Rate limiting**: IdP can limit token requests per client_id
- **Audit logging**: Every token issuance logged in IdP (who, when, from where, which scopes)

#### What Does NOT Apply (M2M Context)

- ~~MFA~~ — no human in the loop, processors are automated services
- ~~Device binding~~ — processors run on VMs/containers/pods, not user devices
- ~~Browser redirects~~ — no Authorization Code flow, only Client Credentials
- ~~Refresh tokens~~ — Client Credentials grant does not use refresh tokens; the processor
  requests a new access token when the current one approaches expiry

#### Token Refresh During Long-Lived Connections

T3/T4 processor connections are long-lived (hours/days). OAuth tokens expire (minutes/hours).
The AWPP protocol supports in-band token refresh without disconnection:

```json
// Processor → Aeon (on control stream, before current token expires)
{
    "type": "token_refresh",
    "oauth_token": "eyJ...<new_token>"
}

// Aeon → Processor
{
    "type": "token_accepted",
    "expires_at": 1712352878
}
```

If the processor fails to refresh before expiry (including grace period), Aeon sends a
`TOKEN_EXPIRED` error on the control stream. The processor has 10 seconds to provide a
fresh token or the connection is terminated. This prevents stale tokens from persisting
on long-lived connections.

#### Defense-in-Depth: Two Independent Secrets

The combination of ED25519 + OAuth means an attacker must compromise **two independent
systems** to impersonate a processor:

```
Factor 1: ED25519 private key    → stored in processor's environment
                                    (Vault, K8s Secret, env var, HSM)
Factor 2: OAuth client_secret    → stored in IdP
                                    (Keycloak, Auth0, Okta, Azure AD)

Both compromised → impersonation possible, but:
  - IdP audit log records token issuance (attacker IP, timestamp)
  - Aeon audit log records ED25519-signed batches (traceable)
  - Two independent revocation paths (revoke key OR revoke client)
```

---

## 16. Testing Strategy

### 16.1 Unit Tests

| Component | Test Scope |
|-----------|-----------|
| `ProcessorTransport` trait | Mock transport, verify call_batch/health/drain contract |
| `InProcessTransport` | Wrap PassthroughProcessor, verify async-sync bridge |
| Batch framing | Serialize/deserialize batch request/response, checksum validation |
| AWPP protocol | Registration handshake, heartbeat, drain, token refresh messages |
| OAuth JWT validation | Verify JWT signature (JWKS), expiry, issuer, audience, claim mapping |
| OAuth JWKS cache | JWKS fetch, cache, refresh on kid miss, graceful degradation on IdP unavailability |
| OAuth + ED25519 combined | Both must pass; ED25519-only accepted when OAuth disabled; rejection when either fails |
| Event identity propagation | Verify outputs carry correct source_event_id after transport |

### 16.2 Integration Tests

| Test | Description |
|------|-------------|
| T1 pipeline round-trip | Source → Native → Sink (existing, unchanged) |
| T2 pipeline round-trip | Source → Wasm → Sink (existing, unchanged) |
| T3 pipeline round-trip | Source → WebTransport → Sink (new) |
| T4 pipeline round-trip | Source → WebSocket → Sink (new) |
| Mixed-tier pipeline | Same pipeline definition, swap processor tier, verify identical output |
| Multi-instance T3 | 2+ processor instances, verify partition distribution |
| Processor disconnect/reconnect | Kill T3/T4 processor, verify reconnection and resume |
| OAuth token expiry + refresh | Token expires mid-session, processor refreshes via control stream, no disconnect |
| OAuth token expiry + no refresh | Token expires, processor fails to refresh, connection terminated after grace period |
| OAuth disabled mode | Processor connects with ED25519 only, no oauth_token field, accepted |
| Backpressure propagation | Slow T3/T4 processor, verify SPSC buffer fills, source pauses |

### 16.3 Benchmarks

| Benchmark | Metrics |
|-----------|---------|
| `transport_overhead_bench` | Per-tier `call_batch()` latency (in-process vs network) |
| `batch_framing_bench` | Batch serialize/deserialize throughput |
| `webtransport_throughput_bench` | T3 events/sec at various batch sizes |
| `websocket_throughput_bench` | T4 events/sec at various batch sizes |
| `tier_comparison_bench` | Side-by-side T1 vs T2 vs T3 vs T4 (same processor logic) |

---

## 17. Implementation Plan

### Phase 12b-1: Core Abstractions (~3-5 days)

**Scope**: Traits, types, in-process adapter, identity types. No network code yet.

| Task | Crate | Est. Lines |
|------|-------|-----------|
| Define `ProcessorTransport` trait | `aeon-types` | ~50 |
| Define `ProcessorHealth`, `ProcessorInfo`, `ProcessorTier` | `aeon-types` | ~40 |
| Define `ProcessorBinding`, `ProcessorConnectionConfig` | `aeon-types` | ~30 |
| Define `ProcessorIdentity`, `PipelineScope` | `aeon-types` | ~50 |
| Add `WebTransport`, `WebSocket` to `ProcessorType` enum | `aeon-types` | ~15 |
| Add `endpoint`, `max_batch_size` to `ProcessorVersion` | `aeon-types` | ~10 |
| Extend `ProcessorRef` with `binding` and `connection` | `aeon-types` | ~15 |
| Implement `InProcessTransport` | `aeon-engine` | ~80 |
| Refactor pipeline to use `ProcessorTransport` | `aeon-engine` | ~100 |
| Implement batch wire framing (serialize/deserialize) | `aeon-types` or `aeon-engine` | ~200 |
| Unit tests for all above | Various | ~300 |

**Gate**: All existing tests pass. T1/T2 pipelines work unchanged via `InProcessTransport`.

### Phase 12b-2: Security & AWPP Protocol (~4-6 days)

**Scope**: ED25519 identity management, challenge-response, per-batch signing, OAuth 2.0 Client Credentials (optional).

| Task | Crate | Est. Lines |
|------|-------|-----------|
| `ProcessorIdentityStore` (CRUD, Raft-replicated) | `aeon-engine` | ~200 |
| ED25519 challenge-response handler | `aeon-engine` | ~150 |
| Per-batch signature generation (SDK-side) | `aeon-native-sdk` (reference) | ~60 |
| Per-batch signature verification (host-side) | `aeon-engine` | ~80 |
| OAuth 2.0 JWT validator (JWKS fetch, cache, verify) | `aeon-engine` | ~200 |
| OAuth claim mapping + processor name correlation | `aeon-engine` | ~60 |
| AWPP token refresh handler (control stream) | `aeon-engine` | ~80 |
| OAuth configuration types (`OAuthConfig`) | `aeon-types` | ~40 |
| Identity management REST endpoints | `aeon-engine` | ~150 |
| Identity management CLI commands | `aeon-cli` | ~100 |
| `RegistryCommand` variants for identity CRUD | `aeon-types` | ~30 |
| Unit tests (challenge-response, signing, revocation, OAuth JWT, token refresh) | Various | ~400 |
| Feature flag `oauth` (gates `jsonwebtoken` dep) | `aeon-engine/Cargo.toml` | ~5 |

**Gate**: ED25519 identity register → challenge → verify → sign → revoke flow works end-to-end in tests. OAuth-enabled mode: JWT validation + ED25519 combined auth passes. OAuth-disabled mode: ED25519-only auth passes (backward compatible).

### Phase 12b-3: WebTransport Processor Host (~5-7 days)

**Scope**: T3 server-side implementation with full AWPP security.

| Task | Crate | Est. Lines |
|------|-------|-----------|
| `WebTransportProcessorHost` (QUIC listener, session mgmt) | `aeon-engine` | ~300 |
| `WebTransportTransport` (call_batch over streams) | `aeon-engine` | ~200 |
| AWPP control stream (challenge, registration, heartbeat, drain) | `aeon-engine` | ~200 |
| Per-pipeline stream isolation (dedicated and shared binding) | `aeon-engine` | ~150 |
| Partition assignment logic | `aeon-engine` | ~100 |
| Integration with pipeline | `aeon-engine` | ~50 |
| Integration tests (T3 loopback with ED25519 auth) | `aeon-engine/tests/` | ~250 |
| Feature flag `webtransport-processor` | `aeon-engine/Cargo.toml` | ~5 |

**Gate**: T3 loopback test passes — Rust client authenticates via ED25519, processes events with signed batches, returns outputs.

### Phase 12b-4: WebSocket Processor Host (~3-5 days)

**Scope**: T4 server-side implementation with AWPP security.

| Task | Crate | Est. Lines |
|------|-------|-----------|
| `WebSocketProcessorHost` (axum WS upgrade endpoint) | `aeon-engine` | ~250 |
| `WebSocketTransport` (call_batch over WS frames) | `aeon-engine` | ~150 |
| Pipeline+partition tagging on WS frames | `aeon-engine` | ~80 |
| Shared binding support (per-pipeline isolation on single WS) | `aeon-engine` | ~100 |
| Integration tests (T4 loopback with ED25519 auth) | `aeon-engine/tests/` | ~250 |
| Feature flag `websocket-processor` | `aeon-engine/Cargo.toml` | ~5 |

**Gate**: T4 loopback test passes with authentication and pipeline isolation.

### Phase 12b-5: Python SDK (~3-5 days)

**Scope**: First external-language SDK.

| Task | Location | Est. Lines |
|------|----------|-----------|
| Transport client (T3 via `aioquic`) | `sdks/python/` | ~200 |
| Transport client (T4 via `websockets`) | `sdks/python/` | ~150 |
| AWPP protocol handler | `sdks/python/` | ~100 |
| `@processor` decorator and `run()` entry point | `sdks/python/` | ~80 |
| Integration test: Python processor via T4 | `sdks/python/tests/` | ~100 |
| Documentation and examples | `sdks/python/` | ~100 |

**Gate**: Python processor connects to Aeon via WebSocket, processes events, outputs verified in sink.

### Phase 12b-6: Go SDK (~3-5 days)

**Scope**: Go SDK with T3 (WebTransport) as primary.

| Task | Location | Est. Lines |
|------|----------|-----------|
| Wire format library | `sdks/go/wire/` | ~300 |
| Transport client (T3 via `quic-go`) | `sdks/go/transport/` | ~250 |
| Processor interface and runner | `sdks/go/` | ~150 |
| Integration test | `sdks/go/` | ~100 |

**Gate**: Go processor connects via WebTransport, processes events end-to-end.

### Phase 12b-7: CLI, REST API, Registry Updates (~2-3 days)

| Task | Crate | Est. Lines |
|------|-------|-----------|
| `--runtime webtransport/websocket` and `--endpoint` flags | `aeon-cli` | ~80 |
| Endpoint-based registration in REST API | `aeon-engine` | ~100 |
| `/api/v1/processors/connect` WebSocket upgrade | `aeon-engine` | ~50 |
| `/api/v1/processors/:name/instances` endpoint | `aeon-engine` | ~80 |
| Registry dual-mode (artifact vs endpoint) | `aeon-engine` | ~60 |
| Pipeline YAML binding and connection config support | `aeon-cli` | ~60 |

### Phase 12b-8: Benchmarks & Hardening (~3-5 days)

| Task | Location |
|------|----------|
| `transport_overhead_bench` | `aeon-engine/benches/` |
| `tier_comparison_bench` | `aeon-engine/benches/` |
| `signing_overhead_bench` (ED25519 sign+verify per batch size) | `aeon-engine/benches/` |
| Reconnection logic (T3/T4 auto-reconnect with re-auth) | `aeon-engine` |
| Multi-instance load balancing | `aeon-engine` |
| 0-RTT for T3 reconnection | `aeon-engine` |
| Shared binding integration test (2 pipelines, 1 processor) | `aeon-engine/tests/` |
| Key rotation test (revoke old, register new, zero downtime) | `aeon-engine/tests/` |
| Documentation finalization | `docs/` |

### Phase 12b-9: Node.js / TypeScript SDK (~3-4 days)

**Scope**: Runtime Node.js processor via T4 WebSocket (complements existing T2 AssemblyScript Wasm path).

| Task | Location | Est. Lines |
|------|----------|-----------|
| Wire format library (binary encode/decode) | `sdks/nodejs/src/wire.ts` | ~200 |
| WebSocket transport client (`ws` package) | `sdks/nodejs/src/transport.ts` | ~150 |
| AWPP protocol handler (ED25519 via `tweetnacl`) | `sdks/nodejs/src/awpp.ts` | ~100 |
| `@processor` decorator and `run()` entry point | `sdks/nodejs/src/index.ts` | ~80 |
| Integration test: Node.js processor via T4 | `sdks/nodejs/tests/` | ~100 |
| npm package config + TypeScript types | `sdks/nodejs/package.json`, `tsconfig.json` | ~30 |

**Gate**: Node.js processor connects to Aeon via WebSocket, authenticates with ED25519, processes events, outputs verified in sink.

**Note**: Node.js developers have two paths: (1) AssemblyScript → T2 Wasm for maximum performance with TypeScript-like syntax, (2) Runtime Node.js → T4 WebSocket for full npm ecosystem access (ML libraries, API clients, etc.). The SDK documents both paths and helps developers choose.

### Phase 12b-10: Java / Kotlin SDK (~4-6 days)

**Scope**: JVM processor via T3 WebTransport (primary) + T4 WebSocket (fallback).

| Task | Location | Est. Lines |
|------|----------|-----------|
| Wire format library (ByteBuffer encode/decode) | `sdks/java/src/main/.../wire/` | ~300 |
| WebTransport transport client (Netty QUIC) | `sdks/java/src/main/.../transport/` | ~250 |
| WebSocket transport client (Netty WS) | `sdks/java/src/main/.../transport/` | ~150 |
| AWPP protocol handler (EdDSA via `java.security`) | `sdks/java/src/main/.../awpp/` | ~120 |
| `@Processor` annotation and runner | `sdks/java/src/main/.../` | ~100 |
| Kotlin coroutine adapter (suspend functions) | `sdks/java/src/main/.../kotlin/` | ~80 |
| Integration tests (T3 + T4) | `sdks/java/src/test/` | ~150 |
| Maven/Gradle build config | `sdks/java/pom.xml`, `build.gradle.kts` | ~50 |

**Gate**: Java processor connects via T3 WebTransport, authenticates, processes events. Kotlin processor uses coroutine adapter. T4 fallback tested.

**Deployment**: Fat JAR (`java -jar processor.jar`), Docker container, K8s pod. Spring Boot starter optional (Phase 12b-14+).

### Phase 12b-11: C# / .NET SDK (~4-6 days)

**Scope**: .NET processor via T3 WebTransport (primary) + T4 WebSocket + T1 NativeAOT (advanced).

| Task | Location | Est. Lines |
|------|----------|-----------|
| Wire format library (Span<byte> encode/decode) | `sdks/dotnet/src/Wire/` | ~250 |
| WebTransport transport client (`System.Net.Quic`) | `sdks/dotnet/src/Transport/` | ~200 |
| WebSocket transport client (`ClientWebSocket`) | `sdks/dotnet/src/Transport/` | ~150 |
| AWPP protocol handler (ED25519 via `System.Security.Cryptography`) | `sdks/dotnet/src/Awpp/` | ~120 |
| Processor interface and runner | `sdks/dotnet/src/` | ~100 |
| NativeAOT T1 export guide + example | `sdks/dotnet/examples/native-aot/` | ~80 |
| Integration tests (T3 + T4) | `sdks/dotnet/tests/` | ~150 |
| NuGet package config | `sdks/dotnet/Aeon.Sdk.csproj` | ~30 |

**Gate**: C# processor connects via T3, authenticates, processes events. T4 fallback tested. NativeAOT example compiles to .so and loads via T1 native loader.

**Unique**: C# is the only non-Rust language that can target T1 (NativeAOT → .so with C-ABI exports via `[UnmanagedCallersOnly]`). SDK includes documentation and example for this advanced path.

### Phase 12b-12: C / C++ SDK (~3-4 days)

**Scope**: Header-only T1 SDK (already partially covered by native-sdk ABI) + T3 network SDK.

| Task | Location | Est. Lines |
|------|----------|-----------|
| Header-only T1 SDK (`aeon_processor.h`) | `sdks/c/include/` | ~200 |
| Wire format C library (encode/decode) | `sdks/c/src/wire.c` | ~300 |
| T3 transport client (via `libquiche` or `ngtcp2`) | `sdks/c/src/transport.c` | ~250 |
| AWPP + ED25519 (via `libsodium` or `openssl`) | `sdks/c/src/awpp.c` | ~120 |
| CMake build system | `sdks/c/CMakeLists.txt` | ~40 |
| Example: T1 native processor (C) | `sdks/c/examples/native/` | ~60 |
| Example: T3 WebTransport processor (C++) | `sdks/c/examples/webtransport/` | ~80 |
| Integration test | `sdks/c/tests/` | ~100 |

**Gate**: C processor compiles to .so, loads via T1 native loader. C++ processor connects via T3 WebTransport.

### Phase 12b-13: PHP SDK (~3-4 days)

**Scope**: PHP processor via T4 WebSocket with multiple deployment model examples.

| Task | Location | Est. Lines |
|------|----------|-----------|
| Wire format library (pack/unpack binary) | `sdks/php/src/Wire.php` | ~200 |
| WebSocket transport client (abstract) | `sdks/php/src/Transport.php` | ~120 |
| Swoole WebSocket adapter | `sdks/php/src/Adapter/SwooleAdapter.php` | ~100 |
| ReactPHP WebSocket adapter | `sdks/php/src/Adapter/ReactPhpAdapter.php` | ~100 |
| Amphp WebSocket adapter | `sdks/php/src/Adapter/AmphpAdapter.php` | ~100 |
| AWPP + ED25519 (via `sodium_crypto_sign`) | `sdks/php/src/Awpp.php` | ~100 |
| Processor interface and runner | `sdks/php/src/Processor.php` | ~80 |
| Example: Swoole deployment | `sdks/php/examples/swoole/` | ~40 |
| Example: ReactPHP deployment | `sdks/php/examples/reactphp/` | ~40 |
| Example: Laravel Octane deployment | `sdks/php/examples/laravel-octane/` | ~60 |
| Integration test (Swoole) | `sdks/php/tests/` | ~100 |
| Composer config | `sdks/php/composer.json` | ~20 |

**Gate**: PHP processor connects to Aeon via Swoole WebSocket, authenticates with ED25519, processes events. ReactPHP and Amphp adapters tested. Laravel Octane example documented.

**Note**: Traditional PHP-FPM is NOT supported — processors require long-running persistent connections. Documentation explicitly states this and recommends Swoole as the primary deployment model.

### Phase 12b-14: Remaining SDKs — Swift, Elixir, Ruby (~3-5 days each, demand-driven)

These are P3/P4 priority and built on demand:

| Language | Tier | Key Dependency | Est. Days |
|----------|------|---------------|-----------|
| Swift | T3 | `Network.framework` (QUIC) | 3-4 |
| Elixir | T4 | `WebSockex` | 3-4 |
| Ruby | T4 | `faye-websocket` | 2-3 |
| Scala | T3 | Netty QUIC (shares Java SDK core) | 2-3 |
| Haskell | T4 | `websockets` (Hackage) | 2-3 |

**Gate per SDK**: Processor connects, authenticates ED25519, processes events end-to-end.

### Total Estimated Effort

| Phase | Scope | Duration | Cumulative |
|-------|-------|----------|-----------|
| 12b-1: Core abstractions | Traits, types, adapter, identity types | 3-5 days | 3-5 days |
| 12b-2: Security & AWPP | ED25519, OAuth 2.0 Client Credentials, AWPP protocol | 4-6 days | 7-11 days |
| 12b-3: WebTransport host | T3 server + AWPP + pipeline isolation | 5-7 days | 12-18 days |
| 12b-4: WebSocket host | T4 server + AWPP + pipeline isolation | 3-5 days | 15-23 days |
| 12b-5: Python SDK | T3/T4 transport + ED25519 signing | 3-5 days | 18-28 days |
| 12b-6: Go SDK | T3 transport + ED25519 signing | 3-5 days | 21-33 days |
| 12b-7: CLI/REST/Registry | Identity mgmt, binding config, YAML | 2-3 days | 23-36 days |
| 12b-8: Benchmarks/Hardening | Performance, reconnection, key rotation | 3-5 days | 26-41 days |
| **--- Core complete (above). Language SDKs below (demand-driven) ---** | | | |
| 12b-9: Node.js/TypeScript SDK | T4 WebSocket + npm ecosystem | 3-4 days | 29-45 days |
| 12b-10: Java/Kotlin SDK | T3 + T4, Netty QUIC, Kotlin coroutines | 4-6 days | 33-51 days |
| 12b-11: C#/.NET SDK | T1 NativeAOT + T3 + T4, System.Net.Quic | 4-6 days | 37-57 days |
| 12b-12: C/C++ SDK | T1 header-only + T3 libquiche | 3-4 days | 40-61 days |
| 12b-13: PHP SDK | T4 Swoole/ReactPHP/Amphp/Laravel Octane | 3-4 days | 43-65 days |
| 12b-14: Swift/Elixir/Ruby/etc. | P3/P4 demand-driven | 8-15 days | 51-80 days |

---

## 18. Risk Assessment

### 18.1 Technical Risks

| Risk | Severity | Likelihood | Mitigation |
|------|----------|-----------|------------|
| **Pipeline refactor breaks T1/T2 performance** | High | Low | InProcessTransport compiles to same code; benchmark before/after |
| **Batch correlation complexity** | Medium | Medium | Use simplified sequential format (Section 8.1) |
| **WebSocket head-of-line blocking** | Medium | Medium | Single-partition per frame mitigates; document T3 as preferred |
| **QUIC library maturity on Windows** | Medium | Low | `wtransport` supports Windows; test early |
| **SDK wire format bugs** | High | Medium | Extensive cross-language round-trip tests; fuzz testing |
| **Processor reconnection storms** | Medium | Low | Exponential backoff with jitter; connection rate limiting |
| **Memory pressure from buffered batches** | Medium | Medium | Configurable max in-flight batches; backpressure propagation |
| **ED25519 library inconsistency across languages** | Medium | Low | Test signing in each SDK against Aeon's verifier; use well-known test vectors |
| **Key management complexity for operators** | Medium | Medium | Provide `aeon keygen` helper command; document integration with Vault, K8s Secrets |
| **Shared binding noisy neighbor** | Medium | Medium | Per-pipeline backpressure; monitoring alerts on latency divergence between pipelines |
| **Pipeline isolation leak in shared mode** | High | Low | Aeon validates pipeline name on every batch response; reject mismatched pipeline tags |

### 18.2 Compatibility Risks

| Risk | Mitigation |
|------|------------|
| Existing `Processor` trait users | Trait unchanged. `InProcessTransport` adapter is transparent. |
| Existing pipeline configs | `ProcessorRef` gains optional fields with defaults. Existing YAML works unchanged. |
| Existing REST API clients | New endpoints only. Existing endpoints unchanged. |
| Wire format versioning | `wire_format: "binary/v1"` in AWPP handshake. Future versions negotiated. |
| `ProcessorType` serde compatibility | New variants use `kebab-case` (`web-transport`, `web-socket`). Old JSON without these variants still deserializes correctly. |

### 18.3 Security Risks

| Risk | Severity | Likelihood | Mitigation |
|------|----------|-----------|------------|
| **Rogue processor connects** | High | Medium | ED25519 challenge-response + OAuth token (if enabled); reject unknown keys |
| **Replay attack on auth** | High | Low | Fresh nonce per connection; nonce not reusable |
| **Private key compromise** | High | Low | Per-instance keys; immediate revocation; OAuth adds second factor (attacker also needs valid token from IdP) |
| **OAuth token theft** | Medium | Low | Tokens are short-lived (15min-1hr); attacker also needs ED25519 private key; IdP audit logs record issuance |
| **IdP unavailability** | Medium | Low | JWKS cached locally with configurable refresh; existing connections unaffected; new connections queue until JWKS available |
| **Man-in-the-middle** | High | Low | TLS mandatory (QUIC = always TLS; WSS required in production) |
| **Cross-pipeline data leak (shared)** | High | Low | Separate streams + Aeon-side pipeline tag validation on every batch |
| **Batch signature forgery** | Critical | Very Low | ED25519 is 128-bit security level; forgery computationally infeasible |
| **Stale OAuth token on long connection** | Medium | Low | AWPP token_refresh message; Aeon terminates connection if token not refreshed within grace period |

### 18.4 Non-Risks (Explicitly Called Out)

- **T1/T2 performance regression**: `InProcessTransport` is a zero-cost wrapper. The async `call_batch()` resolves synchronously for in-process processors — no allocation, no await point, no vtable overhead beyond the trait object dispatch (~1ns).
- **Breaking existing processor implementations**: The `Processor` trait is unchanged. All existing native and Wasm processors continue to work unmodified.
- **Wire format incompatibility**: T3/T4 use the same binary wire format as T1/T2. The only addition is the batch framing header, which is transport-level, not format-level.
- **ED25519 performance impact**: Sign (~60μs) + verify (~150μs) per batch, amortized to ~0.21μs per event at batch size 1024. Less than 4% throughput impact. Not a bottleneck.
- **T1/T2 processors need ED25519**: They don't. In-process processors are authenticated by artifact hash (SHA-256). ED25519 is only for T3/T4 where the processor connects over the network.

---

## 19. Decision Log

| # | Decision | Rationale | Alternatives Considered |
|---|----------|-----------|------------------------|
| D1 | New `ProcessorTransport` trait (additive, not replacing `Processor`) | Non-breaking. All existing code continues to work. | (a) Make `Processor` async — too disruptive; (b) Enum wrapper — less extensible |
| D2 | `&dyn ProcessorTransport` in pipeline (dynamic dispatch) | Simplicity. ~1ns vtable overhead is negligible vs 240ns+ processing. | Enum dispatch — premature optimization |
| D3 | Simplified batch response (sequential, per-event output count) | Easier for SDK developers. Preserves event→output mapping naturally. | Explicit event_map with indices — more complex, marginal benefit |
| D4 | JSON control stream, binary data streams | Control is infrequent (setup + heartbeat) — debuggability matters. Data is high-frequency — binary efficiency matters. | All-binary — harder to debug; All-JSON — too slow for data |
| D5 | WebSocket pipeline+partition tagging | Single-connection model with full pipeline isolation. No port saturation. | One connection per partition — more overhead; 2-byte partition only — no pipeline isolation |
| D6 | T3/T4 processors self-manage state (no host state imports initially) | Out-of-process processors are full programs — they can use their own databases. Adding host state RPC is future work. | Host state via control stream — adds latency and complexity |
| D7 | T4 WebSocket via existing REST API server | Reuses auth, TLS, port. No new listener needed. | Separate WebSocket server — more ports, more config |
| D8 | Processor-initiated connections (processor connects to Aeon) | Works through NAT/firewalls. Processor can be anywhere. | Aeon connects to processor — requires processor to have public endpoint |
| D9 | Wire format v1 unchanged for T3/T4 | Universal contract. SDK developers implement once, works across all tiers. | Per-tier format — fragmentation, more SDK work |
| D10 | Four tiers (not three) — keep WebSocket as T4 | Universal language coverage. Some languages lack HTTP/3 support. WebSocket is the universal fallback. | Three tiers (merge T3/T4) — leaves PHP, Ruby, etc. without a clean option |
| D11 | **ED25519 challenge-response** for T3/T4 authentication | Non-repudiation, per-instance identity, per-batch signing. Fast (~60μs sign). No shared secrets. | Bearer tokens — copyable, no non-repudiation; mTLS-only — harder for SDK developers; JWT — expiry management complexity |
| D12 | **Per-batch ED25519 signing** | Complete audit trail. Any compliance auditor can verify which processor produced which outputs. <4% overhead at batch size 1024. | No signing — no audit trail; Signing only at connection — no per-batch integrity |
| D13 | **Aeon-managed processor RBAC** (not delegated to external systems) | Aeon is the server accepting T3/T4 connections. No external system in the data path to delegate to. Source/sink RBAC is delegated because those systems own the data; for processors, Aeon owns the data flow. | External auth (OIDC) — adds dependency; Network-level only (firewall) — insufficient for multi-tenant |
| D14 | **Dedicated binding as default**, shared as opt-in | Dedicated is simpler, safer, easier to reason about. Shared requires careful pipeline isolation. Default to safe; let operators opt into shared when they understand the tradeoffs. | Shared as default — higher risk of data leakage; No shared mode — limits ROI for cost-conscious deployments |
| D15 | **Per-pipeline data streams** in shared mode | Physical isolation even within shared mode. Aeon validates pipeline tag on every batch. Prevents cross-pipeline data leakage by construction. | Logical tagging without validation — vulnerable to processor bugs; Shared streams with pipeline field — harder to enforce |
| D16 | **OAuth 2.0 Client Credentials as optional Layer 2.5** | Defense-in-depth: ED25519 key theft alone insufficient without valid OAuth token. Integrates with org's existing IdP (Keycloak, Auth0, Okta, Azure AD). Auto-expiring tokens limit exposure window. Independent audit stream via IdP. M2M flow — no MFA/device binding (not applicable to machine-to-machine). | OAuth-only — no per-batch signing, bearer tokens copyable; ED25519-only (current) — no IdP federation, single point of compromise; mTLS client certs — harder to manage across 26+ language SDKs |
| D17 | **MessagePack default, JSON fallback for AWPP transport codec** | MsgPack is compact (~2x smaller than JSON), fast (~4x faster encode/decode), and has mature libraries in all target languages (Python, Go, JS, Java, C#, PHP, Ruby). JSON fallback for debugging and prototyping. Per-pipeline config because each pipeline has a dedicated processor — no need for per-processor codec when binding is dedicated. Codec only applies to T3/T4 data streams; T1/T2 are in-process and never serialize Event envelopes. | Protobuf — adds schema management (.proto files, codegen) for a fixed internal envelope; Bincode — Rust-only, excludes all T3/T4 target languages; JSON-only — 2x larger wire size, 4x slower encode; Configurable per-processor — unnecessary complexity given dedicated binding model |

---

## Appendix A: File Change Summary

| File | Change Type | Description |
|------|-------------|-------------|
| `crates/aeon-types/src/traits.rs` | Modified | Add `ProcessorTransport` trait |
| `crates/aeon-types/src/lib.rs` | Modified | Export new types |
| `crates/aeon-types/src/processor_transport.rs` | **New** | `ProcessorHealth`, `ProcessorInfo`, `ProcessorTier`, `ProcessorBinding`, `ProcessorConnectionConfig` |
| `crates/aeon-types/src/processor_identity.rs` | **New** | `ProcessorIdentity`, `PipelineScope` (ED25519 identity types) |
| `crates/aeon-types/src/registry.rs` | Modified | Add `WebTransport`/`WebSocket` to `ProcessorType`, extend `ProcessorVersion`, extend `ProcessorRef` with `binding`/`connection` |
| `crates/aeon-types/src/transport_codec.rs` | **New** | `TransportCodec` enum, codec encode/decode for Event/Output envelopes (MsgPack + JSON) |
| `crates/aeon-types/src/batch_wire.rs` | **New** | Batch framing serialization/deserialization (including ED25519 signature slot) |
| `crates/aeon-engine/src/transport/mod.rs` | **New** | Transport module |
| `crates/aeon-engine/src/transport/in_process.rs` | **New** | `InProcessTransport` adapter |
| `crates/aeon-engine/src/transport/webtransport_host.rs` | **New** | T3 server with AWPP + ED25519 |
| `crates/aeon-engine/src/transport/websocket_host.rs` | **New** | T4 server with AWPP + ED25519 |
| `crates/aeon-engine/src/transport/awpp.rs` | **New** | AWPP protocol messages, challenge-response logic, token refresh |
| `crates/aeon-engine/src/transport/oauth.rs` | **New** | OAuth 2.0 JWKS fetcher, JWT validation, claim mapping, token expiry tracking |
| `crates/aeon-engine/src/identity_store.rs` | **New** | `ProcessorIdentityStore` (CRUD, Raft-replicated) |
| `crates/aeon-engine/src/pipeline.rs` | Modified | Use `ProcessorTransport` instead of `Processor` |
| `crates/aeon-engine/src/lib.rs` | Modified | Export transport module, identity store |
| `crates/aeon-engine/Cargo.toml` | Modified | New feature flags, optional deps (`ed25519-dalek`) |
| `crates/aeon-cli/src/main.rs` | Modified | New `--runtime`/`--endpoint` flags, `processor identity` subcommands, `--binding`/`--group` pipeline flags |
| `crates/aeon-engine/src/rest_api.rs` | Modified | WebSocket upgrade endpoint, identity CRUD endpoints, instance status |
| `sdks/python/` | Modified | Add T3/T4 transport clients with ED25519 signing |
| `sdks/go/` | **New** | Go SDK with ED25519 signing |

## Appendix B: Dependency Impact

| New Dependency | Crate | Used For | Already in Workspace |
|----------------|-------|----------|---------------------|
| `wtransport` | `aeon-engine` (optional) | T3 processor host | Yes (`aeon-connectors`) |
| `tokio-tungstenite` | `aeon-engine` (optional) | T4 processor host | Yes (`aeon-connectors`) |
| `futures-util` | `aeon-engine` (optional) | WebSocket stream handling | Yes (`aeon-connectors`) |
| `rmp-serde` | `aeon-types`, `aeon-engine` | MessagePack Event/Output envelope codec | **New** |
| `crc32fast` | `aeon-engine` | Batch checksum | Yes (already a dependency) |
| `ed25519-dalek` | `aeon-engine`, `aeon-crypto` | ED25519 sign/verify for processor identity | **New** (but `aeon-crypto` already uses `sha2` + `hex`) |
| `jsonwebtoken` | `aeon-engine` (optional) | JWT validation for OAuth 2.0 Layer 2.5 | **New** (feature-gated behind `oauth`) |
| `rand` | `aeon-engine` | Nonce generation for challenge-response | Yes (transitive via `uuid`) |

One new direct dependency (`ed25519-dalek`). All others are already in the workspace.

## Appendix C: Port Allocation

| Port | Protocol | Purpose | Status |
|------|----------|---------|--------|
| 4470 | QUIC/UDP | Cluster inter-node transport (Raft) | Existing |
| 4471 | HTTP/TCP | REST API, health checks, metrics | Existing |
| 4472 | QUIC/UDP | T3 WebTransport processor connections | Existing (connector), repurpose for processors |
| 4473 | TCP | T4 WebSocket processor connections (standalone) | **New** (optional — can share 4471 via REST API) |

**Recommended**: T4 WebSocket shares port 4471 via REST API upgrade endpoint (`/api/v1/processors/connect`). No new port needed.
