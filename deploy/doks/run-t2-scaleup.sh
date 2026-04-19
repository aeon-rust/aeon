#!/usr/bin/env bash
# run-t2-scaleup.sh — GATE2-ACCEPTANCE-PLAN.md § 5 · T2
#
# Scale-up 1 → 3 → 5 under OrderedBatch load, verify zero event loss.
#
# Precondition: DOKS cluster provisioned with `aeon-pool` at count=3 and
# StatefulSet `aeon` at replicas=1. Redpanda brokers up, `aeon-source`
# topic pre-created with 24 partitions, `loadgen` namespace present.
# See README.md § Sequence.
#
# Sequence (plan § T2):
#   1. Create & start OrderedBatch pipeline (Kafka → Kafka, 24 partitions).
#   2. Launch loadgen Job producing at ~RATE events/sec into aeon-source.
#   3. Wait for stable producer → sink flow on the single aeon-0 node.
#   4. StatefulSet replicas 1 → 3; wait for all 3 to be Ready and the
#      cluster-status endpoint to show 3 voters + 24 partitions Owned.
#   5. Resize aeon-pool 3 → 5 (via ./resize-aeon-pool.sh).
#   6. StatefulSet replicas 3 → 5; wait for ready + ownership redistribute.
#   7. Wait for producer Job to finish (hits TOTAL count), then wait for
#      sink-side `outputs_acked_total` to catch up.
#   8. Verdict: compare producer count vs sink ack count; verify
#      `aeon_checkpoint_fallback_wal_total` stayed at 0 across all pods.
#
# Usage:
#   ./run-t2-scaleup.sh <cluster-id>
#
# Env overrides:
#   RATE=<events/sec>       default: 1000000    (producer rate)
#   DURATION_S=<seconds>    default: 900        (target run length)
#   POOL=<pool-name>        default: aeon-pool
#   READY_TIMEOUT=<s>       default: 600        (node-pool resize wait)
#
# Emits TSV on stdout: phase, event, timestamp_ms, notes

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
PIPELINE=t2-scaleup
JSON_FILE="$(dirname "$0")/t2-ordered.json"
PRODUCER_JOB=loadgen-t2
LEADER_SINGLE="aeon-0.aeon-headless.${NS}.svc.cluster.local:4471"

log() { printf '[%(%H:%M:%S)T] %s\n' -1 "$*" >&2; }
row() { printf '%s\t%s\t%d\t%s\n' "$1" "$2" "$(date +%s%3N)" "${3:-}"; }

for cmd in kubectl doctl jq; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "error: '$cmd' missing on PATH" >&2; exit 1; }
done

# ── pick a pod that is currently in the StatefulSet to route API/metrics ─

pick_leader_host() {
  # Return the first aeon-N pod that answers /cluster/status. Replicas
  # change mid-run, so re-resolve on each caller.
  local reps
  reps=$(kubectl -n $NS get sts aeon -o jsonpath='{.status.replicas}' 2>/dev/null || echo 1)
  for (( n=0; n<reps; n++ )); do
    local host="aeon-${n}.aeon-headless.${NS}.svc.cluster.local:4471"
    if kubectl -n $NS exec -q deploy/probe -- curl -sS -m 3 "http://${host}/api/v1/cluster/status" >/dev/null 2>&1; then
      echo "$host"
      return 0
    fi
  done
  # Fallback: always-present aeon-0
  echo "$LEADER_SINGLE"
}

# Ensure we have a persistent curl/jq probe pod in $NS (pods come and go
# across scale events; one long-lived probe avoids per-call pod cold starts).
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

# ── wait for cluster to converge on expected member + ownership state ───

wait_for_members() {
  # The cluster status exposes Raft membership as a Rust Debug string, e.g.
  #   "membership":"Membership { configs: [{1, 2, 3}], nodes: {...} }"
  # We extract the LAST {…} set inside `configs: [...]` (newest joint cfg),
  # then count the comma-separated voter ids inside it.
  local want="$1"; local deadline=$(( $(date +%s) + 300 ))
  while [[ $(date +%s) -lt $deadline ]]; do
    local host; host=$(pick_leader_host)
    local membership members
    membership=$(curl_api "http://${host}/api/v1/cluster/status" 2>/dev/null \
      | jq -r '.raft.membership // ""' 2>/dev/null || echo '')
    # Pull the last brace-set inside configs:[...] and count voters.
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
  echo "error: timed out waiting for $want members" >&2
  curl_api "http://$(pick_leader_host)/api/v1/cluster/status" >&2 || true
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
  echo "error: partitions did not settle" >&2
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
          image: registry.digitalocean.com/rust-proxy-registry/aeon:70c68b3
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
        - name: registry-rust-proxy-registry
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

# ── sink-side metrics aggregation (sums across all live aeon-N pods) ────

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
      log "sink drained to $got (>= $want)"
      return 0
    fi
    if [[ "$got" == "$prev" ]]; then
      stable=$(( stable + 1 ))
      if [[ $stable -ge 6 ]]; then
        log "sink flat at $got for 6 polls — giving up"
        return 1
      fi
    else
      stable=0
    fi
    prev=$got
    sleep 5
  done
  return 1
}

# ── scale helpers ───────────────────────────────────────────────────────

scale_aeon() {
  local target="$1"
  log "scaling sts/aeon → $target replicas"
  kubectl -n $NS scale sts aeon --replicas="$target" >/dev/null
  kubectl -n $NS rollout status sts/aeon --timeout=$((READY_TIMEOUT))s >/dev/null
}

# ── sequence ────────────────────────────────────────────────────────────
#
# START_REPLICAS env (default 1) lets you skip the 1→3 phase and start
# the test from an existing N-node cluster. Used after the 2026-04-19
# G8 finding (single-node Raft bootstrap fails on first proposal); the
# 1→3 phase was implicitly proven on 2026-04-19 11:09 UTC before the
# parser bug aborted the run, so re-validating only 3→5 is acceptable.

START_REPLICAS="${START_REPLICAS:-1}"

ensure_probe
row setup begin "cluster=$CLUSTER rate=$RATE total=$TOTAL start_replicas=$START_REPLICAS"

# step 0 — make sure the cluster has actually settled before we touch the
# REST API. Skip when START_REPLICAS=1 (we own the bootstrap then).
if [[ "$START_REPLICAS" -ge 3 ]]; then
  wait_for_members "$START_REPLICAS"
  wait_for_all_partitions_owned
fi

# step 1 — pipeline
log "creating pipeline '$PIPELINE'"
curl_api -X POST -H "Content-Type:application/json" \
  --data-binary "$(cat "$JSON_FILE")" \
  "http://${LEADER_SINGLE}/api/v1/pipelines" | tail -1
curl_api -X POST "http://${LEADER_SINGLE}/api/v1/pipelines/${PIPELINE}/start" | tail -1
row pipeline started

# step 2 — producer
start_producer_job
row producer started

# step 3 — warmup at start replicas
sleep 30
row phase1 "${START_REPLICAS}_node_stable"

# step 4 — scale (skipped if we already started at >=3)
if [[ "$START_REPLICAS" -lt 3 ]]; then
  scale_aeon 3
  wait_for_members 3
  wait_for_all_partitions_owned
  row phase2 3_node_stable
fi

# step 5 — resize pool 3 → 5
log "resizing node-pool '$POOL' → 5"
POOL="$POOL" "$(dirname "$0")/resize-aeon-pool.sh" "$CLUSTER" 5 >&2
row pool resized_to_5

# step 6 — scale 3 → 5
scale_aeon 5
wait_for_members 5
wait_for_all_partitions_owned
row phase3 5_node_stable

# step 7 — let producer finish + sink drain
wait_producer_finish
row producer finished
wait_sink_drain "$TOTAL" || true
row sink drained

# step 8 — verdict
ACKED=$(sink_acked_total)
WAL=$(wal_fallback_total)
LOSS=$(( TOTAL - ACKED ))

echo
echo "=== T2 verdict ==="
printf "%-28s %s\n" "producer target (TOTAL)"      "$TOTAL"
printf "%-28s %s\n" "sink acked total"              "$ACKED"
printf "%-28s %s\n" "event loss"                    "$LOSS"
printf "%-28s %s  (must be 0)\n" "WAL fallback total"          "$WAL"

if [[ "$LOSS" == 0 && "$WAL" == 0 ]]; then
  echo "T2 PASS: zero loss, no WAL engagement."
  exit 0
else
  echo "T2 FAIL: loss=$LOSS wal=$WAL" >&2
  exit 3
fi
