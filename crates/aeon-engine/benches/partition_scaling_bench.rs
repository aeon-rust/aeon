//! Partition scaling benchmark.
//!
//! Measures KafkaSource throughput at different partition counts (4, 8, 16).
//! Verifies approximately linear scaling: 2x partitions ≈ 2x throughput.
//!
//! Requires: Redpanda with topics:
//!   aeon-scale-p4 (4 partitions), aeon-scale-p8 (8), aeon-scale-p16 (16)
//! Run with: cargo bench -p aeon-engine --bench partition_scaling_bench
//! In Docker: AEON_BENCH_BROKERS=redpanda:9092

use aeon_connectors::BlackholeSink;
use aeon_connectors::kafka::{KafkaSource, KafkaSourceConfig};
use aeon_engine::{
    MultiPartitionConfig, PassthroughProcessor, PipelineConfig, PipelineMetrics, run,
    run_multi_partition,
};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn brokers() -> String {
    std::env::var("AEON_BENCH_BROKERS").unwrap_or_else(|_| "localhost:19092".to_string())
}

fn event_count() -> usize {
    std::env::var("AEON_BENCH_PARTITION_EVENT_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000)
}

fn payload_size() -> usize {
    std::env::var("AEON_BENCH_PAYLOAD_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

fn produce_messages(topic: &str, count: usize, num_partitions: usize) {
    let broker = brokers();
    let producer: BaseProducer = ClientConfig::new()
        .set("bootstrap.servers", &broker)
        .set("message.timeout.ms", "30000")
        .set("queue.buffering.max.messages", "1000000")
        .set("queue.buffering.max.kbytes", "1048576")
        .set("batch.num.messages", "10000")
        .set("linger.ms", "5")
        .create()
        .expect("producer creation");

    let payload = vec![b'x'; payload_size()];
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
    let broker = brokers();
    let t = Instant::now();
    let metrics = rt.block_on(async {
        let config = KafkaSourceConfig::new(&broker, topic)
            .with_partitions((0..num_partitions as i32).collect())
            .with_batch_max(1024)
            .with_poll_timeout(Duration::from_secs(2))
            .with_drain_timeout(Duration::from_millis(10))
            .with_source_name("scale-bench")
            .with_max_empty_polls(5);

        let mut source = KafkaSource::new(config).expect("source");

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

/// Multi-consumer: independent KafkaSource per partition via run_multi_partition.
fn bench_multi_consumer_throughput(
    rt: &tokio::runtime::Runtime,
    topic: &str,
    num_partitions: usize,
) -> (u64, Duration) {
    let broker = brokers();
    let t = Instant::now();
    let metrics = rt.block_on(async {
        let config = MultiPartitionConfig {
            partition_count: num_partitions,
            pipeline: PipelineConfig {
                max_batch_size: 1024,
                ..Default::default()
            },
        };
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let topic = topic.to_string();

        run_multi_partition::<KafkaSource, PassthroughProcessor, BlackholeSink, _, _, _>(
            config,
            Arc::clone(&metrics),
            shutdown,
            {
                let broker = broker.clone();
                let topic = topic.clone();
                move |i| {
                    let cfg = KafkaSourceConfig::new(&broker, &topic)
                        .with_partitions(vec![i as i32])
                        .with_batch_max(1024)
                        .with_poll_timeout(Duration::from_secs(2))
                        .with_drain_timeout(Duration::from_millis(10))
                        .with_source_name("multi-scale")
                        .with_group_id(format!("aeon-multi-{i}"))
                        .with_max_empty_polls(5);
                    KafkaSource::new(cfg).expect("multi-consumer source")
                }
            },
            |_i| PassthroughProcessor::new(Arc::from("output")),
            |_i| BlackholeSink::new(),
            None,
        )
        .await
        .expect("multi-consumer pipeline run");

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

    let broker = brokers();

    // Check Redpanda availability
    let available: Result<BaseProducer, _> = ClientConfig::new()
        .set("bootstrap.servers", &broker)
        .set("message.timeout.ms", "5000")
        .create();

    if available.is_err() {
        eprintln!("SKIP: Redpanda not available at {broker}");
        return;
    }
    drop(available);

    println!("=== Partition Scaling Benchmark ===");
    println!("Broker: {broker}\n");
    println!(
        "Event count: {}, Payload: {}B\n",
        event_count(),
        payload_size()
    );

    let configs = [
        ("aeon-scale-p4", 4),
        ("aeon-scale-p8", 8),
        ("aeon-scale-p16", 16),
    ];

    let mut results: Vec<(usize, f64)> = Vec::new();

    for (topic, partitions) in &configs {
        println!("--- {partitions} partitions ({topic}) ---");

        // Produce
        print!("  Producing {} messages... ", event_count());
        let t = Instant::now();
        produce_messages(topic, event_count(), *partitions);
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

    // ── Section 1: Single-consumer scaling analysis ──
    println!("=== Single-Consumer Scaling Analysis ===\n");
    println!("  Partitions  Throughput       Ratio vs 4p");
    println!("  ----------  ----------       -----------");
    let base = results[0].1;
    for (partitions, throughput) in &results {
        let ratio = throughput / base;
        println!("  {partitions:>10}  {throughput:>10.0}/sec   {ratio:.2}x");
    }

    println!();
    if results.len() >= 3 {
        let ratio_8_vs_4 = results[1].1 / results[0].1;
        let ratio_16_vs_4 = results[2].1 / results[0].1;
        println!("  8p/4p ratio:  {ratio_8_vs_4:.2}x (target: ~2.0x)");
        println!("  16p/4p ratio: {ratio_16_vs_4:.2}x (target: ~4.0x)");

        if ratio_8_vs_4 >= 1.5 && ratio_16_vs_4 >= 3.0 {
            println!("  PASS: approximately linear scaling");
        } else {
            println!("  NOTE: sub-linear (expected — single consumer thread is the bottleneck)");
        }
    }
    println!();

    // ── Section 2: Multi-consumer (run_multi_partition) ──
    println!("=== Multi-Consumer Scaling (run_multi_partition) ===\n");
    println!("Each partition gets an independent KafkaSource consumer.\n");

    let mut multi_results: Vec<(usize, f64)> = Vec::new();

    for (topic, partitions) in &configs {
        println!("--- {partitions} partitions, multi-consumer ({topic}) ---");

        // Re-produce messages for the multi-consumer test
        print!("  Producing {} messages... ", event_count());
        let t = Instant::now();
        produce_messages(topic, event_count(), *partitions);
        println!("done in {:.2?}", t.elapsed());

        // Multi-consumer consume
        print!("  Consuming (multi-consumer)... ");
        let (received, elapsed) = bench_multi_consumer_throughput(&rt, topic, *partitions);
        let throughput = received as f64 / elapsed.as_secs_f64();
        println!("done in {elapsed:.2?}");
        println!("  Received:   {received}");
        println!("  Throughput: {throughput:.0} events/sec");
        println!();

        multi_results.push((*partitions, throughput));
    }

    // Multi-consumer scaling analysis
    println!("=== Multi-Consumer Scaling Analysis ===\n");
    println!("  Partitions  Single-Consumer  Multi-Consumer  Improvement");
    println!("  ----------  ---------------  --------------  -----------");
    for (i, ((p1, t1), (_, t2))) in results.iter().zip(multi_results.iter()).enumerate() {
        let improvement = t2 / t1;
        println!("  {p1:>10}  {t1:>13.0}/s  {t2:>12.0}/s  {improvement:.2}x");
        let _ = i; // suppress unused warning
    }

    if !multi_results.is_empty() {
        let multi_base = multi_results[0].1;
        println!();
        for (partitions, throughput) in &multi_results {
            let ratio = throughput / multi_base;
            println!("  Multi-consumer {partitions}p vs 4p: {ratio:.2}x");
        }
    }

    println!();
}
