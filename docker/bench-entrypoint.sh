#!/bin/bash
set -e

BENCH_RUN=${AEON_BENCH_RUN_NUMBER:-5}
SAMPLE_SIZE=${AEON_BENCH_SAMPLE_SIZE:-10}
WARMUP_TIME=${AEON_BENCH_WARMUP_TIME:-1}
MEASUREMENT_TIME=${AEON_BENCH_MEASUREMENT_TIME:-2}

echo "=============================================="
echo " Aeon In-Docker Benchmark Suite (Run ${BENCH_RUN})"
echo " Broker: ${AEON_BENCH_BROKERS:-redpanda:9092}"
echo " Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo " Criterion: sample=${SAMPLE_SIZE} warmup=${WARMUP_TIME}s measure=${MEASUREMENT_TIME}s"
echo "=============================================="
echo ""

# Find executables (cargo appends hash, skip .d dep files)
find_bench() {
    local name=$1
    ls /bench/bins/${name}-* 2>/dev/null | grep -v '\.d$' | head -1
}

BLACKHOLE=$(find_bench blackhole_bench)
REDPANDA=$(find_bench redpanda_bench)
COMPONENTS=$(find_bench pipeline_components_bench)
PARTITION=$(find_bench partition_scaling_bench)
MULTI_PART=$(find_bench multi_partition_blackhole_bench)
E2E_DELIVERY=$(find_bench e2e_delivery_bench)
MULTI_RT=$(find_bench multi_runtime)

# ── 1. Blackhole Pipeline ─���─────────────────────────────────────���──
if [ -n "$BLACKHOLE" ]; then
    echo "=== [1/7] Blackhole Pipeline (Aeon internal ceiling) ==="
    echo ""
    "$BLACKHOLE" --bench --sample-size "$SAMPLE_SIZE" --warm-up-time "$WARMUP_TIME" --measurement-time "$MEASUREMENT_TIME" 2>&1 || echo "  (blackhole bench exited with $?)"
    echo ""
else
    echo "SKIP: blackhole_bench not found"
fi

# ── 2. Pipeline Components ─────────────────────────────────────────
if [ -n "$COMPONENTS" ]; then
    echo "=== [2/7] Pipeline Components (SPSC, batch, DAG chain) ==="
    echo ""
    "$COMPONENTS" --bench --sample-size "$SAMPLE_SIZE" --warm-up-time "$WARMUP_TIME" --measurement-time "$MEASUREMENT_TIME" 2>&1 || echo "  (pipeline_components bench exited with $?)"
    echo ""
else
    echo "SKIP: pipeline_components_bench not found"
fi

# ── 3. Redpanda E2E ────────────────────────────────────────────────
if [ -n "$REDPANDA" ]; then
    echo "=== [3/7] Redpanda E2E (same Docker network) ==="
    echo ""
    "$REDPANDA" 2>&1 || echo "  (redpanda bench exited with $?)"
    echo ""
else
    echo "SKIP: redpanda_bench not found"
fi

# ── 4. Partition Scaling ───────────────────────────────────────────
if [ -n "$PARTITION" ]; then
    echo "=== [4/7] Partition Scaling (4/8/16 partitions) ==="
    echo ""
    "$PARTITION" 2>&1 || echo "  (partition_scaling bench exited with $?)"
    echo ""
else
    echo "SKIP: partition_scaling_bench not found"
fi

# ── 5. Multi-Partition Blackhole + FileSink ────────────────────────
if [ -n "$MULTI_PART" ]; then
    echo "=== [5/7] Multi-Partition Scaling (blackhole + filesink) ==="
    echo ""
    "$MULTI_PART" 2>&1 || echo "  (multi_partition_blackhole bench exited with $?)"
    echo ""
else
    echo "SKIP: multi_partition_blackhole_bench not found"
fi

# ── 6. E2E Delivery Mode Benchmark ─────────────────────────────────
if [ -n "$E2E_DELIVERY" ]; then
    echo "=== [6/7] E2E Delivery Mode (Ordered vs Batched) ==="
    echo ""
    "$E2E_DELIVERY" 2>&1 || echo "  (e2e_delivery bench exited with $?)"
    echo ""
else
    echo "SKIP: e2e_delivery_bench not found"
fi

# ── 7. Multi-Runtime Processors ────────────────────────────────────
if [ -n "$MULTI_RT" ]; then
    echo "=== [7/7] Multi-Runtime Processors (native vs Wasm) ==="
    echo ""
    "$MULTI_RT" --bench --sample-size "$SAMPLE_SIZE" --warm-up-time "$WARMUP_TIME" --measurement-time "$MEASUREMENT_TIME" 2>&1 || echo "  (multi_runtime bench exited with $?)"
    echo ""
else
    echo "SKIP: multi_runtime not found"
fi

echo "=============================================="
echo " Benchmark suite complete"
echo "=============================================="
