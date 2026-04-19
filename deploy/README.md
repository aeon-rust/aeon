# Aeon Deployment Configs

Cloud-specific configuration samples for Gate 2 acceptance sessions
(see [`../docs/GATE2-ACCEPTANCE-PLAN.md`](../docs/GATE2-ACCEPTANCE-PLAN.md)).

## Layout

```
deploy/
├── doks/          Session A — DigitalOcean Kubernetes (AMS3)          [active]
├── eks/           Session B — AWS EKS (us-east-1, weekend window)     [post-v0.1]
├── gke/           alternative Session B — GCP GKE                     [placeholder]
├── vke/           price-check option — Vultr Kubernetes Engine        [placeholder]
├── hetzner/       future — self-managed K8s on Hetzner (Talos/K3s)    [placeholder]
├── shared/        provider-agnostic manifests reused on every cluster
├── k8s/           legacy raw manifests (Aeon pipeline + Redpanda)     [pre-helm]
└── systemd/       systemd unit files for bare-VM deploys
```

## Principles

- One helm values file per provider. Storage class names, tolerations,
  and pinning annotations differ between providers — **never share
  values across clouds.**
- `shared/` is the exact same YAML applied on every cluster (topic
  creation, loadgen, Chaos Mesh, Prometheus).
- `test-scripts/` (under `shared/`) read `$KUBECONFIG` and work
  identically on any provider. That is where the rebuild-speed gain
  comes from.

## Rebuild targets (see `GATE2-ACCEPTANCE-PLAN.md` § 3)

From cold (have the artifacts, no cluster yet) to working Aeon pods:

| Target | Est. wall-clock |
|--------|-----------------|
| DOKS + helm install | ~10 min |
| EKS + helm install | ~25 min |
| Hetzner Talos + helm install | ~15 min (after first-time Talos setup) |

## Cost envelopes

See [`../docs/GATE2-ACCEPTANCE-PLAN.md`](../docs/GATE2-ACCEPTANCE-PLAN.md)
§ 3.5 (Session A) and § 12.3 (Session B) for current numbers.
