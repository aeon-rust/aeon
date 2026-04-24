//! Isolated Kafka sink throughput test.
//!
//! Measures KafkaSink.write_batch() throughput with pre-generated outputs.
//! No source, no processor — just measure how fast the sink can drain.
//!
//! Run: cargo bench -p aeon-engine --bench kafka_sink_isolation

use aeon_connectors::kafka::{KafkaSink, KafkaSinkConfig};
use aeon_types::{DeliveryStrategy, Output, Sink};
use bytes::Bytes;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer};
use smallvec::SmallVec;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn brokers() -> String {
    std::env::var("AEON_BENCH_BROKERS").unwrap_or_else(|_| "localhost:19092".to_string())
}

const SINK_TOPIC: &str = "aeon-bench-sink";

fn make_outputs(count: usize, payload_size: usize) -> Vec<Output> {
    let dest: Arc<str> = Arc::from(SINK_TOPIC);
    let payload = Bytes::from(vec![b'x'; payload_size]);
    (0..count)
        .map(|i| Output {
            destination: Arc::clone(&dest),
            key: Some(Bytes::from(format!("key-{}", i % 16))),
            payload: payload.clone(),
            headers: SmallVec::new(),
            source_ts: None,
            source_event_id: None,
            source_partition: None,
            source_offset: None,
            l2_seq: None,
        })
        .collect()
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
    let batch_size: usize = std::env::var("AEON_BENCH_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);

    println!("=== Kafka Sink Isolation Benchmark ===");
    println!("Config: {event_count} events, {payload_size}B payload, batch {batch_size}");
    println!("Broker: {}\n", brokers());

    // ── Test 1: OrderedBatch (default) with linger.ms=0 ──
    println!("--- OrderedBatch + linger.ms=0 ---");
    let outputs = make_outputs(event_count, payload_size);
    rt.block_on(async {
        let sink_config = KafkaSinkConfig::new(brokers(), SINK_TOPIC)
            .with_config("linger.ms", "0")
            .with_strategy(DeliveryStrategy::OrderedBatch);
        let mut sink = KafkaSink::new(sink_config).expect("sink");

        let t = Instant::now();
        for chunk in outputs.chunks(batch_size) {
            sink.write_batch(chunk.to_vec()).await.expect("write_batch");
        }
        sink.flush().await.expect("flush");
        let elapsed = t.elapsed();
        println!(
            "  {event_count} events in {elapsed:.2?} → {:.0} events/sec ({:.0}ns/event)",
            event_count as f64 / elapsed.as_secs_f64(),
            elapsed.as_nanos() as f64 / event_count as f64
        );
    });

    // ── Test 2: OrderedBatch with linger.ms=5 (default, more batching) ──
    println!("\n--- OrderedBatch + linger.ms=5 ---");
    let outputs = make_outputs(event_count, payload_size);
    rt.block_on(async {
        let sink_config = KafkaSinkConfig::new(brokers(), SINK_TOPIC)
            .with_strategy(DeliveryStrategy::OrderedBatch);
        let mut sink = KafkaSink::new(sink_config).expect("sink");

        let t = Instant::now();
        for chunk in outputs.chunks(batch_size) {
            sink.write_batch(chunk.to_vec()).await.expect("write_batch");
        }
        sink.flush().await.expect("flush");
        let elapsed = t.elapsed();
        println!(
            "  {event_count} events in {elapsed:.2?} → {:.0} events/sec ({:.0}ns/event)",
            event_count as f64 / elapsed.as_secs_f64(),
            elapsed.as_nanos() as f64 / event_count as f64
        );
    });

    // ── Test 3: UnorderedBatch (non-blocking, flush at end) ──
    println!("\n--- UnorderedBatch + linger.ms=5 ---");
    let outputs = make_outputs(event_count, payload_size);
    rt.block_on(async {
        let sink_config = KafkaSinkConfig::new(brokers(), SINK_TOPIC)
            .with_strategy(DeliveryStrategy::UnorderedBatch);
        let mut sink = KafkaSink::new(sink_config).expect("sink");

        let t = Instant::now();
        for chunk in outputs.chunks(batch_size) {
            sink.write_batch(chunk.to_vec()).await.expect("write_batch");
        }
        sink.flush().await.expect("flush");
        let elapsed = t.elapsed();
        println!(
            "  {event_count} events in {elapsed:.2?} → {:.0} events/sec ({:.0}ns/event)",
            event_count as f64 / elapsed.as_secs_f64(),
            elapsed.as_nanos() as f64 / event_count as f64
        );
    });

    // ── Test 4: Raw BaseProducer baseline (reference) ──
    println!("\n--- Raw BaseProducer (librdkafka baseline) ---");
    let producer: BaseProducer = ClientConfig::new()
        .set("bootstrap.servers", brokers())
        .set("message.timeout.ms", "30000")
        .set("queue.buffering.max.messages", "1000000")
        .set("queue.buffering.max.kbytes", "1048576")
        .set("batch.num.messages", "10000")
        .set("linger.ms", "5")
        .create()
        .expect("producer");
    let payload = vec![b'x'; payload_size];
    let keys: Vec<String> = (0..16).map(|i| format!("key-{i}")).collect();

    let t = Instant::now();
    for i in 0..event_count {
        loop {
            match producer.send(
                BaseRecord::to(SINK_TOPIC)
                    .payload(&payload)
                    .key(keys[i % 16].as_bytes()),
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
    producer.flush(Duration::from_secs(30)).expect("flush");
    let elapsed = t.elapsed();
    println!(
        "  {event_count} events in {elapsed:.2?} → {:.0} events/sec ({:.0}ns/event)",
        event_count as f64 / elapsed.as_secs_f64(),
        elapsed.as_nanos() as f64 / event_count as f64
    );
}
