# Session A — DOKS AMS3

Operational configs for the DigitalOcean Kubernetes (DOKS) Session A
correctness run. Plan: [`../../docs/GATE2-ACCEPTANCE-PLAN.md`](../../docs/GATE2-ACCEPTANCE-PLAN.md)
§§ 3–10.

## Region

**AMS3** (Amsterdam) — pinned. BLR1 has limited dedicated-CPU SKU
availability at the Regular-SSD + 2 Gbps tier; AMS3 carries the full
range. Premium Intel (NVMe + 10 Gbps) is unavailable on DO in any
region, so Session A is a correctness **floor**, not a ceiling claim.

## Node pools (real DO SKUs, AMS3)

| Pool | Nodes | Droplet | Taint | Purpose |
|------|-------|---------|-------|---------|
| `aeon-pool` | 3 (→ 5 during T2) | `g-8vcpu-32gb` | `workload=aeon:NoSchedule` | Aeon StatefulSet |
| `redpanda-pool` | 3 | `so-4vcpu-32gb` | `workload=redpanda:NoSchedule` | Redpanda brokers |
| default | 1 | `s-4vcpu-8gb` | — | Loadgen, Prom, Grafana, Chaos Mesh |

Cost: **~$2.53/hr** ($1,822/mo if left running — do not).

## Sequence

1. **Pre-flight PPS probe** (~30 min, ~$1) — `pps-probe.sh`. Blocks
   full cluster provisioning until ≥ 500K PPS observed (or caveats
   documented). See plan § 3.6.
2. **Provision cluster** — control-panel or `doctl` (see below).
3. **Install shared manifests** — `../shared/` (topic create, chaos,
   prometheus, loadgen).
4. **Install Aeon + Redpanda** — helm with `values-aeon.yaml` /
   `values-redpanda.yaml`.
5. **Run tests** — T0..T6 per plan § 5.
6. **Tear down** same day (plan § 8).

## doctl provisioning (reference)

```bash
doctl kubernetes cluster create aeon-gate2-a \
  --region ams3 \
  --version latest \
  --count 3 --size g-8vcpu-32gb \
  --tag workload:aeon \
  --ha true

# Add the other two pools
doctl kubernetes cluster node-pool create <cluster-id> \
  --name redpanda-pool --count 3 --size so-4vcpu-32gb \
  --taint workload=redpanda:NoSchedule \
  --label workload=redpanda

doctl kubernetes cluster node-pool create <cluster-id> \
  --name default --count 1 --size s-4vcpu-8gb
```

After create, taint the Aeon pool (HA-enabled pools don't accept
taints at create-time via doctl reliably):

```bash
kubectl taint nodes -l doks.digitalocean.com/node-pool=aeon-pool \
  workload=aeon:NoSchedule
```

## Helm install

```bash
# Aeon
helm install aeon ../../helm/aeon -f values-aeon.yaml -n aeon --create-namespace

# Redpanda (via Redpanda operator — install operator first)
helm install redpanda redpanda/redpanda -f values-redpanda.yaml -n redpanda --create-namespace
```

## Files

- `setup-session-a.sh` — one-shot wrapper. Takes a DOKS cluster id,
  saves kubeconfig, labels the default pool `workload=monitoring`,
  installs cert-manager + the self-signed CA, Redpanda + topic-create
  Job, the Aeon Certificate, Aeon chart (`AEON_REPLICAS`, default 3),
  loadgen pod, kube-prometheus-stack + ServiceMonitors, and Chaos
  Mesh. Env overrides `SKIP_PROMETHEUS=1` / `SKIP_CHAOS=1` for partial
  bring-ups. Idempotent per step via `helm upgrade --install` /
  `kubectl apply`. Usage: `./setup-session-a.sh <cluster-id>`.
- `values-aeon.yaml` — Aeon StatefulSet values, sized for `g-8vcpu-32gb`
- `pps-probe.sh` — 2-droplet iperf3 pre-flight (§ 3.6)
- `values-redpanda.yaml` — *(TBD — Redpanda operator values)*
- `resize-aeon-pool.sh` — idempotent `aeon-pool` VM resize (3 → 5 before
  T2, 5 → 3 after T3). Waits for all pool nodes to be `Ready`, then
  re-applies the `workload=aeon:NoSchedule` taint (DOKS HA pools drop
  taints on scale-up). Usage: `./resize-aeon-pool.sh <cluster> <count>`.
- `install-chaos-mesh.sh` / `uninstall-chaos-mesh.sh` — T5/T6
  prerequisite. Installs Chaos Mesh 2.6.3 into the `chaos` namespace
  with `containerd` runtime pinning for DOKS; uninstall clears live
  experiments + CRDs + namespace in finalizer-safe order. Both are
  idempotent; run before T5, run uninstall before cluster tear-down.
- `t2-ordered.json` / `t3-ordered.json` — OrderedBatch Kafka→Kafka
  pipeline manifests for the T2 and T3 scale runs. Partitions list is
  empty — P1.1d's cluster-ownership resolver fills it in per node.
- `run-t2-scaleup.sh` — T2 driver (§ 5). 1 → 3 → 5 StatefulSet
  scale-up under ~1M ev/s OrderedBatch load, with node-pool resize
  between 3-node and 5-node phases. Verdict: zero producer-vs-acked
  delta, `aeon_checkpoint_fallback_wal_total` stays 0.
  Usage: `./run-t2-scaleup.sh <cluster-id>`.
- `run-t3-scaledown.sh` — T3 driver (§ 5). 5 → 3 → 1 scale-down,
  using explicit `aeon cluster drain` before each `kubectl scale` so
  the leader reassigns partitions before pods terminate (covers the
  plan's "no preStop hook" open sub-question without changing the
  chart). Pool shrinks 5 → 3 at the end.
  Usage: `./run-t3-scaledown.sh <cluster-id>`.
- `chaos-netpart-aeon-2.yaml` — Chaos Mesh `NetworkChaos` manifest
  that isolates `aeon-2` from `aeon-0`/`aeon-1` (direction=both).
  Shared by T5 and the minute-6 interleave in T6.
- `run-t5-split-brain.sh` — T5 driver (§ 5). Applies the NetworkChaos,
  hammers each of the 3 REST endpoints for `WRITE_SECONDS` (default
  120) with pipeline-create writes, heals the partition, then checks
  Raft `last_applied` convergence and per-partition-ownership
  agreement across all members. Pass = majority committed, minority
  rejected, rejoin clean. Usage: `./run-t5-split-brain.sh`.
- `run-t6-sustained.sh` — T6 driver (§ 5). 10-minute OrderedBatch
  run at `RATE` (default 500k ev/s) with four timer-triggered chaos
  events: t=2min leader-pod kill (measures failover), t=4min
  `POST /cluster/rebalance`, t=6min NetworkChaos apply (T5 repeat),
  t=8min heal. Verdict: zero loss, WAL stays 0, failover < 5s.
  Usage: `./run-t6-sustained.sh`.

## G13 — Chaos Mesh on DOKS leaves iptables/tc rules behind (workaround)

Observed during the 2026-04-19 T6 run: after `kubectl delete -f
chaos-netpart-aeon-2.yaml` the top-level `NetworkChaos` resource is
gone and each affected pod's `PodNetworkChaos` reconciles to `spec: {}`,
but the kernel-level `iptables` / `tc` rules injected by the
`chaos-daemon` DaemonSet on the DOKS worker nodes are **not** removed.
Net effect for Aeon: subsequent QUIC `sendmsg` on the cluster-Raft
path fails with `EPERM`, the Raft log stops advancing, and the
post-heal pipeline wedges even though every pod appears `Ready`.

This is an infrastructure gap in the Chaos Mesh ↔ DOKS CNI
integration (not an Aeon code bug). We reproduced it with a clean
install of Chaos Mesh 2.6.3 against a fresh DOKS `g-8vcpu-32gb`
cluster in AMS3; the PPS probe is fine before the experiment runs and
broken after the heal.

**Workaround** — after every NetworkChaos experiment, before starting
the next test, restart the Aeon StatefulSet so each pod re-creates its
QUIC endpoint from a fresh network namespace:

```bash
# 1. Confirm the top-level NetworkChaos + any PodNetworkChaos are gone.
kubectl get networkchaos -n aeon
kubectl get podnetworkchaos -n aeon

# 2. Nudge the kernel rules out by restarting the StatefulSet.
#    Rolling restart keeps quorum if ≥ 3 replicas; for a 1- or 2-node
#    cluster expect a brief leaderless window during the roll.
kubectl -n aeon rollout restart sts/aeon
kubectl -n aeon rollout status  sts/aeon --timeout=120s

# 3. Verify Raft is committing again before resuming the test.
kubectl -n aeon exec aeon-0 -- curl -s http://127.0.0.1:4471/cluster/status \
  | jq '.raft.last_applied, .leader_id'
```

The rolling-restart is cheap on a 3-node cluster (≤ 90 s), but it is
not zero-cost: T6's 10-minute sustained-load run has to pause during
the roll. When Session A is re-run with this workaround, record the
pause in the evidence log so the 10 min wall-clock stays separable
from the active-load window.

Aeon-side gap to watch for (not part of G13): if a future T6 variant
re-applies network partitions back-to-back, the rolling-restart
between iterations will churn the Raft log store. `bootstrap_single_persistent`
handles restart correctly (FT-1 / FT-2), so this is safe — but worth a
note if we observe log-replay cost in the metrics.

Once Session B runs on AWS EKS (different CNI + kernel path), re-try
Chaos Mesh without the workaround. If EKS doesn't exhibit the leak,
the workaround stays DOKS-only and we file the DOKS result upstream
at `chaos-mesh/chaos-mesh`.

## Tear-down criterion

All boxes in plan § 8 ticked, then:

```bash
doctl kubernetes cluster delete <cluster-id> --dangerous
```

Do not leave the cluster running to start unrelated feature work.
