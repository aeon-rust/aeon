//! Redpanda end-to-end throughput benchmark.
//!
//! Measures: Redpanda source → Passthrough → Redpanda sink throughput.
//! Compare against blackhole_bench to calculate headroom ratio.
//!
//! Requires: Redpanda running at localhost:19092 with pre-created topics.
//! Run with: cargo bench -p aeon-engine --bench redpanda_bench

use aeon_connectors::BlackholeSink;
use aeon_connectors::kafka::{KafkaSinkConfig, KafkaSource, KafkaSourceConfig};
use aeon_engine::{PassthroughProcessor, PipelineConfig, PipelineMetrics, run, run_buffered};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn brokers() -> String {
    std::env::var("AEON_BENCH_BROKERS").unwrap_or_else(|_| "localhost:19092".to_string())
}
const SOURCE_TOPIC: &str = "aeon-bench-source";
const SINK_TOPIC: &str = "aeon-bench-sink";

/// Produce N messages using BaseProducer (fire-and-forget, high throughput).
fn produce_messages(count: usize, payload_size: usize) {
    let producer: BaseProducer = ClientConfig::new()
        .set("bootstrap.servers", &brokers())
        .set("message.timeout.ms", "30000")
        .set("queue.buffering.max.messages", "1000000")
        .set("queue.buffering.max.kbytes", "1048576")
        .set("batch.num.messages", "10000")
        .set("linger.ms", "5")
        .create()
        .expect("producer creation");

    let payload = vec![b'x'; payload_size];
    let num_keys: usize = std::env::var("AEON_BENCH_NUM_PARTITIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let keys: Vec<String> = (0..num_keys).map(|i| format!("{i}")).collect();

    for i in 0..count {
        loop {
            match producer.send(
                BaseRecord::to(SOURCE_TOPIC)
                    .payload(&payload)
                    .key(keys[i % num_keys].as_bytes()),
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
}

fn make_source(batch_size: usize) -> KafkaSource {
    let num_partitions: usize = std::env::var("AEON_BENCH_NUM_PARTITIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let config = KafkaSourceConfig::new(&brokers(), SOURCE_TOPIC)
        .with_partitions((0..num_partitions as i32).collect())
        .with_batch_max(batch_size)
        .with_poll_timeout(Duration::from_secs(2))
        .with_drain_timeout(Duration::from_millis(10))
        .with_source_name("bench")
        .with_max_empty_polls(5);

    KafkaSource::new(config).expect("source")
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let broker_addr = brokers();

    // Check if Redpanda is available
    let available: Result<BaseProducer, _> = ClientConfig::new()
        .set("bootstrap.servers", &broker_addr)
        .set("message.timeout.ms", "5000")
        .create();

    if available.is_err() {
        eprintln!("SKIP: Redpanda not available at {broker_addr}");
        return;
    }
    drop(available);

    println!("Broker: {broker_addr}");

    println!("=== Redpanda Throughput Benchmark ===\n");

    let event_count: usize = std::env::var("AEON_BENCH_EVENT_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let payload_size: usize = std::env::var("AEON_BENCH_PAYLOAD_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);

    // ── Step 1: Produce messages ──
    print!("Producing {event_count} messages ({payload_size}B payload)... ");
    let t = Instant::now();
    produce_messages(event_count, payload_size);
    let elapsed = t.elapsed();
    println!(
        "done in {elapsed:.2?} ({:.0} msg/sec)",
        event_count as f64 / elapsed.as_secs_f64()
    );

    // ── Step 2: Source isolation — KafkaSource → BlackholeSink (direct run) ──
    println!("\n--- Source isolation: KafkaSource → BlackholeSink (direct run) ---");
    let t = Instant::now();
    let metrics = rt.block_on(async {
        let mut source = make_source(1024);
        let processor = PassthroughProcessor::new(Arc::from(SINK_TOPIC));
        let mut sink = BlackholeSink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run(&mut source, &processor, &mut sink, &metrics, &shutdown)
            .await
            .expect("pipeline run");

        metrics
    });
    let elapsed = t.elapsed();
    let received = metrics.events_received.load(Ordering::Relaxed);
    println!("  Received:   {received}");
    println!(
        "  Throughput: {:.0} events/sec",
        received as f64 / elapsed.as_secs_f64()
    );
    println!(
        "  Per-event:  {:.0}ns",
        elapsed.as_nanos() as f64 / received.max(1) as f64
    );

    // ── Step 3: Re-produce for full E2E test ──
    print!("\nRe-producing {event_count} messages... ");
    let t = Instant::now();
    produce_messages(event_count, payload_size);
    println!("done in {:.2?}", t.elapsed());

    // ── Step 4: Full E2E — KafkaSource → Passthrough → KafkaSink (direct run, baseline) ──
    println!("\n--- E2E direct run: KafkaSource → KafkaSink (serial loop) ---");
    let t = Instant::now();
    let metrics = rt.block_on(async {
        let mut source = make_source(1024);
        let processor = PassthroughProcessor::new(Arc::from(SINK_TOPIC));
        let sink_config =
            KafkaSinkConfig::new(&brokers(), SINK_TOPIC).with_config("linger.ms", "0"); // minimize delivery latency
        let mut sink = aeon_connectors::kafka::KafkaSink::new(sink_config).expect("sink");
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);

        run(&mut source, &processor, &mut sink, &metrics, &shutdown)
            .await
            .expect("pipeline run");

        metrics
    });
    let elapsed = t.elapsed();
    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);
    println!("  Received:   {received}");
    println!("  Sent:       {sent}");
    println!(
        "  Throughput: {:.0} events/sec",
        received as f64 / elapsed.as_secs_f64()
    );

    // ── Step 5: Re-produce for buffered test ──
    print!("\nRe-producing {event_count} messages... ");
    let t = Instant::now();
    produce_messages(event_count, payload_size);
    println!("done in {:.2?}", t.elapsed());

    // ── Step 6: Buffered pipeline — source/processor/sink run as concurrent tasks ──
    println!("\n--- E2E buffered: KafkaSource → KafkaSink (concurrent tasks, SPSC) ---");
    let t = Instant::now();
    let metrics = rt.block_on(async {
        let source = make_source(1024);
        let processor = PassthroughProcessor::new(Arc::from(SINK_TOPIC));
        let sink_config =
            KafkaSinkConfig::new(&brokers(), SINK_TOPIC).with_config("linger.ms", "0");
        let sink = aeon_connectors::kafka::KafkaSink::new(sink_config).expect("sink");

        let config = PipelineConfig {
            source_buffer_capacity: 256,
            sink_buffer_capacity: 256,
            max_batch_size: 1024,
            ..Default::default()
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));

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

        metrics
    });
    let elapsed = t.elapsed();
    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);
    println!("  Received:   {received}");
    println!("  Sent:       {sent}");
    println!(
        "  Throughput: {:.0} events/sec",
        received as f64 / elapsed.as_secs_f64()
    );
    println!(
        "  Per-event:  {:.0}ns",
        elapsed.as_nanos() as f64 / received.max(1) as f64
    );

    // ── Headroom ratio ──
    println!("\n=== Headroom Ratio ===");
    let blackhole_ceiling: f64 = std::env::var("AEON_BENCH_BLACKHOLE_CEILING")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7_500_000.0);
    let headroom_target: f64 = std::env::var("AEON_BENCH_HEADROOM_TARGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5.0);
    let rp_throughput = received as f64 / elapsed.as_secs_f64();
    let headroom = blackhole_ceiling / rp_throughput;
    println!(
        "Blackhole ceiling: ~{:.1}M events/sec (from blackhole_bench)",
        blackhole_ceiling / 1_000_000.0
    );
    println!("Redpanda E2E:      {rp_throughput:.0} events/sec");
    println!("Headroom ratio:    {headroom:.1}x");
    if headroom >= headroom_target {
        println!("✓ PASS: headroom >= {headroom_target}x — Aeon is not the bottleneck");
    } else {
        println!("✗ FAIL: headroom < {headroom_target}x — investigate pipeline overhead");
    }
    println!();
}
