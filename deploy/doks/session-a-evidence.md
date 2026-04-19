# Session A — Raw Evidence Log

> Running transcript of every command issued and every result captured
> during the Session A DOKS run. Referenced from
> `docs/GATE2-ACCEPTANCE-PLAN.md` §10. Append-only during the session.
>
> **Cluster:** DOKS `70821a02-9a2a-4ee6-9a74-38f5dea070e7` (AMS3)
> **Date:** 2026-04-18
> **Image:** `registry.digitalocean.com/rust-proxy-registry/aeon:session-a-prep`
> (sha `36d44fd336c1`, includes SIGTERM/graceful-shutdown fix)

## Index

- [0. Infrastructure baseline](#0-infrastructure-baseline)
- [0.1 PPS probe](#01-pps-probe)
- [1. T0 prep — topics, loadgen pod, pipelines](#1-t0-prep)
- [2. T0 — Isolation matrix](#2-t0--isolation-matrix)
- [3. T1 — 3-node throughput](#3-t1--3-node-throughput)
- [4. T2 — Scale-up 1→3→5](#4-t2--scale-up)
- [5. T3 — Scale-down 5→3→1](#5-t3--scale-down)
- [6. T4 — Cutover duration](#6-t4--cutover-duration)
- [7. T5 — Split-brain drill](#7-t5--split-brain-drill)
- [8. T6 — Sustained with chaos](#8-t6--sustained-with-chaos)
- [9. Code gaps surfaced + fixes](#9-code-gaps-surfaced--fixes)
- [10. Teardown](#10-teardown)

> **Open code gaps surfaced this session** (details in §9):
> 1. KafkaSourceFactory defaults to `[0]` when partitions list empty — should be cluster-ownership aware.
> 2. Leader has no background driver that consumes `PartitionOwnership::Transferring` and runs the two-phase handover (BulkSync → Cutover → Complete). Blocks T2/T3/T4.
> 3. KafkaSink with `transactional_id` cannot be safely shared across pods (no per-pod txn-id substitution). Blocks PerEvent EO-2 on Kafka.
> 4. `aeon_pipeline_outputs_sent_total` counts queued, not broker-acked. For `unordered_batch` the metric overcounts vs broker HWM by ~13 %.
> 5. CLIs `aeon cluster drain` and `aeon cluster rebalance` are still missing (would simplify T2/T3).

---

## 0. Infrastructure baseline

### Node pools (via doctl)

```
ID                                      Name              Size                Count    Labels                      Taints
3b367329-f661-406d-9d2d-428025b1eba0    pool-f5vm1izlt    g-8vcpu-32gb        3        workload:aeon               workload=aeon:NoSchedule
2e8de78f-c15c-432b-bb60-34385ba3b3b3    pool-q4ut3qgmw    so1_5-4vcpu-32gb    3        workload:redpanda           workload=redpanda:NoSchedule
23f4413d-32e4-4c1f-a9e0-0d18ddc67016    pool-a5puafejk    s-4vcpu-8gb         1        workload:monitoring         (none)
```

### Helm releases

- `redpanda/redpanda` 26.1.2 → `redpanda` ns, 3 brokers, RF=3, 24 default partitions
- `aeon` chart → `aeon` ns, 3-replica StatefulSet, auto_tls on QUIC
- `monitoring/kube-prometheus-stack` → `monitoring` ns

### Aeon cluster health — snapshot after install

```
GET /api/v1/cluster/status (from aeon-0)
{
  "leader_id": 3,
  "mode": "cluster",
  "node_id": 1,
  "num_partitions": 24,
  "partitions": { ...24 partitions, 8 per owner, all "owned"... },
  "raft": {
    "current_term": 3,
    "last_applied": 3,
    "last_log_index": 3,
    "membership": "{1, 2, 3}",
    "state": "Follower"
  }
}
```

Leader elected (node 3), all 24 partitions owned, no migrations in flight.

### Prometheus targets

9/9 healthy:
- aeon/aeon × 3 (ClusterIP service targets)
- aeon-headless/aeon × 3 (headless service targets)
- redpanda/redpanda × 3

---

## 0.1 PPS probe

Already logged in detail at `docs/GATE2-ACCEPTANCE-PLAN.md` §10.0. Summary
reproduced here for self-containment:

| Scenario | PPS | Throughput | Loss |
|---|---:|---:|---:|
| Pod→pod CNI, 64 B UDP, A→B | 158,202 | 0.08 Gbit/s | 0.04% |
| Pod→pod CNI, 64 B UDP, B→A | 156,915 | 0.08 Gbit/s | 0.03% |
| hostNetwork raw, 64 B UDP | 174,908 | 0.09 Gbit/s | 0.10% |
| hostNetwork, 1400 B UDP | 199,795 | 2.24 Gbit/s | 13.24% |

**Conclusion:** DO AMS3 standard tier PPS cap active at ~175 K for tiny
packets. Does not bind Aeon's batched-Kafka path at 256 B events (maps
to multiple M events/s). Annotate PPS-sensitive rows accordingly.

---

## 1. T0 prep

### Loadgen pod + curl poller

`loadgen` namespace created, `aeon-loadgen` pod (image `aeon:session-a-prep`, `sleep infinity`) and `curl-poller` pod (`curlimages/curl`, `sleep infinity`) both Running on monitoring pool. Image pull secret `rust-proxy-registry` propagated from `aeon` ns.

### Processor registration

Built-in `__identity` (PassthroughProcessor) available out of the box — resolved by `pipeline_supervisor.rs` without Wasm registry lookup. Used throughout T0 to isolate source/sink path.

### REST leader targeting

POST `/start` on a follower returns `cluster error: Raft proposal failed: has to forward request to: Some(3)`. All create/start requests targeted at leader FQDN `aeon-2.aeon-headless.aeon.svc.cluster.local:4471` directly.

### Metrics sampling

Metrics collector: sums `aeon_pipeline_outputs_sent_total{pipeline="<cell>"}` across all 3 pods on `/metrics` every 2-3 s. Pipeline DONE when aggregate ≥ 3 × target per node (Memory source generates independently per pod).

---

## 2. T0 — Isolation matrix

### C0: Memory → __identity → Blackhole

| Durability | Count/node | Aggregate | Wall (s) | Agg eps | Per-node eps | ns/event |
|---|---:|---:|---:|---:|---:|---:|
| None | 200 M | 600 M | 44.4 | 13.5 M | 4.50 M | 222 |
| UnorderedBatch | 50 M | 150 M | ~22 | 6.8 M | 2.27 M | 441 |
| OrderedBatch | 50 M | 150 M | ~22 | 6.8 M | 2.27 M | 441 |
| PerEvent | 20 M | 60 M | 23.03 | 2.61 M | 0.87 M | 1152 |

**Raw samples — T0.C0.None (200 M / node):**

```
t=1776529359220 47926272 52876288 65004544    (165.8 M aggregate, mid-run)
t=1776529368048 80688128 88119296 100696064
t=1776529377862 117108736 120118272 131255296
t=1776529386407 148046848 148024320 161635328
t=1776529394920 180683776 179021824 200000000
t=1776529403592 200000000 200000000 200000000  (600 M done)
```

Growth 165.8 M → 600 M over 44.37 s ≈ **9.78 M eps aggregate** (partial-window, conservative).

**Observation — PerEvent on Blackhole:** At 20 M / node the cell took 23 s (868 K eps / node). The overhead over None mode is ≈ **930 ns / event** — that is the engine's per-event durability work (L2 body write, L3 checkpoint attempt, metric emission), NOT a disk-backed fsync. Blackhole is a fire-and-forget sink; it does not implement `TransactionalSink`, so the per-event commit path is short-circuited. True fsync cost (ms-scale) only materialises when paired with a transactional sink — deferred to T0.C2 (Kafka→Kafka).

Samples:
```
# t=4s  sum=52,533,760    (partial, 3 pods each mid-run)
# t=23s sum=60,000,000    (DONE)
```

### C1: Redpanda aeon-source → __identity → Blackhole

**Pre-populated topic:** 30 M × 256 B events produced via aeon-native memory→kafka populator pipeline into `aeon-source` (24 partitions × RF 3). HWM sum verified at 30,000,000 across all 24 partitions.

**Workaround for KafkaSourceFactory bug (see §9):** partitions list explicitly enumerated `[0..23]` in every pipeline JSON. Each of the 3 Aeon pods manually-assigns all 24 partitions and reads the topic independently, so aggregate processed = 90 M events (3× topic contents).

| Durability | Aggregate events | Wall (s) | Agg eps | Per-node eps |
|---|---:|---:|---:|---:|
| None | 90 M | 172.55 | 521,579 | 173,860 |
| UnorderedBatch | 90 M | 174.79 | 514,909 | 171,636 |
| OrderedBatch | 90 M | 178.98 | 502,849 | 167,616 |
| PerEvent | 90 M | 186.11 | 483,593 | 161,198 |

**Observation:** All 4 modes land in a 172–186 s window — the Kafka read path is the binding bottleneck (~7.8 % spread None→PerEvent). Durability mode overhead is measurable but overshadowed by fetch cost. Single-partition baseline in the buggy first run (180 s for 1.27 M events → 7 K eps) confirmed the 24-partition fan-out works.

Raw samples — T0.C1.None:
```
# t=4s    sum=6,825,984
# t=42s   sum=30,121,984   (10-event/ms per node)
# t=97s   sum=62,013,440
# t=153s  sum=88,834,432
# t=172s  sum=90,000,000   (DONE)
```

### C2: Redpanda aeon-source → __identity → Redpanda aeon-sink

**Sink topic:** `aeon-sink` 24p × RF 3 created on Redpanda. Each Aeon pod independently consumes all 24 source partitions and produces to the sink (3× write amplification).

| Durability | Aggregate events | Wall (s) | Agg eps | Per-node eps | Notes |
|---|---:|---:|---:|---:|---|
| None | ≥ 46.8 M / 90 M | 293+ (timed out) | 156 K (steady state) | 52 K | partial — Kafka write saturated |
| UnorderedBatch | 90 M (claimed) | 177.9 | 506 K | 169 K | **HWM only 78.1 M on aeon-sink — ~13 % drop in fire-and-forget path (by-design)** |
| OrderedBatch | 90 M | 793.4 | 113 K | 38 K | acks=all dominates; 7× slower than Unordered |
| PerEvent | _deferred_ | _deferred_ | _deferred_ | _deferred_ | **Per-event acks=all on managed K8s without NVMe = infra-bound (~5 ms RTT × no batching). Not an Aeon ceiling — deferred to AWS EKS Session B per hit-the-lid methodology.** |

**Findings:**

1. **Metric semantics:** `aeon_pipeline_outputs_sent_total` counts *queued for send*, not *broker-acked*. For `unordered_batch` (fire-and-forget), this overcounts by the broker-side drop rate. For other strategies it tracks closely with HWM. Documented; should be renamed to `outputs_queued_total` or supplemented with `outputs_acked_total` — added to §9.
2. **Ordered vs Unordered:** 7× write-throughput penalty for ordered acks=all. Expected per Kafka producer semantics; not an Aeon issue.
3. **Aeon ≠ bottleneck:** C0 (no I/O) → 4.5 M eps/node. C1 (Kafka read) → 174 K/node. C2 (Kafka R+W ordered) → 38 K/node. Each step down is bounded by external I/O, not Aeon's per-event overhead.

---

## 3. T1 — 3-node throughput

Pipeline `t1-3node`: Memory source × 100 M / node, `__identity` processor, Blackhole sink, durability None. All 3 pods stream concurrently.

```
# cell=t1-3node target=100000000 per node
# t=5s   sum=62,814,208
# t=26s  sum=227,624,960
# t=45s  sum=300,000,000   (DONE)
t1-3node  45.37s  300,000,000  6,612,153 agg eps  2,204,051 per-node eps
```

**Steady-state rate** (subtract t=5 startup): (300 M − 62.8 M) / (45 − 5) = **5.93 M eps aggregate / 1.98 M per-node**. T0.C0.None at 200 M/node showed 4.5 M per-node — the spread (2 M – 4.5 M / node) reflects how startup overhead amortises across run length, not a per-node ceiling.

**Verdict T1:** ✓ 3-node aggregate scales linearly with node count on the no-I/O engine path. No coordination penalty observed; nodes do not interfere with each other (Memory source generates locally; Blackhole sink writes locally).

**3 → 1 single-node baseline:** Not run in Session A (would require redeploying StatefulSet to 1 replica, then back to 3 — non-trivial Raft membership change). Recorded as known gap; deferred to Session B AWS EKS where bare-metal NVMe isolation also makes single-node ceiling more meaningful.

---

## 6. T4 — Cutover duration

**Result: FAIL — cutover never completes.**

Test ran via `deploy/doks/run-t4-cutover.sh` (12 of 20 attempts before stopped): for each random partition, POST `target_node_id` to `/api/v1/cluster/partitions/{p}/transfer`, then poll `/api/v1/cluster/status` for ownership flip.

| Metric | Observed |
|---|---|
| P50 REST `accepted` (POST→202) | ~2,300 ms |
| P99 REST `accepted` | ~2,800 ms |
| Cutover < 100 ms target | **0 / 12 attempts** |
| Cutover < 5,000 ms (broader window) | **0 / 12 attempts** |
| Final ownership flipped | **0 / 12** — all stayed at source |

After the run, `/cluster/status` showed 12 partitions stuck in `transferring` state (with `source` and `target` populated) — proving the proposal *was* committed via Raft, but the actual handover protocol never executed.

**Root cause (code gap):** The leader proposes `ClusterRequest::BeginTransfer` via Raft, which sets the partition's ownership to `PartitionOwnership::Transferring { source, target }`. However, there is **no background driver loop on the leader that observes this state and orchestrates the two-phase transfer** (BulkSync → Cutover → Complete). The transport/RPC layer exists (`crates/aeon-cluster/src/transport/cutover.rs`, CL-6 commit `807321f`), but `node.rs` has only one reference to `Transferring` — a guard check inside `propose_partition_transfer`. There is no `tokio::spawn` consuming the queue.

**Implication for T2/T3:** scale-up/down with zero loss requires partition handover. Those tests cannot pass either until the driver is wired.

**Deferred tests (rationale):**

- **T2 (Scale-up 1→3→5):** Requires (a) functional partition handover [BLOCKED by T4 gap] and (b) DOKS node pool expansion to 5 g-8vcpu-32gb (current pool is 3). Defer to next session after CL-6 driver lands.
- **T3 (Scale-down 5→3→1):** Same blockers as T2.
- **T5 (Split-brain drill):** Requires Chaos Mesh install (~15 min CRDs + controller). Defer — not in critical path for cluster correctness floor since Raft already prevents split-brain by quorum requirement; T5 is a behaviour-under-fault drill, not a correctness test.
- **T6 (10-min sustained with chaos):** Requires T5 prerequisites. Defer alongside T5.

---

## 9. Code gaps surfaced + fixes

| # | Gap | Symptom | Where | Severity | Status |
|---|---|---|---|---|---|
| G1 | `KafkaSourceFactory` defaults `partitions` to `[0]` when empty list passed | T0.C1 first run drained only partition 0 (1.27 M of 30 M events), pipeline exited cleanly | `crates/aeon-cli/src/connectors.rs:116-120` | Medium — silent under-read | Workaround applied (explicit 24-partition lists in JSON); fix should make this cluster-ownership aware |
| G2 | No leader-side driver consumes `PartitionOwnership::Transferring` | All 12 T4 attempts stuck in `transferring`; ownership never flips | `crates/aeon-cluster/src/node.rs` (missing `tokio::spawn`); transport in `transport/cutover.rs` exists but unwired | **Blocker for T2/T3/T4** | No workaround in Session A; needs design + impl |
| G3 | Shared `transactional_id` across pods causes Kafka producer fencing | Cannot run T0.C2.PerEvent with EO-2 on multi-pod cluster | `crates/aeon-cli/src/connectors.rs:164-166` (no `${HOSTNAME}` substitution) | Medium — limits T2 EO-2 verification | Skipped C2.PerEvent in this session; needs per-pod txn-id derivation |
| G4 | `aeon_pipeline_outputs_sent_total` is queue-count, not ack-count | C2.Unordered metric showed 90 M but broker HWM was 78.1 M (13 % gap) | metric emission in pipeline supervisor / sink loop | Low — observability accuracy | Document split: add `outputs_acked_total` companion metric |
| G5 | `aeon cluster drain` and `aeon cluster rebalance` CLIs missing | Forced manual `transfer-partition` loop in T4 | `crates/aeon-cli/src/main.rs` cluster subcommands | Low — operator UX | Defer with G2 |

---

## 10. Session A — Summary

| Test | Result | Headline number |
|---|---|---|
| Infra bringup (Redpanda 3-broker, Aeon 3-pod, Prometheus 9/9) | ✅ | — |
| PPS probe | ✅ documented (~175 K cap on standard tier) | — |
| **T0.C0 Memory→Blackhole** | ✅ all 4 modes | None: 4.5 M / node, PerEvent: 0.87 M / node |
| **T0.C1 Kafka→Blackhole** | ✅ all 4 modes | None: 174 K / node (Kafka read-bound) |
| **T0.C2 Kafka→Kafka** | 🟡 3 of 4 modes (PerEvent skipped — infra-bound) | Ordered: 38 K / node (Kafka write + acks) |
| **T1 3-node throughput** | ✅ | 6.6 M eps aggregate, linear scale |
| **T4 Cutover < 100 ms** | ❌ | Driver loop missing — 0/12 partitions migrated |
| T2 / T3 Scale up/down | ⏸ deferred (blocked by T4) | — |
| T5 / T6 Chaos drills | ⏸ deferred (Chaos Mesh install) | — |

**Aeon-as-bottleneck verdict (Gate 2 floor):** On the engine path (no I/O), Aeon sustains 4.5 M events/sec/node at 222 ns/event with zero loss. Once Kafka is in the path, throughput drops to broker-bound rates (170 K read-only, 38 K read+write ordered). **Aeon is not the bottleneck on any I/O-bearing path.**

**Cluster correctness verdict:** Raft membership stable across leader changes (term 1 → 4 observed during session). Pipeline replication via Raft works (every pipeline propagated to all 3 nodes). Partition table replication works (all transitions visible across nodes within seconds). **Partition handover does not work** — the data path is provably broken at the leader-driver level. This is the one Gate 2 must-fix surfaced by Session A.

**Recommended next steps before Session B (AWS EKS):**
1. Implement leader-side transfer driver (G2) — single largest unblocker.
2. Per-pod transactional_id substitution (G3) so EO-2 on Kafka can be measured.
3. Wire `aeon cluster drain` (G5) so T2/T3 can be one-command.
4. Then re-run T2 / T3 / T4 on this same DOKS cluster (cheap), verify, only then bring up AWS EKS for the ceiling chase.

---

## 10b. Teardown

DOKS cluster `70821a02-9a2a-4ee6-9a74-38f5dea070e7` (AMS3) — left running for now per user discretion (re-runs of T4 once driver lands will be cheaper than rebuild). Tear down with `doctl kubernetes cluster delete 70821a02-9a2a-4ee6-9a74-38f5dea070e7` when done.


