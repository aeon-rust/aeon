#!/usr/bin/env bash
# resize-aeon-pool.sh — P2.2a (GATE2-ACCEPTANCE-PLAN.md T2/T3)
#
# Resize the `aeon-pool` DOKS node pool to a target VM count. Used to
# grow the pool 3 → 5 before a T2 Aeon-replicas scale-up, and shrink
# 5 → 3 after the T3 scale-down run. The Aeon StatefulSet replicas
# themselves are scaled separately (see run-t2-scaleup.sh / P2.2d) —
# this script only moves the underlying VMs.
#
# The script is idempotent: if the pool is already at the requested
# count, it exits 0 without calling the DO API. Post-resize it waits
# for every pool VM to become `Ready` in kubectl, then re-applies the
# `workload=aeon:NoSchedule` taint to any new nodes (DOKS HA-enabled
# pools do not persist taints through scale-up events reliably — see
# README.md § "After create, taint the Aeon pool").
#
# Usage:
#   ./resize-aeon-pool.sh <cluster-name-or-id> <target-count>
#
# Env overrides:
#   POOL=<pool-name>            default: aeon-pool
#   TAINT=<key=value:Effect>    default: workload=aeon:NoSchedule
#                               (empty string = skip re-tainting)
#   READY_TIMEOUT=<seconds>     default: 600
#
# Examples:
#   ./resize-aeon-pool.sh aeon-gate2-a 5      # grow 3 → 5 for T2
#   ./resize-aeon-pool.sh aeon-gate2-a 3      # shrink 5 → 3 after T3
#
# Dependencies: doctl, kubectl, jq — must be on PATH and authed against
# the target DOKS cluster.

set -euo pipefail

# ── args ──────────────────────────────────────────────────────────────

CLUSTER="${1:-}"
TARGET="${2:-}"
POOL="${POOL:-aeon-pool}"
TAINT="${TAINT:-workload=aeon:NoSchedule}"
READY_TIMEOUT="${READY_TIMEOUT:-600}"

usage() {
  cat >&2 <<EOF
Usage: $0 <cluster-name-or-id> <target-count>

Env overrides:
  POOL=<pool-name>            default: aeon-pool
  TAINT=<key=value:Effect>    default: workload=aeon:NoSchedule
                              (empty string = skip re-tainting)
  READY_TIMEOUT=<seconds>     default: 600 (10 min)
EOF
  exit 2
}

[[ -z "$CLUSTER" || -z "$TARGET" ]] && usage

if ! [[ "$TARGET" =~ ^[0-9]+$ ]] || (( TARGET < 1 || TARGET > 20 )); then
  echo "error: target must be an integer in [1, 20], got '$TARGET'" >&2
  exit 2
fi

# ── pre-flight ────────────────────────────────────────────────────────

for cmd in doctl kubectl jq; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "error: '$cmd' not found on PATH" >&2
    exit 1
  fi
done

log() { printf '[%(%H:%M:%S)T] %s\n' -1 "$*"; }

# Re-apply the workload taint across every pool node. Safe to call on
# an already-tainted node thanks to --overwrite; a no-op when TAINT is
# explicitly set to the empty string.
retaint_if_needed() {
  if [[ -z "$TAINT" ]]; then
    log "TAINT is empty — skipping re-taint step"
    return 0
  fi
  log "applying taint '$TAINT' to all '$POOL' nodes (--overwrite)"
  kubectl taint nodes -l "doks.digitalocean.com/node-pool=$POOL" \
    "$TAINT" --overwrite >/dev/null
  # Also reapply the matching workload label — DOKS doesn't carry it
  # forward to fresh nodes added by scale-up. Without the label the
  # nodeSelector on the Aeon StatefulSet rejects the new nodes even
  # though the toleration matches the taint. Derived from the taint:
  # `workload=aeon:NoSchedule` → label `workload=aeon`.
  local label_kv="${TAINT%:*}"  # strip ":NoSchedule"
  log "applying label '$label_kv' to all '$POOL' nodes (--overwrite)"
  kubectl label nodes -l "doks.digitalocean.com/node-pool=$POOL" \
    "$label_kv" --overwrite >/dev/null
}

# ── resolve cluster + pool IDs ────────────────────────────────────────

log "resolving cluster '$CLUSTER'"
CLUSTER_ID="$(doctl kubernetes cluster get "$CLUSTER" --format ID --no-header 2>/dev/null || true)"
if [[ -z "$CLUSTER_ID" ]]; then
  echo "error: cluster '$CLUSTER' not found (doctl kubernetes cluster list)" >&2
  exit 1
fi
log "cluster id: $CLUSTER_ID"

log "resolving pool '$POOL'"
POOL_JSON="$(doctl kubernetes cluster node-pool list "$CLUSTER_ID" --output json)"
POOL_ID="$(echo "$POOL_JSON" | jq -r --arg name "$POOL" '.[] | select(.name == $name) | .id')"
CURRENT="$(echo "$POOL_JSON" | jq -r --arg name "$POOL" '.[] | select(.name == $name) | .count')"

if [[ -z "$POOL_ID" ]]; then
  echo "error: pool '$POOL' not found in cluster $CLUSTER_ID" >&2
  echo "available pools:" >&2
  echo "$POOL_JSON" | jq -r '.[] | "  - \(.name) (count=\(.count))"' >&2
  exit 1
fi
log "pool id: $POOL_ID   current count: $CURRENT   target count: $TARGET"

# ── idempotent short-circuit ──────────────────────────────────────────

if [[ "$CURRENT" -eq "$TARGET" ]]; then
  log "already at target count $TARGET — no API call"
  # Still apply taints on the off chance the user dropped one.
  retaint_if_needed
  exit 0
fi

# ── resize ────────────────────────────────────────────────────────────

log "calling doctl node-pool update --count $TARGET (was $CURRENT)"
doctl kubernetes cluster node-pool update "$CLUSTER_ID" "$POOL_ID" \
  --count "$TARGET" \
  --output json >/dev/null

# ── wait for the pool to hit target + all Ready ──────────────────────

log "waiting up to ${READY_TIMEOUT}s for $TARGET nodes in '$POOL' to be Ready"
START="$(date +%s)"
while :; do
  NOW="$(date +%s)"
  ELAPSED=$(( NOW - START ))
  if (( ELAPSED > READY_TIMEOUT )); then
    echo "error: timed out after ${READY_TIMEOUT}s waiting for pool '$POOL' to stabilize at $TARGET Ready nodes" >&2
    kubectl get nodes -l "doks.digitalocean.com/node-pool=$POOL" -o wide >&2 || true
    exit 1
  fi

  # Count pool nodes that report Ready=True (not just .status == Ready,
  # which can briefly show blank on fresh nodes). JSONPath over the
  # conditions array is the robust read.
  TOTAL="$(kubectl get nodes -l "doks.digitalocean.com/node-pool=$POOL" \
    -o json 2>/dev/null | jq '.items | length')"
  READY="$(kubectl get nodes -l "doks.digitalocean.com/node-pool=$POOL" \
    -o jsonpath='{range .items[*]}{range .status.conditions[?(@.type=="Ready")]}{.status}{"\n"}{end}{end}' \
    2>/dev/null | grep -c '^True$' || true)"

  if [[ "$TOTAL" -eq "$TARGET" && "$READY" -eq "$TARGET" ]]; then
    log "pool '$POOL' is at $TARGET Ready nodes (elapsed ${ELAPSED}s)"
    break
  fi
  log "  have=$TOTAL ready=$READY target=$TARGET (elapsed ${ELAPSED}s)"
  sleep 10
done

# ── re-apply taint (HA pool scale-up drops taints on fresh nodes) ─────

retaint_if_needed

log "done. '$POOL' is at $TARGET Ready nodes with taint '$TAINT' applied."
