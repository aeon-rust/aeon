# Session B — AWS EKS (post-v0.1, weekend window)

Operational configs for the AWS EKS Session B **ceiling claim** run.
Plan: [`../../docs/GATE2-ACCEPTANCE-PLAN.md`](../../docs/GATE2-ACCEPTANCE-PLAN.md)
§ 12.

## Prerequisites (all must be true before any `eksctl create` runs)

- [ ] Session 0 complete (local Rancher Desktop baseline captured)
- [ ] Session A complete (DOKS correctness rows all ✅)
- [ ] Code gaps from Session 0/A fixed and merged
- [ ] v0.1 cut **or** pause point explicitly cleared for Session B
- [ ] Weekend window scheduled — hard budget cap **$50**
- [ ] AWS account + IAM user with `AdministratorAccess` or scoped EKS/EC2/VPC policy
- [ ] `aws`, `eksctl`, `kubectl`, `helm` installed locally

## Region

**`us-east-1`** — cheapest on-demand for `i4i.*` and `i3en.*`,
widest instance availability. Single-AZ cluster (all nodes in `us-east-1a`)
so cross-AZ data-transfer ($0.01/GB each way) doesn't contaminate
throughput numbers.

## Cluster shape

| Pool | Nodes | Instance | Rationale |
|------|-------|----------|-----------|
| `aeon-pool` | 3 | `i4i.2xlarge` (8 vCPU, 64 GiB, 1.875 TB NVMe, 18.75 Gbps) | Local NVMe + 10+ Gbps — what DOKS lacks |
| `redpanda-pool` | 3 | `i3en.3xlarge` (12 vCPU, 96 GiB, 7.5 TB NVMe, 25 Gbps) | Remove storage + network as bottleneck |
| `default-pool` | 1 | `m7i.xlarge` (4 vCPU, 16 GiB) | Loadgen, Prom, Chaos Mesh |

**Cost estimate:** ~$6.43/hr cluster + ~$0.10/hr EKS control plane.
6-hr session ≈ **$39**. 24-hr weekend ≈ **$154**. Hard cap $50 →
tear down and resume next weekend if not complete.

**One-time charges to verify are $0 on your bill:**
- **ACM public wildcard cert** — free. Not used anyway (we use
  cert-manager + internal CA inside the cluster; ACM certs cannot be
  mounted into pods).
- **EKS control plane** — billed hourly at $0.10/hr, not a one-time
  charge. $73/mo is only what you pay if the cluster is left running
  all month. 6 hr = $0.60.

## Scope — only what DOKS cannot do

Session B does **not** re-run T2/T3/T4/T5 — those closed on DOKS and
carry over. Session B runs:

1. **T1 at ceiling** — Redpanda→Redpanda, 4 durability modes, on NVMe +
   18.75–25 Gbps. Rate-sweep until Aeon CPU hits 50 % (Gate 1 headroom
   rule), not until infra saturates first. ~60 min.
2. **CPU pinning validation** — enable `cpu-manager-policy=static` on
   kubelet (`cluster.yaml` has the `kubeletExtraConfig` stanza), pin
   Aeon pods to full cores (Guaranteed QoS via integer CPU requests),
   re-run T1. Measure delta vs unpinned. **Only test that strictly
   cannot run on DOKS.** ~90 min.
3. **T6 sustained at ceiling** — 30-min at 80 % of T1 ceiling. Not a
   long soak — validates the ceiling number is sustainable. ~45 min.

Active test time ~3.5 hr. Provisioning ~60 min, teardown ~15 min,
buffer ~60 min → **~6 hours wall-clock**.

## Sequence

```bash
# 0. Spot-vs-on-demand pre-check (read-only, no charges)
#    Records decision + 6-hr cost estimate vs the $50 hard cap.
./check-spot-pricing.sh

# 1. Create cluster (~20 min)
eksctl create cluster -f cluster.yaml

# 2. Install cert-manager + internal CA (for Aeon mTLS)
kubectl apply -f ../shared/cert-manager-install.yaml       # (TBD)

# 3. Install Redpanda operator + cluster
helm install redpanda redpanda/redpanda -f values-redpanda.yaml -n redpanda --create-namespace

# 4. Install Aeon
helm install aeon ../../helm/aeon -f values-aeon.yaml -n aeon --create-namespace

# 5. Apply shared manifests (topic-create, loadgen, chaos)
kubectl apply -f ../shared/

# 6. Run T1, CPU pinning, T6 per plan § 12.4

# 7. Tear down IMMEDIATELY when results recorded
eksctl delete cluster -f cluster.yaml
```

## Files

- `check-spot-pricing.sh` — read-only pre-check wrapping
  `aws ec2 describe-spot-price-history`. Emits SPOT / ON-DEMAND
  decision per pool + 6-hr cost vs $50 cap. Run before `eksctl create`.
- `cluster.yaml` — eksctl cluster config, single-AZ, 3 node groups,
  kubelet pinning enabled on `aeon-pool`
- `values-aeon.yaml` — Aeon values sized for `i4i.2xlarge`
- `values-redpanda.yaml` — *(TBD)*

## Tear-down

Hard rule: EKS cluster runs **only while a test is active**. Spin up
Saturday morning, spin down by Saturday night. Do not let it sleep
overnight — $6.43/hr × 12 hr = $77 of wasted burn.
