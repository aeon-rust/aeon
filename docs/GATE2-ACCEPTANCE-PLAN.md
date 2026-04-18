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

*(Empty until the session runs.)*

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

*(Empty until Session 0 runs.)*

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

*(Empty until Session B runs. Detailed plan written when session is imminent.)*
