#!/usr/bin/env bash
# uninstall-chaos-mesh.sh — P2.2b (GATE2-ACCEPTANCE-PLAN.md T5/T6)
#
# Cleanly remove Chaos Mesh from the Session A DOKS cluster. Intended
# for use before cluster tear-down, or when re-running install from a
# clean slate.
#
# Deletes in this order (active experiments first, then chart, then
# CRDs, then namespace) because stale NetworkChaos resources block
# namespace deletion via their finalizers.
#
# Usage:
#   ./uninstall-chaos-mesh.sh
#
# Env overrides:
#   NAMESPACE=<ns>           default: chaos
#   KEEP_CRDS=<0|1>          default: 0 (delete CRDs); set to 1 to keep
#   WAIT_TIMEOUT=<seconds>   default: 180
#
# Dependencies: helm, kubectl — authed against the target DOKS cluster.

set -euo pipefail

NAMESPACE="${NAMESPACE:-chaos}"
KEEP_CRDS="${KEEP_CRDS:-0}"
WAIT_TIMEOUT="${WAIT_TIMEOUT:-180}"

log() { printf '[%(%H:%M:%S)T] %s\n' -1 "$*"; }

for cmd in helm kubectl; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "error: '$cmd' not found on PATH" >&2
    exit 1
  fi
done

# The CRDs Chaos Mesh ships (drop the whole list so re-install is clean).
CHAOS_CRDS=(
  networkchaos.chaos-mesh.org
  podchaos.chaos-mesh.org
  iochaos.chaos-mesh.org
  timechaos.chaos-mesh.org
  kernelchaos.chaos-mesh.org
  stresschaos.chaos-mesh.org
  dnschaos.chaos-mesh.org
  httpchaos.chaos-mesh.org
  jvmchaos.chaos-mesh.org
  awschaos.chaos-mesh.org
  gcpchaos.chaos-mesh.org
  azurechaos.chaos-mesh.org
  blockchaos.chaos-mesh.org
  physicalmachinechaos.chaos-mesh.org
  schedules.chaos-mesh.org
  workflows.chaos-mesh.org
  workflownodes.chaos-mesh.org
  podnetworkchaos.chaos-mesh.org
  podiochaos.chaos-mesh.org
  podhttpchaos.chaos-mesh.org
  remoteclusters.chaos-mesh.org
  statuschecks.chaos-mesh.org
)

# ── short-circuit if nothing is installed ────────────────────────────

if ! kubectl get ns "$NAMESPACE" >/dev/null 2>&1; then
  log "namespace '$NAMESPACE' not present — nothing to uninstall"
  exit 0
fi

# ── 1. delete any in-flight experiments (finalizers would block ns) ──

log "deleting any live chaos experiments across all namespaces"
for crd in "${CHAOS_CRDS[@]}"; do
  kind="${crd%%.*}"
  if kubectl get crd "$crd" >/dev/null 2>&1; then
    kubectl delete "$kind.chaos-mesh.org" --all --all-namespaces \
      --ignore-not-found --wait=false >/dev/null 2>&1 || true
  fi
done

# ── 2. helm uninstall the chart ──────────────────────────────────────

if helm -n "$NAMESPACE" list -q 2>/dev/null | grep -q '^chaos-mesh$'; then
  log "helm uninstalling chaos-mesh from '$NAMESPACE'"
  helm -n "$NAMESPACE" uninstall chaos-mesh --wait \
    --timeout "${WAIT_TIMEOUT}s" >/dev/null
else
  log "helm release 'chaos-mesh' not found in '$NAMESPACE' — skipping helm uninstall"
fi

# ── 3. remove CRDs unless user asked to keep them ────────────────────

if [[ "$KEEP_CRDS" = "1" ]]; then
  log "KEEP_CRDS=1 — leaving chaos-mesh CRDs in place"
else
  log "deleting chaos-mesh CRDs"
  for crd in "${CHAOS_CRDS[@]}"; do
    kubectl delete crd "$crd" --ignore-not-found >/dev/null 2>&1 || true
  done
fi

# ── 4. delete namespace ──────────────────────────────────────────────

log "deleting namespace '$NAMESPACE' (timeout ${WAIT_TIMEOUT}s)"
kubectl delete namespace "$NAMESPACE" --ignore-not-found \
  --timeout="${WAIT_TIMEOUT}s" >/dev/null

log "done. chaos-mesh removed."
