# GKE — alternative Session B target (placeholder)

GCP Kubernetes Engine is a credible second-platform choice for the
Session B ceiling run if AWS capacity is tight or we want to validate
numbers on a second cloud. Not populated yet.

## Indicative sizing

| Pool | Nodes | Instance | Notes |
|------|-------|----------|-------|
| Aeon | 3 | `c3-standard-8-lssd` (8 vCPU, 32 GiB, 2× 375 GiB local SSD, Tier_1 100 Gbps eligible) | Local SSD bundled in SKU price; no separate $0.08/GB/mo line |
| Redpanda | 3 | `c3-standard-22-lssd` (22 vCPU, 88 GiB, 4× 375 GiB local SSD) | Closest `c3-*-lssd` step with enough local NVMe |
| Default | 1 | `n2-standard-4` (4 vCPU, 16 GiB) | Loadgen/obs |
| Control plane | — | GKE Standard | $0.10/hr |

Estimated hourly: **~$4.83/hr**. 6-hr ≈ $29. 24-hr ≈ $116.
See `docs/GATE2-ACCEPTANCE-PLAN.md` § 12 for full comparison.

## When this beats EKS

- AWS is the default for Session B because of `i4i.*` + EKS pinning +
  operator familiarity. GKE is picked up only if:
  - AWS capacity for `i4i.2xlarge` in `us-east-1a` is unavailable the
    weekend we want to run
  - We want to cross-check the ceiling on a second platform post-v0.1

## Populate-later checklist

- [ ] `cluster.yaml` — `gcloud container clusters create` equivalent
      (or Config Connector / Terraform)
- [ ] `values-aeon.yaml` — sized for `c3-standard-8-lssd`
- [ ] `values-redpanda.yaml`
- [ ] Kubelet CPU pinning — GKE supports `cpu-manager-policy=static`
      via node-pool `kubelet-config` since 1.27
