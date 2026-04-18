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

- `values-aeon.yaml` — Aeon StatefulSet values, sized for `g-8vcpu-32gb`
- `pps-probe.sh` — 2-droplet iperf3 pre-flight (§ 3.6)
- `values-redpanda.yaml` — *(TBD — Redpanda operator values)*

## Tear-down criterion

All boxes in plan § 8 ticked, then:

```bash
doctl kubernetes cluster delete <cluster-id> --dangerous
```

Do not leave the cluster running to start unrelated feature work.
