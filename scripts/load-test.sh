#!/usr/bin/env bash
# Aeon Load Test Suite
# Runs the full test matrix and collects results.
#
# Modes:
#   ./scripts/load-test.sh local      — Run directly (no K8s), requires Redpanda on localhost:19092
#   ./scripts/load-test.sh k8s        — Run on Rancher Desktop K8s cluster
#   ./scripts/load-test.sh blackhole  — Blackhole ceiling only (no Redpanda needed)
#
# Prerequisites:
#   local:     docker compose up -d redpanda redpanda-init
#   k8s:       kubectl apply -f deploy/k8s/namespace.yaml -f deploy/k8s/redpanda.yaml
#   blackhole: cargo build --release -p aeon-e2e-pipeline

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
RESULTS_DIR="$PROJECT_DIR/load-test-results"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
RESULTS_FILE="$RESULTS_DIR/results-$TIMESTAMP.txt"

RUST_WASM="$PROJECT_DIR/samples/processors/wasm-bins/rust-processor.wasm"
AS_WASM="$PROJECT_DIR/samples/processors/wasm-bins/assemblyscript-processor.wasm"
PIPELINE_BIN="$PROJECT_DIR/target/release/aeon-pipeline"
PRODUCER_BIN="$PROJECT_DIR/target/release/aeon-producer"

BROKERS="${BROKERS:-localhost:19092}"
SOURCE_TOPIC="${SOURCE_TOPIC:-aeon-bench-source}"
SINK_TOPIC="${SINK_TOPIC:-aeon-bench-sink}"
EVENT_COUNT="${EVENT_COUNT:-500000}"
PAYLOAD_SIZE="${PAYLOAD_SIZE:-128}"
DURATION="${DURATION:-30}"
BATCH_SIZE="${BATCH_SIZE:-1024}"

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

mkdir -p "$RESULTS_DIR"

log() { echo -e "${CYAN}[$(date +%H:%M:%S)]${NC} $*"; }
header() { echo -e "\n${GREEN}═══════════════════════════════════════════════════════${NC}"; echo -e "${GREEN} $*${NC}"; echo -e "${GREEN}═══════════════════════════════════════════════════════${NC}\n"; }

build_binaries() {
    log "Building release binaries..."
    cd "$PROJECT_DIR"
    cargo build --release -p aeon-e2e-pipeline 2>&1 | tail -3
    log "Building Rust-Wasm processor..."
    (cd samples/processors/rust-wasm && cargo build --release --target wasm32-unknown-unknown 2>&1 | tail -1)
    mkdir -p samples/processors/wasm-bins
    cp samples/processors/rust-wasm/target/wasm32-unknown-unknown/release/aeon_sample_rust_wasm.wasm "$RUST_WASM"
    if [ -f samples/processors/assemblyscript-wasm/build/processor.wasm ]; then
        cp samples/processors/assemblyscript-wasm/build/processor.wasm "$AS_WASM"
    fi
    log "Binaries ready."
}

wait_for_redpanda() {
    log "Waiting for Redpanda at $BROKERS..."
    local retries=30
    while [ $retries -gt 0 ]; do
        if "$PRODUCER_BIN" --brokers "$BROKERS" --topic "$SOURCE_TOPIC" --count 1 --rate 0 2>/dev/null; then
            log "Redpanda is ready."
            return 0
        fi
        retries=$((retries - 1))
        sleep 2
    done
    echo "ERROR: Redpanda not reachable at $BROKERS"
    exit 1
}

produce_events() {
    local count="${1:-$EVENT_COUNT}"
    log "Producing $count events to $SOURCE_TOPIC..."
    "$PRODUCER_BIN" \
        --brokers "$BROKERS" \
        --topic "$SOURCE_TOPIC" \
        --count "$count" \
        --rate 0 \
        --payload-size "$PAYLOAD_SIZE" \
        2>&1 | tail -3
}

run_test() {
    local test_name="$1"
    shift
    header "$test_name"
    log "Running: aeon-pipeline $*"
    local start=$(date +%s%N)

    # Run pipeline with timeout
    timeout "${DURATION}s" "$PIPELINE_BIN" "$@" 2>&1 | tee -a "$RESULTS_FILE" || true

    local end=$(date +%s%N)
    local elapsed_ms=$(( (end - start) / 1000000 ))
    echo "" >> "$RESULTS_FILE"
    echo "--- $test_name: ${elapsed_ms}ms ---" >> "$RESULTS_FILE"
    echo "" >> "$RESULTS_FILE"
    log "Test '$test_name' completed in ${elapsed_ms}ms"
}

# ─── Test: Blackhole Ceiling ─────────────────────────────────────────
test_blackhole_ceiling() {
    local counts=(100000 500000 1000000 5000000)
    for count in "${counts[@]}"; do
        run_test "Blackhole Ceiling ($count events)" \
            --source memory \
            --processor native \
            --sink blackhole \
            --count "$count" \
            --batch-size "$BATCH_SIZE" \
            --payload-size "$PAYLOAD_SIZE"
    done
}

# ─── Test: Wasm Blackhole Ceiling ────────────────────────────────────
test_wasm_blackhole() {
    if [ ! -f "$RUST_WASM" ]; then
        log "SKIP: Rust-Wasm processor not found at $RUST_WASM"
        return
    fi
    run_test "Wasm Blackhole (Rust-Wasm, 1M events)" \
        --source memory \
        --processor wasm \
        --wasm "$RUST_WASM" \
        --sink blackhole \
        --count 1000000 \
        --batch-size "$BATCH_SIZE" \
        --payload-size "$PAYLOAD_SIZE"

    if [ -f "$AS_WASM" ]; then
        run_test "Wasm Blackhole (AS-Wasm, 1M events)" \
            --source memory \
            --processor wasm \
            --wasm "$AS_WASM" \
            --sink blackhole \
            --count 1000000 \
            --batch-size "$BATCH_SIZE" \
            --payload-size "$PAYLOAD_SIZE"
    fi
}

# ─── Test: Redpanda E2E ─────────────────────────────────────────────
test_redpanda_native() {
    produce_events "$EVENT_COUNT"
    run_test "Redpanda E2E (Native, ${EVENT_COUNT} events)" \
        --source redpanda \
        --processor native \
        --sink redpanda \
        --brokers "$BROKERS" \
        --source-topic "$SOURCE_TOPIC" \
        --sink-topic "$SINK_TOPIC" \
        --batch-size "$BATCH_SIZE" \
        --duration "$DURATION" \
        --partitions "0"
}

test_redpanda_rust_wasm() {
    if [ ! -f "$RUST_WASM" ]; then
        log "SKIP: Rust-Wasm processor not found"
        return
    fi
    produce_events "$EVENT_COUNT"
    run_test "Redpanda E2E (Rust-Wasm, ${EVENT_COUNT} events)" \
        --source redpanda \
        --processor wasm \
        --wasm "$RUST_WASM" \
        --sink redpanda \
        --brokers "$BROKERS" \
        --source-topic "$SOURCE_TOPIC" \
        --sink-topic "$SINK_TOPIC" \
        --batch-size "$BATCH_SIZE" \
        --duration "$DURATION" \
        --partitions "0"
}

test_redpanda_as_wasm() {
    if [ ! -f "$AS_WASM" ]; then
        log "SKIP: AssemblyScript-Wasm processor not found"
        return
    fi
    produce_events "$EVENT_COUNT"
    run_test "Redpanda E2E (AS-Wasm, ${EVENT_COUNT} events)" \
        --source redpanda \
        --processor wasm \
        --wasm "$AS_WASM" \
        --sink redpanda \
        --brokers "$BROKERS" \
        --source-topic "$SOURCE_TOPIC" \
        --sink-topic "$SINK_TOPIC" \
        --batch-size "$BATCH_SIZE" \
        --duration "$DURATION" \
        --partitions "0"
}

test_redpanda_blackhole() {
    produce_events "$EVENT_COUNT"
    run_test "Redpanda Source Isolation (Native → Blackhole)" \
        --source redpanda \
        --processor native \
        --sink blackhole \
        --brokers "$BROKERS" \
        --source-topic "$SOURCE_TOPIC" \
        --batch-size "$BATCH_SIZE" \
        --duration "$DURATION" \
        --partitions "0"
}

# ─── K8s mode helpers ────────────────────────────────────────────────
k8s_deploy() {
    log "Deploying to Kubernetes..."
    kubectl apply -f "$PROJECT_DIR/deploy/k8s/namespace.yaml"
    kubectl apply -f "$PROJECT_DIR/deploy/k8s/redpanda.yaml"

    log "Waiting for Redpanda pod..."
    kubectl -n aeon wait --for=condition=ready pod -l app=redpanda --timeout=120s

    log "Waiting for topic init job..."
    kubectl -n aeon wait --for=condition=complete job/redpanda-init --timeout=60s 2>/dev/null || true

    kubectl apply -f "$PROJECT_DIR/deploy/k8s/aeon-pipeline.yaml"
    log "K8s resources deployed."
}

k8s_run_test() {
    local processor="$1"
    local replicas="${2:-1}"
    local deployment="aeon-${processor}"

    header "K8s: $deployment (replicas=$replicas)"

    # Scale down all pipeline deployments
    kubectl -n aeon scale deployment/aeon-native --replicas=0 2>/dev/null || true
    kubectl -n aeon scale deployment/aeon-rust-wasm --replicas=0 2>/dev/null || true
    kubectl -n aeon scale deployment/aeon-as-wasm --replicas=0 2>/dev/null || true
    sleep 2

    # Produce events
    log "Producing $EVENT_COUNT events via K8s job..."
    kubectl -n aeon delete job/aeon-producer 2>/dev/null || true
    kubectl apply -f "$PROJECT_DIR/deploy/k8s/aeon-producer.yaml"
    kubectl -n aeon wait --for=condition=complete job/aeon-producer --timeout=120s
    log "Events produced."

    # Scale up target deployment
    kubectl -n aeon scale "deployment/$deployment" --replicas="$replicas"
    kubectl -n aeon rollout status "deployment/$deployment" --timeout=60s

    # Wait for processing
    log "Processing for ${DURATION}s..."
    sleep "$DURATION"

    # Collect logs
    log "Collecting logs..."
    kubectl -n aeon logs -l "app=aeon-pipeline,processor=$processor" --tail=50 2>&1 | tee -a "$RESULTS_FILE"

    # Scale down
    kubectl -n aeon scale "deployment/$deployment" --replicas=0
    echo "" >> "$RESULTS_FILE"
}

# ─── Main ────────────────────────────────────────────────────────────
main() {
    local mode="${1:-local}"

    header "Aeon Load Test Suite — mode: $mode"
    echo "Results: $RESULTS_FILE"
    echo "Timestamp: $(date)" | tee "$RESULTS_FILE"
    echo "Mode: $mode" | tee -a "$RESULTS_FILE"
    echo "Event count: $EVENT_COUNT" | tee -a "$RESULTS_FILE"
    echo "Payload size: $PAYLOAD_SIZE bytes" | tee -a "$RESULTS_FILE"
    echo "Batch size: $BATCH_SIZE" | tee -a "$RESULTS_FILE"
    echo "" >> "$RESULTS_FILE"

    case "$mode" in
        blackhole)
            build_binaries
            test_blackhole_ceiling
            test_wasm_blackhole
            ;;

        local)
            build_binaries
            wait_for_redpanda

            # CPU ceiling tests (no Redpanda needed)
            test_blackhole_ceiling
            test_wasm_blackhole

            # Redpanda E2E tests
            test_redpanda_native
            test_redpanda_rust_wasm
            test_redpanda_as_wasm
            test_redpanda_blackhole
            ;;

        k8s)
            build_binaries
            log "Building Docker image..."
            docker build -t aeon:latest "$PROJECT_DIR"

            k8s_deploy

            # Single-node tests
            k8s_run_test "native" 1
            k8s_run_test "rust-wasm" 1
            k8s_run_test "as-wasm" 1

            # Multi-replica tests (partition distribution)
            k8s_run_test "native" 3

            log "K8s tests complete."
            ;;

        *)
            echo "Usage: $0 {local|k8s|blackhole}"
            echo ""
            echo "  blackhole  — CPU ceiling only (no infrastructure needed)"
            echo "  local      — Full suite against localhost Redpanda"
            echo "  k8s        — Deploy and test on Rancher Desktop K8s"
            exit 1
            ;;
    esac

    header "Results Summary"
    cat "$RESULTS_FILE"
    echo ""
    log "Full results saved to: $RESULTS_FILE"
}

main "$@"
