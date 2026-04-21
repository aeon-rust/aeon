# Aeon Deployment & Operator Walkthrough

> Day-2 operator guide for Aeon on Kubernetes: Helm install, graceful
> rolling upgrades, pod disruption policy, and zero-downtime pipeline
> upgrade workflows (drain-swap / blue-green / canary).
>
> **Scope.** This document is the *operator* view. Sibling docs cover
> adjacent concerns:
>
> - [`docs/DEPLOYMENT-ARCHITECTURE.md`](DEPLOYMENT-ARCHITECTURE.md) — topology
>   options (single-machine → multi-server → K8s), Redpanda placement,
>   latency analysis.
> - [`docs/CLUSTERING.md`](CLUSTERING.md) — Raft cluster setup, node join/leave,
>   partition transfer (CL-6).
> - [`docs/CLOUD-DEPLOYMENT-GUIDE.md`](CLOUD-DEPLOYMENT-GUIDE.md) — DOKS / EKS /
>   GKE specifics, storage classes, load balancers.
> - [`docs/CONFIGURATION.md`](CONFIGURATION.md) — full YAML manifest reference
>   for pipelines, sources, sinks, processors, upgrade strategies.

---

## Table of Contents

1. [Helm chart overview](#1-helm-chart-overview)
2. [Single-node install](#2-single-node-install)
3. [Cluster-mode install](#3-cluster-mode-install)
4. [Pod disruption policy](#4-pod-disruption-policy)
5. [Graceful shutdown sequence](#5-graceful-shutdown-sequence)
6. [Rolling upgrades (image / chart)](#6-rolling-upgrades-image--chart)
7. [Pipeline upgrade strategies](#7-pipeline-upgrade-strategies)
8. [Operator CLI reference](#8-operator-cli-reference)
9. [Troubleshooting](#9-troubleshooting)

---

## 1. Helm chart overview

The chart lives at `helm/aeon/`. Two deployment modes select at
install time via `cluster.enabled`:

| Mode | Object | When to use |
|------|--------|-------------|
| **Single-node** | `Deployment` + optional `HorizontalPodAutoscaler` | Dev, single-tenant ETL, cost-bound footprints. No Raft quorum; a pod restart = brief write unavailability. |
| **Cluster** (`cluster.enabled: true`) | `StatefulSet` + headless `Service` + `PodDisruptionBudget` | Production multi-replica; Raft consensus; partition transfer; zero-downtime node drains. |

Both modes share the same container image, REST API (port 4471),
QUIC ports (4470 cluster / 4472 external), `/health` + `/ready` probes,
and the SIGTERM drain sequence in §5.

**Key values overlays** already in the repo:

- `values.yaml` — chart defaults; single-node; conservative resources.
- `values-doks.yaml` — 3-node DOKS cluster mode.
- `values-local.yaml` — single-node Rancher Desktop / Kind.

---

## 2. Single-node install

```bash
helm install aeon ./helm/aeon \
  -f ./helm/aeon/values-local.yaml \
  --namespace aeon --create-namespace
```

Verify:

```bash
kubectl -n aeon get pods
kubectl -n aeon port-forward svc/aeon 4471:4471 &
curl http://localhost:4471/health
```

For bursty workloads enable the HPA:

```yaml
autoscaling:
  enabled: true
  minReplicas: 1
  maxReplicas: 4
  targetCPUUtilization: 70
```

HPA fires only when `cluster.enabled: false` — cluster-mode replica
count is fixed by Raft membership and is managed via `aeon cluster
drain` / partition transfer, not the HPA.

---

## 3. Cluster-mode install

```bash
helm install aeon ./helm/aeon \
  -f ./helm/aeon/values-doks.yaml \
  --namespace aeon --create-namespace
```

The StatefulSet provisions `aeon-0`, `aeon-1`, `aeon-2` in parallel
(`podManagementPolicy: Parallel`). Pod identity comes from
`metadata.name` — `AEON_POD_NAME` is injected so each replica knows its
ordinal. The headless `Service` gives each pod a stable DNS name
(`aeon-0.aeon-headless.<ns>.svc.cluster.local`) for Raft QUIC peering.

Once all three replicas are `Ready`, confirm the cluster formed:

```bash
kubectl -n aeon exec aeon-0 -- aeon cluster status
```

A healthy 3-node cluster reports all members with the same `term`,
matching `log_idx` / `applied`, and exactly one `leader`.

For post-install validation and Gate 2 acceptance see
[`docs/CLUSTERING.md` §3](CLUSTERING.md) and
[`docs/GATE2-ACCEPTANCE-PLAN.md`](GATE2-ACCEPTANCE-PLAN.md).

---

## 4. Pod disruption policy

> The Kubernetes object kind is `PodDisruptionBudget` — that identifier
> is fixed by the API server. In Aeon-owned surfaces (values keys,
> comments, docs) we call it the **pod disruption policy** to match the
> project's Capacity-not-Budget convention.

### 4.1 Why it matters for Aeon

Voluntary disruptions — `kubectl drain`, rolling node upgrades,
cluster-autoscaler scale-down — evict pods through the eviction API,
which respects `PodDisruptionBudget`. Without a policy, a single drain
command can evict *all* Aeon replicas on the node simultaneously,
leaving the Raft cluster below quorum and blocking writes until new
pods schedule and catch up (tens of seconds at best).

With the policy in place, the eviction API refuses a drain that would
drop the cluster below the configured floor, forcing the drain to wait
for replacement pods to come online one at a time. End result: Raft
stays writable through node maintenance.

### 4.2 Enabling the policy

```yaml
podDisruption:
  enabled: true
  # Auto-computed Raft quorum floor (floor(N/2) + 1) when both fields
  # are empty. Override either to pin an absolute cap; set only one.
  minAvailable: ""
  maxUnavailable: ""
```

Auto-computed floor by replica count:

| Replicas | Quorum | Auto `minAvailable` | Pods evictable at once |
|---------:|-------:|--------------------:|-----------------------:|
| 1 | 1 | 1 | 0 |
| 3 | 2 | 2 | 1 |
| 5 | 3 | 3 | 2 |
| 7 | 4 | 4 | 3 |

`minAvailable = floor(N/2) + 1` is the openraft quorum rule — any
larger disruption can strand committed log entries on a minority that
cannot elect a new leader.

### 4.3 When to override

- **Single-node dev clusters.** Leave `podDisruption.enabled: false`.
  A PDB with `minAvailable: 1` on a single-replica Deployment blocks
  every node drain, which is rarely what you want in dev.
- **Rolling chart upgrades.** If `maxSurge` lets you run N+1 replicas
  briefly during a rollout, you can safely set `maxUnavailable: 1` on
  any cluster size and keep drains moving faster. Stay conservative
  unless you have tested it.
- **Percentage form.** `minAvailable: "60%"` also works — it scales
  with replica count, useful if replica count is managed by the HPA
  in non-cluster mode.

### 4.4 Verifying

```bash
kubectl -n aeon get pdb aeon
# NAME  MIN AVAILABLE  MAX UNAVAILABLE  ALLOWED DISRUPTIONS  AGE
# aeon  2              N/A              1                    5m

kubectl -n aeon describe pdb aeon
```

If `ALLOWED DISRUPTIONS` is 0 and no pods are unhealthy, check that the
selector matches the pod labels — the PDB selector is
`app.kubernetes.io/name=aeon,app.kubernetes.io/instance=<release>` and
depends on matching labels in the StatefulSet / Deployment template.

---

## 5. Graceful shutdown sequence

When the kubelet sends SIGTERM (pod deletion, drain, rollout), the
container runs this sequence:

```
t = 0 s     kubelet sends SIGTERM to aeon container
            kubelet starts running preStop hook (concurrent)
            kubelet flips pod Endpoint to NotReady

preStop     ── aeon cluster leave (best-effort; leader ignores self-remove)
            ── sleep(preStopDelaySeconds)          ← endpoint propagation

in-process  ── /ready flips to 503                  ← drain in-flight
            ── leader-relinquish (up to leaderRelinquishMs, parallel)
            ── stop sources (SPSC drain, sink flush)
            ── Raft shutdown
            ── exit(0)

t <= 90 s   terminationGracePeriodSeconds exhausted → SIGKILL (safety net)
```

The three tunables (all in `gracefulShutdown:`):

| Field | Default | What it covers |
|-------|--------:|----------------|
| `terminationGracePeriodSeconds` | 90 | Absolute ceiling before SIGKILL. |
| `preStopDelaySeconds` | 5 | Waits for kube-proxy to propagate endpoint removal so in-flight TCP/QUIC traffic observes the pod as Terminating. |
| `leaderRelinquishMs` | 4000 | Time the SIGTERM handler waits for Raft leadership to flip to a follower. Runs in parallel with the preStop sleep. |

Defaults fit the `fast_failover` Raft preset (250 / 750 / 2000 ms);
bump `leaderRelinquishMs` if you switch to `prod_recommended` (500 /
2000 / 6000) or `flaky_network` (500 / 3000 / 12000). See
[`CLUSTERING.md` §6](CLUSTERING.md) for Raft timing details.

**Interaction with the pod disruption policy.** The eviction API gates
*whether* a pod is allowed to start shutting down; the SIGTERM sequence
governs *how cleanly* it does so once eviction is admitted. Both layers
are required — PDB alone cannot prevent data loss if
`terminationGracePeriodSeconds` is too short for the drain to finish.

---

## 6. Rolling upgrades (image / chart)

A Helm chart or image upgrade is just a StatefulSet / Deployment
rollout. The pod disruption policy protects Raft quorum during the
rollout; the SIGTERM sequence in §5 protects each individual pod's
in-flight events.

```bash
helm upgrade aeon ./helm/aeon \
  -f ./helm/aeon/values-doks.yaml \
  --set image.tag=0.3.4
```

Expected progress on a 3-node cluster:

```
aeon-2  Terminating   →  Running (new image)  → Ready
aeon-1  Terminating   →  Running (new image)  → Ready
aeon-0  Terminating   →  Running (new image)  → Ready
```

At every step, at least `minAvailable` pods are Ready, so reads +
writes continue. Monitor progress:

```bash
kubectl -n aeon rollout status statefulset/aeon --watch
kubectl -n aeon exec aeon-0 -- aeon cluster status  # run between pods
```

If the rollout stalls, check:

1. New pod is stuck pending → storage class / PVC.
2. New pod is crash-looping → image pull error, config regression.
3. Rollout is "waiting" → PDB is blocking eviction because an earlier
   replacement pod never became Ready. Fix readiness first.

---

## 7. Pipeline upgrade strategies

Pipeline (processor) upgrades are *orthogonal* to pod rollouts.
They swap the user-supplied processor module inside a running pipeline
without restarting the Aeon process, so there is no Raft churn and no
sink / source re-connect. Three strategies, all via
`POST /api/v1/pipelines/{name}/upgrade[...]` or `aeon pipeline
upgrade`:

### 7.1 Drain-swap (default)

```bash
aeon pipeline upgrade my-pipeline \
  --processor new-mod --version 2 \
  --strategy drain-swap
```

The engine drains the existing SPSC ring, atomically swaps the
processor, and resumes. Downtime measured in microseconds. Use this
unless you explicitly need parallel evaluation or percentage traffic
splitting — it has the smallest blast radius.

### 7.2 Blue-green

```bash
aeon pipeline upgrade my-pipeline \
  --processor new-mod --version 2 \
  --strategy blue-green

# …observe side-by-side metrics…
aeon pipeline cutover  my-pipeline          # flip reads/writes to green
# or
aeon pipeline rollback my-pipeline          # discard green, keep blue
```

Both processors run in parallel; blue continues to serve traffic while
green processes a shadow copy. `cutover` swaps the production path
atomically; `rollback` discards the green processor. Use when you want
to compare output distributions before committing.

### 7.3 Canary

```bash
aeon pipeline upgrade my-pipeline \
  --processor new-mod --version 2 \
  --strategy canary --steps 10,25,50,75,100

aeon pipeline canary-status my-pipeline    # view current split
aeon pipeline promote       my-pipeline    # advance to next step
aeon pipeline rollback      my-pipeline    # abort and keep blue
```

Traffic is split by percentage; `promote` advances through the steps.
Auto-promotion on metric thresholds is roadmapped but not yet
implemented — for now promotion is operator-driven.

See [`docs/E2E-TEST-PLAN.md` §upgrade](E2E-TEST-PLAN.md) for the
underlying `PipelineControl` semantics and
[`docs/CONFIGURATION.md`](CONFIGURATION.md) for manifest defaults
(`upgrade_strategy:`).

---

## 8. Operator CLI reference

All commands talk to the REST API via `--api <url>` (default
`http://localhost:4471`). Every cluster-mutating call goes through the
Raft leader; a 409 response includes an `X-Leader-Id` header that the
CLI surfaces as "current Raft leader is node N".

### 8.1 Cluster membership

| Command | Endpoint | Purpose |
|---------|----------|---------|
| `aeon cluster status [--watch]` | `GET  /api/v1/cluster/status` | Raft term, leader, per-node log index. |
| `aeon cluster drain --node <id>` | `POST /api/v1/cluster/drain` | Ask the leader to migrate all partitions off node `<id>` before shutdown. |
| `aeon cluster transfer-partition --partition <p> --target <id>` | `POST /api/v1/cluster/partitions/{p}/transfer` | Manual single-partition move (CL-6). |
| `aeon cluster rebalance` | `POST /api/v1/cluster/rebalance` | Let the leader compute and execute a balanced partition plan. |
| `aeon cluster leave` | `POST /api/v1/cluster/leave` | Self-removal from voter set; invoked by the Helm preStop hook. |

### 8.2 Pipeline lifecycle

| Command | Endpoint |
|---------|----------|
| `aeon pipeline apply -f pipeline.yaml` | `POST /api/v1/pipelines` |
| `aeon pipeline start <name>` | `POST /api/v1/pipelines/{name}/start` |
| `aeon pipeline stop <name>`  | `POST /api/v1/pipelines/{name}/stop`  |
| `aeon pipeline delete <name>` | `DELETE /api/v1/pipelines/{name}` |
| `aeon pipeline upgrade <name> --strategy ...` | `POST /api/v1/pipelines/{name}/upgrade[/blue-green\|/canary]` |
| `aeon pipeline cutover <name>` | `POST /api/v1/pipelines/{name}/cutover` |
| `aeon pipeline rollback <name>` | `POST /api/v1/pipelines/{name}/rollback` |
| `aeon pipeline promote <name>` | `POST /api/v1/pipelines/{name}/promote` |
| `aeon pipeline canary-status <name>` | `GET  /api/v1/pipelines/{name}/canary-status` |
| `aeon pipeline history <name>` | `GET  /api/v1/pipelines/{name}/history` |

Full REST reference: [`docs/REST-API.md`](REST-API.md).

---

## 9. Troubleshooting

**PDB blocks every drain.** Check `kubectl describe pdb aeon` —
typically `minAvailable` exceeds the replica count after a manual
scale-down, or the label selector doesn't match. For cluster-mode
rollouts, the quorum floor is `floor(N/2)+1`; a 3-node cluster allows
exactly one pod to be disrupted at a time.

**`aeon cluster leave` fails on the last remaining pod.** Expected —
openraft refuses to remove the last voter. The preStop hook suffixes
`|| true` so the pod still terminates cleanly; the Helm uninstall flow
removes the StatefulSet wholesale.

**Rolling upgrade stuck on one pod.** Check `/ready` — a pod stays
`NotReady` while it drains. If it never flips back to Ready, the sink
flush is likely blocked; `kubectl logs -n aeon <pod>` will show the
specific connector + error. `terminationGracePeriodSeconds` is a
backstop — increasing it past 90 s is rarely the right fix.

**Canary traffic imbalance.** `aeon pipeline canary-status` reports the
current split; if actuals diverge from the configured step, the
pipeline likely hit a soft capacity limit (see
[EO-2 Backpressure](EO-2-DURABILITY-DESIGN.md) §7). Metrics:
`aeon_l2_pressure{level}`, `aeon_sink_ack_seq`.

**Raft leadership thrashes during upgrades.** Lengthen
`gracefulShutdown.leaderRelinquishMs` or switch to the
`prod_recommended` Raft timing preset (see `cluster.raftTiming` in
`values.yaml`). Frequent leader churn also suggests a too-aggressive
`fast_failover` preset on a congested network.
