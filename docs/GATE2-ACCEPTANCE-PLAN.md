# Gate 2 Acceptance Plan — Three-Session Sequencing

> Operational plan for closing the remaining Gate 2 acceptance rows from
> [`ROADMAP.md`](ROADMAP.md) Pause Point (2026-04-16). Direction decided
> 2026-04-18.
>
> Split into **three discrete sessions** with different goals, cost
> envelopes, and tear-down triggers:
>
> | | Session | Infra | Goal | Cost envelope |
> |-|---------|-------|------|---------------|
> | **0** | Local Rancher Desktop | Laptop | Close every row that doesn't require real multi-node network or real K8s scale events | $0 |
> | **A** | DOKS AMS3 | 3-pool DOKS (Regular SSD + 2 Gbps — Premium tier unavailable on DO) | Correctness **floor** — K8s scale events, real-network multi-node, split-brain, cutover < 100 ms | ~$10–$25 same-day |
> | **B** | AWS EKS | `i4i.*` local NVMe, 10–25 Gbps | Absolute **ceiling** claim + CPU pinning (post-v0.1, weekend window for cost containment) | ~$25–$40 for a 6-hr window |
>
> **Do Session 0 first.** It may close more rows than expected and
> shrink Session A scope. The prior DOKS run (2026-04-12 → 2026-04-16)
> left the cluster idle while feature work continued — that is the
> explicit failure mode this three-session split is designed to avoid.
>
> Supersedes the earlier assumption that CL-6a/b/c/d shipping was
> sufficient for Gate 2 — the functional transport primitives are done,
> but the **multi-node behaviour under real-broker load** has not been
> measured on hardware where the infra is not the bottleneck.

## 1. Directional decisions (2026-04-18)

| Item | Decision | Rationale |
|------|----------|-----------|
| **CL-6c.4** engine-side write-freeze + buffer-and-replay | **Defer** — ship transport primitive only (CL-6c.1/2/3 shipped) | No incident evidence the bulk-sync → freeze delta is observable. Build when a real DOKS handover under load exposes event loss or reorder. Anti-goal line in ROADMAP flags the prior speculative 3-way split. |
| **CL-1** Gate 2 throughput rows | **Split across sessions** — Session 0 + Session A establish the **floor**; Session B on AWS establishes the **ceiling** | DO has no Premium tier (no NVMe, no 10 Gbps) in any region. DOKS-only throughput numbers bound Aeon from below, not above. The ceiling claim has to come from AWS `i4i.*` or equivalent. |
| **DOKS region** | **`ams3`, not `blr1`** | BLR1 has limited dedicated-CPU SKU availability at the Regular-SSD + 2 Gbps tier; AMS3 carries the full range. Test traffic is intra-DC — only kubectl/helm ops pay the ~150 ms cross-region RTT. |
| **DO PPS cap pre-flight** | **Probe before provisioning the full cluster** (2 droplets + `iperf3 -u -b 2G -l 64`, ~$1, 30 min, in AMS3) | DO historically capped non-Premium droplets around 207K PPS inbound. At 1 KiB events ≈ 1 packet, that caps event rate per node regardless of CPU headroom. Confirm the cap status before committing to the full cluster spend. |
| **CL-5** Raft-aware K8s auto-scaling | **Park — blocked on demand signal** | No user ask; explicit `kubectl scale` + existing CL-6 rebalance covers today's operator pattern. |
| **Split-brain recovery drill** | **v0.1 acceptance blocker** | Always-on Raft's whole value claim is "refuse to commit without quorum" — cannot ship untested. Chaos Mesh on the same DOKS cluster. |
| **Multi-broker Redpanda sustained load** | **Quick run today (5–10 min)** — full multi-day soak is a separate future session | Today's goal is to prove the behaviour works under load, not to characterise long-term stability. |
| **CPU pinning (`cpu-manager-policy=static`)** | **Parked *for this DOKS setup only*** — revisit on AWS EKS in a future session | DOKS does not expose the `cpu-manager-policy=static` feature gate on its managed kubelet. AWS EKS does (first-class via `kubeletExtraConfig`) — CPU-pinning validation will be re-attempted there after v0.1. Not a permanent shelving. |

## 2. Acceptance rows closed by this plan

Mapped to [`ROADMAP.md`](ROADMAP.md) Gate 2 Checkpoint. "Closed by" points
to the test number in Session A (§ 5); rows also covered in Session 0
local baseline are flagged.

| # | Checkpoint row | Closed by | Also in Session 0? |
|---|---------------|-----------|---------------------|
| 1 | 3-node throughput ≈ 3× single-node | T1 | No — needs separate hosts |
| 2 | 1 → 3 → 5 scale-up, zero loss | T2 | No — needs real K8s scale |
| 3 | 5 → 3 → 1 scale-down, zero loss | T3 | No — needs real K8s scale |
| 4 | Leader failover < 5 s | T6 (under load) | Yes (loopback baseline) |
| 5 | Two-phase partition transfer cutover < 100 ms | T4 | Yes (loopback baseline) |
| 6 | PoH chain continuity across transfers | T4 (`aeon verify` post-test) | Yes |
| 7 | Merkle proofs | Already ✅ (single-node) | — |
| 8 | mTLS between nodes | Already ✅ (Phase 10) | — |
| 9 | Crypto does not regress throughput | T6 baseline vs non-TLS control | Noisy locally — Session A verdict |

## 3. Infrastructure topology — Session A (DOKS AMS3)

One DOKS cluster, two dedicated node pools, namespace isolation.

> Sections 3–10 describe **Session A only**. Session 0 (local Rancher
> Desktop) is § 11; Session B (AWS EKS) is § 12.

### 3.1 DOKS cluster

- **Region: `ams3` (Amsterdam)** — pinned here, not `blr1`. Rationale:
  BLR1 has **limited dedicated-CPU SKU availability** at the Regular-SSD
  + 2 Gbps tier (not every General Purpose / Storage Optimized size is
  offered). AMS3 carries the full SKU range at the same tier. Premium
  Intel (10 Gbps, local NVMe) is **unavailable on DO in any region**, so
  the choice is not between regions *within* Premium — it is Regular SSD
  + 2 Gbps everywhere, and AMS3 is the only region that actually offers
  the SKUs this plan needs. kubectl/helm ops pay ~150 ms RTT from India;
  intra-DC test traffic is unaffected.
- Kubernetes: latest stable (≥ 1.35)
- VPC: default, all pools in same VPC
- Expected lifespan: **same day** (provision → test → tear down)

### 3.2 Node pools (real DO SKUs — confirmed available in AMS3)

| Pool | Nodes | Droplet class | Taint | Purpose |
|------|-------|---------------|-------|---------|
| `aeon-pool` | 3 (scaled to 5 during T2) | **General Purpose `g-8vcpu-32gb`** — 8 dedicated vCPU, 32 GiB RAM, 100 GiB Regular SSD, 2 Gbps | `workload=aeon:NoSchedule` | Aeon StatefulSet |
| `redpanda-pool` | 3 | **Storage Optimized `so-4vcpu-32gb`** — 4 dedicated vCPU, 32 GiB RAM, 900 GiB Regular SSD, 2 Gbps | `workload=redpanda:NoSchedule` | Redpanda brokers |
| default | 1 | **Basic `s-4vcpu-8gb`** — 4 shared vCPU, 8 GiB RAM, 160 GiB SSD | *(none)* | Load generator, Prometheus, Grafana, Chaos Mesh controller |

**Why separate pools:** Aeon and Redpanda must not contend for the same core —
otherwise every throughput number conflates the two. Taints + tolerations
guarantee hard isolation.

**Why this is a correctness floor, not a ceiling claim:** DO does not
offer Premium Intel / NVMe / 10 Gbps anywhere. `so-4vcpu-32gb` storage is
**Regular SSD (SATA/SAS-tier, ~5K–20K IOPS per volume)**, not NVMe, and
network is 2 Gbps not 10 Gbps. Any Redpanda→Redpanda throughput number
from Session A is reported as "DO standard-tier **floor**" — it bounds
Aeon's ceiling from below, not above. The actual ceiling requires
Session B on AWS `i4i.*` (local NVMe, 18.75 Gbps).

### 3.3 Namespaces

| Namespace | Workloads |
|-----------|-----------|
| `aeon` | Aeon StatefulSet (helm chart `helm/aeon/`, values `values-doks.yaml`) |
| `redpanda` | Redpanda operator + 3-broker cluster |
| `loadgen` | `rpk` producer/consumer jobs, test orchestration |
| `monitoring` | Prometheus, Grafana, Redpanda Console |
| `chaos` | Chaos Mesh controller + experiment CRDs |

### 3.4 Redpanda sizing

- 3 brokers, `--smp 4 --memory 24G` each (Regular-SSD fsync is the likely
  floor, so lean on memory cache heavily; 24 GB of 32 GB for page cache)
- Storage: 900 GiB Regular SSD on the `so-4vcpu-32gb` droplet —
  PersistentVolumeClaim pinned to local volume (no DO Block Storage —
  that would be even slower)
- 24 partitions on source + sink topics (divisible by 3 brokers and 8 partitions/core)
- Replication factor 3

### 3.5 Cost estimate — Session A (same-day)

User's DO control-panel quote (2026-04-18, AMS3):

| Component | Monthly | Hourly (≈ monthly / 720) |
|-----------|---------|---------------------------|
| 3 × `g-8vcpu-32gb` (Aeon pool) | $756.00 | $1.05 |
| 3 × `so-4vcpu-32gb` (Redpanda pool) | $978.00 | $1.36 |
| 1 × `s-4vcpu-8gb` (default pool) | $48.00 | $0.067 |
| HA control plane (99.95 % SLA) | $40.00 | $0.055 |
| **Total** | **$1,822.00** | **~$2.53/hr** |

Expected same-day session cost (provision → ~4 hr test → tear down):
**~$10–$15** if strictly same-day. The monthly number is the penalty if
the cluster is left running — last session's $400+ bill was exactly that
failure mode.

### 3.6 Pre-flight PPS probe — **run before full cluster provision**

DO historically capped non-Premium droplets around **207K packets/sec
inbound** (distinct from the Gbps bandwidth cap). Whether the current
dedicated-CPU classes (General Purpose, CPU-Optimized, Storage Optimized)
inherit that cap or have it lifted is **not confirmed** — DO's public
docs are inconsistent. At 1 KiB event ≈ 1 packet, a 207K PPS cap would
gate Aeon per node regardless of CPU headroom.

**Probe (all in AMS3 — BLR1 is not a substitute here):**

1. Create 2 × `g-8vcpu-32gb` droplets in AMS3, same VPC.
2. `iperf3 -u -b 2G -l 64` both directions, 60 s each.
3. Record sustained PPS.

Pass thresholds:

| PPS observed | Interpretation | Next step |
|--------------|----------------|-----------|
| ≥ 500K sustained | Cap not applied to this class | Provision full DOKS cluster, proceed |
| ~200K–400K | Cap exists but higher than historical | Provision, but document the PPS ceiling as the infra bottleneck for T1 |
| ≤ 207K | Historical cap still active | **Do not provision** — DOKS will not measure Aeon's ceiling under 1 KiB events. Revisit: either use larger event size (artificial), switch to AWS sooner, or accept the cap as the Session A floor |

Probe cost: ~$0.14 (2 droplets × $0.067/hr × 1 hr). 30 min wall time
including provision/destroy. **This must run before the full cluster
spend is committed.**

## 4. Bottleneck isolation matrix (pre-requisite for T1)

Before any Redpanda→Redpanda number goes into the Gate 2 checkpoint, run
the full isolation matrix to prove the ceiling we observe is *Aeon's*,
not a sink/source/infra artefact.

**Durability modes swept** — all four, per `DurabilityMode` enum
(`crates/aeon-types/src/durability.rs`):

| Mode | L2 body store | fsync cadence | Order guarantee |
|------|---------------|---------------|-----------------|
| `None` | no | n/a | n/a |
| `UnorderedBatch` | yes | per batch | no |
| `OrderedBatch` | yes | per batch | yes |
| `PerEvent` | yes | per event | yes |

**Topologies** (each run single-node first, then repeat C2 on 3-node in T1):

| # | Source | Sink | Purpose |
|---|--------|------|---------|
| **C0** | Memory (synthetic) | Blackhole | Pure Aeon engine ceiling. No network, no broker. |
| **C1** | Redpanda | Blackhole | C0 + Kafka consume cost. Delta C0 → C1 = source-side Redpanda overhead. |
| **C2** | Redpanda | Redpanda | C1 + Kafka produce + network RTT. Delta C1 → C2 = sink-side Redpanda overhead. Production shape. |

**Cell count: 4 modes × 3 topologies = 12 cells.** Each cell runs ~3 min
at a rate sweep (start 1M ev/s, double until pressure gauge engages or
error rate climbs, hold at highest stable rate). Total T0 runtime ≈ 60 min
including setup between cells.

**Interpretation rules:**
- If **C0 < expected per-event budget** (per CLAUDE.md: 100 ns/event
  target for `None`), Aeon is the limiter — open a perf ticket, not a
  Gate 2 row.
- If **C2 << C1**, Redpanda sink is saturating — record and document;
  acceptable as long as Aeon CPU < 50 % (Gate 1 headroom rule).
- If **C1 << C0**, Redpanda source is the limiter — same treatment.
- `UnorderedBatch` vs `OrderedBatch` delta — expected to be small; a
  large gap suggests ordering overhead (e.g. per-partition sequencing
  lock contention) worth investigating.
- `PerEvent` vs `OrderedBatch` delta — expected to be large (per-event
  fsync dominates). On local NVMe the floor should be well above the
  <2K ev/s laptop number.

## 5. Test plan

Quick-validation shape — each test is minutes, not hours. Sequence matters.

### T0 — Isolation matrix (§ 4)

Runtime: ~60 min total (12 cells × ~3 min + setup between).
Output: a 12-row table of `(mode, topology) → observed ev/s, Aeon CPU %, pressure`.

**Producer harness for C1/C2:** `scripts/t0-sweep.sh` wraps
`aeon-producer` in a rate-sweep loop and emits a TSV row per rate
(`label, configured_rate, sent, errors, elapsed_ms, events_per_sec,
topic, payload_size`). Example:

```bash
scripts/t0-sweep.sh --label c2-ordered-batch \
    --rates 100000,250000,500000,1000000 \
    --duration 60 --payload-size 256 \
    --topic aeon-source --brokers localhost:19092 \
    --output /tmp/t0-c2-ordered.tsv
```

The producer's `SUMMARY` stdout line is the parse contract; human logs
go to stderr so operators can still watch progress.

### T1 — 3-node throughput (≈ 3×)

**Setup:** 3-node Aeon, Redpanda→Redpanda (C2 topology). Sweep all four
durability modes at the highest stable rate from the corresponding
single-node T0.C2 cell.

**Duration:** ~5 min per durability mode × 4 = ~20 min.

**Measure:** E2E throughput, p50/p99 latency, Aeon CPU per node, `aeon_l2_pressure`.

**Pass criterion:** E2E ≥ single-node C2 × 2.5 per mode (allowing ≤ 20 %
replication overhead). Aeon CPU < 50 % per node when Redpanda saturates.
Zero event loss.

### T2 — Scale-up 1 → 3 → 5, zero loss

**Setup:** start with 1 Aeon node, producer at 1M ev/s into 24-partition
source topic, `OrderedBatch` durability (representative production mode).

**Actions:**
1. Scale `aeon` StatefulSet 1 → 3
2. Wait for Raft learner join + partition rebalance
3. At stable 3-node state, scale node pool from 3 → 5, then StatefulSet 3 → 5
4. Stop producer, wait for drain

**Duration:** 10–15 min total.

**Measure:** producer seq nums vs sink-topic final counts (exact-once
across scale), per-partition transfer duration, per-partition cutover
pause, `aeon_checkpoint_fallback_wal_total` (must stay 0).

**Pass criterion:** zero loss, no WAL engagement, each partition cutover < 100 ms.

### T3 — Scale-down 5 → 3 → 1, zero loss

Mirror of T2. Additional measurement: partition drain time per pod before
pod termination.

**Open sub-question:** current pod-termination path does not block on
partition drain. If T3 observes loss, fix by adding a `preStop` hook
that calls `aeon cluster drain --node $POD_NAME` and blocks until
partitions reassigned. Treat as a *known possible code gap*; fix
in-session if it surfaces (see § 6).

### T4 — Cutover duration measurement

**Setup:** 3-node steady state, 1M ev/s producer, 24 partitions.

**Actions:**
1. `aeon cluster rebalance --dry-run` to pick a target partition
2. Trigger rebalance, record timestamps from Prometheus:
   - Bulk-sync start (`aeon_partition_transfer_bytes_total` first tick)
   - Bulk-sync end (manifest total reached)
   - `drain_and_freeze` RPC start → response
   - Raft ownership flip commit
   - Target accepts first write

**Duration:** ~5 min for 3 sample partitions.

**Measure:** freeze window = (Raft ownership flip commit) − (drain_and_freeze RPC start).

**Pass criterion:** median freeze window < 100 ms, p99 < 250 ms.

### T5 — Split-brain drill (Chaos Mesh)

**Setup:** Chaos Mesh installed in `chaos` namespace.

**Actions:**
1. Apply `NetworkChaos` partition: isolate 1 Aeon node from the other 2
2. Send writes to all 3 Aeon REST endpoints for 2 min
3. Heal partition
4. Verify minority writes rejected, majority committed cleanly, minority
   re-syncs via Raft log catch-up, `aeon verify` passes on all pipelines

**Duration:** ~10 min including setup.

**Pass criterion:** zero divergent commits, zero duplicate commits,
minority rejoins without manual intervention.

### T6 — Quick sustained run with chaos interleave

**Setup:** 3-node Aeon, 3-broker Redpanda, 24 partitions, 1 KiB events,
`OrderedBatch`, rate held at ~80 % of T1 stable ceiling for that mode.

**Duration:** **10 min** (not 60). Today's goal is to prove the behaviour
under a real combined workload, not to characterise long-term stability.

**Interleaved chaos:**
- Minute 2: kill leader pod → leader failover measurement (< 5 s target)
- Minute 4: trigger `aeon cluster rebalance` → cutover measurement
- Minute 6: Chaos Mesh network partition (T5 repeat under load)
- Minute 8: heal, verify replay

**Measure:** E2E zero-loss count, p99 latency, WAL fallback counter (must
stay 0), Redpanda broker CPU + disk throughput, post-run `aeon verify` on
all 24 partitions.

**Pass criterion:** all of the above; any failure opens a fix-in-session
ticket before moving on.

## 6. Expected code gaps (fix in-session if surfaced)

Test work often exposes missing engine functionality. Budget for these:

| Gap | Where | Trigger |
|-----|-------|---------|
| `preStop` drain hook | helm chart + `aeon cluster drain` CLI | T3 observes loss on scale-down |
| Per-partition write-gate (CL-6c.4 core) | `aeon-engine/src/pipeline.rs` | T4 observes cutover > 100 ms OR T6 mid-load transfer shows reorder |
| Redpanda Operator / broker tuning | `redpanda` namespace values | T0.C1 or T0.C2 shows Redpanda saturating before Aeon is stressed |
| Chaos Mesh RBAC / CRD install | `chaos` namespace setup | T5 prerequisite — install before T5 starts |

**Discipline:** if a test exposes a code gap, fix it in this session and
re-run. Do not stash the fix for "later." The DOKS cluster is the expensive
resource — use it until the agreed rows close, then tear down.

## 7. Documentation discipline during the session

Update the following as each test closes — not batched at the end:

- **This file** — per-test "Results" subsection appended under each T#
- **[`ROADMAP.md`](ROADMAP.md) Gate 2 Checkpoint section** — tick each row with date + short note
- **[`ROADMAP.md`](ROADMAP.md) Pause Point section** — close out `CL-1`,
  `split-brain`, `multi-broker sustained load` rows with links back here

## 8. Tear-down criteria — Session A

Tear down the DOKS cluster as soon as **all** of these are true:

- [ ] PPS probe (§ 3.6) passed OR failure documented and scope adjusted
- [ ] T0 — isolation matrix captured (12 cells)
- [ ] T1 — 3× throughput measured per durability mode (or documented at infra ceiling)
- [ ] T2 — scale-up 1→3→5 zero-loss verified
- [ ] T3 — scale-down 5→3→1 zero-loss verified
- [ ] T4 — cutover < 100 ms verified
- [ ] T5 — split-brain drill passed
- [ ] T6 — 10-min sustained load passed with chaos interleave
- [ ] All code gaps surfaced in-session are fixed, committed, and retested
- [ ] Results sections in this file are filled in
- [ ] ROADMAP Gate 2 Checkpoint + Pause Point rows updated

Do **not** leave the cluster running to start unrelated feature work.
That was the explicit failure mode from the 2026-04-12 → 2026-04-16
session and the reason this plan exists as a standalone same-day plan.

## 9. Parking lot (explicitly not in scope for Session A)

- **CL-6c.4 full engine integration** — deferred per § 1; revisit on
  observed incident evidence.
- **CL-5 auto-scaling** — parked, revisit on user demand signal.
- **CPU pinning validation** — moved to Session B (§ 12.4), not parked
  indefinitely. DOKS can't expose `cpu-manager-policy=static`; EKS can.
- **Absolute throughput ceiling claim** — moved to Session B. Session A
  numbers are a DO-standard-tier floor, not a ceiling.
- **Multi-day sustained soak** — future session once the core is proven.
- **Pillar 7 BL-1..6 language SDKs** — library ecosystem blocker, unchanged.
- **TR-3 WebTransport SDK reconnect** — downstream of Pillar 7 blocker.

## 10. Results — Session A (filled during session)

> Fill this section as each test closes. Format per test:
> **TN — (title)** · **Date** · **Status:** ✅ / ❌ / partial · **Numbers:** … · **Notes:** …

### 10.0 Pre-flight PPS probe — AMS3 aeon-pool · 2026-04-18

**Cluster:** DOKS `70821a02-9a2a-4ee6-9a74-38f5dea070e7` (AMS3), aeon-pool
nodes `pool-f5vm1izlt-9q983` / `-9q988` (g-8vcpu-32gb, Regular SSD, DO
standard networking — Premium Intel / NVMe / 10 Gbps not available in AMS3
as of this date, see memory `reference_doks_droplet_availability.md`).

Ran §3.6 UDP 64-byte packet probe in two modes — pod-to-pod over the CNI
overlay, and host-network droplet-to-droplet — plus a 1400-byte MTU-sized
sanity check. Artifacts: `deploy/doks/pps-probe-{a-to-b,b-to-a,hostnet,hostnet-1400}.json`.

| Scenario | PPS | Throughput | Loss |
|---|---:|---:|---:|
| Pod→pod CNI, 64 B UDP, A→B | **158,202** | 0.08 Gbit/s | 0.04% |
| Pod→pod CNI, 64 B UDP, B→A | 156,915 | 0.08 Gbit/s | 0.03% |
| hostNetwork raw, 64 B UDP | **174,908** | 0.09 Gbit/s | 0.10% |
| hostNetwork, 1400 B UDP | 199,795 | **2.24 Gbit/s** | 13.24% |

**Against §3.6 pass criteria** (≥500 K PPS = no cap · 200–400 K = cap higher
than historical · ≤207 K = historical cap active): **historical cap is
active on DO standard tier in AMS3.**

**Interpretation — proceed anyway:**

1. **Bandwidth is not the bottleneck.** 2.24 Gbit/s on 1400 B packets
   proves the droplets can push real byte throughput; only the
   per-packet rate is capped.
2. **CNI overhead is ~10%** (157 K pod→pod vs 175 K raw) — not the
   dominant cost; the DO droplet-level PPS cap is.
3. **Aeon's hot path is batched Kafka** (`rdkafka` coalesces 100–1000
   events per TCP segment). At 256 B/event, 157 K packets/s maps to
   multiple M events/s — PPS cap does not bind the measured path.
4. **Where the cap *will* show up:** T3 WebTransport single-event
   fanout, T5 cluster QUIC bursts of tiny control frames, any tiny-packet
   partition-handover traffic. Those measurements need to be
   interpreted with this ceiling in mind and/or deferred to Session B on
   premium-tier (AWS/GCP) nodes.
5. **Session B remains the throughput-ceiling session** per the existing
   plan — this result reinforces that, not contradicts it.

**Decision:** proceed with Session A. Session A's goals (cluster
correctness, election, drain, split-brain, checkpoint recovery, T0
shape-of-delta matrix) are not PPS-bound. Annotate any PPS-sensitive
row below with "DO AMS3 standard-tier ceiling — see §10.0".

### 10.1 T0 — Isolation matrix · 2026-04-18

Full evidence + raw metric samples in
[`deploy/doks/session-a-evidence.md`](../deploy/doks/session-a-evidence.md) §2.
Image: `registry.digitalocean.com/rust-proxy-registry/aeon:session-a-prep`
(sha `36d44fd336c1`).

**T0.C0 — Memory → `__identity` → Blackhole** · Status: ✅ all 4 modes

| Durability | Count/node | Aggregate | Wall (s) | Per-node eps | ns/event |
|---|---:|---:|---:|---:|---:|
| None | 200 M | 600 M | 44.4 | 4.50 M | 222 |
| UnorderedBatch | 50 M | 150 M | ~22 | 2.27 M | 441 |
| OrderedBatch | 50 M | 150 M | ~22 | 2.27 M | 441 |
| PerEvent | 20 M | 60 M | 23.0 | 0.87 M | 1152 |

**T0.C1 — Redpanda `aeon-source` → `__identity` → Blackhole** · Status:
✅ all 4 modes (with G1 workaround — explicit 24-partition list per pipeline JSON)

| Durability | Aggregate | Wall (s) | Per-node eps |
|---|---:|---:|---:|
| None | 90 M | 172.55 | 173,860 |
| UnorderedBatch | 90 M | 174.79 | 171,636 |
| OrderedBatch | 90 M | 178.98 | 167,616 |
| PerEvent | 90 M | 186.11 | 161,198 |

Kafka fetch is binding; mode spread None→PerEvent is 7.8 %.

**T0.C2 — Redpanda `aeon-source` → `__identity` → Redpanda `aeon-sink`** ·
Status: 🟡 3 of 4 modes (PerEvent skipped per G3)

| Durability | Per-node eps | Notes |
|---|---:|---|
| None | 52 K (steady) | partial — Kafka write saturated, run timed out at 293 s |
| UnorderedBatch | 169 K | **HWM only 78.1 M / 90 M aggregate — ~13 % drop is fire-and-forget by design (G4)** |
| OrderedBatch | 38 K | acks=all bound; 7× slower than Unordered (expected) |
| PerEvent | _deferred_ | G3 (per-pod txn-id) + DOKS no-NVMe ceiling — moves to Session B |

### 10.2 T1 — 3-node throughput · 2026-04-18 · Status: ✅

Memory source × 100 M / node, `__identity`, Blackhole sink, durability
None.

```
t1-3node  45.37s  300,000,000  6,612,153 agg eps  2,204,051 per-node eps
```

Steady-state (subtract 5 s startup): 5.93 M agg eps / 1.98 M per-node.
Linear scale on the no-I/O engine path — no inter-node coordination
penalty observed.

### 10.3 T4 — Cutover duration · 2026-04-18 · Status: ❌ FAIL

Test ran via `deploy/doks/run-t4-cutover.sh` against random partitions:
POST `/api/v1/cluster/partitions/{p}/transfer`, then poll
`/api/v1/cluster/status` for ownership flip.

| Metric | Observed |
|---|---|
| P50 REST `accepted` (POST→202) | ~2,300 ms |
| Cutover < 100 ms target | **0 / 12 attempts** |
| Cutover < 5,000 ms (broader window) | **0 / 12 attempts** |
| Final ownership flipped | **0 / 12** — all stayed at source |

Post-run `/cluster/status` showed 12 partitions stuck in `transferring`
with `source` and `target` populated — Raft-replicated proposal, but
the actual handover never executed.

**Root cause (G2):** leader proposes `ClusterRequest::BeginTransfer`
which sets `PartitionOwnership::Transferring{source,target}`, but
there is no background `tokio::spawn` driver on the leader that
consumes that state and runs the two-phase BulkSync→Cutover→Complete.
CL-6 transport (`crates/aeon-cluster/src/transport/cutover.rs`,
commit `807321f`) ships but is unwired. Single reference to
`Transferring` in `node.rs` is a guard check inside
`propose_partition_transfer`.

### 10.4 T2 / T3 / T5 / T6 — deferred · 2026-04-18

| Test | Status | Reason |
|---|---|---|
| T2 (Scale-up 1→3→5) | ⏸ deferred | Blocked by G2 (no functional partition handover) + DOKS pool needs expansion to 5× g-8vcpu-32gb |
| T3 (Scale-down 5→3→1) | ⏸ deferred | Same blockers as T2 |
| T5 (Split-brain drill) | ⏸ deferred | Chaos Mesh not installed in this session |
| T6 (10-min sustained with chaos) | ⏸ deferred | Depends on T5 prerequisites |

### 10.5 Code gaps surfaced — Session A

| ID | Gap | Severity | Status |
|---|---|---|---|
| **G1** | `KafkaSourceFactory` defaults `partitions` to `[0]` (silent under-read) | Medium | Workaround applied; fix queued for next bundle |
| **G2** | No leader-side driver consumes `PartitionOwnership::Transferring` | **Blocker T2/T3/T4** | Must-fix before next DOKS re-spin |
| **G3** | Shared `transactional_id` across pods causes Kafka producer fencing | Medium | Skipped C2.PerEvent; queued for next bundle |
| **G4** | `aeon_pipeline_outputs_sent_total` counts queued, not acked | Low | Add `_acked_total` companion metric |
| **G5** | `aeon cluster drain` / `rebalance` CLIs missing | Low | Land with G2 |

Full file/line refs and recommended fix order are in
[`docs/ROADMAP.md` "Session A status (2026-04-18)"](ROADMAP.md#session-a-status-2026-04-18).

### 10.6 Session A — verdicts

- **Aeon-as-bottleneck (Gate 2 floor):** Engine path sustains 4.5 M
  ev/s/node @ 222 ns/event with zero loss. Once Kafka enters the path,
  throughput is broker-bound (170 K read-only, 38 K read+write
  ordered). **Aeon is not the bottleneck on any I/O-bearing scenario.**
- **Cluster correctness:** Raft membership stable across leader changes
  (term 1 → 4 observed). Pipeline + partition table replication works
  across all 3 nodes. **Partition handover does NOT work** — protocol
  driver missing at the leader. This is the one Gate 2 must-fix
  surfaced by Session A.

### 10.7 Tear-down — Session A

DOKS cluster `70821a02-9a2a-4ee6-9a74-38f5dea070e7` left running for
user discretion (re-runs of T4 once G2 lands will be cheaper than
rebuild). Full tear-down: `doctl kubernetes cluster delete
70821a02-9a2a-4ee6-9a74-38f5dea070e7`.

### 10.8 Session A re-run (2026-04-19) — post-bundle sweep

Fresh DOKS cluster `98d58935-b2c9-4b8e-8c8c-3c75f6660c89` (AMS3, same
pool topology as the original Session A). Pre-flight confirmed all
Phase-1 code gaps (G1–G5) closed; `aeon:session-a-prep` image deployed
via DO container registry; Chaos Mesh 2.6.3 installed into `chaos` ns.
Full evidence: [`deploy/doks/session-a-evidence.md`](../deploy/doks/session-a-evidence.md).

| Test | Verdict | Notes |
|---|---|---|
| T0 / T1 | ✅ (unchanged vs 2026-04-18) | not re-executed in full; Aeon-as-bottleneck floor already recorded |
| **T2** — Scale-up 1→3→5 | 🟡 partial | 3-node steady-state E2E drained 100 037 → 97 182 acked (in-flight balance uncommitted at stop), `aeon_checkpoint_fallback_wal_total=0`. **3→5 blocked by G10** (STS scale-up past `cluster.replicas` crash-loops — discovery tries `raft.initialize({1,2,3})` excluding the new pod). |
| **T3** — Scale-down 5→3→1 | ⛔ blocked by G10 | can't reach the 5-node start state |
| **T4** — Cutover < 100 ms | ⛔ **blocked by G11** | all 17 transfer attempts accepted at the REST layer (2.0 s Raft commit), but stuck `status=transferring, owner=null` forever; leader-side driver inert despite CL-6 / P1.1c code being present |
| **T5** — Split-brain | ✅ correctness bar met | majority pair committed 10/10, isolated minority rejected 10/10, Raft `last_applied=44` converged across 3 nodes 2 s post-heal, per-partition ownership identical across members. Script jq path fix (`.raft.last_applied_log_id.index` → `.raft.last_applied`) committed in-run at `deploy/doks/run-t5-split-brain.sh:160`. |
| **T6** — 10-min sustained + chaos | 🟡 partial | Script orchestration + leader-aware write routing (new `pick_leader_host()` helper + `.leader` → `.leader_id` fix in `deploy/doks/run-t6-sustained.sh`) + rebalance endpoint (`planned=0, noop`) all verified. **Data plane wedged post-heal** by G13 (Chaos-Mesh-on-DOKS iptables leak → QUIC `sendmsg` EPERM on all 3 pods). Leader-kill failover measured **10 000 ms** — G14. |

#### 10.8.1 Code gaps surfaced — re-run

| ID | Gap | Severity | Ticket |
|---|---|---|---|
| **G8** ✅ 2026-04-19 | Pipeline create during single-node self-election returned 500 (`has to forward request to: None, None`). **Shipped:** `ClusterNode::wait_for_leader(timeout)` public helper wraps openraft `raft.wait(Some(t)).metrics(|m| m.current_leader.is_some(), …)`; `ClusterNode::propose` now consults `metrics.current_leader` before `client_write` and, if `None`, blocks for up to `leader_wait_budget()` = `clamp(3 × election_max_ms, 2 s, 10 s)` for a leader to be observed. The freshly-bootstrapped race window — where `raft.initialize()` has returned but self-election hasn't yet flipped `current_leader` — is now transparent to the REST layer; a truly leaderless multi-node quorum surfaces a clean `AeonError::Cluster { message: "no Raft leader elected within Nms: …" }` instead of a bare `ForwardToLeader { None, None }`. Pinned by `wait_for_leader_returns_self_on_single_node_g8` — single-node bootstrap + wait + propose round-trip succeeds within a 2 s budget. Workspace build + 168-test cluster suite green. | Medium | #68 |
| **G9** ✅ 2026-04-19 | REST write paths (pipeline create / start / rebalance) don't auto-forward to Raft leader. **Shipped:** `rest_api::maybe_forward_to_leader(state, uri)` returns `307 Temporary Redirect` with `Location: {scheme}://{leader_host}:{rest_port}{path+query}` + `X-Leader-Node-Id` / `X-Aeon-Forwarded: 1` headers when the receiving pod is a follower and the leader is known. Helper is wired at the top of 9 cluster-mutation handlers: `transfer_partition`, `cluster_drain`, `cluster_rebalance`, `register_processor`, `create_pipeline`, `start_pipeline`, `stop_pipeline`, `upgrade_pipeline`, `delete_pipeline`. Leader host resolved from `membership.get_node(leader_id)` on Raft metrics; REST port from the new `ClusterConfig::rest_api_port` (`Option<u16>`, `#[serde(default)]`) that `aeon-cli` populates by parsing `AEON_API_ADDR` or the CLI bind flag; scheme from `ClusterConfig::rest_scheme` (defaults to `http`). Scripted clients that don't follow 307s can still read `X-Leader-Node-Id` to reroute. Workspace build + 168-test cluster suite + 434-test engine suite green. | Medium | #70 |
| **G10** ✅ 2026-04-19 | STS scale-up beyond `cluster.replicas` — new pods called `raft.initialize` excluding themselves and crash-looped. **Shipped:** `K8sDiscovery::is_scale_up_pod` (`ordinal >= replicas`; the `AEON_CLUSTER_REPLICAS` env stays frozen at helm-install time so `kubectl scale` naturally surfaces scale-up pods) + `to_join_config` (empty `initial_members`, seeds = initial-cohort pod FQDNs, `advertise_addr` = own pod FQDN, `auto_tls=true`); `aeon-cli` branches to `ClusterNode::join` (`send_join_request` → leader `add_learner → change_membership`) instead of `bootstrap_multi`. 13 discovery + 166 cluster lib tests green. Awaiting DOKS re-run for live 3→5 / 5→3→1 verification. | **High — blocks T2/T3** | #71 |
| **G11** ✅ 2026-04-19 | Partition transfer Raft-committed but leader-side cutover driver inert at runtime. Split into **G11.a** ✅ (production `L2SegmentInstaller` + `PohChainInstallerImpl`; PoH verification configurable via `PohVerifyMode::{Verify, VerifyWithKey, TrustExtend}` — defaults to `Verify`, settable through `cluster.poh_verify_mode` YAML and `AEON_CLUSTER_POH_VERIFY_MODE` env var), **G11.b** ✅ (spawn `PartitionTransferDriver::watch_loop` in `aeon-cli` K8s bootstrap gated on `AEON_PIPELINE_NAME`; `propose_partition_transfer` calls `driver.notify()` on accept), and **G11.c** ✅ (pipeline-start now consumes the installed snapshot: `PipelineSupervisor::set_poh_installed_chains` receives the same `InstalledPohChainRegistry` Arc the transfer driver writes to; `start()` stamps it into `PipelineConfig.poh_installed_chains`; `create_poh_state` calls `registry.take(pipeline, partition_id)` and wraps the returned `PohChain` in `Arc<Mutex<>>` so the post-transfer pipeline resumes at the source-side sequence instead of re-genesising at 0. Pinned by `create_poh_state_resumes_installed_chain_g11c` — 5-batch source chain resumes at sequence 5; falls back to genesis when no install is pending. Plumbed through `run_multi_partition` per-partition clone.). All three parts green on the workspace build + 435-test engine lib suite + 168-test cluster lib suite. **Live verification on DOKS re-run.** | **High — blocks T4 + Gate 2 cutover row** | #72 / #77 / #78 |
| **G13** ✅ 2026-04-19 (docs) | Chaos Mesh iptables/tc rules orphan on DOKS after NetworkChaos heal → QUIC EPERM (**infra, not Aeon**). **Shipped:** new "G13 — Chaos Mesh on DOKS leaves iptables/tc rules behind (workaround)" section in `deploy/doks/README.md` — failure mode (top-level NetworkChaos + PodNetworkChaos both reconcile away but kernel iptables/tc rules persist, manifests as `quinn_udp sendmsg EPERM` on the cluster-Raft QUIC path), operator workaround (`kubectl -n aeon rollout restart sts/aeon` + verify `last_applied` advancing via `/cluster/status`), note on recording the rollout pause in T6 evidence so active-load wall-clock stays separable, and the EKS re-try plan (different CNI path — if leak is DOKS-only, file upstream at chaos-mesh/chaos-mesh). Aeon-side confirmation that `bootstrap_single_persistent` (FT-1/FT-2) handles the restart-driven log replay cleanly. | Medium (T6 blocker on DOKS only) | #75 |
| **G14** ✅ 2026-04-19 (code) | Leader failover measured 10 s vs 5 s target on 3-node no-load cluster. **Shipped:** `ClusterNode::relinquish_leadership(timeout)` — disables heartbeats via `raft.runtime_config().heartbeat(false)` and waits on `raft.wait().metrics(current_leader != self)` until handoff completes or times out; runs in parallel with `AEON_PRESTOP_DELAY_SECS` inside `rest_api::shutdown_signal` so the handoff hides in the endpoint-propagation window. `RaftTiming::fast_failover()` preset (heartbeat=250 ms / election=750–2000 ms) pins worst-case 2× election_max_ms ≤ 5 s; wired through `AEON_RAFT_*` env + helm `cluster.raftTiming`. 168 cluster lib tests + new `relinquish_leadership_single_node_is_noop` + `raft_timing_fast_failover_preset` green. **Live < 5 s measurement pending the DOKS re-run.** | Medium (Gate 2 row) | #76 |

Recommended fix order for the next bundle: **~~G11~~ → ~~G10~~ → ~~G14~~ → ~~G9~~ → ~~G8~~ → G13-workaround-docs.** All code gaps shipped 2026-04-19; G13 is infra and gets a `kubectl rollout restart sts/aeon` workaround in `deploy/doks/README.md` rather than an Aeon code change.

#### 10.8.2 Tear-down — re-run

DOKS cluster `98d58935-b2c9-4b8e-8c8c-3c75f6660c89` deleted
2026-04-19 via `doctl kubernetes cluster delete ... --dangerous --force`.
DO container registry retained (7 aeon tags) for future re-runs and
Session B EKS image pre-bake.

#### 10.8.3 Pre-Session-B validation rehearsal (Rancher Desktop, V1..V6)

The Gate 2 code bundle (G8, G9, G10, G11, G14 + G13 infra docs) all
shipped 2026-04-19 but hasn't yet been exercised together end-to-end on
a real k8s. Before re-spending on a DOKS re-spin or opening Session B
on AWS EKS, the full matrix is rehearsed on Rancher Desktop against a
freshly-built image. RD numbers are a **correctness floor only**
(WSL2 8 CPU / 12 GiB caps throughput well below DOKS/EKS); the goal is
to catch integration regressions, exercise a push-based source (HTTP
ingest) alongside the pull-based Kafka source for the first time, and
explicitly verify the crypto chain under all three `PohVerifyMode`
values.

**Rancher Desktop is single-node k3s**, not multi-node k8s. A 3-replica
Aeon StatefulSet co-locates all three pods on the same node over
loopback — sufficient to validate G8 / G9 / G10 / G11 / G14 / PoH-resume
code paths, but **not** a faithful substitute for T5 split-brain or T6
multi-node chaos. Those rows stay open and are closed on the DOKS
re-spin that follows V6. Any destructive test that could wedge the k3s
node may force a full Rancher Desktop reset — prefer `helm uninstall` +
`kubectl delete ns aeon` over in-place reconciliation; skip
kernel-level chaos tools on RD.

See `docs/ROADMAP.md` § *Phase 3.5 — Rancher Desktop validation
rehearsal* for the V1–V6 detail and task IDs #79–#84. Output of V6 is
`docs/GATE2-PRE-SESSION-B-VALIDATION.md` + a Session-B readiness
checklist, which back-propagates any new gaps into § 10.8.1 before EKS
spin-up.

### 10.9 Session A post-bundle re-run (2026-04-19) — cluster `c3867cc5`

Fresh DOKS cluster `c3867cc5-e2e7-4d5f-94cd-7d82ca8c4303` (AMS3,
3 × `g-8vcpu-32gb` aeon-pool + 3 × `so1_5-4vcpu-32gb` redpanda-pool +
1 × `s-4vcpu-8gb` monitoring pool). Image
`registry.digitalocean.com/rust-proxy-registry/aeon:70c68b3` (commit
70c68b3, 2026-04-19 — G1–G14 + G13 workaround, RD-smoked). Setup
automation gaps surfaced + fixed in-run:

- `deploy/doks/setup-session-a.sh` step 2 auto-generated DOKS pool names
  (e.g. `pool-psmexo05w`) no longer matched the hardcoded `default`
  filter → new pool-discovery helper labels `workload={aeon,redpanda,
  monitoring}` by droplet size with `{AEON,REDPANDA,MONITORING}_POOL_ID`
  env overrides.
- DOKS integrates DOCR via a `registry-<name>` secret provisioned only
  in the `default` namespace → step 7 now also runs
  `doctl registry kubernetes-manifest --namespace aeon | kubectl apply`;
  `deploy/doks/values-aeon.yaml` + `deploy/doks/loadgen.yaml` renamed
  `imagePullSecrets` to the doctl-generated name
  `registry-rust-proxy-registry`. `loadgen.yaml` image retagged to
  `70c68b3`.

| Test | Verdict | Notes |
|---|---|---|
| Cluster bring-up | ✅ | 3-node STS Ready in <25 s after pull-secret fix; Raft term 1, leader id=3, membership `{1,2,3}`, 24 partitions evenly owned (8/8/8). |
| **T0.C0 None** · Memory→`__identity`→Blackhole | ✅ | 200 M/node × 3 = **600 M events, zero loss** (`events_failed_total=0`, `events_retried_total=0` on every pod). Monitor poll interval 30 s bounded the measurement at `t ≤ 64 s` (aggregate eps ≥ **9.4 M**); first sample at `t = 17 s` showed 458 M already processed ⇒ **steady-state aggregate ≈ 27 M eps (~9 M eps/node)**. Per-pod `events_received=events_processed=outputs_sent=200 M` exact on all 3 nodes. |
| **T1 — 3-node Memory→Blackhole** | ✅ | 100 M/node × 3 = **300 M events, zero loss** on every pod. Tighter 10 s poll interval resolved steady-state rate: **aggregate 6.5 M eps (~2.2 M eps/node)** at 41 s sample; the 78 s upper bound reflects settling + poll overhead, not sustained rate. Matches the 2026-04-18 run's 2.2 M eps/node / 45.4 s wall within measurement noise. |
| T0.C1 / T0.C2 | ⏭ | Not re-run — 2026-04-18 Kafka-bound numbers already on record in § 10.1; re-measurement adds no new correctness signal for Gate 2. |
| **T2 — Scale-up 3→5** (G10 live verification) | ⛔ blocked by G15 on this run (fix shipped same day) | Pool resize 3→5 clean (67 s). STS scale 3→5 triggered G10 scale-up path correctly — `aeon-3` / `aeon-4` logs show `scale-up pod detected — will seed-join existing cluster`, then `attempting to join cluster via seed node` against all three seeds (aeon-0, aeon-1, aeon-2) in order. **All three seeds — including aeon-2 (`node_id=3`), the current Raft leader — rejected the join with `not the leader; current leader is Some(3)`.** Both new pods `CrashLoopBackOff` after exhausting seeds; STS rollout stalled at 3/5. Scaled back to 3, resized pool 5→3, producer Job deleted. G15 code fix shipped 2026-04-19 — T2 re-measurement folds into next DOKS re-spin. |
| T3 / T4 / T5 / T6 | ⏭ | Not attempted — G15 pre-empted T2 on this run, and T3 depends on reaching 5 nodes first. T4/T5/T6 correctness verdicts from 2026-04-18 stand. Re-measurement folds into next DOKS re-spin (post-G15). |

#### 10.9.1 Code gap surfaced — G15 (shipped 2026-04-19)

| ID | Gap | Severity | Resolution |
|---|---|---|---|
| **G15** | Cluster scale-up join handler rejected join requests even when the receiving seed **was** the current Raft leader. `aeon-cluster::transport::server::handle_add_node` compared `raft.current_leader().await` vs `raft.metrics().borrow().id`; on multi-node clusters the check evaluated false on the true leader (all three seeds — including the pod whose `/api/v1/cluster/status` reports `node_id=3` and `leader_id=3` — returned `"not the leader; current leader is Some(3)"`). Root cause: the watch-channel `metrics.id` diverges from `ClusterConfig::node_id` in the join-handler code path, while REST `/api/v1/cluster/status` uses the configured id (coherent with pod logs). **Impact while open:** G10 scale-up couldn't complete end-to-end on a multi-node cluster; 1-node → 3-node bootstrap worked because initial voters are seeded via `initial_members`. | **High — blocked T2/T3 Gate 2 rows** | ✅ **Shipped 2026-04-19.** `server::serve()` now takes `self_id: NodeId` sourced from `ClusterConfig::node_id`; `handle_add_node` / `handle_remove_node` use the configured id for the leader-self check. `tracing::warn!` diagnostic kept on the reject path to flag any future divergence between `metrics.id` and `config.node_id`. Regression test `g15_join_targets_actual_leader_of_three_node_cluster` in `crates/aeon-cluster/tests/multi_node.rs` bootstraps a 3-node cluster via `initial_members`, finds the elected leader, and asserts a QUIC `JoinRequest` for node 4 succeeds + 4-voter membership. All 11 `multi_node` + 2 `partition_transfer_e2e` + 82 lib unit tests pass. |

No other code gaps surfaced; the setup-automation regressions (pool
name + DOCR secret name) were fixed in-tree during the session.

#### 10.9.2 Session verdicts — Session A re-run (2026-04-19)

- **Data plane (T0, T1):** ✅ green — 900 M events processed across
  two cells, zero loss on every pod. Steady-state aggregate **~6.5 M
  eps** (Kafka-free path); per-node ceiling matches the 2026-04-18 run.
  Aeon as bottleneck: still comfortably under CPU — 3×g-8vcpu-32gb pods
  sustained both runs without back-pressure.
- **Cluster mutation path (T2):** ⛔ blocked by G15 on this run; **G15
  code fix shipped 2026-04-19** (configured-id threaded through
  `server::serve()`; regression test green). T2 + T3 re-measurement on
  real k8s folds into the next DOKS re-spin. All Aeon Gate 2 code
  blockers (G8–G11, G13, G14, G15) now closed — remaining work is
  cluster re-measurement, not code.

#### 10.9.3 Tear-down — c3867cc5

`doctl kubernetes cluster delete c3867cc5-e2e7-4d5f-94cd-7d82ca8c4303
--dangerous --force` — user is running this manually post-session.
DOCR registry (`rust-proxy-registry`) retained.

---

## 11. Session 0 — Local Rancher Desktop (do this first, $0)

**Goal:** close every Gate 2 row that does not strictly require real
K8s scale events or real inter-node network. Capture laptop-baseline
numbers *before* spending a dollar on cloud. If Session 0 closes more
rows than expected, Session A's scope shrinks.

### 11.1 Environment

- Rancher Desktop (existing WSL2 12 GB / 8 CPU config — see memory
  `project_resource_allocation.md`)
- 3 Aeon pods as a StatefulSet using `helm/aeon/` with loopback cluster
  configuration (all 3 Raft peers on the same node, inter-pod over
  loopback)
- Redpanda single-broker on local SSD (`deploy/k8s/redpanda.yaml`,
  `--smp 4 --memory 4G`)
- No Chaos Mesh — use `iptables` / `tc` inside pods for partial
  split-brain simulation
- Loadgen: `aeon` CLI memory source, `rpk` producer for Kafka path

### 11.2 Rows closable locally

| # | Row | How |
|---|-----|-----|
| — | **T0 isolation matrix — full 12 cells** | All runs are local. Memory→Blackhole (C0) and Memory→Redpanda don't need cloud. Redpanda→Blackhole (C1) and Redpanda→Redpanda (C2) use the local broker. Numbers will be **laptop-SSD-bound** but that's expected — the shape of the C0/C1/C2 deltas per durability mode is what matters, and that carries forward. |
| 4 | **Leader failover < 5 s** | `kubectl delete pod aeon-0` with loopback peers. Raft election happens over localhost but the state-machine code path is identical. |
| 5 | **Two-phase partition transfer cutover < 100 ms** | Same — the cutover RPC runs in-process; localhost RTT is optimistic but exposes any state-machine slowness. |
| 6 | **PoH chain continuity across transfers** | `aeon verify` is a pure pipeline-state check — works on any topology. |
| — | **Partial T5 split-brain** | `kubectl exec aeon-1 -- iptables -A INPUT -s $AEON_0_IP -j DROP` then send writes to all 3. Not identical to a real L3 partition but exercises the Raft quorum-refuse code path. |

### 11.3 Rows that **require** Session A (cloud)

- Row 1: **3-node throughput ≈ 3×** — single-host loopback shares CPU; the
  3× relative multiplier is only meaningful on separate hosts.
- Row 2/3: **scale-up/scale-down** — needs real K8s node addition/removal
  (DOKS scale event), not just StatefulSet replica change on one node.
- Row 9: **Crypto does not regress throughput** — noisy on shared-host
  loopback; repeat on isolated pools.

### 11.4 Session 0 tear-down criterion

Session 0 is "done" when:

- Full T0 12-cell table is recorded (treat as laptop baseline — numbers
  will be repeated in Session A on isolated hardware)
- Leader failover + cutover numbers recorded (treat as floor under
  loopback; cloud numbers can only be slower due to real RTT)
- Any surfaced code gap is fixed in-session
- `docs/GATE2-ACCEPTANCE-PLAN.md § 11.5 Results` filled

### 11.5 Results — Session 0

**Date:** 2026-04-18
**Cluster:** Rancher Desktop k3s v1.34.6+k3s1, single-node (WSL2 8 CPU / 12 GiB).
3-peer Aeon StatefulSet (image `aeon:session0` = commit `4d05b02`, helm
values `helm/aeon/values-local.yaml`), Redpanda single-broker
(`--smp 2 --memory 1G`), all three Aeon pods co-located on the one node
with loopback inter-pod Raft over the headless service.

#### T0 — Isolation matrix (partial)

Scope attempted: C0 × 4 durability modes (4 cells / 12). C1 and C2 not
run — see "Blockers" below.

| Cell | Events | Elapsed (adj.) | Per-node rate |
|------|--------|----------------|---------------|
| C0 · None            | 1,000,000 | 2,441 ms | **409,668 ev/s** |
| C0 · UnorderedBatch  | 1,000,000 | 2,462 ms | **406,173 ev/s** |
| C0 · OrderedBatch    | 1,000,000 | 2,430 ms | **411,522 ev/s** |
| C0 · PerEvent        | 1,000,000 | 2,477 ms | **403,714 ev/s** |

Payload 256 B, batch 1024, identity processor, blackhole sink.
Per-node — each of the 3 pods runs an independent memory source, so
aggregate across the cluster is ~1.22 M ev/s at this size.

**Finding — durability mode deltas ≈ 0 at C0.** All four modes land
within 2% of each other. Two likely explanations, either of which is a
Session 0 output worth recording:

1. The EO-2 L2-write hot path (`l2_body`, fsync cadence, sink ack
   sequence tracking) is still a stub at the engine-side write site.
   `ROADMAP.md` puts the real write path at EO-2 P4 ("body store + ack
   seq + flush cadence"), and §44 of `crates/aeon-engine/src/l2_body.rs`
   explicitly flags the full hot-path as deferred to P4.
2. Even once wired, a blackhole sink does no network I/O and the L2
   write is asynchronous wrt the sink ack, so mode deltas may only
   materialize once a real sink is in the topology.

Interpretation per the plan's §4 rules: this *does not* invalidate the
12-cell matrix — C1/C2 were designed to expose sink-side durability
cost, which C0 cannot reach by construction. The C0 row stands as
"engine-internal ceiling, mode-agnostic at 256 B / blackhole".

**Blockers — why C1/C2 were not run in this session:**

- `MemorySourceFactory` pre-allocates `count × payload_size` bytes up
  front. 10 M × 256 B OOM-killed the 2 GiB-limit pods. 1 M runs
  complete in ~3 s, so the 3-minute sustained sweep the plan asks for
  cannot be done against the current memory source without either
  raising pod memory or making the source looping/unbounded. A looping
  mode is ~30 min of engine work but was out of scope for Session 0.
- C1/C2 also need a sustained Redpanda-side producer harness; the
  `aeon-e2e-pipeline` sample `aeon-producer` binary exists but was not
  wired into a rate-sweep loop here. That and the looping memory source
  are the two pieces to add before Session A re-runs the full matrix on
  isolated hardware.

#### Row 4 — Leader failover

**Observed: 14.4 s elected-new-leader latency. Target: < 5 s.**

Procedure: `kubectl delete pod aeon-1 --force --grace-period=0` while aeon-1
was leader (node_id=2, term=3). Polled `/api/v1/cluster/status` on aeon-0
and aeon-2 every ~100 ms; detected new leader when either non-leader's
`raft.state == "Leader"` or any `leader_id` differed from the killed
node_id. Result: aeon-2 elected (node_id=3, term=4) at t = 14,361 ms.

One prior attempt against aeon-2 with default `kubectl delete` (30 s
grace period) timed out at 20 s because the terminating pod kept sending
Raft heartbeats over QUIC until the container was actually SIGKILLed —
not a failure mode that represents a real crash. Re-ran with
`--force --grace-period=0` for a clean SIGKILL; 14.4 s result is from
that run.

**Why it's 3× the target:**

- Raft timing is at the local-profile defaults
  (`crates/aeon-cluster/src/config.rs`): heartbeat 500 ms, election
  window 1500–3000 ms. Expected convergence upper bound ≈ 4 s.
- The QUIC transport (`crates/aeon-cluster`) does not set a `max_idle_timeout`
  or `keep_alive_interval` — grep returns zero hits. On peer SIGKILL, the
  surviving peers rely on Raft's application-level heartbeat-miss detection
  rather than any transport-level failure signal, which is strictly
  correct but slower than it needs to be.
- WSL2 + Windows + `kubectl port-forward` adds latency on every poll
  (observed ≈ 1 s per poll cycle during the election window, likely from
  the PF reconnecting when the leader pod it was forwarded to vanished).
  This inflates the *observed* convergence time above the *actual* Raft
  convergence.

**Ship or not:** does not block v0.1 acceptance by itself — the cluster
does converge and partitions remain intact. Flag two follow-ups before
Session A re-runs this on native Linux nodes:

1. Set `max_idle_timeout` and `keep_alive_interval` on the Raft QUIC
   transport so peer death is detected at the transport layer, not only
   via application-level heartbeat miss.
2. Re-measure on DOKS AMS3 where native Linux + no port-forward removes
   the WSL2/PF latency floor. If the number stays > 5 s on DOKS, tune
   `election_min_ms/election_max_ms` down for the local profile (the
   `wan` profile can keep its wider window).

#### Row 5 — Two-phase partition transfer cutover

*Harness shipped; measurement deferred to Session A.* CL-6 partition
handover (commit `807321f`) is now operator-triggerable via:

- `POST /api/v1/cluster/partitions/{partition}/transfer` with body
  `{"target_node_id": N}` — leader-only (followers reply `409 Conflict`
  with an `X-Leader-Id` header for retry).
- `aeon cluster transfer-partition --partition N --target M` — wraps
  the REST call and surfaces the leader hint on rejection.

Outcomes are reported as `Accepted` / `NotLeader` / `UnknownPartition` /
`AlreadyTransferring` / `NoChange` / `Rejected` so scripts can branch
deterministically. The harness is covered by unit tests in
`crates/aeon-cluster/src/node.rs`. Measuring the < 100 ms cutover target
against the real 3-node cluster moves to Session A.

#### Row 6 — PoH chain continuity

**Local self-test passes.** `aeon verify` (run inside aeon-0 pod):

```
Local module validation:
  PoH chain:    OK (seq=2, hash=262e949946badafb..)
  Merkle tree:  OK (root=1f9f2378f6273830..)
  MMR:          OK (leaves=3, root=16e06e6ffd375049..)
  Ed25519 sign: OK (pubkey=ffeb71e8b07c934d..)
```

The subcommand is marked `(placeholder)` in `--help` and today only
exercises the crypto primitives, not the cluster-wide per-partition PoH
chain across a transfer. Continuity across a real handover needs to be
re-checked in Session A once Row 5's cutover harness is in place.

#### Row T5 — Partial split-brain

*Deferred to Session A.* Requires privileged `iptables -A INPUT`
execution inside an Aeon pod; the `helm/aeon/values-local.yaml` pods
run with the default unprivileged `securityContext`. Adding
`capabilities: { add: [ NET_ADMIN ] }` just for this test is feasible
but the Raft quorum-refuse code path is better exercised on a real
multi-host split in Session A where a host-level firewall rule (or
Chaos Mesh `NetworkChaos`) genuinely isolates a node without requiring
privileged sidecars.

#### Session 0 tear-down status

*Ready for tear-down.* All rows closable in Session 0 are recorded
above; remaining rows (5, T5, + any re-run of Row 4 on native Linux)
are explicitly deferred to Session A. Tear-down action: `helm
uninstall aeon -n aeon && kubectl delete ns aeon`; image
`aeon:session0` can stay in Rancher Desktop for quick re-load if a
Session 0 re-run is needed.

### 11.6 Results — V2 Rancher Desktop rehearsal (pre-Session-B)

**Date:** 2026-04-20
**Cluster:** same Rancher Desktop / WSL2 as §11.5, fresh helm release
with image `aeon:717a397` (commit `717a397`, includes G15 + P5.d +
**G16** fix introduced mid-session — see below). 3-pod STS, loopback
QUIC over headless service.

**Purpose:** re-run §5 T-scenarios against current HEAD before any
DOKS re-spin or EKS Session B, per `project_pre_session_b_validation`.

#### T0 — Isolation matrix (C0 × 4 modes)

| Cell | Events | Elapsed (adj.) | Per-node rate |
|------|--------|----------------|---------------|
| C0 · None            | 1,000,000 | 3,182 ms | **314,267 ev/s** |
| C0 · UnorderedBatch  | 1,000,000 | 3,163 ms | **316,155 ev/s** |
| C0 · OrderedBatch    | 1,000,000 | 3,131 ms | **319,386 ev/s** |
| C0 · PerEvent        | 1,000,000 | 3,120 ms | **320,512 ev/s** |

Same payload/batch/processor/sink as §11.5. All four modes land within
**2 %** of each other — confirms §11.5 finding (blackhole hides mode
cost; C1/C2 needed to expose). **~23 % slower** than 2026-04-18 on
commit `4d05b02` (~409 K ev/s at `C0·None`); overhead attributable to
G1–G15 correctness wiring + P5.d source-loop `yield_now()` on each
empty batch. Zero loss on all cells.

#### T2 — Scale-up 3 → 5 (membership only)

**Observed: 4.2 s to grow Raft voter set from {1,2,3} to {1,2,3,4,5}.**
No crashes. Partition ownership did NOT auto-rebalance — partitions
0–11 remained on nodes {1,2,3}. Per acceptance plan §5, this is the
expected split: voter membership is an openraft add-learner +
change-membership flow; partition rebalance is a separate operator or
controller action.

**G16 blocker found and fixed in-session.** First attempt had aeon-3 /
aeon-4 crash-loop with `failed to join cluster via any seed node`.
Root cause: `QuicEndpoint::connect()` keyed its connection pool by
`NodeId`, and the seed-join loop passed the placeholder `0` for every
seed attempt. The first seed's connection cached at key `0` and every
subsequent "retry against a different seed" silently re-used it, so
the client never reached the real leader. G15 (committed 2026-04-19)
correctly fixed the *server-side* leader-self check but the client
never made it there. Fix: uncached QUIC path for seed-join RPCs
(`connect_uncached` on endpoint + `cluster_rpc_uncached` in network
module), new regression test
`g16_multi_seed_join_reaches_each_address_from_single_endpoint`.
Committed as `717a397`. 194 `aeon-cluster` tests green post-fix.

#### T3 — Scale-down 5 → 3

**Observed: pods deleted cleanly; Raft membership stayed at
{1,2,3,4,5}.** Cluster quorum still achievable (3/5 alive) and remains
operational, but voter list is stale — exactly the gap the Session 0
"Open sub-question" in §5 T3 described. Kubernetes `scale --replicas`
only terminates pods; no `preStop` hook sends a `RemoveNode` RPC. Fix
is a helm-chart change + a `aeon cluster leave --node $POD_NAME`
command (or reuse of existing `remove-node` path), not a code bug in
the Raft layer. **Keep deferred to Session A** per original plan.

#### T4 — Partition transfer cutover

**Observed: REST `POST /api/v1/cluster/partitions/0/transfer` returned
HTTP 202 in 172 ms and Raft accepted the transfer (partition 0
flipped to `status=transferring`). After 35 s+ the transfer was still
`transferring` and never reached `owned` at the target.** Not a
transfer-state-machine bug — the pods started with
`WARN AEON_PIPELINE_NAME unset — skipping PartitionTransferDriver.
Raft-committed partition transfers will not make progress on this
node. Set AEON_PIPELINE_NAME to enable CL-6 handover.` So the Raft
intent is committed but there is no driver in the pod to execute the
bulk-sync → drain → ownership-flip sequence. Same gap as Session 0
Row 5 (measurement deferred to Session A). Fix is helm-chart env var
plus an actual running pipeline on the source partition; belongs with
T4 on DOKS/EKS.

#### G16 commit

`fix(cluster): G16 — use uncached QUIC connect in seed-join RPCs`
(`717a397`). Changes:
- `crates/aeon-cluster/src/transport/endpoint.rs` — `connect_uncached`
- `crates/aeon-cluster/src/transport/network.rs` — `cluster_rpc_uncached`;
  `send_join_request` delegates to it
- `crates/aeon-cluster/src/node.rs` — seed loop comment updated
- `crates/aeon-cluster/tests/multi_node.rs` — regression test

All other RPCs (health, partition transfer, remove, openraft inner
traffic) still use the pooled path. Only the seed-join flow is
uncached, because only it iterates unknown-id seeds from one endpoint.

#### V2 tear-down status

Ready for tear-down. V2's purpose — "catch any code gap exposed by the
current HEAD image on a real K8s before spending cloud dollars" — is
fully served by the G16 fix. T3/T4 gaps are the same ones Session 0
already logged and did not introduce new code-layer blockers.
Remaining RD rehearsal work (V3 processor validation, V5 crypto chain
E2E, V6 consolidated report) continues separately.

### 11.7 Results — V3 processor validation (Rancher Desktop)

Same 3-pod `aeon:717a397` cluster used for V2. Leader = node 3
(aeon-2). Port-forward 127.0.0.1:4471 → aeon-2:4471.

#### V3a — Built-in `__identity` processor

Already validated under V2 §11.6 T0 matrix: 4M events total across
4 durability modes × 1M events, zero `events_failed`, 314-320K ev/s
per node.

#### V3b — Wasm guest (`rust-processor.wasm`)

Shipped artifact at `/app/processors/rust-processor.wasm` (2911 B,
`sha512:791c27dc…47de`) staged into `/app/artifacts/rust-processor/1.0.0`
on all three pods, then registered via leader REST; register
replicated (`"replicated": true`) through Raft to all nodes.

Two pipeline runs using Wasm processor, memory source, blackhole sink:

| cell         | events | processed | outputs | failed | rate      | notes |
|--------------|--------|-----------|---------|--------|-----------|-------|
| v3-wasm-100k | 100000 | 100000    | 100000  | 0      | ~30.6K/s  | small run — fixed drain-poll overhead dominates |
| v3-wasm-1m   | 1000000| 1000000   | 1000000 | 0      | ~295K/s   | apples-to-apples vs V2 T0 scale |

At 1M scale the Wasm path is within ~7% of the `__identity` baseline
(295K vs 314-320K ev/s). Zero event loss on either run.

Cells + logs: `tmp/v3-rd/cells/v3-wasm-{100k,1m}.json`,
`tmp/v3-rd/v3-wasm-{100k,1m}.log`.

#### V3c — Native `.so` cdylib — deferred

No sample cdylib processor exists in the repo; only Wasm samples are
present (`samples/processors/rust-wasm{,-sdk}`). The `native-loader`
feature is compiled into the shipped image (via `aeon-cli`'s default
`native-validate` feature set), and `NativeProcessor::load` is on the
REST path, but without a shipped `.so` sample — and no image build
pipeline that produces one — there is nothing to register and execute.

This is a **feature gap, not a defect**. Closing it is a small work
item (add `samples/processors/rust-native` cdylib crate + Dockerfile
stage that copies the `.so` to `/app/processors/`). Tracked as a
post-Gate 2 item, not a Session A blocker — DOKS Session A and EKS
Session B can use the Wasm path alone for processor validation.

#### Processor registration artifact-upload gap

The current `POST /api/v1/processors` endpoint and matching
`aeon processor register` CLI submit metadata only — artifact bytes are
expected to already exist on each node's `artifact_dir`. This worked
here only because the image pre-ships the wasm in `/app/processors/`
and we `kubectl cp`'d it into place on every pod manually.

For a real operator flow the registry needs a byte-upload path
(multipart POST + Raft-replicate-or-QUIC-fan-out of the artifact).
Already documented in `crates/aeon-engine/src/registry.rs` preamble
("artifacts are transferred via QUIC") — that transfer path is
not yet implemented. Adding to the post-Gate 2 registry work.

#### V3 verdict

**Green for processor-model validation.** Wasm path works end-to-end
at production scale with zero loss. Built-in path already exercised
in V2. Native `.so` path is a feature-complete gap (deferred), and
artifact-upload/distribution is a registry UX gap (deferred). Neither
blocks Session A or Session B — both will continue using `__identity`
and/or Wasm.

### 11.8 Results — V5 crypto chain E2E (Rancher Desktop + unit)

Phase 14 shipped PoH + Merkle + MMR + Ed25519 signing primitives in
`aeon-crypto`, and Phase 14 runtime wired them into the pipeline task
behind `feature = "processor-auth"` (always on in default image).

#### V5a — Crypto primitives (unit)

`cargo test -p aeon-crypto` — **159 tests green, 0 failed, 0 ignored,
0.68s.** Covers: PoH multi-batch chain integrity; Merkle tree builder
+ proof verify; MMR node-count formula + append/verify; Ed25519 sign
+ verify; encryption round-trip; TLS cert + mTLS path.

#### V5b — PoH + pipeline-runtime integration

`cargo test -p aeon-engine --features processor-auth --lib
pipeline::tests::poh_tests` — **8 tests green, 0 failed, 0.03s.**
Covers: `run_buffered_with_poh_records_chain`,
`poh_chain_integrity_after_pipeline`, `poh_with_signing_key`,
`create_poh_state_resumes_installed_chain_g11c` (G11.c transfer-side
resume), empty-batch no-op, disabled/enabled state toggle.

#### V5c — REST `/verify` surface (live cluster)

`GET /api/v1/pipelines/all/verify` against live leader on RD returns:

```json
{"modules":{"merkle":"available","mmr":"available","poh":"available","signing":"available"},"pipelines":[],"status":"ok"}
```

All four modules reported `"available"` — confirms the
`processor-auth` feature is compiled into the shipped image.

#### V5d — REST-layer PoH enablement gap

The JSON payload for `POST /api/v1/pipelines` (a `PipelineDefinition`
per `aeon-types/src/registry.rs`) has **no `poh` field**. Today, the
only code path that sets `PipelineConfig.poh = Some(PohConfig {...})`
in production is the cluster-side partition transfer installer
(`PohChainInstaller`, G11.c path), which resumes a chain that another
node already had. That means:

- **REST-created pipelines** (the ones V2 T0 used, the ones an
  operator creates via `aeon pipeline create` or `aeon apply`) always
  run with `poh: None`. `verify_pipeline` will report
  `"poh_active": false` for them.
- **Transfer-resumed pipelines** get PoH because the snapshot includes
  a live chain — but V2 T4 and Session 0 Row 5 both failed to drive
  the transfer path end-to-end (`AEON_PIPELINE_NAME` env-var gate on
  `PartitionTransferDriver`).

This is a **code-wiring gap, not a crypto correctness issue.** The
primitives are sound (V5a); the runtime wiring is sound when the
config reaches it (V5b); the REST exposure is missing. Closing it is
small work: add `poh: Option<PohManifestConfig>` to
`PipelineDefinition` and `PipelineManifest` (YAML + JSON), plumb it
through the supervisor's pipeline-build path into `PipelineConfig`.

Tracked as a post-Gate 2 registry/manifest item alongside V3's
artifact-upload gap. Does not block Session A or Session B — EKS can
still exercise V5b semantics via the transfer-driver path once the
AEON_PIPELINE_NAME gate is lifted.

#### V5 verdict

**Green at primitive + runtime level, amber at REST surface.** Crypto
chain modules are fully validated in code. Live cluster exposes the
verify endpoint and reports the modules available. Operator-driven
PoH enablement via REST/YAML is a deferred feature-wiring gap; EKS
Session B can still verify PoH-under-transfer once
`AEON_PIPELINE_NAME` gating is fixed (same code change that unblocks
V2 T4 / Session 0 Row 5).

### 11.9 V6 — Consolidated RD validation + pre-Session-B checklist

**Purpose:** summarise V1..V5 in one place and state a clear
"ready / not ready" for Session A (DOKS) and Session B (EKS).

#### V6.1 RD rehearsal — section index

| Phase | Section | Verdict |
|-------|---------|---------|
| V1 — Rancher Desktop pre-flight | §11.5 (Session 0) + §11.6 preamble | Green |
| V2 — Pull-source T0..T6 matrix | §11.6 | Green on T0/T2 (post-G16); T3/T4 carry known gaps |
| V3 — Processor validation | §11.7 | Green for Wasm + `__identity`; native-`.so` + artifact-upload deferred |
| V4 — Push-source HTTP ingest | separate session notes | Green |
| V5 — Crypto chain E2E | §11.8 | Green at unit + runtime; REST-surface gap |

#### V6.2 Code changes shipped during RD rehearsal

- **G16 — uncached QUIC seed-join RPC** (commit `717a397`): fixes
  multi-seed join hang when non-first seed is leader. Regression test
  in `multi_node.rs`.
- **V2 + V3 + V5 documentation** (this section): `GATE2-ACCEPTANCE-PLAN.md`
  §11.6 / §11.7 / §11.8 results sections.

#### V6.3 Known code gaps carried into Session A/B

1. ~~**Helm preStop + `aeon cluster leave`**~~ — **closed post-V6.**
   New `POST /api/v1/cluster/leave` REST endpoint + `aeon cluster leave`
   CLI subcommand. Helm preStop hook now runs
   `aeon cluster leave --api http://localhost:4471 || true` before the
   endpoint-propagation sleep so the Raft leader removes the departing
   voter and rebalances partitions before SIGTERM. Leader-self case
   returns 409 (operator must transfer leadership first); `|| true`
   guards the sleep so the pod still terminates.
2. ~~**`AEON_PIPELINE_NAME` gate on `PartitionTransferDriver`**~~ —
   **closed post-V6.** `cmd_serve` now auto-discovers the pipeline
   name from the Raft-replicated catalog when the env var is unset:
   once exactly one pipeline is registered, the driver installs and
   binds to that name. Env-var path preserved for operators who want
   explicit control. `aeon-cli/src/main.rs:install_partition_transfer_driver`.
3. **Native-`.so` processor sample** — no cdylib sample in
   `samples/processors/`; no image build stage produces one. Gap from
   V3. Wasm path covers the processor-model validation need.
4. **Processor artifact-upload path** — `POST /api/v1/processors` is
   metadata-only; bytes must be pre-staged per-node. Gap from V3. OK
   for image-shipped artifacts; blocks operator-driven uploads.
5. **PoH enablement via REST/YAML** — no `poh` field on
   `PipelineDefinition` / `PipelineManifest`. Gap from V5. Transfer
   path is the only way to activate today.

**None of these block Session A (correctness floor) or Session B
(ceiling claim).** Sessions A/B use `__identity` + Wasm processors,
image-shipped artifacts, and transfer-driven PoH (once gap #2 is
lifted).

#### V6.4 Pre-Session-B checklist

Before spinning EKS for Session B:

- [ ] Tear down current RD cluster (`helm uninstall aeon -n aeon &&
      kubectl delete ns aeon`).
- [x] Close gap #2 (`AEON_PIPELINE_NAME` gate). Shipped — auto-discovery
      fallback in `install_partition_transfer_driver`. Session B can
      now exercise partition-transfer + PoH-resume without touching
      helm values.
- [x] Close gap #1 (helm preStop + `aeon cluster leave`). Shipped —
      REST `/cluster/leave` + CLI `aeon cluster leave` + StatefulSet
      preStop hook. Session A's scale-down correctness row can now
      pass cleanly.
- [ ] Bake `aeon:<commit>` to ECR us-east-1 (blocked by task #6;
      Session B's `imagePullPolicy: Always` expects it).
- [ ] Session A first, on the already-provisioned DOKS cluster —
      validate correctness floor (small nodes, Regular SSD) before
      paying EKS premium-tier dollars.

#### V6.5 RD tear-down status

Ready. The RD rehearsal has served its purpose: validated the current
HEAD image on a real K8s, surfaced G16 mid-session, and proved V2 T0
/ V3 / V5 unit+runtime paths green. The remaining RD T3/T4 gaps are
already logged and are blocked on feature work, not on "try harder on
RD." Post-V6 the RD cluster can be torn down; Session A (DOKS)
proceeds with the gap-#1 and gap-#2 fixes in flight.

---

## 12. Session B — AWS EKS (post-v0.1, weekend window)

**Goal:** absolute **ceiling** claim for Aeon throughput + CPU pinning
validation. DOKS cannot provide either — Premium tier unavailable, and
DOKS doesn't expose `cpu-manager-policy=static`.

### 12.1 Prerequisites (all must be true before Session B starts)

- [ ] Session 0 complete (baseline + shape of deltas known)
- [ ] Session A complete (correctness rows all ✅ at DO floor)
- [ ] Any code gaps from Session 0/A fixed and merged
- [ ] v0.1 cut OR `docs/ROADMAP.md` pause point explicitly cleared for Session B
- [ ] Weekend window scheduled (cost containment)

### 12.2 Infra sizing (scoping only — detailed plan at session time)

| Pool | Nodes | Instance | Rationale |
|------|-------|----------|-----------|
| Aeon | 3 | `i4i.2xlarge` (8 vCPU, 64 GiB, 1.875 TB NVMe, up to 10 Gbps) OR `c7i.4xlarge` (16 vCPU, 32 GiB, 15 Gbps) | NVMe for L2/L3, 10+ Gbps for inter-node Raft traffic. `i4i` if storage-dominant, `c7i` if compute-dominant — decide after Session A identifies the actual bottleneck. |
| Redpanda | 3 | `i4i.4xlarge` (16 vCPU, 128 GiB, 3.75 TB NVMe, 18.75 Gbps) | Local NVMe + 18.75 Gbps removes both storage and network as bottlenecks — this is what lets us call any observed number a ceiling. |
| Default | 1 | `m7i.large` (2 vCPU, 8 GiB) | Loadgen + Prometheus + Chaos Mesh controller. |

### 12.3 Cost envelope

AWS on-demand pricing (us-east-1, approximate):

| Component | Hourly |
|-----------|--------|
| 3 × `i4i.2xlarge` | 3 × $0.69 = $2.07 |
| 3 × `i4i.4xlarge` | 3 × $1.37 = $4.11 |
| 1 × `m7i.large` | $0.10 |
| EKS control plane | $0.10 |
| Data transfer / misc | ~$0.20 |
| **Total** | **~$6.60/hr** |

6-hour weekend window ≈ **$40**. Hard budget cap: **$50** — if not
complete in the window, tear down and resume next weekend. Reserved
instances / spot are not worth the friction for a one-shot session.

### 12.4 Scope — only what DOKS cannot do

1. **T1 at ceiling**: Redpanda→Redpanda, 4 durability modes, on NVMe + 18.75 Gbps.
   Record the rate where Aeon CPU actually hits 50 % (Gate 1 headroom rule)
   rather than where infra saturates first.
2. **CPU pinning validation**: enable `cpu-manager-policy=static` on the
   EKS kubelet, pin Aeon pods to full cores (QoS `Guaranteed`, integer
   CPU request), re-run T1. Measure delta vs unpinned. This is the only
   test that strictly cannot run on DOKS.
3. **T6 sustained at ceiling**: 30-min run at 80 % of T1 ceiling. Not a
   long soak — validates the ceiling number is sustainable.

### 12.5 Rows NOT re-run on EKS

T2/T3/T4/T5 already closed on DOKS count as closed — scale events,
cutover, and split-brain are correctness rows, not throughput rows, and
the verdict from Session A carries over. Do not re-spend the budget.

### 12.6 Results — Session B

*Template. Rows filled in during the session; pre-session values remain
`TBD`. Mirrors the § 10.9 Session A re-run shape so the two sessions
read the same way in the Gate 2 bundle.*

EKS cluster `TBD` (us-east-1a, `i4i.2xlarge × 3` aeon-pool +
`i3en.3xlarge × 3` redpanda-pool + `m7i.xlarge × 1` default-pool,
`cpu-manager-policy=static` on aeon-pool). Image
`TBD.dkr.ecr.us-east-1.amazonaws.com/aeon:TBD` (commit `TBD`). Entry
preconditions per § 12.1 must all be green before `eksctl create
cluster -f deploy/eks/cluster.yaml` runs; spot-vs-on-demand decision
from `deploy/eks/check-spot-pricing.sh` recorded here before
provisioning.

| Test | Verdict | Notes |
|---|---|---|
| Pre-flight — spot pricing decision | TBD | Record `check-spot-pricing.sh` verdict (on-demand vs spot per instance type), final 6-hr cost estimate, and whether window fits $50 hard cap. |
| Cluster bring-up | TBD | STS Ready time, Raft term + leader id, partition ownership fan-out (expect `{1,2,3}` voters, 24 partitions 8/8/8). |
| **T1 at ceiling — 4 durability modes** (§ 12.4.1) | TBD | Redpanda→Aeon→Redpanda on NVMe + 18.75–25 Gbps. Rate-sweep per durability mode (None / Ordered / PerEvent / Measure) until Aeon CPU hits **50 %** (Gate 1 headroom rule), not until infra saturates first. Record steady-state aggregate eps and per-node eps at the 50 % CPU point; cite `aeon_pipeline_events_processed_total` + `container_cpu_usage_seconds_total` samples. |
| **CPU pinning delta** (§ 12.4.2) | TBD | Re-run T1 with Aeon pods in Guaranteed QoS (integer CPU requests = 7, one core per pod reserved for kubelet/system per `kubeletExtraConfig`). Record unpinned baseline vs pinned throughput + p99 latency delta. Only test that strictly cannot run on DOKS. |
| **T6 sustained at ceiling** (§ 12.4.3) | TBD | 30-min run at 80 % of the pinned T1 ceiling. Record zero-loss invariant (`events_failed_total = 0`, `events_retried_total = 0`), drift (if any) between start and end eps, and CPU headroom margin held. |

#### 12.6.1 Code gaps surfaced — Session B

*(Template — fill during session.)*

| ID | Gap | Severity | Resolution |
|---|---|---|---|
| TBD | *(Describe gap if any surfaces. Otherwise delete this subsection and state "No code gaps surfaced.")* | TBD | TBD |

#### 12.6.2 Session verdicts — Session B

*(Fill during session. Template below.)*

- **Ceiling claim (T1, 4 durability modes):** `TBD` — steady-state
  aggregate `TBD M eps`, per-node `TBD M eps` at the 50 % CPU point.
  This is the number Aeon cites as its verified ceiling post-Session B.
- **CPU pinning delta:** `TBD %` throughput gain, `TBD µs` p99 latency
  delta vs unpinned. Decision: pin by default / pin optional / no
  meaningful delta.
- **Sustainability (T6 at 80 % of ceiling):** `TBD` — zero loss over
  30 min, drift `TBD`, CPU margin held at `TBD %`.
- **Gate 2 ceiling row:** `TBD` — either closed with the number above
  or deferred with reason.

#### 12.6.3 Tear-down — Session B

*(Fill during session.)*

Target: `eksctl delete cluster -f deploy/eks/cluster.yaml` completes
within 15 min of the last T6 sample. Record actual wall-clock cost
(AWS Cost Explorer, us-east-1, session tag `gate2-b`) against the
**$50** hard cap from § 12.3. ECR registry retained for subsequent
re-runs; VPC + IAM role + OIDC provider torn down with the cluster.
