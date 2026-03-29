//! Partition scaling benchmark.
//!
//! Measures KafkaSource throughput at different partition counts (4, 8, 16).
//! Verifies approximately linear scaling: 2x partitions ≈ 2x throughput.
//!
//! Requires: Redpanda at localhost:19092 with topics:
//!   aeon-scale-p4 (4 partitions), aeon-scale-p8 (8), aeon-scale-p16 (16)
//! Run with: cargo bench -p aeon-engine --bench partition_scaling_bench

use aeon_connectors::BlackholeSink;
use aeon_connectors::kafka::{KafkaSource, KafkaSourceConfig};
use aeon_engine::{PassthroughProcessor, PipelineMetrics, run};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const BROKERS: &str = "localhost:19092";
const EVENT_COUNT: usize = 50_000;
const PAYLOAD_SIZE: usize = 256;

fn produce_messages(topic: &str, count: usize, num_partitions: usize) {
    let producer: BaseProducer = ClientConfig::new()
        .set("bootstrap.servers", BROKERS)
        .set("message.timeout.ms", "30000")
        .set("queue.buffering.max.messages", "1000000")
        .set("queue.buffering.max.kbytes", "1048576")
        .set("batch.num.messages", "10000")
        .set("linger.ms", "5")
        .create()
        .expect("producer creation");

    let payload = vec![b'x'; PAYLOAD_SIZE];
    let keys: Vec<String> = (0..num_partitions).map(|i| format!("{i}")).collect();

    for i in 0..count {
        loop {
            match producer.send(
                BaseRecord::to(topic)
                    .payload(&payload)
                    .key(keys[i % num_partitions].as_bytes()),
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

fn bench_source_throughput(
    rt: &tokio::runtime::Runtime,
    topic: &str,
    num_partitions: usize,
) -> (u64, Duration) {
    let t = Instant::now();
    let metrics = rt.block_on(async {
        let config = KafkaSourceConfig::new(BROKERS, topic)
            .with_partitions((0..num_partitions as i32).collect())
            .with_batch_max(1024)
            .with_poll_timeout(Duration::from_secs(2))
            .with_drain_timeout(Duration::from_millis(10))
            .with_source_name("scale-bench");

        let mut source = KafkaSource::new(config).expect("source");
        source = source.with_max_empty_polls(5);

        let processor = PassthroughProcessor::new(Arc::from("output"));
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
    (received, elapsed)
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    // Check Redpanda availability
    let available: Result<BaseProducer, _> = ClientConfig::new()
        .set("bootstrap.servers", BROKERS)
        .set("message.timeout.ms", "5000")
        .create();

    if available.is_err() {
        eprintln!("SKIP: Redpanda not available at {BROKERS}");
        return;
    }
    drop(available);

    println!("=== Partition Scaling Benchmark ===\n");
    println!("Event count: {EVENT_COUNT}, Payload: {PAYLOAD_SIZE}B\n");

    let configs = [
        ("aeon-scale-p4", 4),
        ("aeon-scale-p8", 8),
        ("aeon-scale-p16", 16),
    ];

    let mut results: Vec<(usize, f64)> = Vec::new();

    for (topic, partitions) in &configs {
        println!("--- {partitions} partitions ({topic}) ---");

        // Produce
        print!("  Producing {EVENT_COUNT} messages... ");
        let t = Instant::now();
        produce_messages(topic, EVENT_COUNT, *partitions);
        println!("done in {:.2?}", t.elapsed());

        // Consume (source → blackhole)
        print!("  Consuming... ");
        let (received, elapsed) = bench_source_throughput(&rt, topic, *partitions);
        let throughput = received as f64 / elapsed.as_secs_f64();
        println!("done in {elapsed:.2?}");
        println!("  Received:   {received}");
        println!("  Throughput: {throughput:.0} events/sec");
        println!();

        results.push((*partitions, throughput));
    }

    // Scaling analysis
    println!("=== Scaling Analysis ===\n");
    println!("  Partitions  Throughput       Ratio vs 4p");
    println!("  ----------  ----------       -----------");
    let base = results[0].1;
    for (partitions, throughput) in &results {
        let ratio = throughput / base;
        println!("  {partitions:>10}  {throughput:>10.0}/sec   {ratio:.2}x");
    }

    // Check linearity
    println!();
    if results.len() >= 3 {
        let ratio_8_vs_4 = results[1].1 / results[0].1;
        let ratio_16_vs_4 = results[2].1 / results[0].1;
        println!("  8p/4p ratio:  {ratio_8_vs_4:.2}x (target: ~2.0x)");
        println!("  16p/4p ratio: {ratio_16_vs_4:.2}x (target: ~4.0x)");

        if ratio_8_vs_4 >= 1.5 && ratio_16_vs_4 >= 3.0 {
            println!("  PASS: approximately linear scaling");
        } else {
            println!(
                "  NOTE: sub-linear scaling (may be limited by single-consumer threading or broker capacity on WSL2)"
            );
        }
    }
    println!();
}
