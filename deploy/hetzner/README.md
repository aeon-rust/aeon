# Hetzner — self-managed K8s future target (placeholder)

Hetzner is the cheapest option for the target topology (~$1.01/hr)
but has **no first-party managed Kubernetes** — realistic paths are
Talos / K3s self-managed, or Cloudfleet (free control plane, BYO
Hetzner nodes). Populate only if we commit to a bare-metal iterative
test rig after v0.1.

## Indicative sizing (Falkenstein or Helsinki)

| Pool | Nodes | Plan | Notes |
|------|-------|------|-------|
| Aeon | 3 | CCX33 (8 dedicated vCPU AMD EPYC, 32 GB, 240 GB NVMe) — EUR 0.1001/hr | Dedicated cores |
| Redpanda | 3 | CCX43 (16 vCPU, 64 GB, 360 GB NVMe) — EUR 0.2003/hr | |
| Default | 1 | CCX13 or CPX31 — ~EUR 0.03/hr | |
| Control plane | — | Self-managed Talos or Cloudfleet free tier | $0 |

Estimated hourly: **~$1.01/hr** compute. 6-hr ≈ $6. 24-hr ≈ $24.

## Why this is deferred past v0.1

- **No managed K8s** — first-time Talos/K3s setup is a half-day tax.
  With a committed `machineconfig` + image, rebuilds are ~15 min, but
  the first session pays the setup cost.
- **10 Gbps shared network** (not dedicated) — a ceiling claim on
  shared bandwidth is weaker than on AWS `i3en.*`'s 25 Gbps dedicated.
- **EU-only** — no US/APAC region. Cross-ocean kubectl/helm adds
  friction.
- **Aeon v0.1 target is proving the engine**, not proving it's
  portable. Post-v0.1 is when price-efficient iteration matters.

## Populate-later checklist

- [ ] `talos-machineconfig.yaml` — worker + control-plane templates
- [ ] `hcloud-provision.sh` — `hcloud server create` for the 7 nodes
- [ ] `values-aeon.yaml`
- [ ] CPU pinning — Talos supports `cpu-manager-policy=static` via
      machine config `.machine.kubelet.extraArgs`
