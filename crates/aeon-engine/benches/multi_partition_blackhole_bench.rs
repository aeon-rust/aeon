//! Multi-partition scaling benchmark (Run 5).
//!
//! Proves `run_multi_partition()` delivers near-linear throughput scaling across
//! independent partition pipelines. No broker dependency — pure Aeon scaling.
//!
//! Test 1: Blackhole (MemorySource → Passthrough → BlackholeSink) — Aeon ceiling per partition count
//! Test 2: FileSink (MemorySource → Passthrough → FileSink) — durable write scaling
//!
//! Run with: cargo bench -p aeon-engine --bench multi_partition_blackhole_bench
//! In Docker: included in bench-entrypoint.sh

use aeon_connectors::file::{FileSink, FileSinkConfig};
use aeon_connectors::{BlackholeSink, MemorySource};
use aeon_engine::{
    MultiPartitionConfig, PassthroughProcessor, PipelineConfig, PipelineMetrics,
    run_multi_partition,
};
use aeon_types::{Event, PartitionId};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

fn events_per_partition() -> usize {
    std::env::var("AEON_BENCH_EVENTS_PER_PARTITION")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000)
}

fn payload_size() -> usize {
    std::env::var("AEON_BENCH_PAYLOAD_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

fn batch_size() -> usize {
    std::env::var("AEON_BENCH_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024)
}

fn partition_counts() -> Vec<usize> {
    std::env::var("AEON_BENCH_PARTITION_COUNTS")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![1, 2, 4, 8])
}

fn make_events(count: usize, psize: usize) -> Vec<Event> {
    let source: Arc<str> = Arc::from("bench-source");
    let payload = Bytes::from(vec![b'x'; psize]);
    (0..count)
        .map(|i| {
            Event::new(
                uuid::Uuid::nil(),
                i as i64,
                Arc::clone(&source),
                PartitionId::new(0),
                payload.clone(),
            )
        })
        .collect()
}

fn bench_blackhole(
    rt: &tokio::runtime::Runtime,
    partition_count: usize,
    events: &[Event],
    bsize: usize,
) -> (u64, std::time::Duration) {
    let t = Instant::now();
    let events_clone = events.to_vec();
    let metrics = rt.block_on(async {
        let config = MultiPartitionConfig {
            partition_count,
            pipeline: PipelineConfig {
                max_batch_size: bsize,
                ..Default::default()
            },
            gate_registry: None,
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_multi_partition::<MemorySource, PassthroughProcessor, BlackholeSink, _, _, _>(
            config,
            Arc::clone(&metrics),
            shutdown,
            move |_i| MemorySource::new(events_clone.clone(), bsize),
            |_i| PassthroughProcessor::new(Arc::from("output")),
            |_i| BlackholeSink::new(),
            None,
        )
        .await
        .expect("multi-partition blackhole run");

        metrics
    });
    let elapsed = t.elapsed();
    let received = metrics.events_received.load(Ordering::Relaxed);
    (received, elapsed)
}

fn bench_filesink(
    rt: &tokio::runtime::Runtime,
    partition_count: usize,
    events: &[Event],
    bsize: usize,
    tmp_dir: &std::path::Path,
) -> (u64, std::time::Duration) {
    let t = Instant::now();
    let events_clone = events.to_vec();
    let dir = tmp_dir.to_path_buf();
    let metrics = rt.block_on(async {
        let config = MultiPartitionConfig {
            partition_count,
            pipeline: PipelineConfig {
                max_batch_size: bsize,
                ..Default::default()
            },
            gate_registry: None,
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        run_multi_partition::<MemorySource, PassthroughProcessor, FileSink, _, _, _>(
            config,
            Arc::clone(&metrics),
            shutdown,
            move |_i| MemorySource::new(events_clone.clone(), bsize),
            |_i| PassthroughProcessor::new(Arc::from("output")),
            {
                let dir = dir.clone();
                move |i| {
                    let path = dir.join(format!("aeon-bench-p{i}.out"));
                    FileSink::new(FileSinkConfig::new(path))
                }
            },
            None,
        )
        .await
        .expect("multi-partition filesink run");

        metrics
    });
    let elapsed = t.elapsed();
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);
    (sent, elapsed)
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let epp = events_per_partition();
    let psize = payload_size();
    let bsize = batch_size();
    let partitions = partition_counts();

    println!("=== Multi-Partition Scaling Benchmark (Run 5) ===");
    println!("Events/partition: {epp}, Payload: {psize}B, Batch: {bsize}");
    println!("Partition sweep: {:?}\n", partitions);

    // ── Test 1: Blackhole (pure Aeon parallel scaling) ──
    println!("--- Test 1: Multi-Partition Blackhole ---\n");
    let events = make_events(epp, psize);
    let mut blackhole_results: Vec<(usize, u64, f64)> = Vec::new();

    for &pc in &partitions {
        let (received, elapsed) = bench_blackhole(&rt, pc, &events, bsize);
        let throughput = received as f64 / elapsed.as_secs_f64();
        println!(
            "  {pc} partition(s): {received} events in {elapsed:.2?} — {throughput:.0} events/sec"
        );
        blackhole_results.push((pc, received, throughput));
    }

    println!("\n  Scaling analysis (blackhole):");
    println!("  Partitions  Throughput       Ratio vs 1p");
    println!("  ----------  ----------       -----------");
    let base_throughput = blackhole_results[0].2;
    for &(pc, _, throughput) in &blackhole_results {
        let ratio = throughput / base_throughput;
        println!("  {pc:>10}  {throughput:>10.0}/sec   {ratio:.2}x");
    }

    // ── Test 2: FileSink (durable write scaling) ──
    println!("\n--- Test 2: Multi-Partition FileSink ---\n");
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let mut filesink_results: Vec<(usize, u64, f64)> = Vec::new();

    for &pc in &partitions {
        let (sent, elapsed) = bench_filesink(&rt, pc, &events, bsize, tmp_dir.path());
        let throughput = sent as f64 / elapsed.as_secs_f64();
        println!(
            "  {pc} partition(s): {sent} outputs in {elapsed:.2?} — {throughput:.0} events/sec"
        );
        filesink_results.push((pc, sent, throughput));
    }

    println!("\n  Scaling analysis (FileSink):");
    println!("  Partitions  Throughput       Ratio vs 1p");
    println!("  ----------  ----------       -----------");
    let base_file = filesink_results[0].2;
    for &(pc, _, throughput) in &filesink_results {
        let ratio = throughput / base_file;
        println!("  {pc:>10}  {throughput:>10.0}/sec   {ratio:.2}x");
    }

    // ── Blackhole vs FileSink comparison ──
    println!("\n--- Blackhole vs FileSink Gap ---\n");
    for (bh, fs) in blackhole_results.iter().zip(filesink_results.iter()) {
        let gap_pct = (fs.2 / bh.2) * 100.0;
        println!(
            "  {}p: blackhole {:.0}/s, filesink {:.0}/s — filesink is {gap_pct:.1}% of blackhole",
            bh.0, bh.2, fs.2
        );
    }

    // ── Acceptance criteria ──
    println!("\n=== Acceptance Criteria ===\n");
    let mut pass = true;

    // Check blackhole scaling targets
    for &(pc, received, throughput) in &blackhole_results {
        let ratio = throughput / base_throughput;
        let (target_ratio, label) = match pc {
            2 => (1.8, "2p >= 1.8x"),
            4 => (3.5, "4p >= 3.5x"),
            8 => (6.0, "8p >= 6.0x"),
            _ => continue,
        };
        let status = if ratio >= target_ratio {
            "PASS"
        } else {
            pass = false;
            "FAIL"
        };
        println!("  Blackhole {label}: {ratio:.2}x — {status}");

        // Zero event loss check
        let expected = epp as u64 * pc as u64;
        if received != expected {
            println!(
                "  WARNING: Expected {expected} events, got {received} ({}p)",
                pc
            );
            pass = false;
        }
    }

    // Check FileSink scaling targets
    for &(pc, _, throughput) in &filesink_results {
        let ratio = throughput / base_file;
        let (target_ratio, label) = match pc {
            2 => (1.7, "2p >= 1.7x"),
            4 => (3.0, "4p >= 3.0x"),
            8 => (5.0, "8p >= 5.0x"),
            _ => continue,
        };
        let status = if ratio >= target_ratio {
            "PASS"
        } else {
            pass = false;
            "FAIL"
        };
        println!("  FileSink {label}: {ratio:.2}x — {status}");
    }

    // Check blackhole vs FileSink gap at 4p
    if let (Some(bh4), Some(fs4)) = (
        blackhole_results.iter().find(|r| r.0 == 4),
        filesink_results.iter().find(|r| r.0 == 4),
    ) {
        let gap = fs4.2 / bh4.2;
        let status = if gap > 0.50 {
            "PASS"
        } else {
            pass = false;
            "FAIL"
        };
        println!(
            "  FileSink/Blackhole gap (4p) > 50%: {:.1}% — {status}",
            gap * 100.0
        );
    }

    println!();
    if pass {
        println!("=== ALL ACCEPTANCE CRITERIA PASSED ===");
    } else {
        println!("=== SOME CRITERIA DID NOT PASS — see above ===");
    }
    println!();
}
