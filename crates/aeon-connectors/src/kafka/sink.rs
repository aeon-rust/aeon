//! Kafka/Redpanda sink — batch produce with FutureProducer.
//!
//! Uses `FutureProducer` for async batch sending with delivery confirmation.
//! All outputs in a `write_batch()` call are enqueued, then awaited together.

use aeon_types::{AeonError, Output, Sink};
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
    /// Optional: additional rdkafka config overrides.
    pub config_overrides: Vec<(String, String)>,
}

impl KafkaSinkConfig {
    /// Create a sink config targeting a specific topic.
    pub fn new(brokers: impl Into<String>, topic: impl Into<String>) -> Self {
        Self {
            brokers: brokers.into(),
            default_topic: topic.into(),
            produce_timeout: Duration::from_secs(5),
            config_overrides: Vec::new(),
        }
    }

    /// Set the produce timeout.
    pub fn with_produce_timeout(mut self, timeout: Duration) -> Self {
        self.produce_timeout = timeout;
        self
    }

    /// Add an rdkafka config override.
    pub fn with_config(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config_overrides.push((key.into(), value.into()));
        self
    }
}

/// Kafka/Redpanda output sink using FutureProducer.
///
/// - Batch produce: all outputs in `write_batch()` are enqueued, then delivery
///   futures are awaited together
/// - Output.destination maps to the Kafka topic (falls back to default_topic)
/// - Output.key maps to the Kafka message key (partition routing)
/// - Output.headers map to Kafka message headers
pub struct KafkaSink {
    producer: FutureProducer,
    config: KafkaSinkConfig,
    /// Count of successfully delivered outputs.
    delivered: u64,
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

        // Apply user overrides
        for (k, v) in &config.config_overrides {
            client_config.set(k, v);
        }

        let producer: FutureProducer = client_config
            .create()
            .map_err(|e| AeonError::connection(format!("kafka producer create failed: {e}")))?;

        tracing::info!(
            topic = %config.default_topic,
            "KafkaSink created"
        );

        Ok(Self {
            producer,
            config,
            delivered: 0,
        })
    }

    /// Number of outputs successfully delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}

impl Sink for KafkaSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<(), AeonError> {
        let mut futures = Vec::with_capacity(outputs.len());

        for output in &outputs {
            let topic = &self.config.default_topic;

            // Build the record
            let mut record = FutureRecord::to(topic).payload(output.payload.as_ref());

            // Set key if present
            if let Some(ref key) = output.key {
                record = record.key(key.as_ref());
            }

            // Set headers if present
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
            futures.push(future);
        }

        // Await all delivery futures
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
            Ok(())
        } else {
            Err(AeonError::connection(format!(
                "kafka produce failed for {} messages: {}",
                errors.len(),
                errors.first().unwrap_or(&String::new())
            )))
        }
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        self.producer
            .flush(Duration::from_secs(30))
            .map_err(|e| AeonError::connection(format!("kafka producer flush failed: {e}")))
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
