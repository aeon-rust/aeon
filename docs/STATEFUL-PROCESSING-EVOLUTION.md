# Aeon — Stateful Processing Evolution

> **Captured 2026-05-03.** This document is the integrated synthesis of
> Aeon's path toward stateful real-time data processing — what to build,
> in what order, why each step is the right one for Aeon (not a copy of
> Flink), and how it composes with what's already shipped.
>
> **Supersedes the framing in:**
> - `docs/WINDOWING-WATERMARKS-DESIGN.md` (2026-04-19) — windowing remains
>   deferred per its prerequisites, but the framing of "what's missing"
>   is updated here.
> - `docs/POSITIONING.md` § 6 — the windowing/CEP gap entry is reframed
>   in Aeon's own vocabulary instead of Flink's.

---

## 0. The thesis in one paragraph

Aeon's existing primitives ARE the stateful-processing primitives. The
PoH chain is a per-partition order proof; the L1/L2/L3 store is a tiered
state backend; UUIDv7 + per-source-kind identity is event-time tracking;
`per_sink_ack_seq` is the delivery watermark; per-partition cutover via
`WriteGate` + `EngineCutoverCoordinator` is sequence-bounded transition.
What's missing isn't infrastructure — it's a coherent user-facing
surface that exposes what already exists, framed in Aeon's vocabulary,
and a small number of new pieces (window-keyed state index, window
assigner DSL, sequence-bounded processor lifecycle) that compose on top
of the existing substrate. The path to "higher than Flink throughput
while staying language-agnostic and hardware-agnostic" is to evolve in
small, integrated layers — no parallel uncoordinated stubs, no Flink
terminology dump.

---

## 1. Why Aeon is structurally different from Flink

Apache Flink's architecture is excellent for what it was designed for —
JVM-hosted stateful streaming with a mature SQL/CEP layer — but it
carries structural costs Aeon does not:

| Property | Flink | Aeon |
|---|---|---|
| Per-event runtime overhead | ~500 ns – 1 µs (JVM + Kryo serialization) | ~50–250 ns (zero-copy `Bytes`, SPSC ring buffers) |
| Processor language | JVM-only first-class (Java, Scala, Kotlin); others via interop | Four tiers — T1 native (Rust/C/.NET, ~240 ns/event), T2 Wasm (Rust/AssemblyScript, ~1.1 µs), T3 WebTransport, T4 WebSocket — each native to its runtime |
| State backend | RocksDB (default) or heap | L1 DashMap (hot) + L2 mmap segments (warm) + L3 redb (durable checkpoint) — explicit per-tier |
| Event ordering signal | Watermark messages injected into stream | **PoH chain** — cryptographically chained per-partition, computed inline, tamper-evident |
| Fault tolerance | Checkpoint barriers (Chandy–Lamport) | EO-2: per-sink ack tracking + L2 replay-from-sequence + L3 checkpoint |
| Job upgrade | Stop/savepoint/restart, or version migration | Path 2 (proposed) — per-partition Raft-committed sequence boundary cutover |
| Cluster consensus | JobManager + ZooKeeper (typically) | openraft, always-on, single-node = trivially-leader |
| Deployment shape | Standalone / YARN / K8s | K8s / VMs / baremetal — single-node and multi-node share the same code path |

Three of these are decisive for Aeon's wedge:

1. **No JVM hop on the hot path.** Aeon's per-event budget (Gate 1 target <100 ns) is an order of magnitude tighter than what JVM stream processors can deliver, before even discussing serialization.
2. **Cryptographic event chain inline, not bolted on.** PoH + Merkle + MMR + Ed25519 root signatures are part of how events are ordered, not a separate audit pipeline. Retrofitting this into Flink would require a hot-path rewrite.
3. **Language-agnostic processors.** A Python team and a Rust team can ship processors against the same engine without an interop layer. Flink's first-class story is JVM only.

These are not optional decorations. They define what Aeon IS. Any
addition (windowing, watermarks, stateful operators) must compose with
them, not bypass them.

---

## 2. Mapping table — Flink concept ↔ Aeon vocabulary

This table is for readers familiar with Flink. **Aeon uses its own
vocabulary throughout the codebase and docs.** The mapping is here so
you can recognize the analogue, not so we can rename.

| Flink concept | Aeon term + where it lives | What Aeon adds |
|---|---|---|
| Checkpoint barrier | **PoH chain sequence** — `crates/aeon-engine/src/partition_install.rs:235`, registry at `pipeline_supervisor.rs:341` | Cryptographically chained, not just a marker; tamper-evident |
| Operator state backend (RocksDB) | **L1 DashMap + L2 mmap (`L2BodyStore` at `l2_body.rs:651`) + L3 redb (`L3RaftLogStore`)** | Three explicit tiers; per-mode durability (None/Per-event/Ordered-batch/Unordered-batch) |
| Source offset | **Per-source-kind identity** — pull (offset, `KafkaSource`), push (UUIDv7 + monotonic, `HttpWebhookSource`), poll (cursor + content-hash, `HttpPollingSource`) | Three first-class strategies; each connector declares its kind |
| 2PC / `TwoPhaseCommitSinkFunction` | **`per_sink_ack_seq` + `AckSeqTracker`** — `checkpoint.rs:60` | Per-sink granularity native; no transactional coordinator needed for most modes |
| Watermark | **PoH chain head + per-sink ack seq** (already exist as monotonic per-partition signals) | Will be exposed as `WatermarkView` in Phase 2 (see § 4); no new propagation channel needed |
| Job stop/savepoint/restart | **Path 2 — sequence-bounded cutover** (proposed, see § 4) — reuses `WriteGate` + `EngineCutoverCoordinator::CutoverOffsets` + `LivePohChainRegistry` | Per-partition Raft-committed boundary; not job-wide stop-the-world |
| Restart strategy | **EO-2 recovery** — `EO2RecoveryPlan::l2_replay_start_seq` at `eo2_recovery.rs:239` | Sequence-bounded replay from `min(per_sink_ack_seq) + 1` |
| Operator chain (fusion) | **SPSC ring buffers** — `rtrb` between source/processor/sink | Zero-copy, no serialization hop between stages |
| Allowed lateness + side outputs | **Late-event policy** (drop / side-output) — to be declared on processor manifest in Phase 3 | — |
| `KeyedStream` + window state | **Window-keyed extension on L1/L2/L3** (Phase 3) — index over existing tiers, not a new store | — |
| `TimeCharacteristic.{Event, Processing, Ingestion}Time` | **`event_time` config on source manifest** + `WatermarkView` derivation in Phase 2 | Aeon's UUIDv7 timestamp doubles as a free ingest-time signal |
| RestartPolicy with externalized checkpoints | **L3 checkpoint store with WAL fallback** — already shipped (M2) | WAL fallback when L3 store is unavailable |
| JobManager HA (ZooKeeper) | **openraft** — always-on, persistent log + state machine (FT-1, FT-2 shipped) | Single-node mode is the same code path; no separate HA setup |

---

## 3. The layered progression toward stateful processing

Reading Aeon's atom history (G-series, CL-series, FT-series, EO-2 P-series)
together, the engine has been building this stack from the bottom up.
The full progression:

| Layer | Capability | Status | Where it lives |
|---|---|---|---|
| **0** | Per-partition pipeline execution | ✅ done (Phase 1+2) | `pipeline.rs`, `pipeline_supervisor.rs` |
| **1** | Per-partition sequence tracking via PoH | ✅ done (CL-6c.4, 2026-04-23) | `partition_install.rs::LivePohChainRegistry` |
| **2** | Sequence-bounded recovery from L2 + L3 | ✅ done (EO-2 §5.1, P-series) | `eo2_recovery.rs::EO2RecoveryPlan` |
| **3** | Sequence-bounded partition migration | ✅ done (CL-6c, 2026-04-16) | `engine_cutover.rs`, `partition_driver.rs`, `write_gate.rs` |
| **4** | **Sequence-bounded processor lifecycle (Path 2)** | ❌ proposed — see Phase 1 of integrated plan in § 4 | New: extends `PipelineControl` + new `RegistryCommand` variants |
| **5** | Per-partition watermark API exposing PoH + ack_seq | ❌ proposed — Phase 2 | New: `WatermarkView` façade over existing state, no new SPSC traffic |
| **6** | Window-keyed state + assigner + trigger | ❌ designed in `WINDOWING-WATERMARKS-DESIGN.md`, deferred — Phase 3 | New: index over L1/L2/L3, processor manifest extension |
| **7** | CEP / streaming SQL | ❌ further-future | Built on layers 5+6 |

Layers 0-3 are the substrate the cluster work shipped. Layer 4 (Path 2)
is the next architectural step. Layer 5 is a thin façade over signals
that already exist. Layer 6 is the bulk of the windowing work and gates
on having layers 4+5 in place.

**The gap is layer 4.** Without it, layers 5 and 6 are blocked on
ad-hoc design decisions that layer 4 forces (partition-aware
PipelineControl, Raft-committed sequence boundary maps).

---

## 4. The integrated plan — no parallel stubs

Sequencing principle: **each layer reuses the one below; nothing new
where existing primitives suffice.**

### Phase 1 — Layer 4: sequence-bounded processor lifecycle

**Scope:** close the cluster correctness gap (G9.c follow-up) AND lay
the architectural foundation for layers 5+6.

- 6 new `RegistryCommand` variants for the lifecycle ops (`BlueGreenStart`,
  `BlueGreenCutover`, `RollbackUpgrade`, `CanaryStart`, `CanaryPromote`,
  `ReconfigureSource`, `ReconfigureSink`) — each carries
  `BTreeMap<PartitionId, Sequence>` boundary
- Boundary computed by leader from existing `LivePohChainRegistry` (no
  new state)
- New `PipelineControl::drain_partitions_at_seq()` reusing `WriteGate` +
  `EO2RecoveryPlan` + `L2BodyStore::iter_from`
- `cluster_applier` dispatches new variants to new supervisor methods
  on every node (Tier 1 strong-consistent declarative state via Raft +
  Tier 2 deterministic per-partition transition at the boundary)
- **Backfill** existing `UpgradePipeline` to also propagate the
  drain-and-swap to followers (closes the pre-existing gap surfaced by
  the G9.b audit)

**Effort:** 4–5 days.
**Outcome:** strongly-consistent per-partition processor transitions at
Raft-committed sequence boundaries. Operator can `aeon pipeline
upgrade` against any pod and get deterministic cluster-wide convergence
at exactly seq N per partition.

### Phase 2 — Layer 5: WatermarkView façade

**Scope:** expose existing per-partition signals through a stable API
so processors and sinks can observe "safe to act through" state. No
new propagation channel.

- New `WatermarkView` trait — three read-only methods, all computed
  views:
  - `processing_time_per_partition()` → reads `LivePohChainRegistry`
    (per-partition PoH sequence head)
  - `delivery_time_per_partition()` → reads `AckSeqTracker`
    (per-partition `min(per_sink_ack_seq)`)
  - `event_time_per_partition(strategy)` → derives from `Event.timestamp`
    using `BoundedOutOfOrderness(Duration)` or `PeriodicAscending`
    heuristics from `WINDOWING-WATERMARKS-DESIGN.md` § 4.2
- Cluster propagation reuses existing CL-6c partition transfer
  (PoH seq + L3 checkpoint cross the cluster automatically)
- No new SPSC message type; no new wire frame; no new state stores

**Effort:** ~1 week.
**Outcome:** processors and sinks have a uniform way to ask "what's
the safe watermark for this partition?" Trigger logic in Phase 3 reads
this. Independent value: makes downstream operators (and operator
tooling) able to reason about per-partition progress.

### Phase 3 — Layer 6: window-keyed state + assigner + trigger

**Scope:** the F1+F2 work designed in `WINDOWING-WATERMARKS-DESIGN.md`,
implemented as an extension over Phases 1+2, NOT as a parallel
subsystem.

- L1 `WindowedKeyedState<K, V>` view — DashMap of
  `(window_id, user_key) → V` per (pipeline, partition)
- L2 spill mechanism reuses existing `L2BodyStore` segment files
  (per-window, per-partition)
- L3 snapshot reuses existing checkpoint records
- Window assigner declared in pipeline manifest:
  ```yaml
  processor:
    name: count-by-key
    window:
      kind: tumbling   # or sliding | session
      size: "1m"
      time_basis: event   # or ingest | processing
      late_policy: drop   # or side-output
  ```
- Trigger fires when `WatermarkView.event_time_per_partition()` (Phase 2)
  crosses window-end
- Emission produces an `Output` per window per key

**Effort:** 6–10 weeks (per the existing windowing design doc, unchanged).
**Outcome:** Aeon can express tumbling, sliding, session windows with
per-key aggregation, with all the throughput properties of layers 0–5.
**Prerequisite:** Phases 1 + 2 must close first; v0.1 should ship before
opening this.

### Phase 4 — Layer 7: CEP / streaming SQL

**Scope:** built on layers 5+6, no parallel infrastructure. Multi-month.
**Prerequisite:** Phase 3 + at least one production user demanding it.

---

## 5. Throughput design budget vs Flink (per-event, hot path)

For a windowed aggregation (1-minute tumbling, count by key, hot in L1):

| Operation | Flink (typical) | Aeon (designed) |
|---|---|---|
| Per-event ingest | 500 ns – 1 µs (JVM + Kryo) | **50–100 ns** (zero-copy `Bytes`, SPSC) |
| Watermark check | ~50 ns (poll WM state) | **~20 ns** (read `LivePohChainRegistry` head, no SPSC traffic) |
| Window state lookup | 200–500 ns (RocksDB or heap) | **~30 ns** (L1 DashMap), spill to L2 ~1–5 µs only when memory pressure |
| Aggregate update | ~50 ns | **~20 ns** (native Rust struct mutation, no boxing) |
| Per-event total (steady state, hot in L1) | **~800 ns – 1.5 µs** | **~150–250 ns** |
| Steady-state throughput per core | 700K–1.2M events/sec | **4–6 M events/sec** |

These are design-budget targets, not measured. Session B benchmarks (see
§ 7) would validate. The structural difference (no JVM, no serialization,
native L1, inline PoH) makes "higher than Flink" plausible-by-design,
not a stretch goal.

---

## 6. Single-node / multi-node uniformity across hardware

The integrated design preserves Aeon's "always-Raft + same code path"
principle:

- **Single-node mode**: no partition transfer ever fires, but PoH chain
  + ack_seq + L1/L2/L3 + window state work identically. Phase 1's
  `RegistryCommand` variants commit through Raft trivially (single
  voter); cutover boundary applies on the one node.
- **Multi-node mode**: partition transfer carries window state via
  existing CL-6c segment streaming (windows are partition-scoped, same
  as L2 bodies). Phase 1's boundary maps cross the cluster via Raft.
- **K8s deployment**: helm chart supports both Deployment (single-node)
  and StatefulSet (multi-node). No change for stateful processing.
- **VM / baremetal deployment**: L2/L3 are filesystem-based (mmap +
  redb). No K8s assumption. PoH chain is in-process. Works anywhere
  with a posix-ish filesystem. The same `aeon serve` binary runs.

The user-facing API stays the same. The operator picks the deployment
shape; the engine adapts. This is the existing principle, preserved
throughout the stateful-processing evolution.

---

## 7. Session B (AWS EKS) investment timing

**Question:** does it make sense to spend on Session B now, given the
stateful-processing direction?

**Context (from ROADMAP):**
- Session B = AWS EKS, premium hardware (i4i.2xlarge / i3en.3xlarge),
  multi-broker Redpanda, T0/T1 ceiling + T6 sustained throughput rows
- P4.iii (ECR pre-bake) is the only remaining Session B prep step
- Phase 3.5 V2–V6 closed 2026-05-02 (correctness floor on Rancher
  Desktop), so the pre-Session-B blocker is resolved
- DOKS sessions only validated correctness — no NVMe, no premium
  network, so they cannot publish a credible ceiling number
- The wedge in `POSITIONING.md` § 5 includes "Rust-class per-event
  overhead" — a number is needed to substantiate it

**Honest decision matrix:**

| Aspect | Argument for Session B now | Argument to defer |
|---|---|---|
| v0.1 release positioning | Need a ceiling number to publish; the wedge is unsubstantiated without it | Could ship v0.1 with DOKS floor numbers + "ceiling pending" caveat |
| Stateful-processing direction | Session B benchmarks the wedge, NOT the stateful additions — they're orthogonal | If we're about to add Phase 1 (Layer 4), benchmark would change again post-Phase 1 |
| Cost / spend discipline | Spot-pricing pre-check + 6-hr cap + tear-down checklist already shipped (P4.ii / P4.iv) | DOKS bills overran in April 2026 (orphan Block Storage + registry) — the discipline note exists but historical costs argue for caution |
| Phase 1 (Layer 4) timing | Path 2 is cluster correctness; benchmarks should include it for representativeness | Adds 4–5 days before Session B is meaningful |

**Recommendation:** **fold Phase 1 (Layer 4 = Path 2) INTO v0.1, then
run Session B once.** Path 2 is cluster correctness — it belongs in the
v0.1 release alongside the existing G9.b fix, not parked as v0.2
foundation. Once Path 2 lands:

1. Ship v0.1 with: G9.b fix + Path 2 + delete_processor_version Raft +
   the new connector cookbook + the GHCR distribution.
2. Run Session B against v0.1 — single 6-hour run, follow the existing
   spot-pricing pre-check + tear-down checklist.
3. Publish the ceiling number alongside v0.1's release notes.
4. **Then** open Phase 2 + 3 (Layers 5 + 6) as the v0.2 broadening
   trajectory. Phase 3 (windowing) gates on at least one concrete user
   demand per `WINDOWING-WATERMARKS-DESIGN.md` § 9.

**What changes vs the prior plan:**
- Pre-existing plan was Gate 2 → Session B → v0.1 cut → broaden.
- New plan is Gate 2 → Phase 1 (Path 2) → Session B → v0.1 cut → Phase 2
  → Phase 3 (windowing) when triggered.
- The change is small: Phase 1 is 4–5 days inserted before Session B.
- Justification: Phase 1 is real cluster correctness, not v0.2
  foundation; including it in v0.1 makes Session B's ceiling number
  representative of what operators would actually deploy.

---

## 8. What this changes about prior framing

The following previously-documented framings are **superseded or
amended** by this document:

| Document | What it said | What the integrated path now says |
|---|---|---|
| `WINDOWING-WATERMARKS-DESIGN.md` § 9 | "Parked until Gate 2 + Session B + v0.1 cut + at least one user." | F1+F2 windowing remains parked on those prerequisites. **Phase 1 (Layer 4 = Path 2) is NOT parked** — it ships as part of v0.1. Phase 2 (WatermarkView façade) opens as soon as Phase 1 lands. Phase 3 (windowing F1+F2) keeps the original prerequisites. |
| `POSITIONING.md` § 6 "Windowing / CEP" gap entry | "F1+F2 ~3 months together." | Effort estimate unchanged. **Reframed as the layered progression**: layers 4 → 5 → 6, each composing on the layer below. The "3 months" is layer 6 specifically, not the entire stateful-processing direction. |
| ROADMAP Phase 4 "Session B (AWS EKS) calibration" | Sequenced after Phase 3.5 closure. | Sequence updated: **Phase 1 (Path 2) → Session B → v0.1 cut → Phase 2 → Phase 3 when triggered**. Phase 1 inserted before Session B for benchmark representativeness. |
| ROADMAP G9.b entry (this session) | "Converting [the 7 vulnerable endpoints] to `RegistryCommand` variants for true cluster-wide replication is a separate atom (deferred)." | Promoted to **Phase 1 of this trajectory** — no longer a vague "separate atom" but the explicit Layer 4 work in this plan. |

Older notes that mention "windowing deferred" / "stateful processing
deferred" remain accurate insofar as **Layer 6 (windowing F1+F2)
remains deferred**. Layer 4 (Path 2) is no longer deferred; Layer 5
(WatermarkView façade) opens once Layer 4 lands.

---

## 9. References

- [`WINDOWING-WATERMARKS-DESIGN.md`](WINDOWING-WATERMARKS-DESIGN.md) —
  the F1+F2 (Layer 6) design sketch this evolution composes on top of
- [`POSITIONING.md`](POSITIONING.md) § 5–6 — competitive framing and the
  wedge that this document broadens, not replaces
- [`EO-2-DURABILITY-DESIGN.md`](EO-2-DURABILITY-DESIGN.md) — the
  durability layer (per_sink_ack_seq, L2 replay, L3 checkpoint) that
  Layers 4+5+6 reuse
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — overall engine architecture
- [`CLUSTERING.md`](CLUSTERING.md) — the multi-node story Layer 4
  extends; § 3.2.1 documents the G9.b follower-routing fix that
  surfaced this discussion
- [`ROADMAP.md`](ROADMAP.md) — phase plan; the integrated trajectory is
  cross-referenced as the path for Phase 4 → v0.1 → v0.2

---

## 10. When to revisit this document

Re-open this synthesis when any of the following changes:

1. Phase 1 (Path 2) ships — update § 3 status, § 4 sequencing, § 8
   superseded-framings table.
2. Session B runs and publishes a ceiling number — update § 5 with
   measured numbers vs design budget.
3. v0.1 cuts — update § 7 to retire the Session B timing decision and
   open Phase 2.
4. A production user asks for windowing — update § 4 Phase 3 to
   re-open the F1+F2 design promotion.
5. A new layer is needed (e.g. distributed exactly-once joins across
   pipelines, beyond CEP) — extend § 3 layer table and § 4 sequencing.
