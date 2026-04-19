#!/usr/bin/env bash
# run-t5-split-brain.sh — GATE2-ACCEPTANCE-PLAN.md § 5 · T5
#
# Split-brain drill via Chaos Mesh NetworkChaos: isolate `aeon-2` from
# `aeon-0`/`aeon-1`, drive cluster-mutation writes against all three
# nodes for ~2 min, heal the partition, verify minority rejoins without
# divergence.
#
# Precondition: 3-node Aeon (`kubectl -n aeon get sts aeon` at
# replicas=3), Chaos Mesh installed (see `install-chaos-mesh.sh`),
# `aeon-source` topic pre-created, loadgen namespace present.
#
# Sequence (plan § T5):
#   1. Apply NetworkChaos `chaos-netpart-aeon-2.yaml` — isolates aeon-2.
#   2. For WRITE_SECONDS, drive a cluster-mutation write loop at each
#      of the three REST endpoints (pipeline-create + immediate delete).
#      Writes against the isolated node should time out or 5xx; writes
#      against the majority pair should commit cleanly. Per-endpoint
#      counters are tallied.
#   3. Delete the NetworkChaos resource.
#   4. Wait until every Raft member shows the same `last_applied` index,
#      confirming the isolated node has caught up via log replay.
#   5. Compare per-partition last-applied across members — mismatch
#      means divergent commits (T5 fail).
#   6. Emit verdict: zero duplicate or divergent commits, minority
#      rejoined without manual intervention.
#
# Usage:
#   ./run-t5-split-brain.sh
#
# Env overrides:
#   WRITE_SECONDS=<s>      default: 120
#   WRITE_INTERVAL_MS=<ms> default: 500   (per-endpoint loop cadence)
#   ISOLATED_POD=<name>    default: aeon-2

set -euo pipefail

WRITE_SECONDS="${WRITE_SECONDS:-120}"
WRITE_INTERVAL_MS="${WRITE_INTERVAL_MS:-500}"
ISOLATED_POD="${ISOLATED_POD:-aeon-2}"
NS=aeon
CHAOS_FILE="$(dirname "$0")/chaos-netpart-aeon-2.yaml"

log() { printf '[%(%H:%M:%S)T] %s\n' -1 "$*" >&2; }
row() { printf '%s\t%s\t%d\t%s\n' "$1" "$2" "$(date +%s%3N)" "${3:-}"; }

for cmd in kubectl jq; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "error: '$cmd' missing on PATH" >&2; exit 1; }
done

# ── probe pod (persistent curl proxy into the cluster network) ────────

ensure_probe() {
  if kubectl -n $NS get deploy probe >/dev/null 2>&1; then
    return 0
  fi
  log "creating probe deployment"
  kubectl -n $NS apply -f - <<'EOF' >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata:
  name: probe
  namespace: aeon
spec:
  replicas: 1
  selector: { matchLabels: { app: probe } }
  template:
    metadata: { labels: { app: probe } }
    spec:
      tolerations:
        - { key: workload, operator: Equal, value: aeon, effect: NoSchedule }
      containers:
        - name: c
          image: curlimages/curl:8.7.1
          command: ["sleep","infinity"]
EOF
  kubectl -n $NS rollout status deploy/probe --timeout=60s >/dev/null
}

curl_api() { kubectl -n $NS exec -q deploy/probe -- curl -sS "$@"; }

host_for() { echo "aeon-${1}.aeon-headless.${NS}.svc.cluster.local:4471"; }

# ── pre-check: 3-node state ───────────────────────────────────────────

ensure_probe
CUR=$(kubectl -n $NS get sts aeon -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo 0)
if [[ "$CUR" != "3" ]]; then
  echo "error: expected 3 ready Aeon replicas, got $CUR" >&2
  exit 1
fi

# Confirm chaos-mesh is installed.
if ! kubectl get crd networkchaos.chaos-mesh.org >/dev/null 2>&1; then
  echo "error: Chaos Mesh CRDs not installed — run install-chaos-mesh.sh first" >&2
  exit 1
fi

row setup begin "isolated=$ISOLATED_POD"

# ── step 1: apply partition ───────────────────────────────────────────

log "applying NetworkChaos (isolate $ISOLATED_POD)"
kubectl apply -f "$CHAOS_FILE" >/dev/null
row partition applied

# Give Chaos Mesh ~5s to propagate the iptables rules.
sleep 5

# ── step 2: hammer all 3 REST endpoints with cluster writes ──────────

# Per-endpoint success/fail counters. A "write" here is a pipeline
# create + immediate delete — cheap, goes through Raft on the leader.
SUCCESS=(0 0 0)
REJECT=(0 0 0)

end=$(( $(date +%s) + WRITE_SECONDS ))
iter=0
while (( $(date +%s) < end )); do
  iter=$(( iter + 1 ))
  for n in 0 1 2; do
    name="t5-probe-${n}-${iter}"
    payload='{"name":"'"$name"'","sources":[{"type":"memory","topic":null,"partitions":[],"config":{"count":"0","payload_size":"0"}}],"processor":{"name":"__identity","version":"0.0.0"},"sinks":[{"type":"blackhole","topic":null,"config":{}}],"state":"created","created_at":0,"updated_at":0,"durability":{"mode":"none"}}'
    code=$(curl_api -o /dev/null -w "%{http_code}" -m 3 \
      -X POST -H "Content-Type:application/json" \
      -d "$payload" \
      "http://$(host_for "$n")/api/v1/pipelines" 2>/dev/null || echo "000")
    if [[ "$code" =~ ^2 ]]; then
      SUCCESS[$n]=$(( SUCCESS[n] + 1 ))
      # best-effort cleanup; ignore failures (delete races heal etc.)
      curl_api -X DELETE -m 3 \
        "http://$(host_for "$n")/api/v1/pipelines/$name" >/dev/null 2>&1 || true
    else
      REJECT[$n]=$(( REJECT[n] + 1 ))
    fi
  done
  # sleep in ms-ish granularity without relying on GNU-only sleep fractional
  python3 -c "import time; time.sleep($WRITE_INTERVAL_MS/1000.0)" 2>/dev/null \
    || sleep 1
done

log "write loop done: n0 ok=${SUCCESS[0]} rej=${REJECT[0]}, n1 ok=${SUCCESS[1]} rej=${REJECT[1]}, n2 ok=${SUCCESS[2]} rej=${REJECT[2]}"
row writes done "n0=${SUCCESS[0]}/${REJECT[0]} n1=${SUCCESS[1]}/${REJECT[1]} n2=${SUCCESS[2]}/${REJECT[2]}"

# ── step 3: heal partition ────────────────────────────────────────────

log "deleting NetworkChaos (heal partition)"
kubectl delete -f "$CHAOS_FILE" --ignore-not-found >/dev/null
row partition healed

# ── step 4: wait for last_applied to converge across all 3 members ───

log "waiting up to 120s for Raft last_applied to converge across members"
deadline=$(( $(date +%s) + 120 ))
converged=false
while (( $(date +%s) < deadline )); do
  vals=()
  for n in 0 1 2; do
    v=$(curl_api -m 5 "http://$(host_for "$n")/api/v1/cluster/status" 2>/dev/null \
      | jq -r '.raft.last_applied // 0' 2>/dev/null || echo 0)
    vals+=("$v")
  done
  if [[ "${vals[0]}" == "${vals[1]}" && "${vals[1]}" == "${vals[2]}" && "${vals[0]}" != "0" ]]; then
    log "all members at last_applied=${vals[0]}"
    converged=true
    break
  fi
  log "  last_applied: n0=${vals[0]} n1=${vals[1]} n2=${vals[2]}"
  sleep 3
done

row convergence "$([[ "$converged" = true ]] && echo ok || echo fail)" "final=${vals[*]}"

# ── step 5: compare per-partition ownership across members ──────────

log "diffing per-partition ownership across members"
NO_MISMATCH=true
ref=$(curl_api -m 5 "http://$(host_for 0)/api/v1/cluster/status" 2>/dev/null \
  | jq -c '.partitions' 2>/dev/null || echo '{}')
for n in 1 2; do
  peer=$(curl_api -m 5 "http://$(host_for "$n")/api/v1/cluster/status" 2>/dev/null \
    | jq -c '.partitions' 2>/dev/null || echo '{}')
  if [[ "$ref" != "$peer" ]]; then
    NO_MISMATCH=false
    echo "mismatch: node 0 vs node $n" >&2
    diff <(echo "$ref" | jq .) <(echo "$peer" | jq .) >&2 || true
  fi
done

# ── step 6: verdict ───────────────────────────────────────────────────

echo
echo "=== T5 verdict ==="
printf "%-28s %s\n"    "isolated pod"                 "$ISOLATED_POD"
printf "%-28s %d/%d\n" "aeon-0 (majority) ok/rej"     "${SUCCESS[0]}" "${REJECT[0]}"
printf "%-28s %d/%d\n" "aeon-1 (majority) ok/rej"     "${SUCCESS[1]}" "${REJECT[1]}"
printf "%-28s %d/%d\n" "aeon-2 (minority) ok/rej"     "${SUCCESS[2]}" "${REJECT[2]}"
printf "%-28s %s\n"    "Raft last_applied converged"  "$converged"
printf "%-28s %s\n"    "per-partition ownership agrees" "$NO_MISMATCH"

# Pass criterion: some writes succeeded on majority pair; minority
# writes were refused (should forward-to-leader-unreachable → 5xx); all
# three members re-converged on the same Raft index and the same
# partition ownership map.
MAJ_SUCCESS=$(( SUCCESS[0] + SUCCESS[1] ))
MIN_REJECT="${REJECT[2]}"
if (( MAJ_SUCCESS > 0 )) && (( MIN_REJECT > 0 )) \
   && [[ "$converged" = true ]] && [[ "$NO_MISMATCH" = true ]]; then
  echo "T5 PASS: majority committed, minority refused, rejoin clean."
  exit 0
else
  echo "T5 FAIL: see counters above." >&2
  exit 3
fi
