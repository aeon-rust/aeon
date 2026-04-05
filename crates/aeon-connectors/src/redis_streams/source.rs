//! Redis Streams source — XREADGROUP consumer.
//!
//! Uses consumer groups for reliable delivery with acknowledgment.
//! Pull-source: `next_batch()` issues XREADGROUP with COUNT and BLOCK.

use aeon_types::{AeonError, Event, PartitionId, Source};
use bytes::Bytes;
use redis::AsyncCommands;
use redis::aio::MultiplexedConnection;
use redis::streams::{StreamReadOptions, StreamReadReply};
use std::sync::Arc;

/// Configuration for `RedisSource`.
pub struct RedisSourceConfig {
    /// Redis connection URL (e.g., "redis://localhost:6379").
    pub url: String,
    /// Redis Stream key to read from.
    pub stream_key: String,
    /// Consumer group name.
    pub group: String,
    /// Consumer name within the group.
    pub consumer: String,
    /// Maximum messages per `next_batch()`.
    pub batch_size: usize,
    /// Block timeout for XREADGROUP (0 = don't block).
    pub block_ms: usize,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
}

impl RedisSourceConfig {
    /// Create a config for reading from a Redis Stream.
    pub fn new(
        url: impl Into<String>,
        stream_key: impl Into<String>,
        group: impl Into<String>,
        consumer: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            stream_key: stream_key.into(),
            group: group.into(),
            consumer: consumer.into(),
            batch_size: 1024,
            block_ms: 1000,
            source_name: Arc::from("redis"),
        }
    }

    /// Set batch size.
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Set block timeout in milliseconds.
    pub fn with_block_ms(mut self, ms: usize) -> Self {
        self.block_ms = ms;
        self
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }
}

/// Redis Streams event source.
///
/// Uses XREADGROUP for consumer-group-based consumption with at-least-once
/// delivery semantics. Messages are acknowledged after being returned
/// from `next_batch()`.
pub struct RedisSource {
    conn: MultiplexedConnection,
    config: RedisSourceConfig,
    pending_ack_ids: Vec<String>,
}

impl RedisSource {
    /// Connect to Redis and ensure the consumer group exists.
    pub async fn new(config: RedisSourceConfig) -> Result<Self, AeonError> {
        let client = redis::Client::open(config.url.as_str())
            .map_err(|e| AeonError::connection(format!("redis client create failed: {e}")))?;

        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AeonError::connection(format!("redis connect failed: {e}")))?;

        // Create consumer group (ignore error if it already exists)
        let result: redis::RedisResult<String> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(&config.stream_key)
            .arg(&config.group)
            .arg("0")
            .arg("MKSTREAM")
            .query_async(&mut conn)
            .await;

        match result {
            Ok(_) => {
                tracing::info!(
                    stream = %config.stream_key,
                    group = %config.group,
                    "RedisSource created consumer group"
                );
            }
            Err(e) => {
                // BUSYGROUP = group already exists, that's fine
                let msg = format!("{e}");
                if !msg.contains("BUSYGROUP") {
                    return Err(AeonError::connection(format!(
                        "redis XGROUP CREATE failed: {e}"
                    )));
                }
            }
        }

        tracing::info!(
            stream = %config.stream_key,
            group = %config.group,
            consumer = %config.consumer,
            "RedisSource connected"
        );

        Ok(Self {
            conn,
            config,
            pending_ack_ids: Vec::new(),
        })
    }

    /// Acknowledge previously read messages.
    async fn ack_pending(&mut self) -> Result<(), AeonError> {
        if self.pending_ack_ids.is_empty() {
            return Ok(());
        }

        let ids: Vec<&str> = self.pending_ack_ids.iter().map(|s| s.as_str()).collect();
        let _: u64 = self
            .conn
            .xack(&self.config.stream_key, &self.config.group, &ids)
            .await
            .map_err(|e| AeonError::connection(format!("redis XACK failed: {e}")))?;

        self.pending_ack_ids.clear();
        Ok(())
    }
}

impl Source for RedisSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // Acknowledge previous batch before reading new one
        self.ack_pending().await?;

        let opts = StreamReadOptions::default()
            .count(self.config.batch_size)
            .block(self.config.block_ms)
            .group(&self.config.group, &self.config.consumer);

        let reply: StreamReadReply = self
            .conn
            .xread_options(&[&self.config.stream_key], &[">"], &opts)
            .await
            .map_err(|e| AeonError::connection(format!("redis XREADGROUP failed: {e}")))?;

        let mut events = Vec::new();
        let now = std::time::Instant::now();

        for stream_key in &reply.keys {
            for entry in &stream_key.ids {
                // Extract "data" field as payload
                let payload: Option<String> = entry.get("data");
                let payload_bytes = match payload {
                    Some(data) => Bytes::from(data.into_bytes()),
                    None => {
                        // Try to serialize all fields as JSON-like string
                        let mut parts = Vec::new();
                        for (k, v) in &entry.map {
                            if let redis::Value::BulkString(bytes) = v {
                                parts.push(format!("{}={}", k, String::from_utf8_lossy(bytes)));
                            }
                        }
                        Bytes::from(parts.join(","))
                    }
                };

                let mut event = Event::new(
                    uuid::Uuid::nil(),
                    0,
                    Arc::clone(&self.config.source_name),
                    PartitionId::new(0),
                    payload_bytes,
                );
                event = event.with_source_ts(now);
                events.push(event);

                self.pending_ack_ids.push(entry.id.clone());
            }
        }

        Ok(events)
    }
}
