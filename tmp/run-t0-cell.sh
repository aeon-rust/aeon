#!/usr/bin/env bash
# Run one T0 cell and print: events/sec, elapsed_seconds, events, failed
# Usage: run-t0-cell.sh <cell-name> <pipeline.json> [timeout_s]
#
# Measures per-node throughput from /metrics on the leader pod via the
# active port-forward on 127.0.0.1:4471. The cell is considered complete
# when `events_processed == events_received` AND the received count
# stops advancing for 3 consecutive samples (memory source has drained).

set -euo pipefail

CELL="$1"
JSON="$2"
TIMEOUT_S="${3:-60}"
HOST="http://127.0.0.1:4471"

scrape() {
    local metric="$1"
    curl -s "$HOST/metrics" | grep -E "^aeon_pipeline_${metric}\{pipeline=\"$CELL\"\}" | awk '{print $2}' | head -1
}

curl -s -X POST "$HOST/api/v1/pipelines" \
    -H "Content-Type: application/json" \
    -d @"$JSON" > /dev/null

START_NS=$(date +%s%N)
curl -s -X POST "$HOST/api/v1/pipelines/$CELL/start" > /dev/null

LAST_RECEIVED=0
STABLE_COUNT=0
DEADLINE=$((START_NS + TIMEOUT_S * 1000000000))

while [ "$(date +%s%N)" -lt "$DEADLINE" ]; do
    sleep 0.2
    RECEIVED=$(scrape events_received_total || echo 0)
    PROCESSED=$(scrape events_processed_total || echo 0)
    OUTPUTS=$(scrape outputs_sent_total || echo 0)
    FAILED=$(scrape events_failed_total || echo 0)
    RECEIVED=${RECEIVED:-0}
    PROCESSED=${PROCESSED:-0}
    OUTPUTS=${OUTPUTS:-0}
    FAILED=${FAILED:-0}

    if [ "$RECEIVED" = "$LAST_RECEIVED" ] && [ "$RECEIVED" -gt 0 ]; then
        STABLE_COUNT=$((STABLE_COUNT + 1))
    else
        STABLE_COUNT=0
    fi
    LAST_RECEIVED="$RECEIVED"

    if [ "$STABLE_COUNT" -ge 3 ] && [ "$PROCESSED" = "$RECEIVED" ] && [ "$OUTPUTS" = "$RECEIVED" ]; then
        break
    fi
done

END_NS=$(date +%s%N)
ELAPSED_NS=$((END_NS - START_NS))
ELAPSED_MS=$((ELAPSED_NS / 1000000))
# Subtract 600ms of stable-check sleep time from elapsed for a slightly
# less pessimistic rate — that's 3 × 0.2s sleeps after drain. Floor at 1ms.
ADJ_MS=$((ELAPSED_MS - 600))
if [ "$ADJ_MS" -lt 1 ]; then ADJ_MS=1; fi
RATE=$((RECEIVED * 1000 / ADJ_MS))

curl -s -X POST "$HOST/api/v1/pipelines/$CELL/stop" > /dev/null
sleep 0.5
curl -s -X DELETE "$HOST/api/v1/pipelines/$CELL" > /dev/null

echo "CELL=$CELL events=$RECEIVED processed=$PROCESSED outputs=$OUTPUTS failed=$FAILED elapsed_ms=$ELAPSED_MS adjusted_ms=$ADJ_MS rate=${RATE} ev/s"
