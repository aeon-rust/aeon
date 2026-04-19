#!/usr/bin/env bash
# setup-session-a.sh — GATE2-ACCEPTANCE-PLAN.md § 3 orchestration wrapper
#
# Bring a freshly-provisioned DOKS cluster to the state expected by the
# Session A test scripts (T0 through T6). Idempotent where possible —
# safe to re-run if an earlier step failed. Not safe to run against a
# cluster that has unrelated workloads (it will `helm install` into
# predictable release names and label default-pool nodes).
#
# Order (each step waits for its prerequisites to be ready before the
# next step begins — the order below is intentional):
#   1.  Save kubeconfig for the given cluster.
#   2.  Label the default pool `workload=monitoring` so Prometheus /
#       loadgen / Chaos Mesh controller pods land there. aeon-pool and
#       redpanda-pool are tainted in doctl provisioning, so these
#       workloads stay off the test node pools.
#   3.  Helm install cert-manager (chart: jetstack/cert-manager).
#   4.  Apply `shared/cert-manager-install.yaml` — self-signed CA +
#       `aeon-internal-ca` ClusterIssuer. Wait for Ready.
#   5.  Helm install Redpanda (operator chart). Wait for brokers Ready.
#   6.  Apply `shared/topic-create-job.yaml` — 24-partition aeon-source
#       and aeon-sink topics, RF=3. Wait for Job Complete.
#   7.  Apply `shared/aeon-certificate.yaml` — issue Aeon mTLS cert.
#   8.  Helm install Aeon with AEON_REPLICAS StatefulSet replicas.
#       Default 3. For T2 the operator re-scales to 1 before running
#       run-t2-scaleup.sh; this wrapper doesn't try to anticipate that.
#   9.  Apply `deploy/doks/loadgen.yaml` (loadgen namespace + idle pod).
#   10. Helm install kube-prometheus-stack with `values-prometheus.yaml`.
#   11. Apply `deploy/doks/servicemonitors.yaml`.
#   12. Run `install-chaos-mesh.sh` (T5/T6 prerequisite).
#
# Usage:
#   ./setup-session-a.sh <cluster-id-or-name>
#
# Env overrides:
#   AEON_REPLICAS=<N>           default: 3
#   REDPANDA_BROKERS=<N>        default: 3
#   CERT_MANAGER_VERSION=<ver>  default: v1.15.3
#   SKIP_PROMETHEUS=<0|1>       default: 0
#   SKIP_CHAOS=<0|1>            default: 0
#   READY_TIMEOUT=<seconds>     default: 600
#
# Dependencies: doctl, kubectl, helm, jq — authed against DigitalOcean.

set -euo pipefail

CLUSTER="${1:-}"
if [[ -z "$CLUSTER" ]]; then
  echo "Usage: $0 <cluster-id-or-name>" >&2
  exit 2
fi

AEON_REPLICAS="${AEON_REPLICAS:-3}"
REDPANDA_BROKERS="${REDPANDA_BROKERS:-3}"
CERT_MANAGER_VERSION="${CERT_MANAGER_VERSION:-v1.15.3}"
SKIP_PROMETHEUS="${SKIP_PROMETHEUS:-0}"
SKIP_CHAOS="${SKIP_CHAOS:-0}"
READY_TIMEOUT="${READY_TIMEOUT:-600}"

HERE="$(dirname "$(readlink -f "$0")")"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
SHARED="$REPO_ROOT/deploy/shared"
DOKS="$REPO_ROOT/deploy/doks"

log() { printf '[%(%H:%M:%S)T] %s\n' -1 "$*" >&2; }
step() { printf '\n[%(%H:%M:%S)T] ── STEP %s · %s\n' -1 "$1" "$2" >&2; }

for cmd in doctl kubectl helm jq; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "error: '$cmd' missing on PATH" >&2; exit 1; }
done

# ── 1. kubeconfig ──────────────────────────────────────────────────────

step 1 "doctl kubeconfig for '$CLUSTER'"
doctl kubernetes cluster kubeconfig save "$CLUSTER" >/dev/null
kubectl cluster-info >/dev/null

# ── 2. label pools ─────────────────────────────────────────────────────

step 2 "label pools workload={aeon,redpanda,monitoring}"
# DOKS auto-generates pool names (e.g. pool-psmexo05w) unless explicitly
# named, so identify pools by droplet size per GATE2-ACCEPTANCE-PLAN.md §3.
# Allow explicit overrides via env (pool IDs from `doctl k8s cluster
# node-pool list`).
pool_id_by_size() {
  doctl kubernetes cluster node-pool list "$CLUSTER" \
    --format ID,Size --no-header \
    | awk -v s="$1" '$2 == s { print $1; exit }'
}

AEON_POOL_ID="${AEON_POOL_ID:-$(pool_id_by_size g-8vcpu-32gb)}"
REDPANDA_POOL_ID="${REDPANDA_POOL_ID:-$(pool_id_by_size so1_5-4vcpu-32gb)}"
MONITORING_POOL_ID="${MONITORING_POOL_ID:-$(pool_id_by_size s-4vcpu-8gb)}"

for var in AEON_POOL_ID REDPANDA_POOL_ID MONITORING_POOL_ID; do
  if [[ -z "${!var}" ]]; then
    echo "error: could not auto-discover $var — set it explicitly." >&2
    exit 1
  fi
done

log "pools: aeon=$AEON_POOL_ID  redpanda=$REDPANDA_POOL_ID  monitoring=$MONITORING_POOL_ID"
kubectl label nodes -l "doks.digitalocean.com/node-pool-id=$AEON_POOL_ID" \
  workload=aeon --overwrite >/dev/null
kubectl label nodes -l "doks.digitalocean.com/node-pool-id=$REDPANDA_POOL_ID" \
  workload=redpanda --overwrite >/dev/null
kubectl label nodes -l "doks.digitalocean.com/node-pool-id=$MONITORING_POOL_ID" \
  workload=monitoring --overwrite >/dev/null

# ── 3. cert-manager chart ──────────────────────────────────────────────

step 3 "helm install cert-manager ($CERT_MANAGER_VERSION)"
helm repo list -o json 2>/dev/null | grep -q '"name":"jetstack"' \
  || helm repo add jetstack https://charts.jetstack.io >/dev/null
helm repo update jetstack >/dev/null
helm upgrade --install cert-manager jetstack/cert-manager \
  --version "$CERT_MANAGER_VERSION" \
  --namespace cert-manager --create-namespace \
  --set crds.enabled=true \
  --set nodeSelector.workload=monitoring \
  --set webhook.nodeSelector.workload=monitoring \
  --set cainjector.nodeSelector.workload=monitoring \
  --wait --timeout "${READY_TIMEOUT}s" >/dev/null
log "cert-manager ready"

# ── 4. self-signed CA + ClusterIssuer ──────────────────────────────────

step 4 "apply shared/cert-manager-install.yaml (self-signed CA + ClusterIssuer)"
kubectl apply -f "$SHARED/cert-manager-install.yaml" >/dev/null
kubectl wait --for=condition=Ready clusterissuer/aeon-internal-ca \
  --timeout=120s >/dev/null
log "aeon-internal-ca ClusterIssuer Ready"

# ── 5. Redpanda ────────────────────────────────────────────────────────

step 5 "helm install redpanda operator chart ($REDPANDA_BROKERS brokers)"
helm repo list -o json 2>/dev/null | grep -q '"name":"redpanda"' \
  || helm repo add redpanda https://charts.redpanda.com >/dev/null
helm repo update redpanda >/dev/null
helm upgrade --install redpanda redpanda/redpanda \
  -f "$DOKS/values-redpanda.yaml" \
  --set statefulset.replicas="$REDPANDA_BROKERS" \
  --namespace redpanda --create-namespace \
  --wait --timeout "${READY_TIMEOUT}s" >/dev/null
log "Redpanda brokers ready"

# ── 6. topic-create Job (needs Redpanda up) ────────────────────────────

step 6 "apply shared/topic-create-job.yaml (24 partitions, RF=3)"
kubectl apply -f "$SHARED/topic-create-job.yaml" >/dev/null
kubectl -n redpanda wait --for=condition=complete \
  job/aeon-topic-create --timeout=180s >/dev/null
log "topics created (aeon-source, aeon-sink, 24 partitions each)"

# ── 7. Aeon certificate ────────────────────────────────────────────────

step 7 "apply shared/aeon-certificate.yaml"
# aeon namespace must exist before Certificate can be created in it.
kubectl create namespace aeon --dry-run=client -o yaml | kubectl apply -f - >/dev/null
# DOCR pull secret — DOKS integrates the registry but only provisions the
# secret in the default namespace, so copy it into `aeon`. Idempotent via
# `apply`. Chart references imagePullSecret name registry-rust-proxy-registry
# (the doctl-generated name, which prefixes registry-<registry-name>).
doctl registry kubernetes-manifest --namespace aeon 2>/dev/null | kubectl apply -f - >/dev/null
kubectl apply -f "$SHARED/aeon-certificate.yaml" >/dev/null
kubectl -n aeon wait --for=condition=Ready certificate/aeon-tls \
  --timeout=120s >/dev/null
log "aeon-tls certificate issued"

# ── 8. Aeon chart ──────────────────────────────────────────────────────

step 8 "helm install aeon (replicas=$AEON_REPLICAS)"
helm upgrade --install aeon "$REPO_ROOT/helm/aeon" \
  -f "$DOKS/values-aeon.yaml" \
  --set cluster.replicas="$AEON_REPLICAS" \
  --namespace aeon \
  --wait --timeout "${READY_TIMEOUT}s" >/dev/null
log "Aeon StatefulSet ready at $AEON_REPLICAS replicas"

# ── 9. loadgen namespace + idle pod ────────────────────────────────────

step 9 "apply deploy/doks/loadgen.yaml"
kubectl apply -f "$DOKS/loadgen.yaml" >/dev/null
kubectl -n loadgen wait --for=condition=Ready pod/loadgen \
  --timeout=180s >/dev/null
log "loadgen pod Ready"

# ── 10. Prometheus + Grafana ───────────────────────────────────────────

if [[ "$SKIP_PROMETHEUS" = "1" ]]; then
  step 10 "SKIP_PROMETHEUS=1 — skipping kube-prometheus-stack"
else
  step 10 "helm install kube-prometheus-stack"
  helm repo list -o json 2>/dev/null | grep -q '"name":"prometheus-community"' \
    || helm repo add prometheus-community https://prometheus-community.github.io/helm-charts >/dev/null
  helm repo update prometheus-community >/dev/null
  helm upgrade --install monitoring prometheus-community/kube-prometheus-stack \
    -f "$DOKS/values-prometheus.yaml" \
    --namespace monitoring --create-namespace \
    --wait --timeout "${READY_TIMEOUT}s" >/dev/null
  kubectl apply -f "$DOKS/servicemonitors.yaml" >/dev/null
  log "Prometheus + Grafana up; Aeon/Redpanda ServiceMonitors applied"
fi

# ── 11. Chaos Mesh (T5/T6) ─────────────────────────────────────────────

if [[ "$SKIP_CHAOS" = "1" ]]; then
  step 11 "SKIP_CHAOS=1 — skipping chaos-mesh install"
else
  step 11 "install Chaos Mesh"
  "$HERE/install-chaos-mesh.sh"
fi

# ── summary ────────────────────────────────────────────────────────────

step "✓" "Session A ready"
cat >&2 <<EOF

Cluster is ready for the T0–T6 sweep.

  Aeon:     kubectl -n aeon get pods,sts
  Redpanda: kubectl -n redpanda get pods,sts
  Loadgen:  kubectl -n loadgen get pods
  Grafana:  kubectl -n monitoring port-forward svc/monitoring-grafana 3000:80

Suggested run order:
  1. T0  cells   — run-t0-cell.sh / run-c1-cell.sh per cell
  2. T1  3-node  — t1-3node.json
  3. T2  scale-up 1→3→5 — ./run-t2-scaleup.sh $CLUSTER   (scale sts to 1 first)
  4. T3  scale-down 5→3→1 — ./run-t3-scaledown.sh $CLUSTER
  5. T4  cutover — ./run-t4-cutover.sh
  6. T5  split-brain — ./run-t5-split-brain.sh
  7. T6  sustained+chaos — ./run-t6-sustained.sh

Tear down with:
  doctl kubernetes cluster delete $CLUSTER --dangerous

EOF
