#!/usr/bin/env bash
# Sustained rate-sweep producer harness for Gate 2 Session A.
#
# Wraps `aeon-producer` in a rate-sweep loop, captures the machine-readable
# SUMMARY line per run, and emits a TSV for post-run analysis. Pairs with
# the T0 isolation matrix in docs/GATE2-ACCEPTANCE-PLAN.md §5 — one invocation
# per cell (C1/C2 × 4 durability modes).
#
# Usage:
#   scripts/t0-sweep.sh --rates 100000,250000,500000 --duration 60 \
#                       --topic aeon-source --brokers localhost:19092 \
#                       --payload-size 256 --output results.tsv
#
# Env overrides: PRODUCER_BIN (defaults to target/release/aeon-producer).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

PRODUCER_BIN="${PRODUCER_BIN:-$PROJECT_DIR/target/release/aeon-producer}"
RATES=""
DURATION=60
TOPIC="aeon-source"
BROKERS="localhost:19092"
PAYLOAD_SIZE=256
OUTPUT=""
LABEL=""

print_usage() {
    cat <<'EOF'
T0 rate-sweep producer harness

USAGE:
  scripts/t0-sweep.sh [OPTIONS]

OPTIONS:
  --rates <csv>          Comma-separated rates in ev/s, e.g. "100000,250000,500000" (required)
  --duration <seconds>   Sustain each rate for N seconds [default: 60]
  --topic <name>         Target topic [default: aeon-source]
  --brokers <addrs>      Kafka/Redpanda brokers [default: localhost:19092]
  --payload-size <n>     Payload size in bytes [default: 256]
  --output <file>        Append TSV rows to file (also echoed to stdout)
  --label <tag>          Free-form label stamped on every row (e.g. cell ID)
  -h, --help             Print this help

The producer's SUMMARY stdout line is parsed per run. TSV columns:
  label  configured_rate  sent  errors  elapsed_ms  events_per_sec  topic  payload_size
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --rates) RATES="$2"; shift 2 ;;
        --duration) DURATION="$2"; shift 2 ;;
        --topic) TOPIC="$2"; shift 2 ;;
        --brokers) BROKERS="$2"; shift 2 ;;
        --payload-size) PAYLOAD_SIZE="$2"; shift 2 ;;
        --output) OUTPUT="$2"; shift 2 ;;
        --label) LABEL="$2"; shift 2 ;;
        -h|--help) print_usage; exit 0 ;;
        *) echo "Unknown arg: $1" >&2; print_usage >&2; exit 1 ;;
    esac
done

if [ -z "$RATES" ]; then
    echo "error: --rates is required" >&2
    print_usage >&2
    exit 1
fi

if [ ! -x "$PRODUCER_BIN" ]; then
    echo "error: producer binary not found or not executable: $PRODUCER_BIN" >&2
    echo "hint: cargo build --release -p aeon-e2e-pipeline --bin aeon-producer" >&2
    exit 1
fi

# TSV header
HEADER=$'label\tconfigured_rate\tsent\terrors\telapsed_ms\tevents_per_sec\ttopic\tpayload_size'
if [ -n "$OUTPUT" ] && [ ! -s "$OUTPUT" ]; then
    echo "$HEADER" > "$OUTPUT"
fi
echo "$HEADER"

# Split rates on comma, iterate
IFS=',' read -ra RATE_ARRAY <<< "$RATES"
for rate in "${RATE_ARRAY[@]}"; do
    rate="$(echo "$rate" | tr -d ' ')"
    count=$((rate * DURATION))
    echo "# sweep: rate=$rate ev/s, duration=${DURATION}s, count=$count" >&2

    # Capture stdout (SUMMARY line) while letting stderr pass through so the
    # operator sees tracing progress. grep isolates the machine line.
    summary=$("$PRODUCER_BIN" \
        --brokers "$BROKERS" \
        --topic "$TOPIC" \
        --count "$count" \
        --rate "$rate" \
        --payload-size "$PAYLOAD_SIZE" \
        | grep -E '^SUMMARY' || true)

    if [ -z "$summary" ]; then
        echo "error: producer did not emit SUMMARY line for rate=$rate" >&2
        continue
    fi

    # SUMMARY\tsent=X\terrors=Y\telapsed_ms=Z\tevents_per_sec=W\tconfigured_rate=R\ttopic=T\tpayload_size=P
    sent=$(echo "$summary" | awk -F'\t' '{for (i=1;i<=NF;i++) if ($i ~ /^sent=/) { sub("sent=","",$i); print $i }}')
    errors=$(echo "$summary" | awk -F'\t' '{for (i=1;i<=NF;i++) if ($i ~ /^errors=/) { sub("errors=","",$i); print $i }}')
    elapsed_ms=$(echo "$summary" | awk -F'\t' '{for (i=1;i<=NF;i++) if ($i ~ /^elapsed_ms=/) { sub("elapsed_ms=","",$i); print $i }}')
    actual=$(echo "$summary" | awk -F'\t' '{for (i=1;i<=NF;i++) if ($i ~ /^events_per_sec=/) { sub("events_per_sec=","",$i); print $i }}')

    row=$(printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s' \
        "$LABEL" "$rate" "$sent" "$errors" "$elapsed_ms" "$actual" "$TOPIC" "$PAYLOAD_SIZE")
    echo "$row"
    if [ -n "$OUTPUT" ]; then
        echo "$row" >> "$OUTPUT"
    fi
done
