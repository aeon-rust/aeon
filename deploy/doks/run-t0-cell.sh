#!/usr/bin/env bash
# Run one T0 cell: create pipeline, start, poll metrics, compute throughput.
#
# Usage: run-t0-cell.sh <pipeline-name> <json-file> <target-count>
# Emits TSV row: cell, duration_s, aggregate_events, aggregate_eps, per_node_eps_max

set -euo pipefail

CELL_NAME="${1:?cell name required}"
JSON_FILE="${2:?json file required}"
TARGET="${3:?target events per node required}"

LEADER="${LEADER:-aeon-1.aeon-headless.aeon.svc.cluster.local:4471}"
POLL_POD="poll-$RANDOM"
NS=aeon

metric_sample() {
    # Sum events_out_total across all 3 pods.
    local sum=0
    for n in 0 1 2; do
        local v=$(kubectl -n $NS run mq$n-$RANDOM --rm -i --restart=Never --image=curlimages/curl --command -- \
            curl -sS --max-time 5 http://aeon-$n.aeon-headless.aeon.svc.cluster.local:4471/metrics 2>/dev/null \
            | grep -E "^aeon_pipeline_outputs_sent_total\{pipeline=\"$CELL_NAME\"\}" \
            | awk '{print $2}')
        v=${v:-0}
        sum=$((sum + ${v%.*}))
    done
    echo "$sum"
}

echo "# cell=$CELL_NAME target=$TARGET per node"

# Push configmap
kubectl -n $NS create configmap cell-$CELL_NAME --from-file=$(basename "$JSON_FILE")=$JSON_FILE --dry-run=client -o yaml | kubectl apply -f - >/dev/null

# Create pipeline via curl pod
kubectl -n $NS run create-$CELL_NAME-$RANDOM --rm -i --restart=Never --image=curlimages/curl \
    --overrides='{"spec":{"containers":[{"name":"c","image":"curlimages/curl","command":["sh","-c","curl -sS -X POST -H Content-Type:application/json -d @/data/'"$(basename "$JSON_FILE")"' http://'"$LEADER"'/api/v1/pipelines"],"volumeMounts":[{"name":"d","mountPath":"/data"}]}],"volumes":[{"name":"d","configMap":{"name":"cell-'"$CELL_NAME"'"}}]}}' 2>/dev/null | tail -1

sleep 2

# Capture start timestamp (millis) and start pipeline
t_start=$(date +%s%3N)
kubectl -n $NS run start-$CELL_NAME-$RANDOM --rm -i --restart=Never --image=curlimages/curl \
    --command -- curl -sS -X POST http://$LEADER/api/v1/pipelines/$CELL_NAME/start 2>/dev/null | tail -1

expect_per_node=$TARGET
expect_total=$((TARGET * 3))

# Poll until aggregate == 3 × TARGET, max 300s
deadline=$((t_start + 300000))
last_sum=0
stable_count=0
while true; do
    now=$(date +%s%3N)
    if [ $now -gt $deadline ]; then
        echo "# TIMEOUT waiting for $expect_total events" >&2
        break
    fi
    sum=$(metric_sample)
    echo "# t=$(( (now - t_start) / 1000 ))s sum=$sum"
    if [ "$sum" -ge "$expect_total" ]; then
        t_end=$now
        break
    fi
    sleep 3
done

# On timeout: stamp t_end now and report the observed aggregate so the
# derived eps reflects what actually happened, not the unmet target.
if [ -z "${t_end:-}" ]; then
    t_end=$(date +%s%3N)
    expect_total=$sum
fi

duration_ms=$((t_end - t_start))
duration_s=$(awk "BEGIN { printf \"%.2f\", $duration_ms / 1000 }")
aggregate_eps=$(awk "BEGIN { printf \"%.0f\", $expect_total / ($duration_ms / 1000) }")
per_node_eps=$(awk "BEGIN { printf \"%.0f\", ($expect_total / 3) / ($duration_ms / 1000) }")

printf "%s\t%s\t%d\t%s\t%s\n" "$CELL_NAME" "$duration_s" "$expect_total" "$aggregate_eps" "$per_node_eps"
