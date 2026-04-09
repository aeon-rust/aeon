//! Isolated measurement of KafkaSource throughput via BlackholeSink.
//!
//! Removes sink-side variables to determine if the bottleneck is source or sink.
//!
//! Requires: Redpanda at localhost:19092 with topic aeon-bench-source pre-populated.
//! Run: cargo bench -p aeon-engine --bench kafka_source_isolation

use aeon_connectors::BlackholeSink;
use aeon_connectors::kafka::{KafkaSource, KafkaSourceConfig};
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

fn produce_messages(count: usize, payload_size: usize) {
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
    let num_keys = 16usize;
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

fn make_source(batch_size: usize, drain_ms: u64) -> KafkaSource {
    let config = KafkaSourceConfig::new(brokers(), SOURCE_TOPIC)
        .with_partitions((0..16_i32).collect())
        .with_batch_max(batch_size)
        .with_poll_timeout(Duration::from_secs(2))
        .with_drain_timeout(Duration::from_millis(drain_ms))
        .with_source_name("bench")
        .with_max_empty_polls(3);

    KafkaSource::new(config).expect("source")
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let available: Result<BaseProducer, _> = ClientConfig::new()
        .set("bootstrap.servers", brokers())
        .set("message.timeout.ms", "5000")
        .create();

    if available.is_err() {
        eprintln!("SKIP: Redpanda not available at {}", brokers());
        return;
    }
    drop(available);

    let event_count: usize = std::env::var("AEON_BENCH_EVENT_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let payload_size: usize = std::env::var("AEON_BENCH_PAYLOAD_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);

    println!("=== Kafka Source Isolation Benchmark ===");
    println!("Config: {event_count} events, {payload_size}B payload, 16 partitions");
    println!("Broker: {}\n", brokers());

    // ── Produce ──
    print!("Producing {event_count} messages... ");
    let t = Instant::now();
    produce_messages(event_count, payload_size);
    let elapsed = t.elapsed();
    println!(
        "done in {elapsed:.2?} ({:.0} msg/sec)",
        event_count as f64 / elapsed.as_secs_f64()
    );

    // ── Test 1: Serial KafkaSource → BlackholeSink (drain 50ms) ──
    println!("\n--- Serial: KafkaSource → Passthrough → BlackholeSink (drain 50ms) ---");
    let t = Instant::now();
    let metrics = rt.block_on(async {
        let mut source = make_source(1024, 50);
        let processor = PassthroughProcessor::new(Arc::from(SINK_TOPIC));
        let mut sink = BlackholeSink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);
        run(&mut source, &processor, &mut sink, &metrics, &shutdown)
            .await
            .expect("pipeline");
        metrics
    });
    let elapsed = t.elapsed();
    let received = metrics.events_received.load(Ordering::Relaxed);
    println!(
        "  Received:   {received}, Throughput: {:.0} events/sec, Per-event: {:.0}ns",
        received as f64 / elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / received.max(1) as f64
    );

    // ── Re-produce ──
    print!("\nRe-producing {event_count} messages... ");
    produce_messages(event_count, payload_size);
    println!("done");

    // ── Test 2: Serial with drain 5ms ──
    println!("\n--- Serial: KafkaSource → Passthrough → BlackholeSink (drain 5ms) ---");
    let t = Instant::now();
    let metrics = rt.block_on(async {
        let mut source = make_source(1024, 5);
        let processor = PassthroughProcessor::new(Arc::from(SINK_TOPIC));
        let mut sink = BlackholeSink::new();
        let metrics = PipelineMetrics::new();
        let shutdown = AtomicBool::new(false);
        run(&mut source, &processor, &mut sink, &metrics, &shutdown)
            .await
            .expect("pipeline");
        metrics
    });
    let elapsed = t.elapsed();
    let received = metrics.events_received.load(Ordering::Relaxed);
    println!(
        "  Received:   {received}, Throughput: {:.0} events/sec, Per-event: {:.0}ns",
        received as f64 / elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / received.max(1) as f64
    );

    // ── Re-produce ──
    print!("\nRe-producing {event_count} messages... ");
    produce_messages(event_count, payload_size);
    println!("done");

    // ── Test 3: Buffered KafkaSource → BlackholeSink ──
    println!("\n--- Buffered: KafkaSource → Passthrough → BlackholeSink (drain 50ms) ---");
    let t = Instant::now();
    let metrics = rt.block_on(async {
        let source = make_source(1024, 50);
        let processor = PassthroughProcessor::new(Arc::from(SINK_TOPIC));
        let sink = BlackholeSink::new();
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
        .expect("pipeline");
        metrics
    });
    let elapsed = t.elapsed();
    let received = metrics.events_received.load(Ordering::Relaxed);
    println!(
        "  Received:   {received}, Throughput: {:.0} events/sec, Per-event: {:.0}ns",
        received as f64 / elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / received.max(1) as f64
    );
}
