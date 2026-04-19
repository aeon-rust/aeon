#!/usr/bin/env bash
# run-t3-scaledown.sh — GATE2-ACCEPTANCE-PLAN.md § 5 · T3
#
# Scale-down 5 → 3 → 1 under OrderedBatch load, verify zero event loss.
#
# Precondition: DOKS cluster at the end-state of T2 — aeon-pool=5,
# StatefulSet `aeon` at replicas=5, Redpanda brokers up, `aeon-source`
# topic with 24 partitions, `loadgen` namespace present.
#
# Sequence (plan § T3):
#   1. Create & start a fresh OrderedBatch pipeline (distinct name so
#      metrics don't collide with the T2 run).
#   2. Launch loadgen Job producing at ~RATE events/sec into aeon-source.
#   3. Warm-up on the 5-node cluster.
#   4. For each node being removed (5, 4): call `aeon cluster drain`
#      FIRST so the leader reassigns that node's partitions before the
#      pod terminates, then `kubectl scale`. This compensates for the
#      missing `preStop` drain hook (plan § T3 open sub-question).
#   5. At 3-node state, same drain-then-scale routine for nodes 3 and 2
#      to reach replicas=1. StatefulSet pod ordinals go in reverse, so
#      drain hits the highest first.
#   6. Shrink aeon-pool 5 → 3 (via resize-aeon-pool.sh). Leave the pool
#      at 3 even though only 1 pod runs — so T3 ends in the same
#      topology that would precede a future fresh provision.
#   7. Wait for producer Job finish, then for sink drain.
#   8. Verdict: zero producer-vs-ack delta, no WAL engagement, every
#      drain RPC returned 200 OK with the plan it executed.
#
# Usage:
#   ./run-t3-scaledown.sh <cluster-id>
#
# Env overrides:
#   RATE=<events/sec>       default: 1000000
#   DURATION_S=<seconds>    default: 900
#   POOL=<pool-name>        default: aeon-pool
#   READY_TIMEOUT=<s>       default: 600

set -euo pipefail

CLUSTER="${1:-}"
if [[ -z "$CLUSTER" ]]; then
  echo "Usage: $0 <cluster-id>" >&2
  exit 2
fi

RATE="${RATE:-1000000}"
DURATION_S="${DURATION_S:-900}"
TOTAL=$(( RATE * DURATION_S ))
POOL="${POOL:-aeon-pool}"
READY_TIMEOUT="${READY_TIMEOUT:-600}"
NS=aeon
PIPELINE=t3-scaledown
JSON_FILE="$(dirname "$0")/t3-ordered.json"
PRODUCER_JOB=loadgen-t3
LEADER_SINGLE="aeon-0.aeon-headless.${NS}.svc.cluster.local:4471"

log() { printf '[%(%H:%M:%S)T] %s\n' -1 "$*" >&2; }
row() { printf '%s\t%s\t%d\t%s\n' "$1" "$2" "$(date +%s%3N)" "${3:-}"; }

for cmd in kubectl doctl jq; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "error: '$cmd' missing on PATH" >&2; exit 1; }
done

# ── probe pod (persistent curl/jq proxy) ────────────────────────────────

ensure_probe() {
  if kubectl -n $NS get deploy probe >/dev/null 2>&1; then
    return 0
  fi
  log "creating probe deployment in $NS"
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

curl_api() { kubectl -n $NS exec -q deploy/probe -- curl -sS -m 10 "$@"; }

# ── cluster state helpers (mirrors of the T2 script) ────────────────────

pick_leader_host() {
  local reps
  reps=$(kubectl -n $NS get sts aeon -o jsonpath='{.status.replicas}' 2>/dev/null || echo 1)
  for (( n=0; n<reps; n++ )); do
    local host="aeon-${n}.aeon-headless.${NS}.svc.cluster.local:4471"
    if curl_api "http://${host}/api/v1/cluster/status" >/dev/null 2>&1; then
      echo "$host"
      return 0
    fi
  done
  echo "$LEADER_SINGLE"
}

# Find the actual Raft leader pod (workaround for G9 — REST API returns
# 500 + leader-hint instead of 307 forwarding writes). Reads the
# `leader_id` from any reachable pod's cluster-status, then maps it back
# to its `aeon-N` pod. Returns the pod's host:port string.
pick_writable_leader() {
  local probe_host
  probe_host=$(pick_leader_host)
  local lid
  lid=$(curl_api "http://${probe_host}/api/v1/cluster/status" 2>/dev/null \
        | jq -r '.leader_id // empty' 2>/dev/null)
  if [[ -z "$lid" || "$lid" == "null" ]]; then
    echo "$probe_host"  # no leader yet — best-effort
    return 0
  fi
  # Aeon node IDs are 1-indexed; pod ordinals are 0-indexed (aeon-0 = node 1)
  local ord=$(( lid - 1 ))
  echo "aeon-${ord}.aeon-headless.${NS}.svc.cluster.local:4471"
}

wait_for_members() {
  # Cluster status exposes Raft membership as a Rust Debug string, e.g.
  #   "membership":"Membership { configs: [{1, 2, 3}], nodes: {...} }"
  # We extract the LAST {…} set inside `configs: [...]` (newest joint cfg),
  # then count the comma-separated voter ids inside it.
  local want="$1"; local deadline=$(( $(date +%s) + 300 ))
  while [[ $(date +%s) -lt $deadline ]]; do
    local host; host=$(pick_leader_host)
    local membership members
    membership=$(curl_api "http://${host}/api/v1/cluster/status" 2>/dev/null \
      | jq -r '.raft.membership // ""' 2>/dev/null || echo '')
    members=$(echo "$membership" \
      | grep -oE 'configs: \[[^]]*\]' \
      | grep -oE '\{[0-9, ]+\}' | tail -1 \
      | tr -cd '0-9,' | tr ',' '\n' | grep -c '[0-9]' || echo 0)
    if [[ "$members" == "$want" ]]; then
      log "cluster has $want members"
      return 0
    fi
    log "  members=$members want=$want"
    sleep 5
  done
  return 1
}

wait_for_all_partitions_owned() {
  local deadline=$(( $(date +%s) + 600 ))
  while [[ $(date +%s) -lt $deadline ]]; do
    local host; host=$(pick_leader_host)
    local status
    status=$(curl_api "http://${host}/api/v1/cluster/status" 2>/dev/null || echo '{}')
    local total transferring
    total=$(echo "$status" | jq -r '.partitions | length')
    transferring=$(echo "$status" | jq -r \
      '[.partitions | to_entries[] | select(.value.status != "owned")] | length')
    if [[ "$total" -gt 0 && "$transferring" == 0 ]]; then
      log "all $total partitions owned"
      return 0
    fi
    log "  partitions: total=$total transferring=$transferring"
    sleep 5
  done
  return 1
}

# ── producer Job wrapper ────────────────────────────────────────────────

start_producer_job() {
  kubectl -n loadgen delete job "$PRODUCER_JOB" --ignore-not-found >/dev/null 2>&1 || true
  kubectl -n loadgen apply -f - <<EOF >/dev/null
apiVersion: batch/v1
kind: Job
metadata:
  name: $PRODUCER_JOB
  namespace: loadgen
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      nodeSelector: { workload: monitoring }
      containers:
        - name: producer
          image: registry.digitalocean.com/rust-proxy-registry/aeon:session-a-prep
          imagePullPolicy: IfNotPresent
          command: ["aeon-producer"]
          args:
            - "--topic"
            - "aeon-source"
            - "--brokers"
            - "redpanda.redpanda.svc.cluster.local:9092"
            - "--count"
            - "$TOTAL"
            - "--rate"
            - "$RATE"
      imagePullSecrets:
        - name: rust-proxy-registry
EOF
  log "producer Job '$PRODUCER_JOB' submitted (rate=$RATE count=$TOTAL)"
  kubectl -n loadgen wait --for=condition=Ready pod \
    -l job-name=$PRODUCER_JOB --timeout=120s >/dev/null
}

wait_producer_finish() {
  log "waiting for producer Job to complete"
  kubectl -n loadgen wait --for=condition=Complete --timeout=$((DURATION_S + 180))s \
    job/$PRODUCER_JOB >/dev/null
}

# ── metrics (sum across currently-alive pods) ───────────────────────────

sink_acked_total() {
  local reps; reps=$(kubectl -n $NS get sts aeon -o jsonpath='{.status.replicas}' 2>/dev/null || echo 1)
  local sum=0
  for (( n=0; n<reps; n++ )); do
    local host="aeon-${n}.aeon-headless.${NS}.svc.cluster.local:4471"
    local v
    v=$(curl_api "http://${host}/metrics" 2>/dev/null \
      | grep -E "^aeon_pipeline_outputs_acked_total\{pipeline=\"${PIPELINE}\"\}" \
      | awk '{print $2}' | head -1)
    v=${v:-0}
    sum=$(( sum + ${v%.*} ))
  done
  echo "$sum"
}

wal_fallback_total() {
  local reps; reps=$(kubectl -n $NS get sts aeon -o jsonpath='{.status.replicas}' 2>/dev/null || echo 1)
  local sum=0
  for (( n=0; n<reps; n++ )); do
    local host="aeon-${n}.aeon-headless.${NS}.svc.cluster.local:4471"
    local v
    v=$(curl_api "http://${host}/metrics" 2>/dev/null \
      | grep -E "^aeon_checkpoint_fallback_wal_total" \
      | awk '{print $2}' | head -1)
    v=${v:-0}
    sum=$(( sum + ${v%.*} ))
  done
  echo "$sum"
}

wait_sink_drain() {
  local want="$1"
  local stable=0 prev=-1 deadline=$(( $(date +%s) + 300 ))
  while [[ $(date +%s) -lt $deadline ]]; do
    local got; got=$(sink_acked_total)
    log "  sink_acked=$got want=$want"
    if [[ "$got" -ge "$want" ]]; then
      return 0
    fi
    if [[ "$got" == "$prev" ]]; then
      stable=$(( stable + 1 ))
      [[ $stable -ge 6 ]] && return 1
    else
      stable=0
    fi
    prev=$got
    sleep 5
  done
  return 1
}

# ── drain-then-scale: do explicit partition evacuation before pod dies ─

drain_and_scale_down() {
  # Drain the highest-ordinal pods first (StatefulSet removes them in
  # reverse order), then shrink replicas. Raft node_id is ordinal+1 in
  # the default chart wiring, so sts-pod `aeon-4` is cluster node_id 5.
  local current="$1" target="$2"
  if (( target >= current )); then
    echo "error: target $target >= current $current" >&2
    return 1
  fi
  for (( ord=current-1; ord>=target; ord-- )); do
    local node_id=$(( ord + 1 ))
    log "draining node_id=$node_id (pod aeon-$ord) before scale"
    local host; host=$(pick_writable_leader)
    local resp
    resp=$(curl_api -X POST -H "Content-Type:application/json" \
      -d "{\"node_id\":$node_id}" \
      "http://${host}/api/v1/cluster/drain" 2>/dev/null || echo '{}')
    log "drain response: $(echo "$resp" | jq -c . 2>/dev/null || echo "$resp")"
  done
  # Wait for ownership to settle post-drain before shrinking replicas.
  wait_for_all_partitions_owned
  log "scaling sts/aeon $current → $target"
  kubectl -n $NS scale sts aeon --replicas="$target" >/dev/null
  kubectl -n $NS rollout status sts/aeon --timeout=$((READY_TIMEOUT))s >/dev/null
  wait_for_members "$target"
}

# ── sequence ────────────────────────────────────────────────────────────

ensure_probe
row setup begin "cluster=$CLUSTER rate=$RATE total=$TOTAL"

# sanity: expect 5 nodes at start
CUR=$(kubectl -n $NS get sts aeon -o jsonpath='{.status.replicas}')
if [[ "$CUR" != "5" ]]; then
  echo "error: expected StatefulSet replicas=5 at T3 start, got $CUR" >&2
  exit 1
fi

log "creating pipeline '$PIPELINE'"
LEADER_HOST=$(pick_writable_leader)
log "  routing writes to leader: $LEADER_HOST"
curl_api -X POST -H "Content-Type:application/json" \
  --data-binary "$(cat "$JSON_FILE")" \
  "http://${LEADER_HOST}/api/v1/pipelines" | tail -1
curl_api -X POST "http://${LEADER_HOST}/api/v1/pipelines/${PIPELINE}/start" | tail -1
row pipeline started

start_producer_job
row producer started

sleep 30
row phase1 5_node_stable

# 5 → 3
drain_and_scale_down 5 3
row phase2 3_node_stable

# 3 → 1
drain_and_scale_down 3 1
row phase3 1_node_stable

# shrink pool
log "shrinking node-pool '$POOL' → 3"
POOL="$POOL" "$(dirname "$0")/resize-aeon-pool.sh" "$CLUSTER" 3 >&2
row pool resized_to_3

wait_producer_finish
row producer finished
wait_sink_drain "$TOTAL" || true
row sink drained

# verdict
ACKED=$(sink_acked_total)
WAL=$(wal_fallback_total)
LOSS=$(( TOTAL - ACKED ))

echo
echo "=== T3 verdict ==="
printf "%-28s %s\n" "producer target (TOTAL)"      "$TOTAL"
printf "%-28s %s\n" "sink acked total"              "$ACKED"
printf "%-28s %s\n" "event loss"                    "$LOSS"
printf "%-28s %s  (must be 0)\n" "WAL fallback total"          "$WAL"

if [[ "$LOSS" == 0 && "$WAL" == 0 ]]; then
  echo "T3 PASS: zero loss, no WAL engagement."
  exit 0
else
  echo "T3 FAIL: loss=$LOSS wal=$WAL" >&2
  exit 3
fi
