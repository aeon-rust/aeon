# Processor Deployment, Registry & Lifecycle Management

> This document captures the full design for processor deployment in Aeon.
> It covers isolation models, the Processor Registry, pipeline lifecycle,
> upgrade strategies (drain-swap, blue-green, canary), dynamic `.so` loading,
> and cluster-aware deployment. Referenced from `docs/ROADMAP.md` (Phases 12–14).
>
> Related: `docs/INSTALLATION.md` (ports, multi-version operation, directory layout)
>
> **Implementation status** (updated 2026-04-11):
>
> | Component | Status |
> |-----------|--------|
> | Wasm loading (`WasmModule::from_bytes`) | **Implemented + tested** |
> | Native `.so`/`.dll` loading (`NativeProcessor::load`) | **Implemented + tested** (C-ABI, .NET NativeAOT) |
> | ProcessorRegistry (CRUD + artifact storage) | **Implemented + tested** |
> | REST API — `POST /api/v1/processors` (register) | **Implemented + tested** |
> | REST API — all pipeline lifecycle endpoints | **Implemented + tested** (19 REST tests) |
> | PipelineManager state machine (upgrade/blue-green/canary) | **Implemented** — state transitions tracked |
> | Hot-swap orchestrator (drain → swap → resume) | **Implemented** — `PipelineControl` + `run_buffered_managed()`: pause source → drain SPSC rings → swap processor/source/sink → resume. Blue-green shadow + canary traffic splitting also wired. 8 managed tests. |
> | T3/T4 processor replacement | **Working** — reconnect-based; routing table auto-updates on connect/disconnect |
> | Source/Sink zero-downtime reconfiguration | **Implemented** — `PipelineControl.drain_and_swap_source()`/`drain_and_swap_sink()`: pause → drain SPSC rings → swap via `Box<dyn Any>` downcast → resume. 2 tests (source-swap, sink-swap). |
> | SHA-512 artifact verification | **Implemented** — `sha2::Sha512` (replaced `DefaultHasher` placeholder) |
> | Child process isolation tier | **Design only** — not implemented |
> | File watcher / config hot-reload | **Implemented** — `aeon dev watch --artifact <path>`: `notify` crate watches .wasm/.so/.dll, debounced 500ms, triggers `PipelineControl.drain_and_swap()`. TickSource (1 event/sec) → StdoutSink for dev loop. |
>
> **Zero-downtime processor deployment per tier:**
> - **T1 Rust native** (compiled in): requires Aeon rebuild + redeploy
> - **T1 Native `.so`**: designed for `dlclose`/`dlopen` swap — orchestrator not yet wired
> - **T2 Wasm**: designed for module re-instantiate (~1ms) — orchestrator not yet wired
> - **T3 WebTransport**: **works now** — new processor connects, old disconnects, routing auto-updates
> - **T4 WebSocket**: **works now** — same reconnect-based replacement as T3

---

## 1. Design Principles

1. **Processor changes must never require an Aeon recompile or restart** (Wasm path).
   Native `.so` processors avoid recompile of Aeon itself — only the processor is recompiled.
2. **Processor lifecycle is independent per pipeline.** Starting, stopping, upgrading, or
   crashing one pipeline must not affect any other running pipeline.
3. **Zero-downtime upgrades by default.** Three upgrade strategies (drain-swap, blue-green,
   canary) — all designed for zero or near-zero downtime.
4. **Cluster-aware from day one.** The Processor Registry and pipeline definitions are
   Raft-replicated. Single-node is just a cluster of size 1 — same code path.
5. **Trust-tiered isolation.** Match isolation level to trust level and performance needs.
6. **Three equivalent interfaces.** Every management operation is available via CLI, REST API,
   and YAML manifest. CLI is primary for operators. REST API enables programmatic integration
   (CI/CD, custom dashboards, orchestration tools). YAML manifests enable declarative,
   version-controlled configuration. All three are first-class — none is a wrapper around another.

---

## 2. Processor Execution Tiers

Aeon supports three tiers of processor execution, each with different performance and
isolation characteristics:

### 2.1 Wasm In-Process (Standard Path)

The default for all languages. Processor is compiled to a `.wasm` component and loaded
by Wasmtime inside the Aeon process.

| Aspect | Details |
|--------|---------|
| Performance | ~3–5% overhead vs native |
| Isolation | Memory sandbox (Wasmtime), fuel metering, namespace isolation |
| Languages | Any language with Wasm target (Rust, TypeScript, Python, Go, Java, C#, PHP, C/C++) |
| Artifact | `.wasm` file (typically 1–50KB) |
| Hot-swap | Unload old module, load new module (~1ms). No Aeon restart. |
| Safety | Sandboxed — a buggy processor cannot crash Aeon or access other processors' memory |
| Best for | Standard processors, multi-language teams, rapid iteration |

```
Developer writes processor (any language)
    ↓
aeon build ./myprocessor  →  myprocessor.wasm
    ↓
aeon processor register my-enricher ./myprocessor.wasm
    ↓
Aeon loads via Wasmtime (sandboxed, metered)
```

### 2.2 Native Dynamic Library (`.so` / `.dylib` / `.dll`)

For trusted, high-throughput processors that need near-native performance without
recompiling the Aeon binary itself.

| Aspect | Details |
|--------|---------|
| Performance | ~0–2% overhead (function pointer indirection via `dlopen`) |
| Isolation | **None** — runs in Aeon's process space with full memory access |
| Languages | Any language that compiles to C-ABI shared libraries (Rust, C, C++, Go, Zig) |
| Artifact | `.so` (Linux), `.dylib` (macOS), `.dll` (Windows) |
| Hot-swap | `dlclose(old)` + `dlopen(new)` after pipeline drain. No Aeon restart. |
| Safety | **No sandbox** — a bad `.so` can crash Aeon or corrupt memory |
| Best for | Trusted internal processors, maximum throughput (20M+/sec) |

#### C-ABI Contract

Native `.so` processors must export these symbols:

```rust
// Required exports (C-ABI)
#[no_mangle]
pub extern "C" fn aeon_processor_create(config_ptr: *const u8, config_len: usize) -> *mut c_void;

#[no_mangle]
pub extern "C" fn aeon_processor_destroy(ctx: *mut c_void);

#[no_mangle]
pub extern "C" fn aeon_process(
    ctx: *mut c_void,
    event_ptr: *const u8, event_len: usize,
    out_buf: *mut u8, out_capacity: usize, out_len: *mut usize,
) -> i32;  // 0 = success, non-zero = error code

#[no_mangle]
pub extern "C" fn aeon_process_batch(
    ctx: *mut c_void,
    events_ptr: *const u8, events_len: usize,
    out_buf: *mut u8, out_capacity: usize, out_len: *mut usize,
) -> i32;

// Optional: metadata
#[no_mangle]
pub extern "C" fn aeon_processor_name() -> *const c_char;

#[no_mangle]
pub extern "C" fn aeon_processor_version() -> *const c_char;
```

Aeon provides a Rust SDK crate (`aeon-processor-native-sdk`) that generates these
exports from an idiomatic Rust `impl Processor` block, so developers never write
`extern "C"` manually.

### 2.3 Child Process (Full OS Isolation)

For untrusted or crash-prone processors that need complete isolation from the Aeon runtime.

| Aspect | Details |
|--------|---------|
| Performance | ~5–15% overhead (IPC via Unix socket or shared memory) |
| Isolation | Full OS process isolation (separate address space, can be containerized) |
| Languages | Any (communicates via IPC protocol, not Wasm or C-ABI) |
| Artifact | Executable binary, Docker image, or `.wasm` run in separate process |
| Hot-swap | Spawn new process, two-phase transfer, old process exits. Zero pause. |
| Safety | Complete isolation — crash cannot affect Aeon or other processors |
| Best for | Untrusted third-party processors, processors with native dependencies, crash-prone workloads |

```
┌─── Aeon Process ──────────────┐     ┌─── Child Process ──────────┐
│  Pipeline Controller          │     │  Processor binary           │
│  ←── shared memory ring ────→ │ IPC │  (own address space)        │
│  (or Unix domain socket)      │     │  Can be its own container   │
└───────────────────────────────┘     └─────────────────────────────┘
```

### 2.4 Tier Selection Summary

```
                    ┌─────────────────────────────────────────┐
                    │        Need maximum throughput?          │
                    │         (20M+ events/sec)                │
                    └──────┬──────────────────┬───────────────┘
                       Yes │                  │ No
                           ▼                  ▼
                  ┌─────────────┐    ┌──────────────────┐
                  │ Trust the   │    │ Wasm in-process   │
                  │ processor?  │    │ (standard path)   │
                  └──┬──────┬──┘    └──────────────────┘
                 Yes │      │ No
                     ▼      ▼
            ┌──────────┐  ┌───────────────┐
            │ Native   │  │ Child process │
            │ .so      │  │ (OS isolated) │
            └──────────┘  └───────────────┘
```

---

## 3. Processor Registry

The Processor Registry is a versioned catalog of processor artifacts, replicated across
the Aeon cluster via Raft consensus.

### 3.1 Registry Data Model

```
Processor Registry (Raft-replicated state)
│
├── Processors (catalog)
│   ├── my-enricher
│   │   ├── type: wasm
│   │   ├── versions:
│   │   │   ├── v1 — 2026-03-15, sha512=abc..., status=archived
│   │   │   ├── v2 — 2026-03-20, sha512=def..., status=available
│   │   │   └── v3 — 2026-04-01, sha512=ghi..., status=active
│   │   └── merkle_proof: <proof that artifact is in Merkle log>
│   │
│   └── my-aggregator
│       ├── type: native-so
│       ├── platform: linux-x86_64
│       ├── versions:
│       │   └── v1 — 2026-04-01, sha512=jkl..., status=active
│       └── merkle_proof: <proof>
│
├── Pipelines (bindings — see Section 5)
│
└── Deployment History (audit log)
    ├── 2026-04-01T00:00Z: my-enricher:v3 deployed to orders-pipeline (canary 10%)
    ├── 2026-04-01T00:05Z: my-enricher:v3 promoted to 50%
    └── 2026-04-01T00:10Z: my-enricher:v3 promoted to 100%
```

### 3.2 Registry Operations (CLI)

```bash
# Register a new processor (first version)
aeon processor register my-enricher ./enricher.wasm
# Output: Registered my-enricher:v1 (wasm, sha512=abc..., 12KB)

# Register a new version (auto-increments)
aeon processor register my-enricher ./enricher-v2.wasm
# Output: Registered my-enricher:v2 (wasm, sha512=def..., 14KB)

# Register a native .so processor
aeon processor register my-aggregator ./aggregator.so --type native
# Output: Registered my-aggregator:v1 (native-so, linux-x86_64, sha512=jkl..., 2.1MB)

# List all processors
aeon processor list
# Output:
# NAME             TYPE        ACTIVE    VERSIONS  PIPELINES
# my-enricher      wasm        v3        3         orders-pipeline
# my-aggregator    native-so   v1        1         clicks-pipeline

# Show versions
aeon processor versions my-enricher
# Output:
# VERSION  DATE        STATUS     SIZE    SHA512
# v1       2026-03-15  archived   12KB    abc123...
# v2       2026-03-20  available  14KB    def456...
# v3       2026-04-01  active     15KB    ghi789...

# Inspect specific version
aeon processor inspect my-enricher:v3

# Delete a processor (only if not bound to any pipeline)
aeon processor delete my-enricher:v1
```

### 3.3 Registry API (Programmatic)

In addition to the CLI, the registry is accessible via REST API on the HTTP management
port (default: 4471, see `docs/INSTALLATION.md`). See Section 8.2 for the complete
REST API specification.

```
POST   /api/v1/processors                          # Register
GET    /api/v1/processors                          # List
GET    /api/v1/processors/{name}                   # Inspect
GET    /api/v1/processors/{name}/versions          # List versions
GET    /api/v1/processors/{name}/versions/{ver}    # Get specific version
DELETE /api/v1/processors/{name}/versions/{ver}    # Delete version
GET    /api/v1/processors/{name}/proof             # Merkle proof of artifact
```

### 3.4 Cluster Replication Flow

When a processor is registered:

```
1. CLI sends .wasm/.so bytes to cluster leader over QUIC
2. Leader validates artifact:
   - Wasm: Wasmtime pre-compilation check (valid component?)
   - Native: symbol resolution check (exports aeon_process?)
3. Leader computes SHA-512 hash of artifact
4. Leader proposes to Raft: ProcessorRegistered { name, version, hash, bytes }
5. Raft replicates to majority of nodes
6. Each node stores artifact locally (filesystem or embedded store)
7. Merkle proof generated for artifact (append to Merkle Mountain Range)
8. PoH checkpoint includes registry state hash
```

On single-node clusters, this is the same flow — Raft quorum of 1, no network hop.

---

## 4. Pipeline Lifecycle

A pipeline is the binding of a source, processor, and sink. Each pipeline runs
independently with its own lifecycle.

### 4.1 Pipeline Definition

```yaml
# manifest.yaml
pipelines:
  orders-pipeline:
    source:
      type: redpanda
      topic: orders
      partitions: [0, 1, 2, 3, 4, 5, 6, 7]

    processor:
      name: my-enricher
      version: v3                    # Pin to specific version
      tier: wasm                     # wasm | native | child-process (auto-detected if omitted)

    sink:
      type: redpanda
      topic: enriched-orders

    # Upgrade strategy (see Section 6)
    upgrade:
      strategy: canary
      initial_percent: 10
      increment: 10
      evaluation_window: 60s
      auto_promote: true
      rollback_on:
        error_rate_threshold: 0.01
        p99_latency_threshold: 10ms

  clicks-pipeline:
    source:
      type: redpanda
      topic: clicks
      partitions: [0, 1, 2, 3]

    processor:
      name: my-aggregator
      version: v1
      tier: native

    sink:
      type: redpanda
      topic: click-aggregates

    upgrade:
      strategy: drain-swap           # Simple default
```

### 4.2 Pipeline Lifecycle Commands

```bash
# Create pipeline from manifest
aeon pipeline create -f manifest.yaml

# Or create inline
aeon pipeline create orders-pipeline \
  --source redpanda://orders \
  --processor my-enricher:v3 \
  --sink redpanda://enriched-orders

# Start / stop (independent per pipeline)
aeon pipeline start orders-pipeline
aeon pipeline stop orders-pipeline        # Does NOT affect clicks-pipeline

# Status
aeon pipeline status
# Output:
# PIPELINE          PROCESSOR         STATUS    UPTIME     EVENTS      RATE
# orders-pipeline   my-enricher:v3    running   2h 15m     4.2B        520K/s
# clicks-pipeline   my-aggregator:v1  running   1h 30m     800M        148K/s

# Detailed status
aeon pipeline status orders-pipeline

# History (like AWS Glue job runs)
aeon pipeline history orders-pipeline
# Output:
# START               END                 PROCESSOR         EVENTS     STATUS
# 2026-04-01T00:00Z   (running)           my-enricher:v3    4.2B       running
# 2026-03-28T12:00Z   2026-03-31T23:59Z   my-enricher:v2    12.1B      stopped (manual)
# 2026-03-20T00:00Z   2026-03-28T11:59Z   my-enricher:v1    8.5B       stopped (upgrade)
```

### 4.3 Pipeline Isolation

Each pipeline operates independently:

- **Own partition assignments** — partitions are bound to a specific pipeline
- **Own ring buffers** — SPSC buffers per pipeline, no shared hot-path state
- **Own processor instance** — separate Wasm module instance or `.so` context
- **Own metrics** — per-pipeline throughput, latency, error counters
- **Own fault tolerance** — per-pipeline DLQ, circuit breaker, retry config
- **Independent lifecycle** — start/stop/crash/upgrade affects only that pipeline

In the cluster, the Partition Manager assigns partitions to pipelines across nodes:

```
Node 1 (Leader)              Node 2                   Node 3
┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐
│ orders-pipeline  │    │ orders-pipeline  │    │ clicks-pipeline │
│ partitions 0-3   │    │ partitions 4-7   │    │ partitions 0-3  │
│ enricher:v3      │    │ enricher:v3      │    │ aggregator:v1   │
│                  │    │                  │    │                 │
│ clicks-pipeline  │    │                  │    │                 │
│ (no partitions   │    │                  │    │                 │
│  on this node)   │    │                  │    │                 │
└─────────────────┘    └─────────────────┘    └─────────────────┘
```

---

## 5. Upgrade Strategies

Aeon supports three upgrade strategies, configurable per pipeline. All three are designed
for zero or near-zero downtime.

### 5.1 Drain + Swap (Default)

The simplest strategy. Brief pause while the processor is swapped.

```
Timeline:
─────────────────────────────────────────────────────────
  Running v1          Drain    Swap   Running v2
  ████████████████████ ░░░░░ ▓ ██████████████████████
                       ~50ms  ~1ms
─────────────────────────────────────────────────────────
Total pause: <100ms (drain time + module load time)
```

**Flow:**

```
1. User: aeon pipeline upgrade orders-pipeline --processor enricher:v2
2. Controller signals pipeline: "prepare upgrade"
3. Source stops polling (backpressure: no new events)
4. In-flight events drain through processor → sink
5. Sink flushes and confirms all events committed
6. Old processor unloaded:
   - Wasm: Wasmtime module dropped
   - Native: dlclose(handle)
7. New processor loaded:
   - Wasm: Wasmtime module instantiated (~1ms)
   - Native: dlopen(new_path), resolve symbols
8. Source resumes polling from last committed offset
9. Pipeline running with v2
```

**Config:**
```yaml
upgrade:
  strategy: drain-swap
```

**When to use:** Most upgrades. Simple, reliable, predictable. The sub-100ms pause is
imperceptible for nearly all workloads.

### 5.2 Blue-Green

Two full processor instances run simultaneously. Instant cutover from old to new.

```
Timeline:
─────────────────────────────────────────────────────────
  v1: ████████████████████████████████ ░░░ (drain old)
  v2:                     ▓ ███████████████████████████
                    Warm up  Cutover
                    (shadow)  (instant)
─────────────────────────────────────────────────────────
Zero pause: v2 takes over partitions, v1 drains remaining in-flight
```

**Flow:**

```
1. User: aeon pipeline upgrade orders-pipeline --processor enricher:v2 --strategy blue-green
2. Controller loads v2 processor alongside v1 (both instantiated)
3. v2 warms up in shadow mode (receives copy of events, output discarded)
4. Once v2 is confirmed healthy:
   a. New events routed to v2
   b. v1 drains its remaining in-flight events
   c. v1 unloaded after drain completes
5. Rollback: if v2 shows errors during warm-up, abort and stay on v1
```

**Config:**
```yaml
upgrade:
  strategy: blue-green
  warmup_events: 1000        # Shadow-process this many events before cutover
  warmup_timeout: 30s        # Max time for warm-up phase
  rollback_on:
    error_rate_threshold: 0.01
    p99_latency_threshold: 10ms
```

**When to use:** When even a sub-100ms pause is unacceptable, or when you want to
validate the new processor under real load before committing.

### 5.3 Canary

Gradual traffic splitting between old and new processors. The safest strategy for
critical production pipelines.

```
Timeline:
─────────────────────────────────────────────────────────
  v1: ████████████████ ██████████████ ████████████ ░░░
       100%            90%            50%          drain
  v2:                  ▓▓             ▓▓▓▓▓▓       ████
                       10%            50%          100%
      ├── eval ──┤ ├── eval ──┤ ├── eval ──┤
─────────────────────────────────────────────────────────
Zero pause: traffic gradually shifts, auto-rollback on anomaly
```

**Flow:**

```
1. User: aeon pipeline upgrade orders-pipeline --processor enricher:v2 --strategy canary
2. Controller loads v2 alongside v1
3. Traffic split begins: 10% of partitions assigned to v2, 90% stay on v1
4. Evaluation window (e.g., 60s): compare v2 metrics against v1
   - Error rate, P99 latency, throughput per event
5. If v2 healthy → promote: increase v2 traffic by increment (e.g., +10%)
6. Repeat steps 4-5 until v2 reaches 100%
7. v1 unloaded after final drain
8. At ANY step: if v2 metrics breach thresholds → automatic rollback to v1
```

**Traffic splitting implementation:**
- Partition-level splitting: assign N% of partitions to v2 (coarse-grained, simple)
- Event-level splitting: route N% of events within each partition to v2
  (fine-grained, requires running both processors per partition)
- Default: partition-level (simpler, no dual-processor overhead)

**Config:**
```yaml
upgrade:
  strategy: canary
  initial_percent: 10          # Start with 10% traffic to new version
  increment: 10                # Increase by 10% each step
  evaluation_window: 60s       # Observe for 60s before each promotion step
  auto_promote: true           # Automatically promote if metrics healthy
  auto_rollback: true          # Automatically rollback if thresholds breached
  rollback_on:
    error_rate_threshold: 0.01       # >1% error rate → rollback
    p99_latency_threshold: 10ms      # P99 >10ms → rollback
    throughput_drop_threshold: 0.20  # >20% throughput drop → rollback
```

**Manual canary control:**

```bash
# Start canary deployment
aeon pipeline upgrade orders-pipeline --processor enricher:v2 --strategy canary

# Check canary status
aeon pipeline canary-status orders-pipeline
# Output:
# CANARY STATUS: orders-pipeline
# Old: enricher:v1 (90% traffic, 8 partitions)
# New: enricher:v2 (10% traffic, 1 partition)
# Metrics (v2 vs v1):
#   Error rate:  0.001% vs 0.002%  ✓
#   P99 latency: 3.2ms vs 3.5ms   ✓
#   Throughput:  52K/s vs 468K/s   ✓ (proportional)
# Next promotion in: 42s

# Manual promotion (override auto)
aeon pipeline promote orders-pipeline --percent 50

# Manual rollback
aeon pipeline rollback orders-pipeline
# Output: Rolled back orders-pipeline to enricher:v1 (100% traffic)
```

### 5.4 Strategy Comparison

| Aspect | Drain + Swap | Blue-Green | Canary |
|--------|-------------|------------|--------|
| Downtime | <100ms pause | Zero | Zero |
| Risk | All-at-once | All-at-once (but validated) | Gradual |
| Resource overhead | None (sequential) | 2x processor during transition | 2x processor during rollout |
| Rollback speed | Requires another swap | Instant (old still loaded) | Instant (shift traffic back) |
| Complexity | Low | Medium | High |
| Best for | Most upgrades | Zero-downtime requirement | Critical production pipelines |
| Metrics-based auto-rollback | No | Yes (during warm-up) | Yes (at every step) |

---

## 6. Cluster Integration

### 6.1 Raft-Replicated State

The following state is replicated via Raft across all cluster nodes:

```
Raft State Machine
├── Cluster membership (nodes, roles)
├── Partition map (partition → node assignment)
├── Processor Registry
│   ├── Processor catalog (names, versions, artifacts)
│   └── Artifact hashes (SHA-512)
├── Pipeline definitions
│   ├── Source/processor/sink bindings
│   ├── Upgrade strategy config
│   └── Current state (running/stopped/upgrading)
├── Deployment history (audit log)
└── PoH checkpoints (global integrity)
```

### 6.2 Cross-Node Upgrade Coordination

When a pipeline spans multiple nodes (partitions distributed across the cluster),
upgrades are coordinated by the Raft leader:

```
Canary upgrade of orders-pipeline (8 partitions across 2 nodes):

Step 1 — Initial (10%):
  Node 1: partitions 0-3 → enricher:v1
  Node 2: partitions 4-6 → enricher:v1
  Node 2: partition  7   → enricher:v2  (canary)

Step 2 — Promote (50%):
  Node 1: partitions 0-1 → enricher:v1
  Node 1: partitions 2-3 → enricher:v2
  Node 2: partitions 4-5 → enricher:v1
  Node 2: partitions 6-7 → enricher:v2

Step 3 — Full (100%):
  Node 1: partitions 0-3 → enricher:v2
  Node 2: partitions 4-7 → enricher:v2
```

The leader ensures:
- All nodes have the new processor artifact before starting the upgrade
- Promotion/rollback decisions are consistent (single decision point)
- PoH chain continuity is maintained across processor swaps
- Merkle proofs cover the upgrade event itself (tamper-evident audit trail)

### 6.3 Integrity During Upgrades

- **PoH continuity**: The hash chain continues across processor swaps. The swap event
  itself is recorded in the PoH chain: `hash[n] = SHA-512(hash[n-1] || "processor_swap" || new_version_hash || timestamp)`
- **Merkle proof**: The processor artifact's SHA-512 hash is included in the Merkle
  Mountain Range, providing tamper-evident proof that a specific processor version
  processed a specific batch of events.
- **Audit trail**: Every deployment action (register, upgrade, promote, rollback) is
  recorded in the Raft log with timestamps, actor, and artifact hashes.

---

## 7. Hot-Swap Runtime Mechanics

### 7.1 Wasm Hot-Swap

```rust
// Pseudocode for Wasm processor hot-swap
async fn hot_swap_wasm(pipeline: &mut Pipeline, new_wasm_bytes: &[u8]) -> Result<()> {
    // 1. Pre-compile new module (can fail early, before any disruption)
    let new_module = wasmtime::Module::new(&engine, new_wasm_bytes)?;
    let new_instance = new_module.instantiate(&mut store)?;
    validate_wit_exports(&new_instance)?;

    // 2. Drain in-flight events
    pipeline.source.pause();
    pipeline.drain_in_flight().await?;

    // 3. Swap (atomic from pipeline's perspective)
    let old_instance = std::mem::replace(&mut pipeline.processor, new_instance);
    drop(old_instance);  // Old module freed

    // 4. Resume
    pipeline.source.resume();
    Ok(())
}
```

Key detail: the new module is **pre-compiled and validated before** the pipeline is
paused. This minimizes the swap window.

### 7.2 Native `.so` Hot-Swap

```rust
// Pseudocode for native .so hot-swap
async fn hot_swap_native(pipeline: &mut Pipeline, new_so_path: &Path) -> Result<()> {
    // 1. Pre-load and validate new library (before any disruption)
    let new_lib = libloading::Library::new(new_so_path)?;
    let new_process: Symbol<ProcessFn> = new_lib.get(b"aeon_process")?;
    let new_batch: Symbol<ProcessBatchFn> = new_lib.get(b"aeon_process_batch")?;
    let new_create: Symbol<CreateFn> = new_lib.get(b"aeon_processor_create")?;
    let new_ctx = new_create(config_ptr, config_len);

    // 2. Drain in-flight events
    pipeline.source.pause();
    pipeline.drain_in_flight().await?;

    // 3. Destroy old context, close old library
    let old_destroy: Symbol<DestroyFn> = old_lib.get(b"aeon_processor_destroy")?;
    old_destroy(old_ctx);
    drop(old_lib);  // dlclose

    // 4. Install new processor
    pipeline.processor = NativeProcessor { lib: new_lib, ctx: new_ctx, process: new_process };

    // 5. Resume
    pipeline.source.resume();
    Ok(())
}
```

### 7.3 Child Process Hot-Swap

```
1. Spawn new child process with new processor version
2. New process connects to Aeon via IPC (shared memory or Unix socket)
3. Two-phase partition transfer:
   a. PREPARE: new process receives partition assignment
   b. COMMIT: old process stops accepting events, new process takes over
4. Old process drains in-flight events
5. Old process exits
6. Zero pause — overlapping execution during transfer
```

### 7.4 Source/Sink Connector Reconfiguration

Sources and sinks are created when a pipeline starts and live for its lifetime.
The pipeline runner takes generic `S: Source` and `K: Sink` type parameters,
so swapping connectors requires stopping the pipeline.

**Same-type config change** (e.g., changing Kafka broker address):
```
1. Create new connector instance with new config (pre-validate)
2. Drain in-flight events through old connector
3. Atomically swap connector reference
4. Resume pipeline
```
This is architecturally similar to processor hot-swap and uses the same
drain→swap→resume pattern. **Not yet implemented.**

**Cross-type change** (e.g., Kafka → NATS):
Not feasible zero-downtime within a single pipeline (different generic types).
Use blue-green pipeline deployment: start new pipeline with new config, cut over
traffic, stop old pipeline. The `PipelineManager` state machine supports this.

**Current workaround**: `aeon pipeline stop` → update config → `aeon pipeline start`.
Downtime is limited to the drain + restart window (typically < 1 second for
in-process connectors, longer for remote connectors with flush requirements).

---

## 8. Unified Management Interfaces

Every management operation in Aeon is available through three equivalent interfaces.
None is a wrapper around another — all three interact directly with the cluster's
Raft state machine via the same internal API layer.

```
┌─────────┐  ┌──────────┐  ┌──────────────┐
│  CLI    │  │ REST API │  │ YAML Manifest│
│ (aeon)  │  │ (HTTP)   │  │ (declarative)│
└────┬────┘  └────┬─────┘  └──────┬───────┘
     │            │               │
     ▼            ▼               ▼
┌─────────────────────────────────────────┐
│     Internal Management API Layer       │
│  (validates, authorizes, routes)        │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│  Raft State Machine (cluster-replicated)│
│  Processor Registry + Pipeline Defs     │
└─────────────────────────────────────────┘
```

### 8.1 CLI (Operator-Primary)

The CLI is the primary interface for operators and developers. All examples in this
document use CLI syntax.

```bash
# Run pipeline from manifest
aeon run -f <manifest.yaml>

# Processor management
aeon processor register <name> <artifact-path> [--type wasm|native]
aeon processor list
aeon processor versions <name>
aeon processor inspect <name>[:<version>]
aeon processor delete <name>[:<version>]

# Pipeline management
aeon pipeline create -f <manifest.yaml>
aeon pipeline create <name> --source <uri> --processor <name:ver> --sink <uri>
aeon pipeline start <name>
aeon pipeline stop <name>
aeon pipeline status [<name>]
aeon pipeline history <name>

# Upgrade management
aeon pipeline upgrade <name> --processor <name:ver> [--strategy drain-swap|blue-green|canary]
aeon pipeline promote <name> [--percent <N>]
aeon pipeline rollback <name>
aeon pipeline canary-status <name>

# Declarative management (GitOps-friendly)
aeon apply -f <manifest.yaml>           # Create/update processors and pipelines
aeon apply -f <manifest.yaml> --dry-run # Preview changes without applying
aeon export -f <output.yaml>            # Export current state as YAML
aeon diff -f <manifest.yaml>            # Diff current state against manifest

# Cluster management
aeon cluster status
aeon cluster add <addr>
aeon cluster remove <node-id>
aeon cluster rebalance

# Development
aeon new <name> --lang <language>
aeon build <path>
aeon validate <artifact>
aeon dev --processor <path> [--source memory] [--sink stdout]
aeon deploy <artifact> --pipeline <name> [--target <cluster-addr>]

# Monitoring
aeon top                          # Real-time terminal dashboard
aeon verify                       # PoH/Merkle chain integrity check
```

### 8.2 REST API (Programmatic Integration)

The REST API runs on each Aeon node on the HTTP management port (default: **4471**,
see `docs/INSTALLATION.md`). Served via axum, same HTTP server as `/health` and
`/metrics`. Requests to followers are automatically proxied to the Raft leader for
write operations. Read operations can be served by any node.

All endpoints accept and return JSON. Artifact uploads use `multipart/form-data`.

#### Processor Endpoints

```
POST   /api/v1/processors
       Body: multipart/form-data { name, artifact (file), type? }
       Response: { name, version, type, sha512, size_bytes, created_at }

GET    /api/v1/processors
       Response: [{ name, type, active_version, version_count, pipelines }]

GET    /api/v1/processors/{name}
       Response: { name, type, active_version, versions, bound_pipelines }

GET    /api/v1/processors/{name}/versions
       Response: [{ version, created_at, sha512, size_bytes, status }]

GET    /api/v1/processors/{name}/versions/{version}
       Response: { version, created_at, sha512, size_bytes, status, artifact_url }

DELETE /api/v1/processors/{name}/versions/{version}
       Response: 204 No Content (fails if bound to active pipeline)

GET    /api/v1/processors/{name}/proof
       Response: { merkle_proof, poh_checkpoint }
```

#### Pipeline Endpoints

```
POST   /api/v1/pipelines
       Body: { name, source, processor, sink, upgrade? }
       Response: { name, status, processor, created_at }

GET    /api/v1/pipelines
       Response: [{ name, processor, status, uptime, events_total, rate }]

GET    /api/v1/pipelines/{name}
       Response: { name, processor, source, sink, status, partitions, metrics }

DELETE /api/v1/pipelines/{name}
       Response: 204 No Content (must be stopped first)

POST   /api/v1/pipelines/{name}/start
       Response: { status: "running" }

POST   /api/v1/pipelines/{name}/stop
       Response: { status: "stopped" }

GET    /api/v1/pipelines/{name}/history
       Response: [{ start, end, processor, events_total, status }]

GET    /api/v1/pipelines/{name}/metrics
       Response: { throughput, latency_p50, latency_p95, latency_p99, error_rate }
```

#### Upgrade Endpoints

```
POST   /api/v1/pipelines/{name}/upgrade
       Body: { processor: "name:version", strategy?, initial_percent?, ... }
       Response: { status: "upgrading", strategy, from_version, to_version }

POST   /api/v1/pipelines/{name}/promote
       Body: { percent: 50 }
       Response: { old_percent, new_percent, status }

POST   /api/v1/pipelines/{name}/rollback
       Response: { rolled_back_to, status }

GET    /api/v1/pipelines/{name}/canary
       Response: { old_version, new_version, old_percent, new_percent, metrics_comparison }
```

#### Cluster Endpoints

```
GET    /api/v1/cluster/status
       Response: { nodes, leader, term, partitions, registry_version }

POST   /api/v1/cluster/nodes
       Body: { addr }
       Response: { node_id, status }

DELETE /api/v1/cluster/nodes/{node-id}
       Response: 204 No Content

POST   /api/v1/cluster/rebalance
       Response: { moves: [{ partition, from_node, to_node }] }
```

#### Authentication

REST API uses the same mTLS certificates as inter-node QUIC transport. For environments
where mTLS is impractical (dev, CI/CD), API key authentication is supported:

```bash
# API key header
curl -H "Authorization: Bearer <api-key>" https://aeon:4471/api/v1/pipelines

# mTLS (production)
curl --cert client.crt --key client.key https://aeon:4471/api/v1/pipelines
```

#### Example: CI/CD Integration via REST API

```python
import requests

AEON_API = "https://aeon-cluster:4471/api/v1"
HEADERS = {"Authorization": f"Bearer {API_KEY}"}

# Register new processor version
with open("enricher-v2.wasm", "rb") as f:
    resp = requests.post(f"{AEON_API}/processors", headers=HEADERS,
                         files={"artifact": f}, data={"name": "my-enricher"})
    version = resp.json()["version"]
    print(f"Registered my-enricher:{version}")

# Trigger canary upgrade
resp = requests.post(f"{AEON_API}/pipelines/orders-pipeline/upgrade",
                     headers=HEADERS,
                     json={"processor": f"my-enricher:{version}", "strategy": "canary",
                           "initial_percent": 10})

# Poll canary status
import time
while True:
    status = requests.get(f"{AEON_API}/pipelines/orders-pipeline/canary",
                          headers=HEADERS).json()
    if status["new_percent"] == 100:
        print("Canary complete!")
        break
    if status.get("rolled_back"):
        print("Canary failed, rolled back!")
        break
    time.sleep(30)
```

### 8.3 YAML Manifest (Declarative Configuration)

YAML manifests provide declarative, version-controlled configuration. Apply with
`aeon apply -f <manifest.yaml>` (similar to `kubectl apply`).

#### Full Manifest Example

```yaml
# aeon-manifest.yaml
# Apply with: aeon apply -f aeon-manifest.yaml

processors:
  my-enricher:
    type: wasm
    artifact: ./processors/enricher.wasm     # Local path (uploaded on apply)
    # Or: artifact: registry://my-enricher:v3  (already registered)

  my-aggregator:
    type: native
    artifact: ./processors/aggregator.so

pipelines:
  orders-pipeline:
    source:
      type: redpanda
      topic: orders
      partitions: [0, 1, 2, 3, 4, 5, 6, 7]

    processor:
      name: my-enricher
      version: v3

    sink:
      type: redpanda
      topic: enriched-orders

    upgrade:
      strategy: canary
      initial_percent: 10
      increment: 10
      evaluation_window: 60s
      auto_promote: true
      rollback_on:
        error_rate_threshold: 0.01
        p99_latency_threshold: 10ms
        throughput_drop_threshold: 0.20

  clicks-pipeline:
    source:
      type: redpanda
      topic: clicks
      partitions: [0, 1, 2, 3]

    processor:
      name: my-aggregator
      version: v1

    sink:
      type: redpanda
      topic: click-aggregates

    upgrade:
      strategy: drain-swap
```

#### Manifest Operations

```bash
# Apply manifest (creates/updates processors and pipelines)
aeon apply -f aeon-manifest.yaml

# Dry-run (show what would change, don't apply)
aeon apply -f aeon-manifest.yaml --dry-run
# Output:
# + processor/my-enricher: register v3 (wasm, 15KB)
# ~ pipeline/orders-pipeline: update processor my-enricher:v2 → v3 (canary)
# = pipeline/clicks-pipeline: no changes

# Export current state as YAML (backup / version control)
aeon export -f current-state.yaml

# Diff current state against manifest
aeon diff -f aeon-manifest.yaml
```

### 8.4 Interface Equivalence Matrix

Every operation is available through all three interfaces:

| Operation | CLI | REST API | YAML Manifest |
|-----------|-----|----------|---------------|
| Register processor | `aeon processor register` | `POST /api/v1/processors` | `processors:` section in manifest |
| List processors | `aeon processor list` | `GET /api/v1/processors` | `aeon export` |
| Create pipeline | `aeon pipeline create` | `POST /api/v1/pipelines` | `pipelines:` section in manifest |
| Start pipeline | `aeon pipeline start` | `POST .../start` | `status: running` in manifest |
| Stop pipeline | `aeon pipeline stop` | `POST .../stop` | `status: stopped` in manifest |
| Upgrade processor | `aeon pipeline upgrade` | `POST .../upgrade` | Change `version:` in manifest + `aeon apply` |
| Canary promote | `aeon pipeline promote` | `POST .../promote` | N/A (operational, not declarative) |
| Rollback | `aeon pipeline rollback` | `POST .../rollback` | Revert `version:` + `aeon apply` |
| Cluster status | `aeon cluster status` | `GET /api/v1/cluster/status` | N/A (read-only) |
| Apply full config | `aeon apply -f` | Multiple API calls | Native |

---

## 9. Developer Workflow

### 9.1 Local Development (Inner Loop)

```bash
# Scaffold a new processor
aeon new my-enricher --lang typescript
# Creates:
#   my-enricher/
#   ├── package.json
#   ├── src/
#   │   └── processor.ts    # process() and process_batch() stubs
#   ├── wit/                 # WIT bindings (pre-generated)
#   ├── Makefile             # Build to .wasm
#   └── .aeon/
#       └── manifest.yaml    # Local test config

# Develop with hot-reload (watches for changes, recompiles, reloads)
cd my-enricher
aeon dev --source memory --sink stdout
# Output: Watching ./src for changes... Pipeline running.
# [event] → my-enricher → [stdout output]
# (edit processor.ts, save)
# Recompiling... Reloaded in 1.2s.

# Build for deployment
aeon build .
# Output: Built my-enricher.wasm (14KB, WIT-validated)

# Validate without running
aeon validate ./my-enricher.wasm
# Output: ✓ Exports: process, process_batch
#         ✓ WIT contract: aeon:processor/process@1.0.0
#         ✓ Fuel metering: compatible
```

### 9.2 Deployment (Outer Loop)

```bash
# Register with the cluster
aeon processor register my-enricher ./my-enricher.wasm
# Output: Registered my-enricher:v1 (wasm, 14KB)

# Bind to a pipeline
aeon pipeline create orders-pipeline \
  --source redpanda://orders \
  --processor my-enricher:v1 \
  --sink redpanda://enriched-orders \
  --upgrade-strategy canary

# Start the pipeline
aeon pipeline start orders-pipeline

# Later: deploy a new version
aeon processor register my-enricher ./my-enricher-v2.wasm
aeon pipeline upgrade orders-pipeline --processor my-enricher:v2
# Output: Canary deployment started (10% → my-enricher:v2)

# Monitor
aeon pipeline canary-status orders-pipeline

# If something goes wrong
aeon pipeline rollback orders-pipeline
```

### 9.3 CI/CD Integration

```yaml
# .github/workflows/deploy-processor.yml
name: Deploy Processor
on:
  push:
    paths: ['processors/my-enricher/**']

jobs:
  build-and-deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Build processor
        run: aeon build ./processors/my-enricher

      - name: Validate
        run: aeon validate ./processors/my-enricher/my-enricher.wasm

      - name: Register new version
        run: aeon processor register my-enricher ./processors/my-enricher/my-enricher.wasm
             --target ${{ secrets.AEON_CLUSTER_ADDR }}

      - name: Deploy (canary)
        run: aeon pipeline upgrade orders-pipeline --processor my-enricher:latest
             --target ${{ secrets.AEON_CLUSTER_ADDR }}
```

---

## 10. Kubernetes-Native Patterns

### 10.1 ConfigMap for Wasm Processors

Wasm artifacts are small enough (1–50KB) to store in ConfigMaps:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: aeon-processors
  namespace: aeon
binaryData:
  my-enricher.wasm: <base64-encoded .wasm>
  my-aggregator.wasm: <base64-encoded .wasm>
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: aeon
spec:
  template:
    spec:
      containers:
        - name: aeon
          image: aeon:latest
          ports:
            - containerPort: 4471    # HTTP API + health + metrics
              name: http
            - containerPort: 4470    # QUIC inter-node (multi-node cluster)
              protocol: UDP
              name: quic
          volumeMounts:
            - name: processors
              mountPath: /processors
              readOnly: true
      volumes:
        - name: processors
          configMap:
            name: aeon-processors
```

Update the ConfigMap → rolling restart picks up new processors.

### 10.2 PersistentVolumeClaim for Large Artifacts

For native `.so` processors or large Wasm modules:

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: aeon-processors-pvc
spec:
  accessModes: [ReadWriteMany]
  resources:
    requests:
      storage: 1Gi
```

### 10.3 Init Container for Processor Fetching

Pull processors from a registry (OCI, S3, etc.) at pod startup:

```yaml
initContainers:
  - name: fetch-processors
    image: aeon-processor-fetcher:latest
    command: ['sh', '-c', 'aeon processor pull my-enricher:v3 -o /processors/']
    volumeMounts:
      - name: processors
        mountPath: /processors
```

### 10.4 Future: Aeon K8s Operator

A custom Kubernetes operator that manages processor deployments declaratively:

```yaml
apiVersion: aeon.io/v1
kind: AeonPipeline
metadata:
  name: orders-pipeline
spec:
  source:
    type: redpanda
    topic: orders
  processor:
    name: my-enricher
    version: v3
    artifact: oci://registry.example.com/processors/my-enricher:v3
  sink:
    type: redpanda
    topic: enriched-orders
  upgrade:
    strategy: canary
    initialPercent: 10
```

The operator watches for changes to `AeonPipeline` resources and triggers upgrades
via the Aeon API.

---

## 11. Comparison with Existing Systems

| Aspect | AWS Glue | Apache Flink | Aeon (this design) |
|--------|----------|-------------|-------------------|
| Cold start | 1–3 min (serverless) | 10–30s (JobManager) | <100ms (Wasm) / <1ms (.so) |
| Job isolation | Full (separate cluster) | Per-slot (TaskManager) | Per-pipeline (hybrid tiers) |
| Upgrade impact | None (new cluster) | Savepoint + restart | Zero (canary/blue-green) or <100ms (drain-swap) |
| Job registry | AWS Glue Catalog | None (JAR uploads) | Built-in versioned registry (Raft-replicated) |
| Language support | Python, Scala (Spark) | Java, Python (PyFlink) | Any (via Wasm) + Rust/C/Go (.so) |
| Traffic splitting | N/A (batch) | N/A | Canary with auto-promote/rollback |
| Management interfaces | Web console, CLI, SDK | Web UI, CLI, REST | CLI + REST API + YAML manifest (all equivalent) |
| Integrity proof | N/A | N/A | PoH + Merkle (tamper-evident) |
| Cost model | Pay per DPU-hour | Cluster resources | Self-hosted (fixed infra) |

---

## 12. Implementation Phases

This design is implemented across Phases 12–14 of the Aeon roadmap:

- **Phase 12a — Core SDKs + Dev Tooling**: Rust Wasm + Rust native + TypeScript Wasm SDKs,
  `aeon new/build/validate`, `aeon dev` basic, Dockerfile.dev, benchmark gate.
- **Phase 12b — Additional Language SDKs** (post-Phase 14): Python, Go, Java, C#/.NET,
  PHP, C/C++ SDKs.
- **Phase 13a — Registry + Pipeline Core**: Registry (Raft-replicated), pipeline lifecycle,
  drain-swap upgrade, REST API (axum, CRUD, auth middleware), CLI management commands,
  deferred Phase 10 items (encryption-at-rest, cert expiry metric, RBAC).
- **Phase 13b — Advanced Upgrades + DevEx**: Blue-green, canary upgrades, YAML manifest
  (`aeon apply/export/diff`), `aeon top`, `aeon verify`, child process execution tier.
- **Phase 14 — Production Readiness**: Production Dockerfile, K8s manifests, Helm chart,
  CI/CD templates, systemd, rolling binary upgrade, K8s operator (future).

See `docs/ROADMAP.md` for phase details and acceptance criteria.

---

## 13. Remaining Work — Zero-Downtime Deployment & Management (2026-04-11)

Tracked in `docs/ROADMAP.md` as P10.

### 13.1 Bug Fixes — ✅ All Done (2026-04-11)

| # | Item | File(s) | Status |
|---|------|---------|--------|
| ~~ZD-1~~ | ~~`POST /api/v1/processors` route missing~~ | `aeon-engine/src/rest_api.rs` | **Fixed** — route + handler + test added (19 REST tests) |
| ~~ZD-2~~ | ~~CLI sends PascalCase enum values~~ | `aeon-cli/src/main.rs` | **Fixed** — `"wasm"`, `"native-so"`, `"available"` (kebab-case) |
| ~~ZD-3~~ | ~~SHA-512 hash is a placeholder~~ | `aeon-engine/src/registry.rs` | **Fixed** — `sha2::Sha512::digest()` replaces `DefaultHasher` |

### 13.2 Processor Hot-Swap Orchestration (Enables Zero-Downtime for T1 .so and T2 Wasm)

> ✅ **ZD-4 Done** (2026-04-11) — `PipelineControl` + `run_buffered_managed()` implemented.

| # | Item | File(s) | Details |
|---|------|---------|---------|
| ~~ZD-4~~ | ~~Wire drain→swap→resume into pipeline runner~~ | `aeon-engine/src/pipeline.rs` | **Done** — `PipelineControl` (paused flag + drain/swap Notify + processor slot), `run_buffered_managed()` with source pause check and processor swap loop. `Source::pause()`/`resume()` trait methods added with MemorySource/KafkaSource overrides. 2 tests: hot-swap zero-loss + managed-no-swap. |
| ~~ZD-5~~ | ~~Blue-green runtime wiring~~ | `aeon-engine/src/pipeline.rs` | **Done (2026-04-11)** — `PipelineControl.start_blue_green()` installs green processor via `UpgradeAction` slot; processor task picks it up (green installed, blue remains active). `cutover_blue_green()` swaps green→active. `rollback_upgrade()` drops green. REST `/cutover` and `/rollback` call PipelineControl when handle exists. 2 tests: cutover + rollback. |
| ~~ZD-6~~ | ~~Canary traffic splitting~~ | `aeon-engine/src/pipeline.rs` | **Done (2026-04-11)** — `PipelineControl.start_canary(proc, pct)` installs canary processor; processor task splits events by `event.id % 100 < pct` (deterministic). `set_canary_pct()` adjusts percentage via `AtomicU8`. `complete_canary()` promotes canary to sole processor. 2 tests: canary-split + canary-complete. |

### 13.3 Source/Sink Zero-Downtime Reconfiguration

| # | Item | Approach | Details |
|---|------|----------|---------|
| ZD-7 | Same-type source config change | Drain→swap→resume | **Done (2026-04-11)** — `PipelineControl.drain_and_swap_source()`: pause source → drain rings → downcast `Box<dyn Any>` to concrete `S` → swap → resume. Test: `managed_pipeline_source_swap`. |
| ZD-8 | Same-type sink config change | Drain→swap→resume | **Done (2026-04-11)** — `PipelineControl.drain_and_swap_sink()`: pause source → drain rings → downcast `Box<dyn Any>` to concrete `K` → swap in `run_sink_task` → resume. Test: `managed_pipeline_sink_swap`. |
| ZD-9 | Cross-type connector change | Blue-green pipeline | Different generic types → cannot swap within pipeline. Use `PipelineManager` blue-green: start new pipeline with new connectors, cutover, stop old. Already supported by state machine (ZD-5 must be wired first). |

### 13.4 Additional Improvements

| # | Item | Details |
|---|------|---------|
| ZD-10 | In-flight batch replay on T3/T4 disconnect | When a T3/T4 session disconnects during upgrade, in-flight batches awaiting responses fail with "session closed". Need: track unanswered batches, replay to new session. |
| ZD-11 | Wasm state transfer on hot-swap | Per-instance in-memory state (`HostState.state` HashMap) is lost on swap. Option: serialize state before swap, inject into new instance. Low priority — stateless processors preferred. |
| ~~ZD-12~~ | ~~Config file watcher (`aeon dev`)~~ | **Done (2026-04-11)** — `aeon dev watch --artifact <path>`. `notify` v7 watches parent dir (handles editor delete+recreate). 500ms debounce, loads processor via `WasmModule::from_bytes` or `NativeProcessor::load`, calls `PipelineControl.drain_and_swap()`. TickSource → StdoutSink dev loop. |
| ZD-13 | Child process isolation tier | Design in Section 2.3 is complete. Implementation: spawn child, IPC via Unix socket or shared memory, two-phase partition transfer. Low priority. |

### 13.5 TLS Certificate Handling (Verified Correct)

No code changes needed — documenting for reference:

- **`TlsMode::Auto`** (self-signed): single-node only. Explicitly blocked for multi-node by
  `validate_not_auto_with_peers()` which returns error: _"Use tls.mode 'pem' with a shared CA."_
- **`TlsMode::Pem`** (CA-signed PEM files): required for production. Supports any CA (self-signed
  internal CA for cluster QUIC, Let's Encrypt for external REST API via Ingress).
- **`CertificateStore::reload()`**: re-reads PEM files from disk without restart. Combined with
  cert-manager auto-renewal, certificates rotate seamlessly.
- **REST API (port 4471)**: plain TCP — no built-in TLS. External termination via K8s Ingress
  with Let's Encrypt is the production pattern. See `docs/CLOUD-DEPLOYMENT-GUIDE.md` §5.3.
- **Cluster QUIC (port 4470)**: mandatory mTLS via `rustls` + `quinn`. Multi-node clusters
  enforce `TlsConfig` with cert/key/ca PEM paths.
- **Connector TLS**: per-connector via `ConnectorTlsConfig` (None / SystemCa / Pem).
  Kafka/Redpanda connectors translate to rdkafka `ssl.*` settings.
