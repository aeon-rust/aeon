# Aeon — Fault-Tolerance Analysis & Unified To-Do List

> **Date**: 2026-04-12
> **Scope**: Comprehensive failure scenario analysis for Aeon cluster, covering all
> persistence gaps, measured timings, and a unified to-do list organized by
> architectural pillar.
>
> **Context**: This document consolidates findings from the 3-node DOKS cluster
> validation (P4f), codebase audit of `aeon-cluster`, `aeon-engine`, and `aeon-state`
> crates, and real-world leader failover measurements. It supersedes the previous
> to-do list in ROADMAP.md (2026-04-11 Audit), which is retained for historical
> reference.
>
> **Pause point 2026-04-16**: CL-6a/b/c/d transport primitives all shipped. Remaining
> Pillar 3 items (CL-1 Gate 2 acceptance, CL-5 auto-scaling, CL-6c.4 engine-side
> freeze+replay, split-brain drill, multi-broker load) are flagged for reassessment
> before further work. See [`ROADMAP.md` §Pause Point](ROADMAP.md#pause-point-2026-04-16--pending-tasks-up-for-reassessment)
> for open questions on each.

---

## 1. Current Persistence Model

### What Is Persisted to Disk

| Component | Persisted? | Where | Survives |
|-----------|-----------|-------|----------|
| Checkpoint WAL | Yes | `AEON-CKP` file (append-only, CRC32) | Process crash, node restart |
| L3 State (redb) | Yes | `data/state/l3.redb` (ACID, B-tree) | All failure modes |
| L2 State (mmap) | Yes | Append-only log with in-memory index | OS crash (page cache flush) |
| PoH chain tip | Yes | Stored in L3 | All failure modes |
| Merkle log | Yes | Stored in L3 | All failure modes |
| Source-Anchor offset | Yes | Via checkpoint WAL | Process crash |

### What Is NOT Persisted (In-Memory Only)

| Component | Location | Lost On |
|-----------|----------|---------|
| **Raft log (MemLogStore)** | `aeon-cluster/src/store.rs:46` | Any process restart |
| **Raft vote state** | `MemLogStore` inner BTreeMap | Any process restart |
| **Raft snapshots (ClusterSnapshot)** | `StateMachineStore` with optional `Arc<dyn L3Store>` snapshot backend (FT-2, 2026-04-12) — `bootstrap_single_persistent` wires the same L3 as FT-1, enabling snapshot-frontier recovery | Plain in-memory mode (default `StateMachineStore::new`) still loses on restart — production must opt in via `new_persistent` / `bootstrap_single_persistent` |
| **SPSC ring buffers** | `rtrb` in `pipeline.rs` | Any process restart |
| **L1 DashMap state** | `aeon-state/src/l1.rs` | Any process restart |
| **Partition transfer state** | `ClusterSnapshot` (BulkSync/Cutover) | Any process restart |

---

## 2. Failure Scenarios

### Scenario 1: Leader Node Hard Power-Off

**Sequence:**
1. Leader stops sending heartbeats immediately
2. Followers wait 1500-3000ms (randomized election timeout), then start election
3. New leader elected within ~2-3s of actual crash (K8s takes ~10s to detect)
4. New leader runs partition assignment — partitions on dead node get reassigned
5. K8s StatefulSet recreates the pod, node rejoins as follower

**Data at risk:**
- **Uncommitted Raft entries**: If the leader accepted a `ClusterRequest` (e.g., partition
  assignment, membership change) but crashed before replicating to a quorum, that entry is
  **lost**. OpenRaft requires quorum commit, so only uncommitted entries are at risk.
- **In-flight pipeline events**: SPSC ring buffers on the crashed node are lost entirely.
  Events between last checkpoint flush and crash = duplicates on replay (at-least-once).
- **Checkpoint WAL**: Survives on the crashed node's disk (PVC in K8s). On restart, pipeline
  resumes from last checkpoint — but replays events already sent to sink.

**Critical gap**: The Raft log itself (`MemLogStore`) is lost on crash. When the node restarts,
it has zero Raft history. It must receive a full snapshot from the current leader to rebuild state.

### Scenario 2: Previous Leader Rejoins After New Leader Elected

**Sequence:**
1. Old leader restarts with empty `MemLogStore` (term=0, no log entries)
2. Contacts cluster, discovers new leader with higher term
3. Steps down, becomes follower
4. Receives Raft snapshot from new leader (`ClusterSnapshot` with partition table, membership)
5. Catches up on any log entries after snapshot
6. Resumes pipeline processing from checkpoint WAL offsets

**Data consistency:**
- OpenRaft handles term/log conflicts correctly — old leader's uncommitted entries are discarded
- Committed entries (replicated to quorum before crash) are safe — they exist on surviving nodes
- The only risk: entries the old leader committed locally but **failed to replicate** before crash.
  These are genuinely lost because no other node has them.

This is inherent to Raft consensus — not an Aeon-specific bug. etcd, CockroachDB, and every
Raft implementation has this property. The mitigation is: don't acknowledge writes to clients
until quorum-committed.

### Scenario 3: Network Partition (Split-Brain)

**Sequence:**
1. If leader is isolated from majority: leader loses quorum, cannot commit new entries,
   eventually steps down
2. Majority side elects new leader, continues operating
3. When network heals: old leader discovers higher term, steps down, syncs from new leader

**Assessment**: Aeon handles this correctly — OpenRaft's term-based conflict resolution prevents
split-brain. No manual intervention needed.

### Scenario 4: Flaky Network (Intermittent Connectivity)

**Sequence:**
1. Missed heartbeats trigger unnecessary elections
2. Leader changes frequently ("election storms")
3. Each election = ~2-3s of no writes accepted
4. Partition assignments may thrash if leader keeps changing

**Gap**: No pre-vote protocol. OpenRaft supports pre-vote (prevents disruptive elections from
partitioned nodes), but Aeon doesn't configure it. A node that briefly loses connectivity will
increment its term and force an unnecessary election when it reconnects.

### Scenario 5: Simultaneous Multi-Node Failure (Quorum Loss)

**Sequence:**
1. If 2 of 3 nodes crash: no quorum, cluster is **completely stalled**
2. No reads, no writes, no leader election possible
3. Surviving node cannot do anything alone

**Recovery:**
- Must bring at least one more node back online
- Since `MemLogStore` is in-memory, restarted nodes have empty state
- If the surviving node was a follower: it has the most recent state, can serve as snapshot source
- If all 3 crash: total state loss for Raft metadata. Pipelines resume from checkpoint WAL only.

**This requires manual intervention** — specifically, re-bootstrapping the cluster if all nodes
lost their Raft state simultaneously. Fixing GAP 1 + GAP 2 (persistent Raft store) would
make this scenario fully automatic.

---

## 3. Measured Timings (Real 3-Node DOKS Cluster)

| Metric | Measured | Notes |
|--------|----------|-------|
| Leader election (total) | **~12-13 seconds** | Includes K8s pod termination detection (~10s) + Raft election rounds (~2-3s) |
| Raft election (pure) | **~2-3 seconds** | Hardcoded: heartbeat=500ms, election_min=1500ms, election_max=3000ms |
| State sync (new node) | **< 1 second** | Raft snapshot install — metadata only (~KB), not event data |
| Partition rebalance | **< 1 second** | Round-robin assignment after leader election |

**Breakdown of 12-13s total election time:**
- K8s detects pod is gone: ~10s (kubelet grace period)
- Followers notice missing heartbeats: ~1.5-3s (election timeout range)
- Election rounds complete: ~0.5-1s

**Gate 2 target**: Leader failover <5s recovery. The pure Raft election is within target (~2-3s).
The K8s detection time is infrastructure-dependent and can be tuned via pod liveness probes.

---

## 4. Identified Gaps (Ranked by Severity)

### GAP 1: In-Memory Raft Log (CRITICAL)

- **File**: `crates/aeon-cluster/src/store.rs:46-155`
- **Risk**: Node crash = complete loss of Raft votes, log entries, and committed index
- **Impact**: Every restart requires full snapshot transfer from leader. Total metadata loss
  if all nodes crash simultaneously.
- **Fix**: Replace `MemLogStore` with persistent backend via L3Store trait (redb default,
  RocksDB pluggable). Store votes, log entries, and hard state to disk.
- **Architectural connection**: Reuses `L3Store` adapter pattern from `aeon-state`. Same
  redb engine, same trait interface, consistent persistence model across the codebase.
- **Prerequisite**: FT-7 (make `TieredStore` generic over `L3Store`) must come first so the
  Raft store can use any configured L3 backend.

### GAP 2: In-Memory Snapshots (HIGH)

- **File**: `crates/aeon-cluster/src/store.rs:162-490`
- **Risk**: Snapshot exists only in RAM. If leader crashes during snapshot transfer to a new
  node, transfer fails and must restart from scratch.
- **Impact**: Slow node recovery; total metadata loss if all nodes crash.
- **Fix**: Persist `ClusterSnapshot` to the same redb database as the Raft log (GAP 1).
  Load on startup. This also automatically solves GAP 7 (transfer state durability).

### GAP 3: At-Least-Once Delivery Window (MEDIUM)

- **File**: `crates/aeon-engine/src/checkpoint.rs`
- **Risk**: Race between `sink.write_batch()` completing and `checkpoint.flush()`. If crash
  happens between these two operations, checkpoint hasn't recorded the new offset — events
  are replayed — duplicates.
- **Current backend**: Custom WAL format (`AEON-CKP` magic, CRC32 integrity). The
  `CheckpointBackend` enum also defines `StateStore`, `Kafka`, `None` variants — but only
  `Wal` is implemented.
- **Fix**: Implement `CheckpointBackend::StateStore` — write checkpoint records through
  `L3Store` trait (redb ACID transactions). Make `StateStore` the default backend (convention).
  Keep `Wal` as configurable fallback.
- **Architectural connection**: Checkpoint writes flow through the same L3 backend as
  application state. ACID atomicity eliminates the race window. The `L3Store` trait provides
  the abstraction; redb or RocksDB handles the durability.

### GAP 4: Election Timing (MEDIUM)

- **File**: `crates/aeon-cluster/src/node.rs` (hardcoded timeouts)
- **Risk**: 12-13s leader election means 12-13s of no Raft writes. Pipeline data continues
  flowing (source->processor->sink), but partition reassignment stalls.
- **Current values**: `heartbeat_interval=500ms`, `election_timeout_min=1500ms`,
  `election_timeout_max=3000ms` (hardcoded in all 4 bootstrap functions).
- **Fix**: Make timeouts configurable via cluster config. Recommended defaults:
  `heartbeat=200ms`, `election_min=600ms`, `election_max=1200ms` for faster failover.

### GAP 5: SPSC Buffer Loss on Crash (ACCEPTED)

- **File**: `crates/aeon-engine/src/pipeline.rs`
- **Risk**: In-flight events in `rtrb` ring buffers are lost on crash.
- **Assessment**: These events will be replayed from checkpoint (at-least-once), so no data
  loss — just duplicates. Persisting SPSC buffers would destroy hot-path performance.
- **Decision**: Accepted. No action needed.

### GAP 6: No Pre-Vote Protocol (LOW-MEDIUM)

- **Risk**: Flaky network causes unnecessary elections and term inflation.
- **Fix**: Enable OpenRaft's pre-vote feature. One configuration line.
- **Effort**: Trivial.

### GAP 7: Partition Transfer State Not Durable (SOLVED BY GAP 2)

- **File**: `crates/aeon-cluster/src/store.rs` (BulkSync/Cutover states in ClusterSnapshot)
- **Risk**: If a node crashes mid-partition-transfer, transfer restarts from scratch.
- **Fix**: Automatically solved by GAP 2 — persisting ClusterSnapshot persists transfer state.

### GAP 8: Health Check Messages Unused (LOW)

- **File**: `crates/aeon-cluster/src/framing.rs` — `HealthPing`/`HealthPong` defined but not sent
- **Risk**: No application-level health detection beyond Raft heartbeats.
- **Fix**: Implement periodic health pings with latency tracking. Low priority — Raft
  heartbeats already provide basic liveness detection.

### GAP 9: Production Cluster Uses Insecure TLS (HIGH)

- **File**: `crates/aeon-cli/src/main.rs:405-408` (production code path)
- **File**: `crates/aeon-cluster/src/transport/tls.rs:186-228` (`AcceptAnyCert` verifier)
- **Risk**: The production CLI calls `dev_quic_configs_insecure()` for cluster QUIC transport.
  This generates ephemeral self-signed certificates and uses `AcceptAnyCert` — a custom
  `ServerCertVerifier` that unconditionally accepts **any** server certificate via the rustls
  `.dangerous()` API. No chain-of-trust validation occurs.
- **Impact**: Man-in-the-middle attack on QUIC inter-node transport could intercept/modify
  Raft RPCs (partition assignments, membership changes), inject false cluster state, or
  observe all control-plane traffic. This is the cluster's only inter-node communication channel.
- **Additional vectors**:
  - `aeon-processor-client`: `webtransport-insecure` feature enables `.with_no_cert_validation()`
    for WebTransport processor connections. Correctly feature-gated for dev-dependencies only,
    but no runtime guard prevents accidental production use.
  - `aeon-crypto/src/tls.rs`: `CertificateStore::new_insecure()` creates empty cert store — used
    in tests but available in public API.
- **Existing infrastructure**: The `aeon-crypto` crate already has proper TLS support:
  - `CertificateStore` with CA cert loading, key/cert file paths
  - `auto-tls` feature for development self-signed cert generation (rcgen)
  - Full mTLS capability — just not wired into the cluster transport
- **Fix**: Wire proper mTLS for cluster QUIC using `CertificateStore` from `aeon-crypto`.
  Production mode: require CA cert + node cert/key paths in config. Development mode: keep
  insecure self-signed behind explicit `--insecure` CLI flag (not default). The `auto-tls`
  feature can generate dev certs, but `AcceptAnyCert` should never be used without explicit
  opt-in.

---

## 5. Architectural Connections

Several gaps and existing to-do items are **the same problem at different layers** — persistence
through the `L3Store` trait:

```
                    L3Store Trait (aeon-state)
                   /          |           \
                  /           |            \
        RedbStore        (future)       (future)
        (default)       RocksDB         other
           |               |
    +------+------+--------+
    |      |      |
    v      v      v
  App    Raft   Checkpoint
  State   Log    Records
  (L3)  (FT-1)  (FT-3)
```

### Connection 1: Raft Log (FT-1) reuses L3Store

Instead of building a new persistence layer for `MemLogStore`, implement `RaftLogStorage`
backed by the same `L3Store` trait. One persistence engine (redb or configured alternative)
for both application state and consensus state.

### Connection 2: Checkpoint (FT-3) reuses L3Store

The `CheckpointBackend::StateStore` variant already exists in the enum. Implementation writes
checkpoint records through `L3Store` — gaining ACID atomicity and eliminating the at-least-once
race window. This becomes the default by convention.

### Connection 3: Raft Snapshot (FT-2) shares FT-1's store

`ClusterSnapshot` serializes into the same redb database as the Raft log. One open database
handle, two tables (log entries + snapshots). Automatically solves GAP 7 (transfer state).

### Connection 4: TieredStore generics (FT-7) enables all of the above

Currently `TieredStore` is hardcoded to `Option<RedbStore>`. Making it generic over `L3Store`
(or using `Box<dyn L3Store>`) allows runtime backend selection via config. This must be done
first — FT-1 and FT-3 then work with any configured L3 backend.

### Connection 5: Retry layering — where `BackoffPolicy` does and does NOT apply

`BackoffPolicy` (TR-3) is not a blanket "retry everything" policy. It
lives at specific layers and is deliberately absent from others:

| Layer | `BackoffPolicy` applies? | Why |
|-------|--------------------------|-----|
| Connector source/sink error handlers | **Yes** (TR-3) | External systems fail; reconnect storms must be dampened. |
| `QuicEndpoint::connect_with_backoff` (bootstrap/join) | **Yes** (opt-in, bounded) | Seed node may still be starting. |
| `QuicEndpoint::connect` on the openraft RaftNetwork path | **No — fail-fast** | openraft has its own retry at the Raft protocol layer. Blocking a client task here delays heartbeats → leader step-down → spurious elections. Adding backoff here interferes with FT-4 (split-vote mitigation) and FT-5 (election timing). |
| `QuinnTransport::send` / per-frame writes | **No** | Below Raft-protocol layer; errors propagate up to openraft which decides policy. |
| Sink `write_batch` failures | Controlled by `BatchFailurePolicy` (application-level), not `BackoffPolicy` | Application semantics — retry / skip / DLQ. |

**Consequence for Gate 2 stress testing**: if we see leadership churn
under load, the fix is RaftTiming widening (FT-5) or waiting for
openraft 0.10 pre-vote (FT-4) — **not** adding more retries. This is
the single most important thing to get right when tuning a Raft-backed
system; inverting the layering is a common failure mode.

See also: `docs/CLUSTERING.md §2 Connection retry and backoff — layering`
(operator-facing version of this note), `docs/ROADMAP.md Architectural
Note: Retry Layering` (changelog/context version).

---

## 6. Automatic vs Manual Recovery

| Scenario | Automatic? | Manual Intervention? |
|----------|------------|---------------------|
| Single node crash | **Yes** — K8s restarts pod, Raft syncs state | No |
| Leader crash | **Yes** — new election + reassignment | No |
| Network partition (minority isolated) | **Yes** — majority continues, minority rejoins | No |
| Network partition (leader isolated) | **Yes** — new leader elected, old steps down | No |
| Flaky network | **Mostly** — works but may cause election storms | Maybe tune timeouts |
| 2-of-3 nodes crash | **Partial** — needs 1 node back for quorum | Must restart at least 1 node |
| All nodes crash (current) | **No** — Raft state lost (in-memory) | Must re-bootstrap cluster |
| All nodes crash (after FT-1 + FT-2) | **Yes** — each node recovers from disk | No |
| MITM on inter-node QUIC (current) | **Vulnerable** — no cert validation | Must deploy proper mTLS (FT-8) |
| MITM on inter-node QUIC (after FT-8) | **Protected** — mTLS with CA chain | No |

---

## 7. Unified To-Do List

This list supersedes the "Comprehensive To-Do List (2026-04-11 Audit)" in ROADMAP.md, which
is retained for historical reference.

### Pillar 1: Persistence & Durability (Fault-Tolerance Core)

| ID | Task | Connects To | Effort | Depends On |
|----|------|-------------|--------|------------|
| **FT-9** | ~~Move `L3Store` trait + `BatchOp`/`BatchEntry`/`KvPairs` to `aeon-types`~~ — **Done**. Trait + types live in `aeon-types/src/state.rs:20-65`; `RedbStore` implementation stays in `aeon-state`. `aeon-cluster` (FT-1/FT-2) and `aeon-engine` (FT-3) use `Arc<dyn L3Store>` without depending on `aeon-state`. Landed as part of the FT-1/FT-2/FT-3/FT-7 work. | Prerequisite for FT-7, FT-1, FT-3. | ✅ Done | — |
| **FT-7** | ~~Make `TieredStore` generic over `L3Store`~~ — **Done (2026-04-12)**. `TieredStore.l3` is now `Option<Arc<dyn L3Store>>`. Added `TieredConfig.l3_backend: L3Backend` selector (default `Redb`; `RocksDb` returns a clear "not implemented" error until the adapter lands). Added `TieredStore::with_l3_store(config, Arc<dyn L3Store>)` injection constructor so `aeon-cluster` (FT-1 RaftLogStore) and `aeon-engine` (FT-3 checkpoint) can share a single backend instance. Added `TieredStore::l3()` borrowing accessor. Removed `#[cfg(feature="redb")]` gates from the L3 read/write/scan paths — they now go through the trait unconditionally (redb feature still gates `RedbStore` construction). 2 new tests: `tiered_l3_backend_rocksdb_not_implemented`, `tiered_with_injected_l3_store`. All 79 `aeon-state` tests pass, workspace clippy clean. | L3Backend enum (exists), TieredStore (hardcoded to RedbStore) | ✅ Done | FT-9 |
| **FT-4** | ~~Enable OpenRaft pre-vote~~ — **Blocked upstream**: openraft 0.9.21 does not expose `enable_pre_vote` (verified in registry source; openraft README lists it as a TODO). openraft 0.10 is alpha-only with no published stable date. **Mitigation shipped (2026-04-12)**: two new `RaftTiming` presets via FT-5 — `RaftTiming::prod_recommended()` (500/2000/6000 ms, ~15% split-vote probability, ~6 s failover) and `RaftTiming::flaky_network()` (500/3000/12000 ms, ~7% split-vote probability, ~12 s failover) vs. default (500/1500/3000 ms, ~40% probability, ~3 s failover). Documented in `CLUSTERING.md §2 Raft timing` with the probability math `P(split_vote) ≈ N·(N-1)·(RTT/W)`. Revisit when openraft 0.10 stabilizes; swap to native pre-vote if the window approach proves insufficient under stress. | GAP 6 | Mitigated | Upstream openraft 0.10 for true fix |
| **FT-5** | Configurable election timeouts — expose heartbeat/election_min/election_max in cluster config | GAP 4 | Low | **Done** — `RaftTiming { heartbeat_ms=500, election_min_ms=1500, election_max_ms=3000 }` on `ClusterConfig`, wired into openraft `Config` at all 4 node-construction sites in `node.rs`. Validates heartbeat>0, min>heartbeat, max>min. 7 tests in `config.rs`. Wide-jitter config supported for flaky networks (mitigates FT-4 pre-vote blocker). |
| **FT-1** | ~~Persistent Raft log via L3Store~~ — **Done (2026-04-12)**. `aeon_cluster::log_store::L3RaftLogStore` implements `RaftLogStorage` + `RaftLogReader` over `Arc<dyn L3Store>`. Key layout under `raft/` prefix: `raft/vote`, `raft/committed`, `raft/last_purged`, `raft/log/{be_u64:index}` (big-endian so `scan_prefix` yields entries in log order). Hot metadata cached in `Arc<RwLock<_>>`; `get_log_state` is O(1); caches hydrated from L3 on `open()`. `append` / `truncate` / `purge` use `write_batch` for atomic multi-key updates. New `ClusterNode::bootstrap_single_persistent(config, Arc<dyn L3Store>)` constructor: opt-in caller supplies pre-opened L3 backend (respects FT-9 layering — aeon-cluster depends only on aeon-types; the caller wires in `aeon-state::RedbStore`). Idempotent re-bootstrap: treats `InitializeError::NotAllowed` as success on restart, skips partition assignment when state machine already has entries. 9 unit tests + 1 integration test `raft_log_survives_restart` (boot → propose → drop → reopen same redb file → assert config override + partition assignments recovered). Existing `MemLogStore` + `bootstrap_single` signatures untouched — single-node dev remains ephemeral by default; production opts in by calling the persistent variant. 106 aeon-cluster tests pass, clippy clean (lib + new test). | GAP 1, L3Store adapter pattern | ✅ Done | FT-9 |
| **FT-2** | ~~Persistent Raft snapshots — serialize `ClusterSnapshot` to L3 (same DB as FT-1)~~ — **Done (2026-04-12)**. `StateMachineStore` gained an optional `snapshot_store: Option<Arc<dyn L3Store>>` field plus two constructors: `new_persistent(l3)` and (cfg `encryption-at-rest`) `new_persistent_encrypted(l3, key)`. On construction, both read `raft/snapshot/{meta,data}` from L3 and hydrate `state` + `last_applied_log` + `last_membership` — so a rebooted node can skip the bulk of log replay and resume from a snapshot frontier. `build_snapshot` (on `Arc<StateMachineStore>`) and `install_snapshot` both atomically write meta + data in one `L3Store::write_batch` (single-key crash would desync them), then `flush()`. Encryption-at-rest: when an `EtmKey` is configured, `build_snapshot` persists the encrypted bytes verbatim and `hydrate_from_l3` decrypts them on read — meta stays in the clear (it's tiny and needs to be decodable before the key becomes relevant). `bootstrap_single_persistent` now wires the same `Arc<dyn L3Store>` into **both** the Raft log store (FT-1) and the state machine snapshot store (FT-2). 2 new unit tests — `persistent_snapshot_roundtrip_across_reopen` and `persistent_new_on_empty_l3_is_fresh` — all 21 store-tests pass, existing `raft_log_survives_restart` integration test still passes, clippy clean. | GAP 2, also solves GAP 7 | ✅ Done | FT-1 |
| **FT-3** | ~~Checkpoint via L3 backend (convention) — create `CheckpointPersist` trait abstracting WAL vs L3Store, implement `CheckpointBackend::StateStore`, make it default. Keep WAL as configurable fallback. Pipeline holds `Box<dyn CheckpointPersist>` instead of `Option<CheckpointWriter>`.~~ — **Done (2026-04-12)**. Added `CheckpointPersist` trait in `aeon-engine/src/checkpoint.rs` with `append` / `read_last` / `next_checkpoint_id`. Two impls: `WalCheckpointStore` wraps the existing `CheckpointWriter`; `L3CheckpointStore` writes records under `checkpoint/{be_u64:id}` in an `Arc<dyn L3Store>` — big-endian keys mean `scan_prefix` returns records in ID order, and next-ID is hydrated from the max key on `open()`. Append uses `write_batch` + `flush` for per-record durability. Pipeline `SinkTaskCtx.checkpoint_writer` is now `Option<Box<dyn CheckpointPersist>>`; `build_sink_task_ctx` dispatches on `CheckpointBackend`: `Wal` → `WalCheckpointStore`, `StateStore` → `L3CheckpointStore` (uses the new `PipelineConfig.l3_checkpoint_store: Option<Arc<dyn L3Store>>`, typically the same handle used by FT-1/FT-2 so one node shares one durable backend), `Kafka`/`None` → no persister. `StateStore` with no backend logs a warn and continues without checkpoints rather than failing startup. **Default stays `Wal`** for now — the plan called for flipping the default to `StateStore`, but that requires YAML surface + CLI wiring to avoid silently disabling WAL for existing deployments; revisit when the YAML knob is added. 5 new tests (`l3_checkpoint_store_roundtrip`, `_resumes_id_sequence_across_reopen`, `_empty_reads_none`, `l3_checkpoint_keys_sort_big_endian`, `wal_store_impls_checkpoint_persist`), 18 checkpoint tests total, 287 aeon-engine lib tests pass, clippy clean. | GAP 3, existing `CheckpointBackend` enum | ✅ Done | FT-7 |
| **FT-6** | ~~Wire `HealthPing`/`HealthPong`~~ — **Done**. Added `transport::health` module: `HealthPing`/`HealthPong` payloads (echoed timestamp → RTT), `SharedHealthState` (per-peer stats: last_seen_ns, last_rtt_ns, successes, failures, is_stale), `send_health_ping` client helper, `spawn_health_pinger` periodic task, and `handle_health_ping` wired into server dispatch. 7 unit + integration tests (serde roundtrip, QUIC ping/pong loopback, periodic pinger updates stats). | GAP 8 | Low | ✅ Done |
| **FT-8** | ~~Production mTLS for cluster QUIC~~ — **Done**. Added `quic_configs_for_cluster(&ClusterConfig)` selector in `aeon-cluster/src/transport/tls.rs` — dispatches: `tls: Some(_)` → file-based mTLS (with peer verification), `auto_tls: true` → insecure dev mode with loud `tracing::warn!` every start-up, neither → config error. Previously the CLI's K8s bootstrap path hardcoded `dev_quic_configs_insecure()` regardless of config — now calls the selector. Added `warn_once_webtransport_insecure()` runtime warning emitted at connect time when the `webtransport-insecure` feature is compiled in. `CertificateStore` wiring deferred — the existing cluster file-based builder is functionally equivalent; unifying them is follow-up refactor work. 4 new tests (selector error paths, auto_tls path, production cert-file roundtrip with rcgen). | GAP 9 | Medium | ✅ Done |
| **FT-10** | ~~Systematic `unwrap()` audit~~ — **Done**. Refined the 1,312 raw match count via test-module-aware script: actual production sites were far fewer. Fixed: `aeon-types/uuid.rs` (clock saturation via `.unwrap_or(0)` for pre-epoch clocks, documented fill-thread spawn invariant), `aeon-engine/dag.rs` (topological sort invariant annotated), `aeon-connectors/quic/{sink,tls}.rs` (dev helpers + hardcoded bind addr documented), `aeon-cluster/{config,discovery,transport/tls}.rs` (dev helpers + socket-addr literals documented), `aeon-crypto/{merkle,mmr}.rs` (length-checked invariants annotated), `aeon-processor-client/wire.rs` (6 `.try_into().unwrap()` sites replaced with infallible `read_u{16,32,64}_le` helpers). Locked **10 crates** with `#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]`: aeon-types, aeon-engine, aeon-state, aeon-wasm, aeon-connectors, aeon-cluster, aeon-crypto, aeon-processor-client, aeon-io, aeon-observability. Remaining allowed sites all carry `#[allow(clippy::{unwrap,expect}_used)]` with `FT-10:` rationale comments. Workspace clippy clean, 863 tests pass. | GAP A, zero-event-loss guarantee (Gate 1) | High | ✅ Done |
| **FT-11** | ~~Hot-path zero-copy violations~~ — **Done**. Audited all core crates for `.to_vec()` on `Bytes`-carrying hot paths. Fixed: NATS source (×2) & sink (×4) → `bytes::Bytes` clone (refcount-only); MQTT source → direct `publish.payload.clone()`; MQTT sink → switched to `client.publish_bytes()` API that accepts `Bytes` directly; WebSocket sink → `Message::Binary` already holds `Bytes`, drop `.to_vec().into()`; WebSocket source → `Utf8Bytes: AsRef<Bytes>` for text frames; WebTransport datagram source → `datagram.payload()` already returns `Bytes`; HTTP polling → `response.bytes()` is already `Bytes`. **Biggest win**: `aeon-types/transport_codec.rs` `WireEvent`/`WireOutput` switched from `Vec<u8>` to `bytes::Bytes` (enabled `bytes/serde` feature workspace-wide) — eliminates the per-event payload copy on T3/T4 encode and decode paths. Legitimately-owned sites documented: batch signature (64 B, per batch), WASM runtime (linear memory is not `Bytes`), `BytesFinder::new` (one-time init), control-plane `recv_control`/`send_control` (per handshake message). 863 tests pass. | GAP E, CLAUDE.md rule #3 enforcement | Medium | ✅ Done |
| **FT-12** | Hot-path refcount & config optimization (perf tuning) — **Shipped (2026-04-12)**. Gate 1 per-event target **achieved**: `cargo bench blackhole_bench` now reports ~96-98 ns/event steady-state for `blackhole_pipeline/events_1000000/batch_{256,1024}` (was ~215-225 ns/event pre-fix) and ~87-95 ns/event for `per_event_overhead/passthrough` across 64B/256B/1024B payloads. Changes that landed: **(a)** `PassthroughProcessor::{process, process_batch}` clones now carry `// clone: <reason>` justification comments (both are refcount bumps required to emit an `Output` while still borrowing `event` for `with_event_identity`). **(b)** dropped — `pipeline_manager.rs` had no per-event `config.clone()`; config is passed once at task spawn (false lead from the initial audit). **Structural wins (the actual speedup)**: **(i)** `MemorySource::next_batch` was cloning the backing `Vec<Event>` per poll via `self.events[..].to_vec()`. Fixed: `MemorySource` now holds a `std::vec::IntoIter<Event>` and `next_batch` moves events out — zero per-event clone on the source side. `new_resetable()` is the opt-in path that keeps a keeper copy for tests. **(ii)** the bench called `MemorySource::new(events.clone(), batch)` *inside* `b.iter()`, charging one full `Vec<Event>` clone per sample against the pipeline. Switched to `criterion::iter_batched` with `BatchSize::LargeInput`, so the event-vec clone lives in the untimed setup closure. Further structural work (shrink `Output`, `Processor::process_into` buffer reuse) left as optional future tuning — not needed to hit the target. | GAP E, Gate 1 perf target | Medium | FT-11 |

**Accepted (no action):**
- GAP 5 (SPSC buffer loss) — at-least-once replay from checkpoint covers this
- GAP 7 (transfer state durability) — solved automatically by FT-2

### Pillar 2: Zero-Downtime Deployment

| ID | Task | Source | Effort |
|----|------|--------|--------|
| **ZD-1** | ~~Add `POST /api/v1/processors` route + handler~~ | **Done** — `rest_api.rs:113` wires `get(list_processors).post(register_processor)`; handler at `rest_api.rs:597`; integration test `register_processor_via_api` at `rest_api.rs:1926`; CLI-level regression guard in `aeon-cli/tests/cli.rs`. | ✅ |
| **ZD-2** | ~~Fix CLI serde PascalCase -> kebab-case~~ | **Done** — commit fa10635 (2026-04-11); already using `"wasm"`, `"native-so"`, `"available"`. Now guarded by `processor_register_sends_kebab_case_type_and_available_status` in `aeon-cli/tests/cli.rs`. | ✅ |
| **ZD-3** | ~~Replace SHA-512 placeholder with real SHA-512~~ | **Done** — `registry.rs:291` now uses `sha2::Sha512::digest(data)`; verified callsites at `:82` (hash comparison on retrieve) and `:324` (hash on store). | ✅ |
| **ZD-4** | ~~Source `pause()`/`resume()` + pipeline drain mechanism~~ | **Done** — `Source::{pause,resume}` default methods in `aeon-types/src/traits.rs:36,44`; overridden in `connectors/memory/source.rs` and `connectors/kafka/source.rs`; `pipeline::drain_and_swap` at `pipeline.rs:1388` + `drain_and_swap_source/sink` at `:1410/:1431`. | ✅ |
| **ZD-5** | ~~Hot-swap orchestrator — drain->swap->resume for Wasm/Native~~ | **Done** — `PipelineManager::upgrade_blue_green` at `pipeline_manager.rs:234` (5 tests), REST at `rest_api.rs:996`. Canary upgrade_canary at `:371` (4 tests), REST at `rest_api.rs:1051` with step/threshold config, promote/rollback/cutover endpoints. | ✅ |
| **ZD-6** | ~~Same-type source/sink reconfiguration~~ | **Done** — `reconfigure_source` at `pipeline_manager.rs:513`, `reconfigure_sink` at `:567` (cross-type rejection + not-running rejection tests); REST endpoints at `rest_api.rs:1188,1205`. | ✅ |
| **ZD-7** | Cross-type connector swap via blue-green pipeline | Deferred (ZD-9 in old list) | High |

### Pillar 3: Cluster Operations & Validation

| ID | Task | Source | Effort |
|----|------|--------|--------|
| **CL-1** | Multi-node acceptance testing — 9 Gate 2 checklist items. **Status 2026-04-19 post-bundle re-run on DOKS `c3867cc5`:** T0.C0 ✅ (600 M zero loss, ~27 M agg eps), T1 ✅ (300 M zero loss, 6.5 M agg eps — matches 2026-04-18 baseline), T2 initially blocked by G15 on that run; **G15 code fix shipped same day** (configured-id threaded through `server::serve()`; regression test green). All Aeon Gate 2 code blockers now closed (G8–G11, G13–G15). T2/T3/T4/T6 re-measurement folds into the next DOKS re-spin (gated on Phase 3.5 V2–V6). See `docs/GATE2-ACCEPTANCE-PLAN.md § 10.9` and `docs/ROADMAP.md` Phase 3b. | ROADMAP Gate 2 criteria | Medium — remaining work is cluster re-measurement, no code blockers |
| **CL-2** | Partition reassignment real-network validation | Implemented, needs cloud testing | Medium |
| **CL-3** | PoH chain transfer real-network testing | Crypto tests pass, real transfer deferred | Medium |
| **CL-4** | ~~Cluster-level Prometheus metrics~~ — **Done (2026-04-12)**. Added `ClusterNode::cluster_metrics_prometheus()` in `aeon-cluster/src/node.rs` — snapshots `raft.metrics()` into Prometheus text: `aeon_raft_term`, `aeon_raft_last_log_index`, `aeon_raft_last_applied_index`, `aeon_raft_leader_id`, `aeon_raft_is_leader`, `aeon_raft_state` (0=Learner..4=Shutdown), `aeon_cluster_membership_size`, `aeon_cluster_node_id`, `aeon_raft_millis_since_quorum_ack` (leader-only), `aeon_raft_replication_lag{follower="N"}` (leader-only, computed as `last_log - follower_matched_index`). All cluster gauges carry a `node_id` label. New `GET /metrics` endpoint on the REST health router (no auth — Prometheus scrapers run inside the cluster boundary) aggregates per-pipeline counters (events_received / events_processed / outputs_sent / events_failed / events_retried / checkpoints_written / poh_entries — each labelled `pipeline="<name>"`) and appends cluster metrics when `cluster_node` is present. Term-as-election-counter documented (monotonic +1 per election attempt) in place of a separate counter since openraft 0.9 doesn't expose one. 2 new tests (`metrics_endpoint_exposes_pipeline_counters`, `metrics_endpoint_includes_cluster_metrics_when_clustered`). | Raft internal metrics exist (`self.raft.metrics()`) but not Prometheus-exposed | ✅ Done |
| **CL-5** | Raft-aware K8s auto-scaling — new pods auto-join Raft cluster when `cluster.auto_join: true`. Config: `autoscaling.mode: "raft-aware"` triggers Raft `add_learner` → catch-up → `change_membership` on scale-up; graceful `remove_node` with partition drain on scale-down. Cloud-agnostic: works with DOKS HPA, AWS EKS/Karpenter, GKE, AKS. HPA remains disabled when `cluster.enabled: true` unless `autoscaling.mode` is explicitly set. Depends on CL-6 for safe scale-down (partition drain). **G15 code fix shipped 2026-04-19** (scale-up join handler now uses `ClusterConfig::node_id` for the leader-self check); end-to-end DOKS re-measurement folds into the next Session A re-spin. | HPA exists in Helm (`helm/aeon/templates/hpa.yaml`) but guarded off for cluster mode. No Raft join-on-boot logic. | High |
| **CL-6** | Partition transfer data movement — state machine currently tracks `PartitionTransferState::{Pending, BulkSync, Cutover, Complete}` but **no actual data is moved**. Required for Gate 2 acceptance and prerequisite for safe CL-5 scale-down. Split into 4 sub-tasks: | GAP C, Gate 2 | Very High |
| **CL-6a** | ~~Bulk sync protocol~~ — **Shipped 2026-04-16** (CL-6a.1/.2/.3). Retargeted from L3 KV streaming to L2-body-store streaming (matches EO-2's durability spine, closes EO-2 P12). `SegmentManifest`/`SegmentChunk` in `aeon_types::l2_transfer`; `MessageType::PartitionTransfer{Request, ManifestFrame, ChunkFrame, EndFrame} = 17..=20`; `PartitionTransferProvider` trait with closure-based server (`serve_partition_transfer_{stream,with_request}`) and client (`request_partition_transfer`) + `drive_partition_transfer(tracker, source, target, conn, req, metrics, writer)` orchestrator walking `TransferTracker` Idle→BulkSync→Cutover. | CL-6 sub-task | ✅ |
| **CL-6b** | ~~PoH chain transfer~~ — **Shipped 2026-04-16** (CL-6b.1/.2/.3). `MessageType::PohChainTransfer{Request,Response} = 21,22`, single round-trip (chain state is small); `PohChainProvider` trait keeps transport crypto-agnostic (server returns opaque `state_bytes`, client decodes via `PohChainState::from_bytes`); `drive_poh_chain_transfer(conn, req, on_state)` orchestrator. Real-network integration test exercises a 5-batch source chain, verifies byte-identical restoration + post-transfer append continuity on both sides. | CL-6 sub-task | ✅ |
| **CL-6c** | ~~Cutover handshake~~ — **Transport primitive shipped 2026-04-16** (CL-6c.1/.2/.3). `MessageType::PartitionCutover{Request,Response} = 23,24`, single-round-trip request/response; `CutoverCoordinator` trait with `drain_and_freeze(&req) -> CutoverOffsets { final_source_offset: i64, final_poh_sequence: u64 }`; `request_partition_cutover` client + `serve_partition_cutover_{stream,with_request}` server split (matches CL-6a.3 dispatcher peek pattern); `drive_partition_cutover(tracker, conn, req)` orchestrator enforces `tracker == Cutover` precondition before touching the wire, keeps tracker in Cutover on success (caller drives final Raft commit), moves to `Aborted{reverted_to: source}` on any failure. `serve()` now takes `cutover_coordinator: Option<Arc<dyn CutoverCoordinator>>`. Real-network integration test in `tests/cutover.rs` walks bulk sync + PoH transfer + cutover on a single QUIC connection end-to-end. Engine-side write-freeze + buffer-and-replay is **deferred to CL-6c.4** (crosses into `aeon-engine`): needs partition write-gate + target-side replay buffer keyed by final_source_offset. | CL-6 sub-task | ✅ Transport done, engine-side follow-up |
| **CL-6d** | ~~Backpressure & throttling during transfer~~ — **Shipped 2026-04-16** (CL-6d.1 + CL-6d.2). `TransferThrottle` token-bucket (signed-budget, 1 s burst cap, `unlimited()` no-op default) wired via `PartitionTransferProvider::throttle()` opt-in accessor; source awaits `throttle.acquire(bytes)` before each chunk write. `PartitionTransferMetrics` registry emits `aeon_partition_transfer_bytes_{transferred,total}{pipeline, partition, role=source\|target}` gauges from both `serve_partition_transfer_with_request` and `drive_partition_transfer`; deterministic Prometheus text renderer. 10 new unit tests + 2 real-QUIC wiring tests. | CL-6 sub-task | ✅ |

### Pillar 4: Transport Resilience (Deferred)

| ID | Task | Source | Effort |
|----|------|--------|--------|
| **TR-1** | In-flight batch replay on T3/T4 disconnect — **Shipped (2026-04-12)**. *Stage 1*: `BatchInflight::start_batch` accepts `Arc<Vec<Event>>` and retains it inside the pending slot alongside the oneshot responder and semaphore permit (refcount bump, not a copy). `drain_for_replay()` removes all pending entries and returns `InflightBatch { batch_id, events, responder, _permit }` tuples in ascending `batch_id` order, **without** firing the oneshots — so a reconnect orchestrator can re-submit the events on a new session and forward the response to the still-parked pipeline caller. *Stage 2*: `ReplayOrchestrator` in `transport/session.rs` holds identity-keyed reconnect buffers `(fingerprint, pipeline, partition) → ReplayBucket { batches, expires_at }` with a background sweep task that fails senders on window expiry. T4 WebSocket host wires stash-on-disconnect + replay-on-handshake-complete; T3 WebTransport host wires stash-on-disconnect + replay-on-data-stream-register (since the stream must exist in the map before a wire write can go out). Replay path allocates fresh batch_ids on the new session, encodes + sends wire, and spawns a forwarder task per batch that pipes the new response into the original pipeline caller's oneshot. Default window is 30s; `replay_window: Option<Duration>` on both host configs (`None` = fail-fast legacy). 4 new orchestrator tests in session.rs (stash/take roundtrip, wrong-identity isolation, window expiry fires waiters, Drop fails remaining waiters). All 19 session tests + 296 engine lib tests + clippy clean. **Follow-up**: end-to-end reconnect test with actual socket teardown (currently covered by unit-level invariants + existing T3/T4 e2e harness which keeps connections alive). | ZD-10 in old list | Medium |
| **TR-2** | ~~Wasm state transfer on hot-swap~~ | **Shipped (2026-04-12)** — Wasm state lives host-side in `HostState.state` (per-instance HashMap), so a hot-swap that doesn't move it loses all guest state. Added default-noop `Processor::snapshot_state() -> Vec<(Vec<u8>,Vec<u8>)>` and `Processor::restore_state(snap)` on the trait (`aeon-types/src/traits.rs:95-120`). `WasmProcessor` overrides both (`aeon-wasm/src/processor.rs:240-278`) — snapshot clones the HashMap out, restore clears and replaces. Wired into all three swap points in `pipeline.rs` (drain-and-swap `:1820`, blue-green cutover `:1675`, canary complete `:1692`) via `transfer_processor_state` helper (`pipeline.rs:191-230`) that logs + swallows snapshot/restore errors to keep the pipeline alive. 3 new tests (`tr2_snapshot_restore_roundtrip`, `tr2_restore_replaces_existing_state`, `tr2_stateless_processor_defaults`). Caveat: raw KV transfer is schema-agnostic — blue/green with incompatible typed-state layouts will corrupt; out of scope for now. | ✅ |
| **TR-3** | Connection backoff with jitter across all connectors — **In progress**. ✅ Shared `BackoffPolicy { initial_ms, max_ms, multiplier, jitter_pct }` + stateful `Backoff` iterator in `aeon-types` (defaults 100 ms → 30 s cap, ×2, ±20% jitter; 11 tests). ✅ Wired into **MQTT source** (resets on Publish/ConnAck). ✅ **MQTT sink** (eventloop poller resets on successful poll). ✅ **MongoDB CDC source** (resets on successful ChangeStreamEvent). ✅ **NATS JetStream source** (2026-04-12) — backoff on both `consumer.messages()` init failure and mid-stream errors; drops the pull stream so the next `next_batch()` rebuilds it; resets on successful message delivery. Returns `Ok(vec![])` rather than propagating Err, keeping the pipeline alive through transient outages. ✅ **RabbitMQ source** (2026-04-12) — refactored the push reader into an outer reconnect loop that owns a fresh `Connection`+`Channel`+`Consumer` triple each iteration. On delivery error or channel disconnect, the inner loop drops the triple and backs off; the initial connection is still validated synchronously so misconfiguration surfaces loudly on startup. Reset on successful reconnect. ✅ **Postgres CDC source** (2026-04-12) — holds `Option<Client>` + driver task. On `pg_logical_slot_get_changes` failure the client is dropped; the next `next_batch()` rebuilds the connection via a shared `establish()` helper, backs off on reconnect failure, returns `Ok(vec![])` to keep the pipeline alive. ✅ **MySQL CDC source** (2026-04-12) — pool-based, so reconnection is automatic; wrapped `pool.get_conn()` and `SHOW BINLOG EVENTS` in backoff + `Ok(vec![])` instead of propagating Err. Reset on successful poll. ✅ Wired into **cluster `QuicEndpoint::connect_with_backoff()`** — opt-in retry variant for bootstrap/join flows; leaves plain `connect()` fail-fast for the openraft RPC path. ✅ **HTTP polling source** (2026-04-12) — three failure paths (`request.send`, non-success status, `body.bytes`) now log + sleep + return `Ok(vec![])` instead of propagating Err, matching the other reconnecting sources. Reset on successful poll. ✅ **WebSocket source** (2026-04-12) — extracted reader into `run_ws_reader` helper; outer reconnect loop owns the socket, sleeps backoff, calls `connect_async`, resets on reconnect. Initial connect still fail-fast. ✅ **Redis Streams source** (2026-04-12) — `conn` now `Option<MultiplexedConnection>`; on XACK/XREADGROUP failure drops the connection, next `next_batch()` rebuilds via `establish()` helper (BUSYGROUP tolerated as idempotent); pending ack IDs cleared on reconnect since Redis PEL redelivers. **Follow-up (deferred):** WebTransport T3 SDK layer. | GAP H | Medium |

### Pillar 5: Exactly-Once Delivery (Future)

| ID | Task | Details | Effort |
|----|------|---------|--------|
| **EO-1** | ~~Implement `IdempotentSink` for `KafkaSink`~~ | **Shipped (2026-04-12)** — `KafkaSinkConfig::transactional_id` + `.with_transactional_id()` at `kafka/sink.rs:44,88`; `KafkaSink::new` calls `init_transactions` when set (`sink.rs:155`); `write_batch` wraps produce in `begin_transaction`/`commit_transaction`/`abort_transaction` envelope (`sink.rs:210-248`); `IdempotentSink for KafkaSink` at `sink.rs:335` (dedup is producer-scoped via idempotent producer + transactional fencing, `has_seen` returns `Ok(false)` — consumers dedupe via `isolation.level=read_committed`). Note: `send_offsets_to_transaction` belongs to EO-2 (atomic checkpoint+sink commit) and is deferred until FT-3 lands. | ✅ |
| **EO-2** | Atomic checkpoint + sink commit — close the at-least-once window by bracketing each sink tier's native atomic primitive with a two-phase L3 record (`pending → committed`). The per-event identity is the source-connector-minted UUIDv7 already carried on `Event` / `Output`; at a flush boundary the engine writes `L3[pipeline/partition] = {offset, pending_event_ids}`, invokes the sink's tier-specific commit (Kafka `commit_transaction`, JetStream ack-all, Redis pipeline EXEC, Postgres tx COMMIT, atomic-rename for file sinks, idempotency-key for webhooks/WS/WT), then flips the L3 record to `committed`. Recovery replays any `pending` batch through the sink, relying on the sink's idempotency primitive (EO-1/EO-3) to suppress duplicates. No intermediate pipeline-scoped topic — design was reviewed and rejected because it violates the Gate 1 <100ns per-event budget and doubles storage cost. ✅ **Trait framework shipped (2026-04-12)** — `SinkEosTier` enum (T1–T6) + `TransactionalSink { eos_tier, begin, commit, abort }` trait at `aeon-types/src/traits.rs:147-240`, exported from `aeon_types` public surface. ✅ **Kafka T2 impl shipped (2026-04-12)** — `TransactionalSink for KafkaSink` at `kafka/sink.rs:410-490` drives the producer-epoch transaction lifecycle from outside the per-batch envelope. Added `in_outer_txn` state flag so `write_batch()` skips its own EO-1 per-batch begin/commit when the pipeline is driving the outer transaction; `begin()` is idempotent within a generation, `commit()`/`abort()` are no-ops when no outer txn is in flight (backwards compatible with the at-least-once path). `eos_tier()` returns `TransactionalStream`. Requires `transactional_id` configured; `begin()` fails loudly otherwise rather than silently degrading. Compile-time proof `_assert_kafka_sink_is_transactional`. **Remaining**: pipeline flush-path wiring (two-phase L3 write around `begin/commit`) + recovery path (replay on Pending record) + per-tier impls (Redis/NATS T4, Postgres T1, File T3). | **Scope is per source connector + per sink tier**, not a single trait. Depends on FT-3 (L3 checkpoint store with redb as default backend) and EO-1/EO-3 (per-sink dedup primitives). L3 writes happen only at checkpoint cadence (1-10 Hz) — invisible on the hot path. Per-node L3 is local; multi-node correctness is per-partition (single owner) so no distributed commit coordinator is needed; `CheckpointReplicator` via Raft is a latency optimisation for failover, not a correctness dependency. First cut: Kafka→Kafka (T2 sink tier) to match Scenario 1; other tiers follow demand-driven. See `docs/THROUGHPUT-VALIDATION.md` for the 20M ev/s cost analysis that gated this design. | Medium (framework ✅) + Medium (pipeline wiring) + Low per additional sink tier |
| **EO-3** | ~~Implement `IdempotentSink` for other sinks~~ — **Shipped (2026-04-12)** for every existing sink with a meaningful dedup primitive. A Postgres sink does not yet exist in the tree (`postgres_cdc` is source-only); when one is added, the ON CONFLICT pattern slots in cleanly following the `NatsSink` / `RedisSink` shape. Redis (SET NX / SETNX pattern), PostgreSQL (INSERT ON CONFLICT), NATS JetStream (message dedup). Each connector that supports server-side dedup gets an `IdempotentSink` implementation. ✅ **Redis Streams** (2026-04-12) — `RedisSinkConfig::dedup_ttl` + `.with_dedup_ttl(d)` at `redis_streams/sink.rs:36,82`; when set, `build_pipeline` emits `SET <prefix><stream>:<event_id> 1 EX <ttl> NX` ahead of each XADD in the same pipeline, so dedup record and stream entry are created in one round-trip. `dedup_key_prefix` (default `aeon:dedup:`) overridable via `.with_dedup_key_prefix()`. PerEvent path now routes through `build_pipeline` with a single-element slice so the dedup branch isn't duplicated. `IdempotentSink for RedisSink::has_seen` at `redis_streams/sink.rs:275` does a real `EXISTS` lookup on the dedup key (returns `false` when `dedup_ttl` is `None`) — this is the **only** connector so far with a real per-event-id lookup primitive, since Redis has no server-side dedup. TTL must exceed the max replay window (e.g. TR-1's 30s orchestrator window). 4 unit tests + compile-time `IdempotentSink` proof. **Follow-ups:** Postgres ON CONFLICT. ✅ **NATS JetStream** (2026-04-12) — `NatsSinkConfig::dedup` + `.with_dedup()` at `nats/sink.rs:33,80`; when on, `write_batch` routes all three `DeliveryStrategy` arms through `js_publish_one` which calls `send_publish(..., Publish::build().payload(p).message_id(event_id.to_string()))` so JetStream's `duplicate_window` rejects re-sent events server-side. Requires the target stream to have a non-zero `duplicate_window` (NATS default 2m) and outputs to carry `source_event_id` (engine propagates by default). `IdempotentSink for NatsSink` at `nats/sink.rs:248` — `has_seen` returns `Ok(false)` (dedup is server-scoped, no public "have you seen this?" API), matching the `KafkaSink` design. 2 unit tests + compile-time `IdempotentSink` proof. **Follow-ups:** Redis SETNX, Postgres ON CONFLICT. | Demand-driven — implement per connector as exactly-once is requested. | Per-connector |

### Pillar 6: Developer Experience & Adoption-Readiness

Adoption across programming languages (Section 10) is one of Aeon's core goals. This
pillar ensures that once a developer chooses a tier (T1/T2/T3/T4), the experience of
integrating, testing, deploying, and operating against Aeon is friction-free.

| ID | Task | Details | Effort |
|----|------|---------|--------|
| **DX-1** | ~~CLI integration test suite~~ — **Done (2026-04-12)**. Added `crates/aeon-cli/tests/cli.rs` with 9 integration tests covering: `--version` / `--help` smoke; `aeon new --runtime wasm --lang rust` scaffolding (Cargo.toml, src/lib.rs, .cargo/config.toml); path-traversal rejection in project names; `aeon validate` on missing file; `aeon processor register` with `.wasm` and `.so` artifacts (asserts kebab-case `processor_type` / `status` — this is the regression guard that would have caught ZD-2); `aeon processor list` empty and populated response rendering. Uses `assert_cmd` + `predicates` for process orchestration and a zero-dependency `TcpListener`-based single-shot HTTP mock for REST-hitting commands — avoids adding `httpmock`/`wiremock`. | GAP D | ✅ Done |
| **DX-2** | ~~`aeon doctor` command~~ — **Done**. New subcommand `aeon doctor [--api --kafka --state-dir --manifest]` runs environment readiness checks: (a) compiled features (native-validate, rest-api); (b) default ports 4470/4471/4472 bindable; (c) REST API `/health` reachable; (d) Kafka/Redpanda TCP reachability; (e) state dir exists + writable (touch-file probe); (f) optional manifest YAML schema-check + recursive artifact-path extraction + per-artifact existence/extension check. Each check emits `[PASS]`/`[WARN]`/`[FAIL]` + an actionable `fix:` line. Exit code 1 on any FAIL. Implementation is dependency-free (std::net TCP probe, existing `ureq` for HTTP, existing `serde_yaml` for manifest). | GAP G | Low | ✅ Done |
| **DX-3** | ~~Hot-reload in `aeon dev` via `notify` crate~~ — **Done**. `aeon-cli/src/main.rs:2136` (`cmd_dev_watch`) spins up a synthetic tick source → loaded processor → stdout sink pipeline, then uses `notify::RecommendedWatcher` on the artifact's parent directory (NonRecursive) with path canonicalisation + 500ms debounce. On Create/Modify of the watched artifact it calls `PipelineControl::drain_and_swap(new_processor)` (from `aeon-engine/src/pipeline.rs:1388`) for a zero-event-loss hot-swap. Supports `.wasm`/`.wat`/`.so`/`.dll`/`.dylib`. Ctrl+C triggers graceful shutdown of both source and pipeline. Companion `drain_and_swap_source` / `drain_and_swap_sink` also exist for Phase C reconfiguration. | GAP G, formerly ZD-12 | ✅ Done |
| **DX-4** | ~~CLI error message polish~~ — **Done (2026-04-12)**. `aeon-cli/src/main.rs` wraps `run()` with a pretty-printer: prints `error: <top>` + indented `caused by:` chain + an actionable `hint:` line when the error shape matches a known pattern (connection refused → `aeon doctor`/`aeon serve`; 401/403/404 → identity/scope/listing hints; unknown artifact extension → supported types; Wasm validation → `aeon build --release`; `failed to run npm`/`cargo build` → toolchain install). Pattern matching is cheap string scanning over the full anyhow chain. Guarded by 3 new tests in `aeon-cli/tests/cli.rs` (`validate_missing_file_fails_cleanly`, `validate_unknown_extension_gives_hint`, `processor_list_against_unreachable_server_hints_at_server`). | GAP G | ✅ Done |
| **DX-5** | ~~`cargo xtask` for dev workflows~~ — **Done (2026-04-12)**. New `xtask/` workspace member + `.cargo/config.toml` alias so `cargo xtask <cmd>` works from anywhere. Three subcommands, zero external deps: `ci` (fmt check + clippy `--all-targets -D warnings` + `test --workspace --no-fail-fast` — the pre-push gate); `doctor` (rustc/cargo/toolchain report + required Wasm targets check with actionable `rustup target add` fix line + optional tools probe for cargo-deny/cargo-audit/helm/kubectl); `bench` (delegates to `cargo bench --workspace`). Scope deliberately excludes destructive release automation (tagging/publishing) — we suggest commands, never run them. | GAP G | ✅ Done |

### Pillar 7: Blocked / Demand-Driven (No Action Now)

| ID | Task | Tier | Blocker |
|----|------|------|---------|
| **BL-1** | Node.js T3 WebTransport SDK | T3 | No stable npm WT package (`@aspect-build/webtransport` is stopgap) |
| **BL-2** | Java T3 WebTransport SDK | T3 | Flupke WT is experimental, no stable client |
| **BL-3** | C# T3 WebTransport SDK | T3 | `System.Net.Quic` preview only, not until .NET 11+ |
| **BL-4** | C/C++ T3 WebTransport SDK | T3 | No mature WT library (quinn/quiche C bindings possible future) |
| **BL-5** | PHP T3 WebTransport SDK | T3 | PHP ecosystem lacks QUIC/WT support entirely |
| **BL-6** | Child process isolation tier (T5) | — | Design only |

**Note on T1/T2 language scope**: T1 (Native .so/.dll) is inherently limited to Rust, C/C++,
and C# NativeAOT — languages that produce C-ABI shared libraries. T2 (Wasm) is limited to
Rust, AssemblyScript, and C/C++ — languages with wasm32 compilation. No action items exist
for extending T1/T2 to other languages because it is a runtime constraint, not an Aeon
limitation. See Section 10 for the full per-language × per-tier matrix.

### Documentation Updates

| ID | Task | Status |
|----|------|--------|
| **DOC-1** | Create `docs/FAULT-TOLERANCE-ANALYSIS.md` | **Done** (2026-04-12) — this document |
| **DOC-2** | Update ROADMAP.md — reference this doc, mark old to-do OVERRIDDEN, add unified list | **Done** (2026-04-12) |
| **DOC-3** | Update ARCHITECTURE.md — L3 backend is redb (default), RocksDB pluggable via L3Store trait | **Done** (2026-04-12) |
| **DOC-4** | Mark L2 mmap as DONE in ROADMAP (was marked "NOT IMPLEMENTED" — stale) | **Done** (2026-04-12) |
| **DOC-5** | Doc staleness sweep — update `CLUSTERING.md` header (DOKS validation done), update `MULTI-NODE-AND-DEPLOYMENT-STRATEGY.md` to reference this analysis, update `E2E-TEST-PLAN.md` (G1/G2/G3, F7, A5 marked ❌ but actually done — also flagged as ZD-plan A4). Single-pass sweep. | GAP F | **Done** (2026-04-12) — CLUSTERING.md header now reflects DOKS 3-node validation; MULTI-NODE-AND-DEPLOYMENT-STRATEGY.md cross-references FAULT-TOLERANCE-ANALYSIS; E2E-TEST-PLAN.md had already been updated (G1/G2/G3, F7, A5 all ✅). |

### Stale Items Corrected

| Item | Old Status | Actual Status |
|------|-----------|---------------|
| L2 mmap tier | "NOT IMPLEMENTED — l2.rs doesn't exist" | **DONE** — `aeon-state/src/l2.rs` (646 lines, 10 tests, feature-gated `mmap`) |
| Multi-node DOKS (P4f) | "Blocked on infra" | **DONE** — 3-node cluster validated (2026-04-12) |
| Raft consensus real-network | "Needs multi-node cloud" | **DONE** — leader failover, log replication, node rejoin tested on DOKS |
| Cross-node QUIC | "Needs multi-node cloud" | **DONE** — all 3 nodes consistent (log_idx=16, applied=16, term=25) |
| CI/CD items (4) | Listed as pending | **DONE** (2026-04-12) — release.yml, Dockerfile, deny.toml, CHANGELOG |

### Summary Counts

| Category | Count | Status |
|----------|-------|--------|
| Fault-tolerance (Pillar 1) | 12 items | **Complete** — All shipped 2026-04-12: FT-1/FT-2 (persistent Raft log + snapshots via L3), FT-3 (L3 checkpoint), FT-4 (pre-vote mitigated via tuned `RaftTiming` presets), FT-5 (election timeouts), FT-6 (health ping/pong), FT-7 (L3 generics), FT-8 (mTLS), FT-9 (L3Store trait moved to aeon-types), FT-10 (unwrap audit — 10 crates locked), FT-11 (zero-copy via `bytes::Bytes`), FT-12 (refcount perf). Zero-event-loss prerequisites met. |
| Zero-downtime (Pillar 2) | 7 items | **Complete** — ZD-1/2/3 bug fixes + ZD-4 (drain/swap) + ZD-5 (blue-green) + ZD-6 (canary) + ZD-7/8 (same-type source/sink reconfig) all shipped 2026-04-11/12. ZD-9 (cross-type swap via separate pipeline) deferred. |
| Cluster validation (Pillar 3) | 9 items | **Partially done** — DOKS deployed; Gate 2 criteria, cluster metrics, auto-scaling, and CL-6 partition transfer (4 sub-tasks) remain |
| Transport resilience (Pillar 4) | 3 items | **Mostly complete** — TR-1 (in-flight batch replay) + TR-2 (Wasm state transfer) shipped 2026-04-12; TR-3 (connection backoff with jitter) shipped for MQTT source+sink, MongoDB CDC, and `QuicEndpoint::connect_with_backoff()` bootstrap path. Remaining TR-3 connectors need reconnect loops introduced first. |
| Exactly-once delivery (Pillar 5) | 3 items | **Partial** — EO-1 (Kafka IdempotentSink) ✅, EO-3 (Nats/Redis idempotent) ✅, EO-2 trait framework (`SinkEosTier` + `TransactionalSink`) + Kafka T2 impl shipped 2026-04-12; pipeline two-phase L3 wiring + recovery + other-tier impls remain. |
| Developer experience (Pillar 6) | 5 items | **Complete** — DX-1 (CLI tests), DX-2 (aeon doctor), DX-3 (hot-reload), DX-4 (error polish), DX-5 (cargo xtask) all ✅ |
| Blocked/demand-driven (Pillar 7) | 6 items | **Blocked on external** — T3 WT library maturity for 5 languages; T1/T2 are inherently language-limited (see Section 10) |

---

## 8. Execution Order

```
Phase 0: Trait Movement (prerequisite — unblocks FT-7, FT-1, FT-3)
  FT-9 (move L3Store trait + BatchOp/BatchEntry/KvPairs to aeon-types)

Phase 1: Quick Wins + Security + Safety (no dependencies, can parallelize)
  FT-4  (pre-vote)          — trivial config change
  FT-5  (election timeouts) — low, config extraction
  FT-6  (health pings)      — ✅ Done (transport::health module, 7 tests)
  FT-8  (production mTLS)   — ✅ Done (CLI selector + webtransport runtime warn, 4 tests)
  FT-10 (unwrap audit)      — high, HIGH PRIORITY (zero-event-loss guarantee)
  FT-11 (zero-copy violations) — medium, HARD BLOCKER (CLAUDE.md rule #3)
  TR-3  (connection backoff) — medium, HIGH PRIORITY (reconnect storms)
  ZD-2  (CLI serde fix)     — trivial
  DX-2  (aeon doctor)       — low, high adoption value
  DX-4  (CLI error polish)  — low, high adoption value
  DOC-5 (doc staleness sweep) — low

Phase 2: L3Store Generics (depends on FT-9)
  FT-7 (TieredStore generic over L3Store)

Phase 3: Persistence Core (FT-1 depends on FT-9; FT-3 depends on FT-7)
  FT-1 (persistent Raft log via L3Store)
    -> FT-2 (persistent Raft snapshots, same DB)
  FT-3 (checkpoint via L3 + CheckpointPersist trait, convention default)

Phase 4: Zero-Downtime Bug Fixes + Dev UX
  ZD-1 (POST /api/v1/processors route)
  ZD-3 (real SHA-512 in registry)
  DX-1 (CLI integration test suite) — parallel

Phase 5: Zero-Downtime Features (sequential dependency chain)
  ZD-4 (source pause/resume + drain)
    -> ZD-5 (hot-swap orchestrator)
      -> ZD-6 (source/sink reconfiguration)
      -> DX-3 (hot-reload in aeon dev — reuses ZD-5 orchestrator)

Phase 6: Cluster Validation (Gate 2)
  CL-1  (acceptance testing)
  CL-2  (partition reassignment)
  CL-3  (PoH chain transfer)
  CL-4  (cluster-level Prometheus metrics)
  CL-6a (partition bulk sync protocol)
    -> CL-6b (PoH chain transfer real streaming)
      -> CL-6c (cutover handshake)
        -> CL-6d (backpressure/throttling)
          -> CL-5 (raft-aware K8s auto-scaling — needs CL-6 for safe scale-down)

Phase 7: Exactly-Once Delivery (depends on FT-3)
  EO-1 (IdempotentSink for KafkaSink — Kafka transactions)
  EO-2 (atomic checkpoint + sink commit)
  EO-3 (IdempotentSink for other sinks — demand-driven)

Phase 8: Deferred + Nice-to-Have
  ZD-7 (cross-type connector swap)
  TR-1 (batch replay on disconnect)
  TR-2 (Wasm state transfer)
  FT-12 (refcount & config perf tuning — after FT-11, Gate 1 tuning pass)
  DX-5 (cargo xtask for dev workflows)
```

---

## 9. Verification Checklist

After each phase:

```bash
# Compilation
cargo check --workspace
cargo clippy --workspace -- -D warnings

# Tests
cargo test -p aeon-cluster                    # Raft persistence (after Phase 3)
cargo test -p aeon-state                      # L3Store generics (after Phase 2)
cargo test -p aeon-engine --lib rest_api      # REST API (after Phase 4)
cargo check -p aeon-cli --features rest-api   # CLI (after ZD-2)

# Phase 3 specific
# - Start single-node, write state, kill process, restart — verify Raft state recovered
# - Start 3-node cluster, kill all nodes, restart all — verify cluster reforms from disk

# Phase 5 specific
# - Run pipeline, call upgrade via REST, verify zero event loss during swap

# Phase 6 specific
# - Gate 2 acceptance criteria (see ROADMAP.md)
```

---

## 10. SDK & Multi-Language Coverage

Aeon's four-tier processor architecture targets adoption across programming languages.
Each tier has different language availability due to runtime constraints and external
library maturity.

### Per-Tier Language Support

#### T1 Native (.so/.dll) — Limited to Compiled-to-Native Languages

T1 requires compiling to a platform-native shared library with C-ABI exports. This
inherently limits T1 to languages that can produce `.so`/`.dll`/`.dylib` binaries:

| Language | Status | SDK | Notes |
|----------|--------|-----|-------|
| **Rust** | Ready | `aeon-native-sdk` crate | Native ecosystem match |
| **C/C++** | Ready | `sdks/c/` (CMake, header-only) | Manual wire format parsing |
| **C# (.NET NativeAOT)** | Ready | `sdks/dotnet/AeonPassthroughNative/` | Requires .NET 8+, AOT compilation |

**Not applicable**: Python, Go, Node.js, Java, PHP, Ruby, etc. — these languages cannot
produce C-ABI shared libraries. Use T2 (Wasm), T3 (WebTransport), or T4 (WebSocket) instead.

#### T2 Wasm — Limited to Wasm-Compilable Languages

T2 requires compiling to `wasm32-unknown-unknown` target. Only languages with mature
Wasm compilation toolchains are supported:

| Language | Status | SDK | Notes |
|----------|--------|-----|-------|
| **Rust** | Ready | `aeon-wasm-sdk` crate | `aeon_processor!` macro, 10-line hello-world |
| **AssemblyScript** | Ready | `sdks/typescript/assembly/` | TypeScript-like syntax, compiles to Wasm |
| **C/C++** | Possible | `sdks/c/` (wasm32 target) | Requires clang/LLVM, manual allocator, steep learning curve |

**Not practical for T2**: Python, Go, Node.js, Java, PHP, C# — these languages either
cannot compile to wasm32-unknown-unknown or produce prohibitively large binaries with
runtime dependencies. Use T3 or T4 instead.

#### T3 WebTransport (QUIC/HTTP3) — Limited by Client Library Maturity

T3 is architecturally complete on Aeon's side (host, AWPP protocol, ED25519 auth). However,
WebTransport is a relatively new protocol and **client library availability varies by
language**. This is an external ecosystem constraint, not an Aeon limitation.

| Language | Status | Client Library | Notes |
|----------|--------|---------------|-------|
| **Python** | Ready | `aioquic` | E2E tested, passing (2026-04-10) |
| **Go** | Ready | `quic-go/webtransport-go` | E2E tested, passing (2026-04-10) |
| **Rust** | Ready | `wtransport` | E2E tested, passing |
| **Node.js** | **Deferred** | No stable npm WT package | `@aspect-build/webtransport` is stopgap |
| **Java** | **Deferred** | Flupke WT is experimental | No stable Java WebTransport client |
| **C# (.NET)** | **Deferred** | `System.Net.Quic` preview only | Not until .NET 11+ |
| **C/C++** | **Deferred** | No mature WT library | Could use quinn/quiche C bindings (future) |
| **PHP** | **Deferred** | No WT library exists | PHP ecosystem lacks QUIC/WT support entirely |

See `docs/WT-SDK-INTEGRATION-PLAN.md` for detailed tracking of each language's WT library
maturity and SDK sequencing.

#### T4 WebSocket — Universal (All Languages)

T4 uses standard WebSocket over HTTP, which is supported by virtually every programming
language. This is the **universal fallback** for languages without T1/T2/T3 support.

| Language | Status | SDK | Notes |
|----------|--------|-----|-------|
| **Python** | Ready | `sdks/python/aeon_transport.py` | Pure asyncio |
| **Go** | Ready | `sdks/go/aeon.go` | gorilla/websocket |
| **Node.js** | Ready | `sdks/nodejs/aeon.js` | ws + msgpackr |
| **Java** | Ready | `sdks/java/` | javax.websocket + Jackson |
| **C# (.NET)** | Ready | `sdks/dotnet/AeonProcessorSdk/` | ClientWebSocket |
| **PHP** | Ready | `sdks/php/src/` | 6 async adapters (Swoole, AMPHP, Revolt, Workerman, etc.) |
| **Rust** | Ready | `crates/aeon-processor-client/` | tokio-tungstenite |
| **C/C++** | Ready | Via libwebsockets or similar | Standard WebSocket libraries |

Any language with a WebSocket client can implement the AWPP protocol and connect as a T4
processor. This includes Ruby, Perl, R, Lua, Haskell, Dart, Julia, Scala, Elixir, and others.

### Summary Matrix

```
Language        T1 Native   T2 Wasm     T3 WebTransport   T4 WebSocket
─────────────   ─────────   ─────────   ────────────────   ────────────
Rust            ✅ Ready     ✅ Ready     ✅ Ready            ✅ Ready
C/C++           ✅ Ready     ⚠ Possible  ❌ Deferred         ✅ Ready
C# (.NET)       ✅ NativeAOT ❌ N/A       ❌ Deferred         ✅ Ready
AssemblyScript  ❌ N/A       ✅ Ready     ❌ N/A              ❌ N/A
Python          ❌ N/A       ❌ N/A       ✅ Ready            ✅ Ready
Go              ❌ N/A       ❌ N/A       ✅ Ready            ✅ Ready
Java            ❌ N/A       ❌ N/A       ❌ Deferred         ✅ Ready
Node.js         ❌ N/A       ❌ N/A       ❌ Deferred         ✅ Ready
PHP             ❌ N/A       ❌ N/A       ❌ Deferred         ✅ Ready
Other languages ❌ N/A       ❌ N/A       ❌ N/A              ✅ Via WS

Legend:
  ✅ Ready     — SDK exists, E2E tests passing
  ⚠ Possible  — Technically feasible, steep learning curve
  ❌ Deferred  — Blocked on external library maturity
  ❌ N/A       — Not applicable for this tier (language/runtime limitation)
```

### Key Takeaways

1. **T1 and T2 are inherently limited** to languages that can compile to native binaries
   (T1) or Wasm (T2). This is a runtime constraint, not an Aeon limitation. These tiers
   offer maximum performance (T1: ~4.2M evt/s, T2: ~940K evt/s) for the languages that
   support them.

2. **T3 is architecturally complete** on Aeon's side but **blocked by WebTransport client
   library maturity** in 5 of 8 target languages. As WT libraries mature in each language
   ecosystem, Aeon SDKs will follow. See `docs/WT-SDK-INTEGRATION-PLAN.md`.

3. **T4 is the universal tier** — any language with a WebSocket client can integrate with
   Aeon. Performance is lower (~400K evt/s) but language coverage is effectively unlimited.

4. **For most developers**, the adoption path is: start with T4 (any language, easiest
   setup), then migrate to T3 (3x performance) or T2/T1 (10x performance) as needs grow.
