#!/usr/bin/env bash
# T0.C1 cell runner: Kafka source consumes entire topic (30M total across 3 nodes).
# Emits TSV: cell, duration_s, aggregate_events, aggregate_eps, per_node_eps_max

set -euo pipefail

CELL_NAME="${1:?cell name required}"
JSON_FILE="${2:?json file required}"
EXPECT_TOTAL="${3:?expected aggregate events required}"

LEADER="${LEADER:-aeon-1.aeon-headless.aeon.svc.cluster.local:4471}"
NS=aeon

metric_sample() {
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

echo "# cell=$CELL_NAME expect=$EXPECT_TOTAL aggregate"

kubectl -n $NS create configmap cell-$CELL_NAME --from-file=$(basename "$JSON_FILE")=$JSON_FILE --dry-run=client -o yaml | kubectl apply -f - >/dev/null

kubectl -n $NS run create-$CELL_NAME-$RANDOM --rm -i --restart=Never --image=curlimages/curl \
    --overrides='{"spec":{"containers":[{"name":"c","image":"curlimages/curl","command":["sh","-c","curl -sS -X POST -H Content-Type:application/json -d @/data/'"$(basename "$JSON_FILE")"' http://'"$LEADER"'/api/v1/pipelines"],"volumeMounts":[{"name":"d","mountPath":"/data"}]}],"volumes":[{"name":"d","configMap":{"name":"cell-'"$CELL_NAME"'"}}]}}' 2>/dev/null | tail -1

sleep 2

t_start=$(date +%s%3N)
kubectl -n $NS run start-$CELL_NAME-$RANDOM --rm -i --restart=Never --image=curlimages/curl \
    --command -- curl -sS -X POST http://$LEADER/api/v1/pipelines/$CELL_NAME/start 2>/dev/null | tail -1

deadline=$((t_start + ${CELL_TIMEOUT_MS:-300000}))
while true; do
    now=$(date +%s%3N)
    if [ $now -gt $deadline ]; then
        echo "# TIMEOUT waiting for $EXPECT_TOTAL events" >&2
        break
    fi
    sum=$(metric_sample)
    echo "# t=$(( (now - t_start) / 1000 ))s sum=$sum"
    if [ "$sum" -ge "$EXPECT_TOTAL" ]; then
        t_end=$now
        break
    fi
    sleep 3
done

# If we hit the deadline before reaching EXPECT_TOTAL, use the last observed
# wallclock as t_end and substitute the observed aggregate for EXPECT_TOTAL so
# the derived eps reflects reality.
if [ -z "${t_end:-}" ]; then
    t_end=$(date +%s%3N)
    EXPECT_TOTAL=$sum
fi

duration_ms=$((t_end - t_start))
duration_s=$(awk "BEGIN { printf \"%.2f\", $duration_ms / 1000 }")
aggregate_eps=$(awk "BEGIN { printf \"%.0f\", $EXPECT_TOTAL / ($duration_ms / 1000) }")
per_node_eps=$(awk "BEGIN { printf \"%.0f\", ($EXPECT_TOTAL / 3) / ($duration_ms / 1000) }")

printf "%s\t%s\t%d\t%s\t%s\n" "$CELL_NAME" "$duration_s" "$EXPECT_TOTAL" "$aggregate_eps" "$per_node_eps"
