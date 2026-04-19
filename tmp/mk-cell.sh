#!/usr/bin/env bash
# Generate a T0 cell JSON from template. Usage:
#   mk-cell.sh <cell-name> <source-type> <sink-type> <durability-mode> [count]
set -euo pipefail
NAME="$1"; SRC="$2"; SNK="$3"; MODE="$4"
COUNT="${5:-1000000}"

# Source config by type
case "$SRC" in
  memory)
    SRC_CONFIG="\"count\": \"$COUNT\", \"payload_size\": \"256\", \"batch_size\": \"1024\""
    SRC_TOPIC_FIELD='"topic": null'
    ;;
  kafka)
    SRC_CONFIG="\"brokers\": \"redpanda.aeon.svc.cluster.local:9092\", \"group_id\": \"aeon-$NAME\", \"auto_offset_reset\": \"earliest\""
    SRC_TOPIC_FIELD='"topic": "aeon-bench-source"'
    ;;
esac

# Sink config by type
case "$SNK" in
  blackhole)
    SNK_CONFIG='{}'
    SNK_TOPIC_FIELD='"topic": null'
    ;;
  kafka)
    SNK_CONFIG='{"brokers": "redpanda.aeon.svc.cluster.local:9092", "acks": "all"}'
    SNK_TOPIC_FIELD='"topic": "aeon-bench-sink"'
    ;;
esac

cat <<JSON
{
  "name": "$NAME",
  "sources": [{"type": "$SRC", $SRC_TOPIC_FIELD, "partitions": [], "config": {$SRC_CONFIG}}],
  "processor": {"name": "__identity", "version": "0.0.0"},
  "sinks": [{"type": "$SNK", $SNK_TOPIC_FIELD, "config": $SNK_CONFIG}],
  "upgrade_strategy": "blue-green",
  "state": "created",
  "created_at": 0,
  "updated_at": 0,
  "assigned_node": null,
  "transport_codec": "msgpack",
  "upgrade_state": null,
  "durability": {
    "mode": "$MODE",
    "checkpoint": {"backend": "state_store"},
    "flush": {}
  }
}
JSON
