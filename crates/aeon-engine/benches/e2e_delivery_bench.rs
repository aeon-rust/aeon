//! E2E Delivery Strategy Benchmark — PerEvent vs OrderedBatch vs UnorderedBatch across sink types.
//!
//! Tests three sink connectors (Blackhole, File, Redpanda) across all three delivery strategies
//! to validate the delivery architecture and measure throughput/ordering trade-offs.
//!
//! Key measurements:
//! - Throughput (events/sec) for each mode × sink combination
//! - Batched/Ordered speedup ratio
//! - Event loss detection (sent vs received)
//!
//! Environment variables:
//! - AEON_BENCH_BROKERS: Redpanda broker address (default: localhost:19092)
//! - AEON_BENCH_EVENT_COUNT: Number of events per test (default: 100000)
//! - AEON_BENCH_PAYLOAD_SIZE: Payload size in bytes (default: 256)
//! - AEON_BENCH_BATCH_SIZE: Max batch size (default: 1024)
//! - AEON_BENCH_FLUSH_INTERVAL_MS: Batched mode flush interval (default: 100)
//! - AEON_BENCH_MAX_PENDING: Batched mode max pending (default: 50000)
//!
//! Run with: cargo bench -p aeon-engine --bench e2e_delivery_bench

use aeon_connectors::BlackholeSink;
use aeon_connectors::MemorySource;
use aeon_connectors::file::{FileSink, FileSinkConfig};
use aeon_connectors::kafka::{KafkaSink, KafkaSinkConfig, KafkaSource, KafkaSourceConfig};
use aeon_engine::delivery::{CheckpointBackend, CheckpointConfig, DeliveryConfig, FlushStrategy};
use aeon_engine::{PassthroughProcessor, PipelineConfig, PipelineMetrics, run_buffered};
use aeon_types::{DeliveryStrategy, Event, PartitionId};
use bytes::Bytes;
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn brokers() -> String {
    std::env::var("AEON_BENCH_BROKERS").unwrap_or_else(|_| "localhost:19092".to_string())
}

fn event_count() -> usize {
    std::env::var("AEON_BENCH_EVENT_COUNT")
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

fn flush_interval_ms() -> u64 {
    std::env::var("AEON_BENCH_FLUSH_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}

fn max_pending() -> usize {
    std::env::var("AEON_BENCH_MAX_PENDING")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000)
}

fn num_partitions() -> usize {
    std::env::var("AEON_BENCH_NUM_PARTITIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16)
}

const SOURCE_TOPIC: &str = "aeon-e2e-source";
const SINK_TOPIC: &str = "aeon-e2e-sink";

/// Generate N test events with the given payload size.
fn make_events(count: usize, psize: usize) -> Vec<Event> {
    let payload = Bytes::from(vec![b'x'; psize]);
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

/// Delete and recreate a topic to ensure a clean slate.
async fn reset_topic(topic: &str, partitions: i32) {
    let admin: AdminClient<rdkafka::client::DefaultClientContext> = ClientConfig::new()
        .set("bootstrap.servers", brokers())
        .create()
        .expect("admin client");

    let opts = AdminOptions::new().operation_timeout(Some(Duration::from_secs(10)));

    // Delete (ignore errors — topic may not exist)
    let _ = admin.delete_topics(&[topic], &opts).await;
    // Wait for deletion to propagate
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Recreate
    let new_topic = NewTopic::new(topic, partitions, TopicReplication::Fixed(1));
    admin
        .create_topics(&[new_topic], &opts)
        .await
        .expect("create topic");

    // Wait for topic to be ready
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Produce N messages to the source topic.
fn produce_messages(topic: &str, count: usize, payload_size: usize) -> Duration {
    let producer: BaseProducer = ClientConfig::new()
        .set("bootstrap.servers", brokers())
        .set("message.timeout.ms", "30000")
        .set("queue.buffering.max.messages", "1000000")
        .set("queue.buffering.max.kbytes", "1048576")
        .set("batch.num.messages", "10000")
        .set("linger.ms", "5")
        .create()
        .expect("producer creation");

    let payload = vec![b'x'; payload_size];
    let nparts = num_partitions();
    let keys: Vec<String> = (0..nparts).map(|i| format!("{i}")).collect();

    let t = Instant::now();
    for i in 0..count {
        loop {
            match producer.send(
                BaseRecord::to(topic)
                    .payload(&payload)
                    .key(keys[i % nparts].as_bytes()),
            ) {
                Ok(()) => break,
                Err((
                    rdkafka::error::KafkaError::MessageProduction(
                        rdkafka::types::RDKafkaErrorCode::QueueFull,
                    ),
                    _,
                )) => {
                    producer.poll(Duration::from_millis(100));
                }
                Err((e, _)) => panic!("produce failed: {e}"),
            }
        }
        if i % 10_000 == 0 {
            producer.poll(Duration::ZERO);
        }
    }
    producer
        .flush(Duration::from_secs(30))
        .expect("flush failed");
    t.elapsed()
}

fn make_source(topic: &str) -> KafkaSource {
    let nparts = num_partitions();
    let config = KafkaSourceConfig::new(brokers(), topic)
        .with_partitions((0..nparts as i32).collect())
        .with_batch_max(batch_size())
        .with_poll_timeout(Duration::from_secs(2))
        .with_drain_timeout(Duration::from_millis(10))
        .with_source_name("e2e-bench")
        .with_max_empty_polls(5);

    KafkaSource::new(config).expect("source")
}

fn make_pipeline_config(strategy: DeliveryStrategy) -> PipelineConfig {
    PipelineConfig {
        source_buffer_capacity: 8192,
        sink_buffer_capacity: 8192,
        max_batch_size: batch_size(),
        delivery: DeliveryConfig {
            strategy,
            flush: FlushStrategy {
                interval: Duration::from_millis(flush_interval_ms()),
                max_pending: max_pending(),
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

struct BenchResult {
    mode: &'static str,
    sink_type: &'static str,
    events_received: u64,
    outputs_sent: u64,
    elapsed: Duration,
}

impl BenchResult {
    fn throughput(&self) -> f64 {
        self.events_received as f64 / self.elapsed.as_secs_f64()
    }

    fn per_event_ns(&self) -> f64 {
        self.elapsed.as_nanos() as f64 / self.events_received.max(1) as f64
    }
}

// ─── Test 1: Blackhole Sink ──────────────────────────────────────

async fn bench_blackhole(strategy: DeliveryStrategy) -> BenchResult {
    let mode_name = match strategy {
        DeliveryStrategy::PerEvent => "PerEvent",
        DeliveryStrategy::OrderedBatch => "OrdBatch",
        DeliveryStrategy::UnorderedBatch => "UnordBatch",
    };

    // Use MemorySource to eliminate broker overhead — pure pipeline measurement
    let events = make_events(event_count(), payload_size());
    let source = MemorySource::new(events, batch_size());
    let processor = PassthroughProcessor::new(Arc::from("blackhole"));
    let sink = BlackholeSink::new();
    let config = make_pipeline_config(strategy);
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

    BenchResult {
        mode: mode_name,
        sink_type: "Blackhole",
        events_received: metrics.events_received.load(Ordering::Relaxed),
        outputs_sent: metrics.outputs_sent.load(Ordering::Relaxed),
        elapsed,
    }
}

// ─── Test 2: File Sink ───────────────────────────────────────────

async fn bench_file(strategy: DeliveryStrategy) -> BenchResult {
    let mode_name = match strategy {
        DeliveryStrategy::PerEvent => "PerEvent",
        DeliveryStrategy::OrderedBatch => "OrdBatch",
        DeliveryStrategy::UnorderedBatch => "UnordBatch",
    };

    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = tmp.path().to_path_buf();

    let events = make_events(event_count(), payload_size());
    let source = MemorySource::new(events, batch_size());
    let processor = PassthroughProcessor::new(Arc::from("file"));
    let file_config = FileSinkConfig::new(&path).with_strategy(strategy);
    let sink = FileSink::new(file_config);
    let config = make_pipeline_config(strategy);
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

    // Verify output file has the right number of lines
    let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    let line_count = content.lines().count();
    let expected = metrics.outputs_sent.load(Ordering::Relaxed) as usize;
    if line_count != expected {
        eprintln!("  WARNING: File line count mismatch: got {line_count}, expected {expected}");
    }

    BenchResult {
        mode: mode_name,
        sink_type: "FileSink",
        events_received: metrics.events_received.load(Ordering::Relaxed),
        outputs_sent: metrics.outputs_sent.load(Ordering::Relaxed),
        elapsed,
    }
}

// ─── Test 3: Redpanda/Kafka Sink ─────────────────────────────────

async fn bench_redpanda(strategy: DeliveryStrategy) -> BenchResult {
    let mode_name = match strategy {
        DeliveryStrategy::PerEvent => "PerEvent",
        DeliveryStrategy::OrderedBatch => "OrdBatch",
        DeliveryStrategy::UnorderedBatch => "UnordBatch",
    };

    let count = event_count();
    let psize = payload_size();

    // Reset topics for clean measurement
    reset_topic(SOURCE_TOPIC, num_partitions() as i32).await;
    reset_topic(SINK_TOPIC, num_partitions() as i32).await;

    // Produce messages to source topic
    produce_messages(SOURCE_TOPIC, count, psize);

    let source = make_source(SOURCE_TOPIC);
    let processor = PassthroughProcessor::new(Arc::from(SINK_TOPIC));

    let mut sink_config = KafkaSinkConfig::new(brokers(), SINK_TOPIC).with_strategy(strategy);
    // Optimize producer settings per strategy
    match strategy {
        DeliveryStrategy::PerEvent => {
            sink_config = sink_config.with_config("linger.ms", "0");
        }
        DeliveryStrategy::OrderedBatch => {
            // OrderedBatch sends in order, awaits at batch boundary — moderate linger
            sink_config = sink_config
                .with_config("linger.ms", "2")
                .with_config("batch.num.messages", "10000");
        }
        DeliveryStrategy::UnorderedBatch => {
            // UnorderedBatch: highest throughput, no ordering guarantee
            sink_config = sink_config
                .with_config("linger.ms", "5")
                .with_config("batch.num.messages", "10000");
        }
    }

    let sink = KafkaSink::new(sink_config).expect("kafka sink");
    let config = make_pipeline_config(strategy);
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

    BenchResult {
        mode: mode_name,
        sink_type: "Redpanda",
        events_received: metrics.events_received.load(Ordering::Relaxed),
        outputs_sent: metrics.outputs_sent.load(Ordering::Relaxed),
        elapsed,
    }
}

fn print_result(r: &BenchResult) {
    println!(
        "  {:>10} | {:>10} | {:>10} events | {:>10} sent | {:>10.0}/sec | {:>8.0}ns/evt | {:.2?}",
        r.mode,
        r.sink_type,
        r.events_received,
        r.outputs_sent,
        r.throughput(),
        r.per_event_ns(),
        r.elapsed,
    );
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let count = event_count();
    let psize = payload_size();
    let bsize = batch_size();
    let flush_ms = flush_interval_ms();
    let max_pend = max_pending();

    println!("=== E2E Delivery Mode Benchmark (Run 8) ===");
    println!("Events: {count}, Payload: {psize}B, Batch: {bsize}");
    println!("Flush interval: {flush_ms}ms, Max pending: {max_pend}");
    println!();

    // Check Redpanda availability
    let broker_addr = brokers();
    let rp_available: Result<BaseProducer, _> = ClientConfig::new()
        .set("bootstrap.servers", &broker_addr)
        .set("message.timeout.ms", "5000")
        .create();
    let has_redpanda = rp_available.is_ok();
    drop(rp_available);

    if has_redpanda {
        println!("Broker: {broker_addr} (available)");
    } else {
        println!("Broker: {broker_addr} (NOT available — skipping Redpanda tests)");
    }
    println!();

    // ─── Test 1: Blackhole Sink ──────────────────────────────────
    println!("--- Test 1: Blackhole Sink (pure pipeline overhead) ---");
    println!();
    println!(
        "  {:>10} | {:>10} | {:>10}        | {:>10}      | {:>13}    | {:>12}    | Duration",
        "Mode", "Sink", "Received", "Sent", "Throughput", "Per-event"
    );

    let bh_per_event = rt.block_on(bench_blackhole(DeliveryStrategy::PerEvent));
    print_result(&bh_per_event);

    let bh_ord_batch = rt.block_on(bench_blackhole(DeliveryStrategy::OrderedBatch));
    print_result(&bh_ord_batch);

    let bh_unord_batch = rt.block_on(bench_blackhole(DeliveryStrategy::UnorderedBatch));
    print_result(&bh_unord_batch);

    let bh_ob_speedup = bh_ord_batch.throughput() / bh_per_event.throughput();
    let bh_ub_speedup = bh_unord_batch.throughput() / bh_per_event.throughput();
    println!();
    println!("  OrdBatch/PerEvent speedup: {bh_ob_speedup:.2}x");
    println!("  UnordBatch/PerEvent speedup: {bh_ub_speedup:.2}x");
    println!(
        "  Event loss: pe={}/{}, ob={}/{}, ub={}/{}",
        bh_per_event.outputs_sent,
        bh_per_event.events_received,
        bh_ord_batch.outputs_sent,
        bh_ord_batch.events_received,
        bh_unord_batch.outputs_sent,
        bh_unord_batch.events_received
    );
    println!();

    // ─── Test 2: File Sink ───────────────────────────────────────
    println!("--- Test 2: File Sink (durable I/O) ---");
    println!();

    let file_per_event = rt.block_on(bench_file(DeliveryStrategy::PerEvent));
    print_result(&file_per_event);

    let file_ord_batch = rt.block_on(bench_file(DeliveryStrategy::OrderedBatch));
    print_result(&file_ord_batch);

    let file_unord_batch = rt.block_on(bench_file(DeliveryStrategy::UnorderedBatch));
    print_result(&file_unord_batch);

    let file_ob_speedup = file_ord_batch.throughput() / file_per_event.throughput();
    let file_ub_speedup = file_unord_batch.throughput() / file_per_event.throughput();
    println!();
    println!("  OrdBatch/PerEvent speedup: {file_ob_speedup:.2}x");
    println!("  UnordBatch/PerEvent speedup: {file_ub_speedup:.2}x");
    println!(
        "  Event loss: pe={}/{}, ob={}/{}, ub={}/{}",
        file_per_event.outputs_sent,
        file_per_event.events_received,
        file_ord_batch.outputs_sent,
        file_ord_batch.events_received,
        file_unord_batch.outputs_sent,
        file_unord_batch.events_received
    );
    println!();

    // ─── Test 3: Redpanda Sink ───────────────────────────────────
    if has_redpanda {
        println!("--- Test 3: Redpanda Sink (network I/O, same topic) ---");
        println!();

        let rp_per_event = rt.block_on(bench_redpanda(DeliveryStrategy::PerEvent));
        print_result(&rp_per_event);

        let rp_ord_batch = rt.block_on(bench_redpanda(DeliveryStrategy::OrderedBatch));
        print_result(&rp_ord_batch);

        let rp_unord_batch = rt.block_on(bench_redpanda(DeliveryStrategy::UnorderedBatch));
        print_result(&rp_unord_batch);

        let rp_ob_speedup = rp_ord_batch.throughput() / rp_per_event.throughput();
        let rp_ub_speedup = rp_unord_batch.throughput() / rp_per_event.throughput();
        println!();
        println!("  OrdBatch/PerEvent speedup: {rp_ob_speedup:.2}x");
        println!("  UnordBatch/PerEvent speedup: {rp_ub_speedup:.2}x");
        println!(
            "  Event loss: pe={}/{}, ob={}/{}, ub={}/{}",
            rp_per_event.outputs_sent,
            rp_per_event.events_received,
            rp_ord_batch.outputs_sent,
            rp_ord_batch.events_received,
            rp_unord_batch.outputs_sent,
            rp_unord_batch.events_received
        );
        println!();

        // ─── Summary ─────────────────────────────────────────────
        println!("=== Summary ===");
        println!();
        println!(
            "  {:>12} | {:>12} | {:>12} | {:>12} | {:>10} | {:>10}",
            "Sink", "PerEvent", "OrdBatch", "UnordBatch", "OB/PE", "UB/PE"
        );
        println!(
            "  {:>12} | {:>12} | {:>12} | {:>12} | {:>10} | {:>10}",
            "────────────",
            "────────────",
            "────────────",
            "────────────",
            "──────────",
            "──────────"
        );
        println!(
            "  {:>12} | {:>10.0}/s | {:>10.0}/s | {:>10.0}/s | {:>8.2}x | {:>8.2}x",
            "Blackhole",
            bh_per_event.throughput(),
            bh_ord_batch.throughput(),
            bh_unord_batch.throughput(),
            bh_ob_speedup,
            bh_ub_speedup
        );
        println!(
            "  {:>12} | {:>10.0}/s | {:>10.0}/s | {:>10.0}/s | {:>8.2}x | {:>8.2}x",
            "FileSink",
            file_per_event.throughput(),
            file_ord_batch.throughput(),
            file_unord_batch.throughput(),
            file_ob_speedup,
            file_ub_speedup
        );
        println!(
            "  {:>12} | {:>10.0}/s | {:>10.0}/s | {:>10.0}/s | {:>8.2}x | {:>8.2}x",
            "Redpanda",
            rp_per_event.throughput(),
            rp_ord_batch.throughput(),
            rp_unord_batch.throughput(),
            rp_ob_speedup,
            rp_ub_speedup
        );
        println!();

        // Acceptance criteria
        println!("=== Acceptance Criteria ===");
        println!();
        let all_results = [
            &bh_per_event,
            &bh_ord_batch,
            &bh_unord_batch,
            &file_per_event,
            &file_ord_batch,
            &file_unord_batch,
            &rp_per_event,
            &rp_ord_batch,
            &rp_unord_batch,
        ];
        let zero_loss = all_results
            .iter()
            .all(|r| r.events_received == r.outputs_sent);

        let checks = [
            (
                "Blackhole UnordBatch >= PerEvent",
                bh_ub_speedup >= 0.95,
                format!("{bh_ub_speedup:.2}x"),
            ),
            (
                "FileSink UnordBatch > 2x PerEvent",
                file_ub_speedup > 2.0,
                format!("{file_ub_speedup:.2}x"),
            ),
            (
                "Redpanda UnordBatch > 5x PerEvent",
                rp_ub_speedup > 5.0,
                format!("{rp_ub_speedup:.2}x"),
            ),
            (
                "Redpanda OrdBatch > PerEvent",
                rp_ob_speedup > 1.0,
                format!("{rp_ob_speedup:.2}x"),
            ),
            (
                "Redpanda UnordBatch > 10K/s",
                rp_unord_batch.throughput() > 10_000.0,
                format!("{:.0}/s", rp_unord_batch.throughput()),
            ),
            (
                "Zero event loss (all 9 tests)",
                zero_loss,
                "check all".to_string(),
            ),
        ];

        let mut all_pass = true;
        for (name, pass, value) in &checks {
            let status = if *pass { "PASS" } else { "FAIL" };
            if !pass {
                all_pass = false;
            }
            println!("  {status}: {name} — {value}");
        }
        println!();
        if all_pass {
            println!("=== ALL CRITERIA PASSED ===");
        } else {
            println!("=== SOME CRITERIA DID NOT PASS ===");
        }
    } else {
        // No Redpanda — just summary for blackhole + file
        println!("=== Summary (no Redpanda) ===");
        println!();
        println!(
            "  {:>12} | {:>12} | {:>12} | {:>12} | {:>10} | {:>10}",
            "Sink", "PerEvent", "OrdBatch", "UnordBatch", "OB/PE", "UB/PE"
        );
        println!(
            "  {:>12} | {:>12} | {:>12} | {:>12} | {:>10} | {:>10}",
            "────────────",
            "────────────",
            "────────────",
            "────────────",
            "──────────",
            "──────────"
        );
        println!(
            "  {:>12} | {:>10.0}/s | {:>10.0}/s | {:>10.0}/s | {:>8.2}x | {:>8.2}x",
            "Blackhole",
            bh_per_event.throughput(),
            bh_ord_batch.throughput(),
            bh_unord_batch.throughput(),
            bh_ob_speedup,
            bh_ub_speedup
        );
        println!(
            "  {:>12} | {:>10.0}/s | {:>10.0}/s | {:>10.0}/s | {:>8.2}x | {:>8.2}x",
            "FileSink",
            file_per_event.throughput(),
            file_ord_batch.throughput(),
            file_unord_batch.throughput(),
            file_ob_speedup,
            file_ub_speedup
        );
    }

    println!();
}
