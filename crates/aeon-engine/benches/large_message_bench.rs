//! Large Message Benchmark — measures pipeline overhead across payload sizes.
//!
//! Sweeps 256B, 1KB, 10KB, 100KB, 1MB payloads through a MemorySource →
//! PassthroughProcessor → BlackholeSink pipeline to measure how per-event
//! overhead and throughput scale with message size.
//!
//! Key measurements per payload size:
//! - Throughput (events/sec and MB/sec)
//! - Per-event overhead (ns)
//! - Zero event loss verification
//!
//! Environment variables:
//! - AEON_BENCH_EVENT_COUNT: Events per size tier (default: 50000)
//! - AEON_BENCH_BATCH_SIZE: Max batch size (default: 512)
//!
//! Run with: cargo bench -p aeon-engine --bench large_message_bench

use aeon_connectors::BlackholeSink;
use aeon_connectors::MemorySource;
use aeon_engine::delivery::{CheckpointBackend, CheckpointConfig, DeliveryConfig, FlushStrategy};
use aeon_engine::{PassthroughProcessor, PipelineConfig, PipelineMetrics, run_buffered};
use aeon_types::{DeliveryStrategy, Event, PartitionId};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

fn event_count() -> usize {
    std::env::var("AEON_BENCH_EVENT_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000)
}

fn batch_size() -> usize {
    std::env::var("AEON_BENCH_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512)
}

/// Payload sizes to benchmark.
const PAYLOAD_SIZES: &[(usize, &str)] = &[
    (256, "256B"),
    (1_024, "1KB"),
    (10_240, "10KB"),
    (102_400, "100KB"),
    (1_048_576, "1MB"),
];

fn make_events(count: usize, payload_size: usize) -> Vec<Event> {
    let payload = Bytes::from(vec![b'P'; payload_size]);
    let source_name: Arc<str> = Arc::from("bench");
    (0..count)
        .map(|i| {
            Event::new(
                uuid::Uuid::now_v7(),
                i as i64,
                Arc::clone(&source_name),
                PartitionId::new(0),
                payload.clone(),
            )
        })
        .collect()
}

fn make_pipeline_config() -> PipelineConfig {
    PipelineConfig {
        source_buffer_capacity: 4096,
        sink_buffer_capacity: 4096,
        max_batch_size: batch_size(),
        delivery: DeliveryConfig {
            strategy: DeliveryStrategy::UnorderedBatch,
            flush: FlushStrategy {
                interval: std::time::Duration::from_millis(50),
                max_pending: 100_000,
                adaptive: false,
                ..Default::default()
            },
            checkpoint: CheckpointConfig {
                backend: CheckpointBackend::None,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

struct SizeResult {
    label: &'static str,
    payload_size: usize,
    events: u64,
    sent: u64,
    elapsed: std::time::Duration,
}

impl SizeResult {
    fn events_per_sec(&self) -> f64 {
        self.events as f64 / self.elapsed.as_secs_f64()
    }

    fn mb_per_sec(&self) -> f64 {
        let total_bytes = self.events as f64 * self.payload_size as f64;
        total_bytes / (1024.0 * 1024.0) / self.elapsed.as_secs_f64()
    }

    fn per_event_ns(&self) -> f64 {
        self.elapsed.as_nanos() as f64 / self.events.max(1) as f64
    }

    fn zero_loss(&self) -> bool {
        self.events == self.sent
    }
}

async fn bench_size(payload_size: usize, label: &'static str) -> SizeResult {
    let count = event_count();
    // Scale down event count for very large payloads to keep runtime reasonable
    let adjusted_count = match payload_size {
        s if s >= 1_048_576 => count / 10, // 1MB: 1/10 events
        s if s >= 102_400 => count / 5,    // 100KB: 1/5 events
        _ => count,
    };

    let events = make_events(adjusted_count, payload_size);
    let source = MemorySource::new(events, batch_size());
    let processor = PassthroughProcessor::new(Arc::from("blackhole"));
    let sink = BlackholeSink::new();
    let config = make_pipeline_config();
    let metrics = Arc::new(PipelineMetrics::new());
    let shutdown = Arc::new(AtomicBool::new(false));

    let t = Instant::now();
    run_buffered(
        source,
        processor,
        sink,
        config,
        Arc::clone(&metrics),
        shutdown,
        None,
    )
    .await
    .expect("pipeline run");
    let elapsed = t.elapsed();

    SizeResult {
        label,
        payload_size,
        events: metrics.events_received.load(Ordering::Relaxed),
        sent: metrics.outputs_sent.load(Ordering::Relaxed),
        elapsed,
    }
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let count = event_count();
    let bsize = batch_size();

    println!("=== Large Message Benchmark ===");
    println!("Base events: {count}, Batch size: {bsize}");
    println!("Pipeline: MemorySource → Passthrough → BlackholeSink (UnorderedBatch)");
    println!();
    println!(
        "  {:>6} | {:>8} | {:>12} | {:>12} | {:>10} | {:>7} | {:>8}",
        "Size", "Events", "Events/sec", "MB/sec", "ns/event", "Loss", "Time"
    );
    println!(
        "  {:>6} | {:>8} | {:>12} | {:>12} | {:>10} | {:>7} | {:>8}",
        "──────", "────────", "────────────", "────────────", "──────────", "───────", "────────"
    );

    let mut results = Vec::new();

    for &(size, label) in PAYLOAD_SIZES {
        let r = rt.block_on(bench_size(size, label));
        let loss = if r.zero_loss() { "0" } else { "LOSS!" };
        println!(
            "  {:>6} | {:>8} | {:>10.0}/s | {:>10.1}/s | {:>10.0} | {:>7} | {:>5.2?}",
            r.label,
            r.events,
            r.events_per_sec(),
            r.mb_per_sec(),
            r.per_event_ns(),
            loss,
            r.elapsed,
        );
        results.push(r);
    }

    println!();

    // Gate 1 check: per-event overhead <100ns for small messages
    if let Some(small) = results.first() {
        let pass = small.per_event_ns() < 100.0;
        let status = if pass { "PASS" } else { "FAIL" };
        println!(
            "  {status}: Per-event overhead <100ns at {}: {:.0}ns",
            small.label,
            small.per_event_ns()
        );
    }

    // Throughput scaling: MB/sec should increase (or stay flat) with larger payloads
    if results.len() >= 2 {
        let small_mbps = results[0].mb_per_sec();
        let large_mbps = results.last().unwrap().mb_per_sec();
        let ratio = large_mbps / small_mbps;
        let pass = ratio > 1.0;
        let status = if pass { "PASS" } else { "INFO" };
        println!(
            "  {status}: MB/sec scaling ({}→{}): {:.1}x ({:.1} → {:.1} MB/s)",
            results[0].label,
            results.last().unwrap().label,
            ratio,
            small_mbps,
            large_mbps,
        );
    }

    // Zero loss across all sizes
    let all_zero_loss = results.iter().all(|r| r.zero_loss());
    let status = if all_zero_loss { "PASS" } else { "FAIL" };
    println!("  {status}: Zero event loss across all payload sizes");

    println!();
}
