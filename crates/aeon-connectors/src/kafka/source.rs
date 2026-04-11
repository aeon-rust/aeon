//! Kafka/Redpanda source — manual partition assignment, batch polling.
//!
//! Uses `StreamConsumer` for async integration with tokio.
//! Manual `assign()` instead of `subscribe()` — Aeon manages partition ownership.

use aeon_types::{AeonError, CoreLocalUuidGenerator, Event, PartitionId, Source};
use bytes::Bytes;
use rdkafka::TopicPartitionList;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::{Headers, Message};
use smallvec::SmallVec;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

/// Configuration for `KafkaSource`.
pub struct KafkaSourceConfig {
    /// Kafka/Redpanda broker addresses (e.g., "localhost:19092").
    pub brokers: String,
    /// Topic to consume from.
    pub topic: String,
    /// Partitions to manually assign.
    pub partitions: Vec<i32>,
    /// Maximum messages per `next_batch()` call.
    pub batch_max_messages: usize,
    /// Timeout waiting for the first message of a batch.
    pub poll_timeout: Duration,
    /// Timeout for draining additional messages within a batch.
    pub drain_timeout: Duration,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Consumer group ID. Required by rdkafka even for manual assign.
    /// Default: "aeon-manual".
    pub group_id: String,
    /// Max consecutive empty polls before the source signals exhaustion.
    /// Default: 10. Set higher for continuous streaming, lower for finite tests.
    pub max_empty_polls: u32,
    /// Optional: additional rdkafka config overrides.
    pub config_overrides: Vec<(String, String)>,
}

impl KafkaSourceConfig {
    /// Create a config for consuming from a single topic with given partitions.
    pub fn new(brokers: impl Into<String>, topic: impl Into<String>) -> Self {
        Self {
            brokers: brokers.into(),
            topic: topic.into(),
            partitions: vec![0],
            batch_max_messages: 1024,
            poll_timeout: Duration::from_secs(1),
            drain_timeout: Duration::from_millis(10),
            source_name: Arc::from("kafka"),
            group_id: "aeon-manual".to_string(),
            max_empty_polls: 10,
            config_overrides: Vec::new(),
        }
    }

    /// Set which partitions to consume (manual assignment).
    pub fn with_partitions(mut self, partitions: Vec<i32>) -> Self {
        self.partitions = partitions;
        self
    }

    /// Set maximum messages per batch.
    pub fn with_batch_max(mut self, max: usize) -> Self {
        self.batch_max_messages = max;
        self
    }

    /// Set the first-message poll timeout.
    pub fn with_poll_timeout(mut self, timeout: Duration) -> Self {
        self.poll_timeout = timeout;
        self
    }

    /// Set the drain timeout for filling the rest of the batch after the first message.
    pub fn with_drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Set the source name used in Event.source.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Set the consumer group ID (required by rdkafka even for manual assign).
    pub fn with_group_id(mut self, group_id: impl Into<String>) -> Self {
        self.group_id = group_id.into();
        self
    }

    /// Set max consecutive empty polls before signaling exhaustion.
    pub fn with_max_empty_polls(mut self, max: u32) -> Self {
        self.max_empty_polls = max;
        self
    }

    /// Add an rdkafka config override (e.g., "fetch.min.bytes", "1024").
    pub fn with_config(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config_overrides.push((key.into(), value.into()));
        self
    }
}

/// Kafka/Redpanda event source using manual partition assignment.
///
/// - Uses `StreamConsumer` for async tokio integration
/// - `assign()` instead of `subscribe()` — no consumer group rebalance
/// - Batch polling: waits for first message, then drains available messages
///   within `drain_timeout` up to `batch_max_messages`
/// - Zero-copy payloads via `Bytes::copy_from_slice` (rdkafka owns the buffer,
///   so we must copy once — but the copy stays in Bytes for downstream zero-copy)
pub struct KafkaSource {
    consumer: StreamConsumer,
    config: KafkaSourceConfig,
    /// Tracks consecutive empty polls for exhaustion detection.
    consecutive_empty_polls: u32,
    /// Max consecutive empty polls before returning empty batch (source "done").
    max_empty_polls: u32,
    /// Per-source UUIDv7 generator (SPSC pool, ~1-2ns per UUID).
    /// Mutex for Sync (Source: Send + Sync), only accessed in next_batch.
    uuid_gen: Mutex<CoreLocalUuidGenerator>,
    /// When true, next_batch() returns empty immediately (drain mechanism).
    paused: bool,
}

impl KafkaSource {
    /// Create a new KafkaSource and assign partitions.
    pub fn new(config: KafkaSourceConfig) -> Result<Self, AeonError> {
        let mut client_config = ClientConfig::new();
        client_config
            .set("bootstrap.servers", &config.brokers)
            .set("group.id", &config.group_id)
            .set("enable.auto.commit", "false") // Aeon manages offsets
            .set("auto.offset.reset", "earliest")
            .set("fetch.min.bytes", "1")
            .set("fetch.wait.max.ms", "100")
            .set("queued.min.messages", "100000") // Pre-fetch aggressively
            .set("queued.max.messages.kbytes", "65536"); // 64MB pre-fetch buffer

        // Apply user overrides
        for (k, v) in &config.config_overrides {
            client_config.set(k, v);
        }

        let consumer: StreamConsumer = client_config
            .create()
            .map_err(|e| AeonError::connection(format!("kafka consumer create failed: {e}")))?;

        // Manual partition assignment.
        // With enable.auto.commit=false and auto.offset.reset=earliest,
        // the consumer starts from the beginning when no committed offset exists.
        let mut tpl = TopicPartitionList::new();
        for &partition in &config.partitions {
            tpl.add_partition(&config.topic, partition);
        }
        consumer
            .assign(&tpl)
            .map_err(|e| AeonError::connection(format!("kafka partition assign failed: {e}")))?;

        tracing::info!(
            topic = %config.topic,
            partitions = ?config.partitions,
            batch_max = config.batch_max_messages,
            "KafkaSource assigned partitions"
        );

        // Use core_id 0 for UUID generation. In multi-partition pipelines,
        // each partition's KafkaSource will run on its own core — but the core_id
        // for UUID generation is about uniqueness, not affinity. Core 0 is safe
        // for single-source; multi-partition uses distinct generators per partition.
        let uuid_gen = CoreLocalUuidGenerator::new(0);

        let max_empty_polls = config.max_empty_polls;
        Ok(Self {
            consumer,
            config,
            consecutive_empty_polls: 0,
            max_empty_polls,
            uuid_gen: Mutex::new(uuid_gen),
            paused: false,
        })
    }

    /// Convert a borrowed rdkafka message into an owned Event.
    fn msg_to_event(
        &self,
        msg: &rdkafka::message::BorrowedMessage<'_>,
    ) -> Result<Event, AeonError> {
        // rdkafka owns the buffer, so we copy into Bytes once.
        // Downstream the Bytes is zero-copy (Arc-based).
        let payload = match msg.payload() {
            Some(data) => Bytes::copy_from_slice(data),
            None => Bytes::new(), // Tombstone / null payload
        };

        let partition = PartitionId::new(msg.partition() as u16);
        let timestamp = msg.timestamp().to_millis().unwrap_or(0) * 1_000_000; // ms → ns

        let event_id = self
            .uuid_gen
            .lock()
            .map_err(|_| AeonError::connection("UUID generator mutex poisoned"))?
            .next_uuid();

        let mut event = Event::new(
            event_id,
            timestamp,
            Arc::clone(&self.config.source_name),
            partition,
            payload,
        );

        // Store Kafka offset for checkpoint resume position
        event.source_offset = Some(msg.offset());

        // Stamp ingestion time for E2E latency measurement
        event = event.with_source_ts(Instant::now());

        // Propagate Kafka headers as event metadata
        if let Some(headers) = msg.headers() {
            let mut metadata = SmallVec::new();
            for header in headers.iter() {
                let key: Arc<str> = Arc::from(header.key);
                let value: Arc<str> = match header.value {
                    Some(v) => Arc::from(String::from_utf8_lossy(v).as_ref()),
                    None => Arc::from(""),
                };
                metadata.push((key, value));
            }
            event.metadata = metadata;
        }

        Ok(event)
    }
}

impl Source for KafkaSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        if self.paused {
            return Ok(Vec::new());
        }

        let mut events = Vec::with_capacity(self.config.batch_max_messages);

        // Step 1: Wait for the first message with full timeout.
        // This handles connection bootstrapping and idle periods.
        let first = tokio::time::timeout(self.config.poll_timeout, self.consumer.recv()).await;

        match first {
            Ok(Ok(msg)) => {
                self.consecutive_empty_polls = 0;
                events.push(self.msg_to_event(&msg)?);
            }
            Ok(Err(e)) => {
                return Err(AeonError::connection(format!("kafka recv error: {e}")));
            }
            Err(_) => {
                // Timeout — no messages available
                self.consecutive_empty_polls += 1;
                if self.consecutive_empty_polls >= self.max_empty_polls {
                    return Ok(Vec::new()); // Signal exhaustion
                }
                return Ok(events); // Empty batch (lull)
            }
        }

        // Step 2: Drain additional messages with short timeout.
        // After receiving one message, rapidly consume more to fill the batch.
        while events.len() < self.config.batch_max_messages {
            let drain = tokio::time::timeout(self.config.drain_timeout, self.consumer.recv()).await;

            match drain {
                Ok(Ok(msg)) => {
                    events.push(self.msg_to_event(&msg)?);
                }
                Ok(Err(e)) => {
                    return Err(AeonError::connection(format!("kafka recv error: {e}")));
                }
                Err(_) => break, // Drain timeout — return what we have
            }
        }

        Ok(events)
    }

    async fn pause(&mut self) {
        self.paused = true;
    }

    async fn resume(&mut self) {
        self.paused = false;
    }
}

/// Helper to create a Redpanda-optimized source config.
///
/// Same as Kafka but with Redpanda-friendly defaults:
/// - Larger batch size and aggressive pre-fetching
/// - Short drain timeout for batch filling
pub fn redpanda_source_config(
    brokers: impl Into<String>,
    topic: impl Into<String>,
) -> KafkaSourceConfig {
    KafkaSourceConfig::new(brokers, topic)
        .with_config("fetch.wait.max.ms", "50")
        .with_config("fetch.min.bytes", "1")
        .with_batch_max(2048)
        .with_drain_timeout(Duration::from_millis(5))
}
