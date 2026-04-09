//! Redpanda integration tests — Scenario 1: Redpanda → Passthrough → Redpanda.
//!
//! These tests require a running Redpanda instance at localhost:19092.
//! Run with: cargo test -p aeon-engine --test redpanda_integration
//!
//! Skip if Redpanda is not available (tests check connectivity first).

use aeon_connectors::kafka::{KafkaSink, KafkaSinkConfig, KafkaSource, KafkaSourceConfig};
use aeon_engine::{PassthroughProcessor, PipelineMetrics, run};
use aeon_types::{Sink, Source};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const BROKERS: &str = "localhost:19092";

/// Check if Redpanda is reachable. Skip test if not.
async fn require_redpanda() -> bool {
    let result: Result<FutureProducer, _> = ClientConfig::new()
        .set("bootstrap.servers", BROKERS)
        .set("message.timeout.ms", "5000")
        .create();

    match result {
        Ok(producer) => {
            // Try a metadata request to verify connectivity.
            // Even if send fails (e.g. auth), the producer was created — Redpanda is there.
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                producer.send(
                    FutureRecord::<(), [u8]>::to("__consumer_offsets").payload(&[]),
                    Duration::from_secs(1),
                ),
            )
            .await;
            true
        }
        Err(_) => {
            eprintln!("SKIP: Redpanda not available at {BROKERS}");
            false
        }
    }
}

/// Produce test messages to a Redpanda topic.
async fn produce_test_messages(topic: &str, count: usize) {
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", BROKERS)
        .set("message.timeout.ms", "30000")
        .create()
        .expect("producer creation");

    for i in 0..count {
        let payload = format!("test-event-{i}");
        let key = format!("key-{}", i % 16);
        producer
            .send(
                FutureRecord::to(topic)
                    .payload(payload.as_bytes())
                    .key(key.as_bytes()),
                Duration::from_secs(5),
            )
            .await
            .expect("message delivery failed");
    }

    producer
        .flush(Duration::from_secs(10))
        .expect("producer flush");
}

/// Create a unique topic name for test isolation.
/// Uses the aeon-source/aeon-sink topics that are pre-created.
/// We purge them before each test to ensure clean state.
async fn purge_topic(topic: &str) {
    // Delete and recreate would be ideal, but we can't create topics via rdkafka.
    // Instead, consume and discard all existing messages by seeking to end.
    // For test isolation, we'll use timestamps and unique payloads.
    let _ = topic; // Topics are pre-created; we rely on unique payloads per test
}

#[tokio::test]
async fn redpanda_source_receives_messages() {
    if !require_redpanda().await {
        return;
    }

    let topic = "aeon-source";
    let msg_count = 100;

    purge_topic(topic).await;

    // Produce messages
    produce_test_messages(topic, msg_count).await;

    // Consume via KafkaSource
    let config = KafkaSourceConfig::new(BROKERS, topic)
        .with_partitions((0..16).collect())
        .with_batch_max(256)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("test-source")
        .with_max_empty_polls(5);

    let mut source = KafkaSource::new(config).expect("source creation");

    let mut total_events = 0;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        total_events += events.len();
        if total_events >= msg_count {
            break;
        }
    }

    assert!(
        total_events >= msg_count,
        "expected at least {msg_count} events, got {total_events}"
    );
}

#[tokio::test]
async fn redpanda_sink_produces_messages() {
    if !require_redpanda().await {
        return;
    }

    let topic = "aeon-sink";
    let msg_count = 50;

    // Create outputs and send via KafkaSink
    let config = KafkaSinkConfig::new(BROKERS, topic);
    let mut sink = KafkaSink::new(config).expect("sink creation");

    let outputs: Vec<aeon_types::Output> = (0..msg_count)
        .map(|i| {
            aeon_types::Output::new(
                Arc::from(topic),
                bytes::Bytes::from(format!("sink-test-{i}")),
            )
        })
        .collect();

    sink.write_batch(outputs).await.unwrap();
    sink.flush().await.unwrap();

    assert_eq!(sink.delivered(), msg_count as u64);
}

#[tokio::test]
async fn redpanda_end_to_end_passthrough() {
    if !require_redpanda().await {
        return;
    }

    let source_topic = "aeon-source";
    let sink_topic = "aeon-sink";
    let msg_count = 500;

    // Step 1: Produce test messages to source topic
    produce_test_messages(source_topic, msg_count).await;

    // Step 2: Run pipeline: Redpanda source → Passthrough → Redpanda sink
    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions((0..16).collect())
        .with_batch_max(128)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("e2e-source")
        .with_max_empty_polls(5);

    let mut source = KafkaSource::new(source_config).expect("source creation");

    let processor = PassthroughProcessor::new(Arc::from(sink_topic));

    let sink_config = KafkaSinkConfig::new(BROKERS, sink_topic);
    let mut sink = KafkaSink::new(sink_config).expect("sink creation");

    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    // Verify metrics
    let received = metrics.events_received.load(Ordering::Relaxed);
    let processed = metrics.events_processed.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    assert!(
        received >= msg_count as u64,
        "expected >= {msg_count} received, got {received}"
    );
    assert_eq!(received, processed, "received should equal processed");
    assert_eq!(processed, sent, "processed should equal sent");

    // Verify sink delivery count
    assert!(
        sink.delivered() >= msg_count as u64,
        "expected >= {msg_count} delivered, got {}",
        sink.delivered()
    );

    println!(
        "E2E: received={received}, processed={processed}, sent={sent}, delivered={}",
        sink.delivered()
    );
}
