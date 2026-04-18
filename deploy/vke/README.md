# Vultr Kubernetes Engine (VKE) — price-check option (placeholder)

Vultr is the cheapest managed-K8s option (~$2.82–$3.17/hr for the
target topology, free VKE control plane), but **not** the recommended
path for v0.1 Session A/B. Populate only if we decide to run a
price-comparison session post-v0.1.

## Indicative sizing

| Pool | Nodes | Plan | Notes |
|------|-------|------|-------|
| Aeon | 3 | Optimized Cloud Compute 8 vCPU / 32 GB / 160 GB NVMe ($240/mo) | Dedicated vCPU |
| Redpanda | 3 | Optimized 16 vCPU (preferred over Bare Metal — VKE-BM integration is limited) | Keep inside VKE |
| Default | 1 | Optimized 4 vCPU / 16 GB ($120/mo) | Loadgen/obs |
| Control plane | — | VKE | **free** |

Estimated hourly: **~$3.17/hr all-Optimized**. 6-hr ≈ $19. 24-hr ≈ $76.

## Caveats to validate before committing

- VKE is a newer managed K8s — fewer battle-scars than EKS/GKE. Check
  version cadence and CNI behaviour.
- Bare Metal nodes do not join VKE cleanly as worker nodes. Redpanda
  on BM would require standalone cluster outside VKE.
- 10 Gbps on Optimized instances, up to 25 Gbps on VX1 (verify tier).
- 10 TB egress included per instance — good for test.

## Populate-later checklist

- [ ] `cluster.yaml` — via Vultr API or `vultr-cli`
- [ ] `values-aeon.yaml`
- [ ] `values-redpanda.yaml`
- [ ] Verify `cpu-manager-policy=static` is exposable on VKE
