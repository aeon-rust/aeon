# Shared manifests (provider-agnostic)

Applied identically on every cluster (DOKS, EKS, GKE, VKE, Hetzner)
during Gate 2 sessions. These files reference no cloud-specific
storage classes, node labels, or pool taints.

## Files (populate incrementally as each session needs them)

| File | Purpose | Used in |
|------|---------|---------|
| `topic-create-job.yaml` | 24-partition source + sink topics, RF=3 | Session A/B prerequisite |
| `loadgen-job.yaml` | `rpk produce` + consumer seqcheck pods | T1, T6 |
| `chaos-mesh-install.yaml` | Chaos Mesh controller install | T5, T6 |
| `prometheus-grafana.yaml` | Prometheus + Grafana + Aeon dashboards | Every session |
| `cert-manager-install.yaml` | cert-manager + internal-CA ClusterIssuer | mTLS sessions |
| `test-scripts/t0-isolation.sh` | T0 12-cell isolation matrix driver | Session A/B |
| `test-scripts/t1-throughput.sh` | T1 3-node throughput driver | Session A/B |
| `test-scripts/t2-scaleup.sh`, `t3-scaledown.sh` | T2/T3 drivers | Session A only |
| `test-scripts/t4-cutover.sh` | T4 cutover measurement | Session A only |
| `test-scripts/t5-splitbrain.sh` | T5 Chaos Mesh split-brain drill | Session A only |
| `test-scripts/t6-sustained.sh` | T6 sustained run with chaos interleave | Session A/B |

## Invariants

- All test scripts read `$KUBECONFIG` — never hard-code provider.
- All pods tolerate `workload=loadgen:NoSchedule` and land on the
  default node pool.
- All Prometheus ServiceMonitors use the same label
  (`app.kubernetes.io/part-of=aeon-gate2`) regardless of cluster.

## Populate order

1. `topic-create-job.yaml` first — needed before any T0 run
2. `cert-manager-install.yaml` — needed when `cluster.tls.enabled=true`
3. `test-scripts/t0-isolation.sh` — runs before anything else in both sessions
4. Remaining scripts as each test approaches
