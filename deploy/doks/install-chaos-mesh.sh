#!/usr/bin/env bash
# install-chaos-mesh.sh — P2.2b (GATE2-ACCEPTANCE-PLAN.md T5/T6)
#
# Install Chaos Mesh into the `chaos` namespace of the Session A DOKS
# cluster. Required before running T5 (split-brain drill) or T6 (10-min
# sustained with chaos interleave).
#
# DOKS runs containerd, so we pin the chaos-daemon runtime accordingly.
# The install is idempotent: re-running on an already-installed cluster
# is a no-op (`helm upgrade --install`). After install the script waits
# for every chaos-mesh pod to be Ready and verifies the NetworkChaos
# CRD is registered.
#
# Usage:
#   ./install-chaos-mesh.sh
#
# Env overrides:
#   NAMESPACE=<ns>              default: chaos
#   CHART_VERSION=<semver>      default: 2.6.3
#   READY_TIMEOUT=<seconds>     default: 300
#
# Dependencies: helm, kubectl — authed against the target DOKS cluster.

set -euo pipefail

NAMESPACE="${NAMESPACE:-chaos}"
CHART_VERSION="${CHART_VERSION:-2.6.3}"
READY_TIMEOUT="${READY_TIMEOUT:-300}"

log() { printf '[%(%H:%M:%S)T] %s\n' -1 "$*"; }

for cmd in helm kubectl; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "error: '$cmd' not found on PATH" >&2
    exit 1
  fi
done

# ── ensure helm repo is present + fresh ──────────────────────────────

if ! helm repo list -o json 2>/dev/null | grep -q '"name":"chaos-mesh"'; then
  log "adding chaos-mesh helm repo"
  helm repo add chaos-mesh https://charts.chaos-mesh.org >/dev/null
fi
log "refreshing helm repo index"
helm repo update chaos-mesh >/dev/null

# ── install / upgrade ────────────────────────────────────────────────

log "installing chaos-mesh $CHART_VERSION into namespace '$NAMESPACE'"
# MSYS_NO_PATHCONV=1: stop Git Bash on Windows from mangling the
# Linux-side socketPath into 'C:/Program Files/Git/run/containerd/...'.
# Tolerations: chaos-daemon must run on every node it might inject into
# (DOKS pools are tainted workload=aeon|redpanda:NoSchedule), otherwise
# the DaemonSet only schedules on the untainted monitoring node.
MSYS_NO_PATHCONV=1 helm upgrade --install chaos-mesh chaos-mesh/chaos-mesh \
  --version "$CHART_VERSION" \
  --namespace "$NAMESPACE" \
  --create-namespace \
  --set chaosDaemon.runtime=containerd \
  --set chaosDaemon.socketPath=/run/containerd/containerd.sock \
  --set "chaosDaemon.tolerations[0].key=workload" \
  --set "chaosDaemon.tolerations[0].operator=Equal" \
  --set "chaosDaemon.tolerations[0].value=aeon" \
  --set "chaosDaemon.tolerations[0].effect=NoSchedule" \
  --set "chaosDaemon.tolerations[1].key=workload" \
  --set "chaosDaemon.tolerations[1].operator=Equal" \
  --set "chaosDaemon.tolerations[1].value=redpanda" \
  --set "chaosDaemon.tolerations[1].effect=NoSchedule" \
  --set dashboard.securityMode=false \
  --wait \
  --timeout "${READY_TIMEOUT}s" >/dev/null

# ── wait for every pod Ready ─────────────────────────────────────────

log "waiting up to ${READY_TIMEOUT}s for chaos-mesh pods in '$NAMESPACE' to be Ready"
kubectl -n "$NAMESPACE" wait --for=condition=Ready pods --all \
  --timeout="${READY_TIMEOUT}s" >/dev/null

# ── sanity: NetworkChaos CRD present ─────────────────────────────────

if ! kubectl get crd networkchaos.chaos-mesh.org >/dev/null 2>&1; then
  echo "error: networkchaos.chaos-mesh.org CRD not registered after install" >&2
  exit 1
fi
log "NetworkChaos CRD registered"

# ── summary ──────────────────────────────────────────────────────────

log "chaos-mesh install complete. Pods:"
kubectl -n "$NAMESPACE" get pods -o wide
log "done. T5/T6 prerequisites met."
