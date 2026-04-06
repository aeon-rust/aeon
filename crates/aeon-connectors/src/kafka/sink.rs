//! Kafka/Redpanda sink — batch produce with FutureProducer.
//!
//! Supports three delivery strategies:
//! - **PerEvent**: Each output is enqueued and its delivery future awaited individually.
//! - **OrderedBatch** (default): All outputs in `write_batch()` are enqueued in order,
//!   then all delivery futures awaited together before returning.
//! - **UnorderedBatch**: `write_batch()` enqueues outputs into rdkafka's internal buffer
//!   and returns immediately. `flush()` calls `producer.flush()` to await all
//!   pending deliveries. Higher throughput, downstream sorts by UUIDv7.

use aeon_types::{AeonError, BatchResult, DeliveryStrategy, Output, Sink};
use rdkafka::config::ClientConfig;
use rdkafka::message::OwnedHeaders;
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};
use std::time::Duration;

/// Configuration for `KafkaSink`.
pub struct KafkaSinkConfig {
    /// Kafka/Redpanda broker addresses.
    pub brokers: String,
    /// Default destination topic (used when Output.destination is not overridden).
    pub default_topic: String,
    /// Produce timeout per message.
    pub produce_timeout: Duration,
    /// Timeout for `flush()` — how long to wait for all pending deliveries.
    /// Default: 30 seconds.
    pub flush_timeout: Duration,
    /// Optional: additional rdkafka config overrides.
    pub config_overrides: Vec<(String, String)>,
    /// Delivery strategy — controls how write_batch handles acks.
    pub strategy: DeliveryStrategy,
}

impl KafkaSinkConfig {
    /// Create a sink config targeting a specific topic.
    pub fn new(brokers: impl Into<String>, topic: impl Into<String>) -> Self {
        Self {
            brokers: brokers.into(),
            default_topic: topic.into(),
            produce_timeout: Duration::from_secs(5),
            flush_timeout: Duration::from_secs(30),
            config_overrides: Vec::new(),
            strategy: DeliveryStrategy::default(),
        }
    }

    /// Set the produce timeout.
    pub fn with_produce_timeout(mut self, timeout: Duration) -> Self {
        self.produce_timeout = timeout;
        self
    }

    /// Set the flush timeout (how long to wait for pending deliveries on flush).
    pub fn with_flush_timeout(mut self, timeout: Duration) -> Self {
        self.flush_timeout = timeout;
        self
    }

    /// Add an rdkafka config override.
    pub fn with_config(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config_overrides.push((key.into(), value.into()));
        self
    }

    /// Set the delivery strategy.
    pub fn with_strategy(mut self, strategy: DeliveryStrategy) -> Self {
        self.strategy = strategy;
        self
    }
}

/// Kafka/Redpanda output sink using FutureProducer.
///
/// - **PerEvent**: each output is enqueued and its delivery future awaited individually.
/// - **OrderedBatch** (default): all outputs are enqueued in order, then all delivery
///   futures awaited together before returning. Ordering guaranteed by idempotent producer.
/// - **UnorderedBatch**: `write_batch()` enqueues into rdkafka's internal buffer and
///   returns immediately. `flush()` waits for all pending deliveries.
///
/// - Output.destination maps to the Kafka topic (falls back to default_topic)
/// - Output.key maps to the Kafka message key (partition routing)
/// - Output.headers map to Kafka message headers
pub struct KafkaSink {
    producer: FutureProducer,
    config: KafkaSinkConfig,
    /// Count of successfully delivered outputs.
    delivered: u64,
    /// Count of outputs enqueued but not yet confirmed (UnorderedBatch tracking).
    pending: u64,
}

impl KafkaSink {
    /// Create a new KafkaSink.
    pub fn new(config: KafkaSinkConfig) -> Result<Self, AeonError> {
        let mut client_config = ClientConfig::new();
        client_config
            .set("bootstrap.servers", &config.brokers)
            .set("message.timeout.ms", "30000")
            .set("queue.buffering.max.messages", "100000")
            .set("queue.buffering.max.kbytes", "1048576") // 1GB
            .set("batch.num.messages", "10000")
            .set("linger.ms", "5"); // Small linger for batching

        // Enable idempotent producer for OrderedBatch — guarantees ordering
        // even with multiple in-flight requests.
        if config.strategy.preserves_order() {
            client_config.set("enable.idempotence", "true");
        }

        // Apply user overrides
        for (k, v) in &config.config_overrides {
            client_config.set(k, v);
        }

        let producer: FutureProducer = client_config
            .create()
            .map_err(|e| AeonError::connection(format!("kafka producer create failed: {e}")))?;

        tracing::info!(
            topic = %config.default_topic,
            strategy = ?config.strategy,
            "KafkaSink created"
        );

        Ok(Self {
            producer,
            config,
            delivered: 0,
            pending: 0,
        })
    }

    /// Number of outputs successfully delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Number of outputs enqueued but not yet confirmed (UnorderedBatch).
    pub fn pending(&self) -> u64 {
        self.pending
    }
}

impl Sink for KafkaSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let count = outputs.len();

        // Collect event IDs for BatchResult tracking.
        let event_ids: Vec<_> = outputs.iter().filter_map(|o| o.source_event_id).collect();

        // Enqueue all outputs into rdkafka's internal producer queue.
        // FutureProducer.send() copies data into librdkafka's buffer and returns
        // a future for the delivery confirmation.
        let mut futures = Vec::with_capacity(count);

        for output in &outputs {
            let topic = &self.config.default_topic;
            let mut record = FutureRecord::to(topic).payload(output.payload.as_ref());

            if let Some(ref key) = output.key {
                record = record.key(key.as_ref());
            }

            let owned_headers;
            if !output.headers.is_empty() {
                let mut headers = OwnedHeaders::new();
                for (k, v) in &output.headers {
                    headers = headers.insert(rdkafka::message::Header {
                        key: k.as_ref(),
                        value: Some(v.as_ref().as_bytes()),
                    });
                }
                owned_headers = Some(headers);
            } else {
                owned_headers = None;
            }

            if let Some(headers) = owned_headers {
                record = record.headers(headers);
            }

            let future = self.producer.send(record, self.config.produce_timeout);

            match self.config.strategy {
                DeliveryStrategy::PerEvent => {
                    // Await each delivery future individually.
                    match future.await {
                        Ok(_) => {
                            self.delivered += 1;
                        }
                        Err((e, _)) => {
                            return Err(AeonError::connection(format!(
                                "kafka produce failed: {e}"
                            )));
                        }
                    }
                }
                DeliveryStrategy::OrderedBatch | DeliveryStrategy::UnorderedBatch => {
                    futures.push(future);
                }
            }
        }

        match self.config.strategy {
            DeliveryStrategy::PerEvent => {
                // All futures already awaited above.
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::OrderedBatch => {
                // Await all delivery futures at batch boundary.
                // Idempotent producer guarantees ordering even with pipelining.
                let mut errors = Vec::new();
                for future in futures {
                    match future.await {
                        Ok(_) => {
                            self.delivered += 1;
                        }
                        Err((e, _)) => {
                            errors.push(format!("{e}"));
                        }
                    }
                }

                if errors.is_empty() {
                    Ok(BatchResult::all_delivered(event_ids))
                } else {
                    Err(AeonError::connection(format!(
                        "kafka produce failed for {} messages: {}",
                        errors.len(),
                        errors.first().unwrap_or(&String::new())
                    )))
                }
            }
            DeliveryStrategy::UnorderedBatch => {
                // Drop the futures — rdkafka's internal queue holds the messages.
                // Delivery confirmations happen in the background via librdkafka's
                // polling thread. flush() will wait for all pending deliveries.
                //
                // We intentionally do NOT await futures here — that's the entire
                // point of UnorderedBatch. The data is already in librdkafka's buffer.
                self.pending += count as u64;
                Ok(BatchResult::all_pending(event_ids))
            }
        }
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        // Wait for all messages in rdkafka's internal queue to be delivered.
        // In PerEvent/OrderedBatch, pending is always 0 (futures awaited in write_batch).
        // In UnorderedBatch, this is the checkpoint boundary where acks are collected.
        self.producer
            .flush(self.config.flush_timeout)
            .map_err(|e| AeonError::connection(format!("kafka producer flush failed: {e}")))?;

        // All pending messages are now delivered.
        self.delivered += self.pending;
        self.pending = 0;
        Ok(())
    }
}

/// Helper to create a Redpanda-optimized sink config.
pub fn redpanda_sink_config(
    brokers: impl Into<String>,
    topic: impl Into<String>,
) -> KafkaSinkConfig {
    KafkaSinkConfig::new(brokers, topic)
        .with_config("linger.ms", "1") // Redpanda handles small batches well
        .with_config("batch.num.messages", "10000")
}
