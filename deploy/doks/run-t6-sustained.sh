#!/usr/bin/env bash
# run-t6-sustained.sh — GATE2-ACCEPTANCE-PLAN.md § 5 · T6
#
# 10-minute sustained OrderedBatch run with 4 interleaved chaos events.
# Chaos events are time-sequenced from test start:
#   t = 2 min  — kill current leader pod → measure failover time
#   t = 4 min  — POST /cluster/rebalance → measure cutover
#   t = 6 min  — apply NetworkChaos (isolate aeon-2) [T5 repeat under load]
#   t = 8 min  — delete NetworkChaos (heal)
#   t = 10 min — producer finishes, script begins drain wait + verdict
#
# Precondition: 3-node Aeon at replicas=3, Chaos Mesh installed,
# aeon-source topic 24 partitions, loadgen namespace present.
#
# Usage:
#   ./run-t6-sustained.sh
#
# Env overrides:
#   RATE=<events/sec>     default: 500000  (~80% of T1 cell-C2 ceiling)
#   DURATION_S=<seconds>  default: 600     (10 min)

set -euo pipefail

RATE="${RATE:-500000}"
DURATION_S="${DURATION_S:-600}"
TOTAL=$(( RATE * DURATION_S ))
NS=aeon
PIPELINE=t6-sustained
PRODUCER_JOB=loadgen-t6
JSON_FILE="$(dirname "$0")/t2-ordered.json"   # same shape as T2
CHAOS_FILE="$(dirname "$0")/chaos-netpart-aeon-2.yaml"

log() { printf '[%(%H:%M:%S)T] %s\n' -1 "$*" >&2; }
row() { printf '%s\t%s\t%d\t%s\n' "$1" "$2" "$(date +%s%3N)" "${3:-}"; }

for cmd in kubectl jq; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "error: '$cmd' missing on PATH" >&2; exit 1; }
done

# ── probe pod ──────────────────────────────────────────────────────────

ensure_probe() {
  if kubectl -n $NS get deploy probe >/dev/null 2>&1; then return 0; fi
  log "creating probe deployment"
  kubectl -n $NS apply -f - <<'EOF' >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: { name: probe, namespace: aeon }
spec:
  replicas: 1
  selector: { matchLabels: { app: probe } }
  template:
    metadata: { labels: { app: probe } }
    spec:
      tolerations:
        - { key: workload, operator: Equal, value: aeon, effect: NoSchedule }
      containers:
        - { name: c, image: "curlimages/curl:8.7.1", command: ["sleep","infinity"] }
EOF
  kubectl -n $NS rollout status deploy/probe --timeout=60s >/dev/null
}

curl_api() { kubectl -n $NS exec -q deploy/probe -- curl -sS -m 10 "$@"; }
host_for() { echo "aeon-${1}.aeon-headless.${NS}.svc.cluster.local:4471"; }

# ── cluster state helpers ─────────────────────────────────────────────

leader_pod() {
  # Ask each pod who the leader is; first valid answer wins. The
  # leader is returned as the pod name (aeon-<N-1>).
  for n in 0 1 2; do
    local id
    id=$(curl_api "http://$(host_for "$n")/api/v1/cluster/status" 2>/dev/null \
      | jq -r '.leader_id // empty' 2>/dev/null)
    if [[ -n "$id" && "$id" != "null" ]]; then
      echo "aeon-$(( id - 1 ))"
      return 0
    fi
  done
  return 1
}

pod_has_leader() {
  for n in 0 1 2; do
    local id
    id=$(curl_api -m 3 "http://$(host_for "$n")/api/v1/cluster/status" 2>/dev/null \
      | jq -r '.leader_id // 0' 2>/dev/null || echo 0)
    if [[ "$id" != "0" && "$id" != "null" ]]; then
      return 0
    fi
  done
  return 1
}

# Resolve current leader host:port. Falls back to aeon-0 if leader is
# unknown (e.g. mid-election). Used to route cluster-mutation writes
# so we don't trip G9 (REST API doesn't auto-forward to leader).
pick_leader_host() {
  local pod; pod=$(leader_pod 2>/dev/null || echo "aeon-0")
  echo "${pod}.aeon-headless.${NS}.svc.cluster.local:4471"
}

wait_for_leader() {
  local deadline=$(( $(date +%s) + 30 ))
  while (( $(date +%s) < deadline )); do
    if pod_has_leader; then return 0; fi
    sleep 1
  done
  return 1
}

# ── producer Job wrapper ──────────────────────────────────────────────

start_producer_job() {
  kubectl -n loadgen delete job "$PRODUCER_JOB" --ignore-not-found >/dev/null 2>&1 || true
  kubectl -n loadgen apply -f - <<EOF >/dev/null
apiVersion: batch/v1
kind: Job
metadata: { name: $PRODUCER_JOB, namespace: loadgen }
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
  kubectl -n loadgen wait --for=condition=Ready pod \
    -l job-name=$PRODUCER_JOB --timeout=120s >/dev/null
}

wait_producer_finish() {
  kubectl -n loadgen wait --for=condition=Complete --timeout=$((DURATION_S + 180))s \
    job/$PRODUCER_JOB >/dev/null
}

# ── metrics ────────────────────────────────────────────────────────────

sink_acked_total() {
  local sum=0 reps; reps=$(kubectl -n $NS get sts aeon -o jsonpath='{.status.replicas}')
  for (( n=0; n<reps; n++ )); do
    local v
    v=$(curl_api "http://$(host_for "$n")/metrics" 2>/dev/null \
      | grep -E "^aeon_pipeline_outputs_acked_total\{pipeline=\"${PIPELINE}\"\}" \
      | awk '{print $2}' | head -1)
    v=${v:-0}
    sum=$(( sum + ${v%.*} ))
  done
  echo "$sum"
}

wal_fallback_total() {
  local sum=0 reps; reps=$(kubectl -n $NS get sts aeon -o jsonpath='{.status.replicas}')
  for (( n=0; n<reps; n++ )); do
    local v
    v=$(curl_api "http://$(host_for "$n")/metrics" 2>/dev/null \
      | grep -E "^aeon_checkpoint_fallback_wal_total" \
      | awk '{print $2}' | head -1)
    v=${v:-0}
    sum=$(( sum + ${v%.*} ))
  done
  echo "$sum"
}

wait_sink_drain() {
  local want="$1"
  local prev=-1 stable=0 deadline=$(( $(date +%s) + 300 ))
  while (( $(date +%s) < deadline )); do
    local got; got=$(sink_acked_total)
    log "  sink_acked=$got want=$want"
    if (( got >= want )); then return 0; fi
    if [[ "$got" == "$prev" ]]; then
      stable=$(( stable + 1 ))
      (( stable >= 6 )) && return 1
    else
      stable=0
    fi
    prev=$got
    sleep 5
  done
  return 1
}

# ── chaos events ───────────────────────────────────────────────────────

# Kill the current leader pod and time how long until a new leader is
# elected. Target per plan: < 5 seconds.
event_kill_leader() {
  local vic; vic=$(leader_pod) || { log "could not determine leader; skipping kill"; return 0; }
  log "event: killing leader pod '$vic'"
  row chaos kill_leader_start "$vic"
  local t0; t0=$(date +%s%3N)
  kubectl -n $NS delete pod "$vic" --grace-period=0 --force >/dev/null 2>&1 || true
  # Wait for a fresh leader to exist that isn't "0"
  local new_leader=""
  local deadline=$(( $(date +%s) + 30 ))
  while (( $(date +%s) < deadline )); do
    new_leader=$(leader_pod 2>/dev/null || echo "")
    if [[ -n "$new_leader" && "$new_leader" != "$vic" ]]; then
      break
    fi
    sleep 0.5 2>/dev/null || sleep 1
  done
  local t1; t1=$(date +%s%3N)
  local ms=$(( t1 - t0 ))
  row chaos kill_leader_done "${ms}ms new_leader=${new_leader}"
  # record for verdict
  echo "$ms" > /tmp/t6_failover_ms
}

event_rebalance() {
  log "event: POST /cluster/rebalance"
  row chaos rebalance_start
  local host; host=$(pick_leader_host)
  local t0; t0=$(date +%s%3N)
  local resp
  resp=$(curl_api -X POST "http://${host}/api/v1/cluster/rebalance" 2>/dev/null || echo '{}')
  local t1; t1=$(date +%s%3N)
  row chaos rebalance_done "$(( t1 - t0 ))ms"
  log "rebalance response: $(echo "$resp" | jq -c . 2>/dev/null | head -c 200 || echo "$resp")"
}

event_partition() {
  log "event: apply NetworkChaos (isolate aeon-2)"
  kubectl apply -f "$CHAOS_FILE" >/dev/null
  row chaos partition_applied
}

event_heal() {
  log "event: heal NetworkChaos"
  kubectl delete -f "$CHAOS_FILE" --ignore-not-found >/dev/null
  row chaos partition_healed
}

# ── sequence ──────────────────────────────────────────────────────────

ensure_probe
CUR=$(kubectl -n $NS get sts aeon -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo 0)
if [[ "$CUR" != "3" ]]; then
  echo "error: expected 3 ready Aeon replicas, got $CUR" >&2
  exit 1
fi
if ! kubectl get crd networkchaos.chaos-mesh.org >/dev/null 2>&1; then
  echo "error: Chaos Mesh not installed" >&2
  exit 1
fi

row setup begin "rate=$RATE duration=${DURATION_S}s"

# Re-use the T2 JSON but give the pipeline a T6-specific name by
# patching in-flight (configmap-based create with renamed payload).
log "creating pipeline '$PIPELINE'"
PATCHED=$(jq --arg n "$PIPELINE" '.name=$n' "$JSON_FILE")
LEADER_HOST=$(pick_leader_host)
log "  leader host: $LEADER_HOST"
curl_api -X POST -H "Content-Type:application/json" \
  -d "$PATCHED" \
  "http://${LEADER_HOST}/api/v1/pipelines" | tail -1
curl_api -X POST "http://${LEADER_HOST}/api/v1/pipelines/${PIPELINE}/start" | tail -1
row pipeline started

start_producer_job
row producer started

T_START=$(date +%s)

# Schedule events by sleeping forward to each mark.
sleep_until() {
  local target=$(( T_START + $1 ))
  local now=$(date +%s)
  local delta=$(( target - now ))
  if (( delta > 0 )); then sleep "$delta"; fi
}

sleep_until 120;  event_kill_leader
wait_for_leader || log "warning: no leader in 30s after kill"
sleep_until 240;  event_rebalance
sleep_until 360;  event_partition
sleep_until 480;  event_heal

# Producer should keep running until min 10 on its own rate pacing.
wait_producer_finish
row producer finished
wait_sink_drain "$TOTAL" || true
row sink drained

# ── verdict ────────────────────────────────────────────────────────────

ACKED=$(sink_acked_total)
WAL=$(wal_fallback_total)
LOSS=$(( TOTAL - ACKED ))
FAILOVER_MS=$(cat /tmp/t6_failover_ms 2>/dev/null || echo "n/a")

echo
echo "=== T6 verdict ==="
printf "%-28s %s\n" "producer target (TOTAL)"       "$TOTAL"
printf "%-28s %s\n" "sink acked total"              "$ACKED"
printf "%-28s %s\n" "event loss"                    "$LOSS"
printf "%-28s %s ms  (target < 5000)\n" "leader failover latency"         "$FAILOVER_MS"
printf "%-28s %s  (must be 0)\n" "WAL fallback total"          "$WAL"

PASS=true
(( LOSS > 0 )) && PASS=false
(( WAL > 0 )) && PASS=false
if [[ "$FAILOVER_MS" != "n/a" ]] && (( FAILOVER_MS > 5000 )); then PASS=false; fi

if [[ "$PASS" = true ]]; then
  echo "T6 PASS: zero loss, no WAL engagement, failover within target."
  exit 0
else
  echo "T6 FAIL: see verdict above." >&2
  exit 3
fi
