//! Redis Streams source — XREADGROUP consumer.
//!
//! Uses consumer groups for reliable delivery with acknowledgment.
//! Pull-source: `next_batch()` issues XREADGROUP with COUNT and BLOCK.

use aeon_types::{AeonError, Backoff, BackoffPolicy, CoreLocalUuidGenerator, Event, PartitionId, Source};
use bytes::Bytes;
use redis::AsyncCommands;
use redis::aio::MultiplexedConnection;
use redis::streams::{StreamReadOptions, StreamReadReply};
use std::sync::{Arc, Mutex};

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
    /// TR-3 reconnect backoff. On XREADGROUP / XACK failure (broker down,
    /// network partition), the source drops its connection, sleeps for the
    /// current delay, and returns an empty batch so the pipeline stays alive.
    /// The next `next_batch()` rebuilds the connection and retries.
    pub backoff: BackoffPolicy,
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
            backoff: BackoffPolicy::default(),
        }
    }

    /// Override the reconnect backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
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
    client: redis::Client,
    /// Lazy connection — None after a network error, rebuilt on next
    /// `next_batch()`. Start fully populated after `new()` succeeds.
    conn: Option<MultiplexedConnection>,
    config: RedisSourceConfig,
    pending_ack_ids: Vec<String>,
    backoff: Backoff,
    uuid_gen: Mutex<CoreLocalUuidGenerator>,
}

impl RedisSource {
    /// Connect to Redis and ensure the consumer group exists.
    pub async fn new(config: RedisSourceConfig) -> Result<Self, AeonError> {
        let client = redis::Client::open(config.url.as_str())
            .map_err(|e| AeonError::connection(format!("redis client create failed: {e}")))?;

        // Initial connect is validated synchronously so misconfiguration
        // (bad URL, wrong auth) surfaces on startup. Runtime failures later
        // fall into the backoff path.
        let conn = Self::establish(&client, &config).await?;

        tracing::info!(
            stream = %config.stream_key,
            group = %config.group,
            consumer = %config.consumer,
            "RedisSource connected"
        );

        let backoff = Backoff::new(config.backoff);
        Ok(Self {
            client,
            conn: Some(conn),
            config,
            pending_ack_ids: Vec::new(),
            backoff,
            uuid_gen: Mutex::new(CoreLocalUuidGenerator::new(0)),
        })
    }

    /// Build a fresh connection and (re-)ensure the consumer group exists.
    ///
    /// Used both by the synchronous initial connect and by the reconnect path
    /// on transient failure. Idempotent: `XGROUP CREATE` yields BUSYGROUP on
    /// repeat calls which is treated as success.
    async fn establish(
        client: &redis::Client,
        config: &RedisSourceConfig,
    ) -> Result<MultiplexedConnection, AeonError> {
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AeonError::connection(format!("redis connect failed: {e}")))?;

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
                let msg = format!("{e}");
                if !msg.contains("BUSYGROUP") {
                    return Err(AeonError::connection(format!(
                        "redis XGROUP CREATE failed: {e}"
                    )));
                }
            }
        }

        Ok(conn)
    }

    /// Acknowledge previously read messages.
    async fn ack_pending(&mut self) -> Result<(), AeonError> {
        if self.pending_ack_ids.is_empty() {
            return Ok(());
        }

        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| AeonError::connection("redis conn not established"))?;
        let ids: Vec<&str> = self.pending_ack_ids.iter().map(|s| s.as_str()).collect();
        let _: u64 = conn
            .xack(&self.config.stream_key, &self.config.group, &ids)
            .await
            .map_err(|e| AeonError::connection(format!("redis XACK failed: {e}")))?;

        self.pending_ack_ids.clear();
        Ok(())
    }
}

impl Source for RedisSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // TR-3: if we don't have a live connection, try to rebuild it.
        // On failure, back off and return an empty batch so the pipeline
        // stays alive across Redis restarts / transient network drops.
        if self.conn.is_none() {
            match Self::establish(&self.client, &self.config).await {
                Ok(c) => {
                    tracing::info!(stream = %self.config.stream_key, "redis reconnected");
                    self.conn = Some(c);
                    self.backoff.reset();
                    // Pending acks from the pre-disconnect batch are gone
                    // with the dropped connection — Redis will redeliver via
                    // the consumer group's PEL. Clear our local tracker.
                    self.pending_ack_ids.clear();
                }
                Err(e) => {
                    let delay = self.backoff.next_delay();
                    tracing::warn!(
                        error = %e,
                        delay_ms = delay.as_millis() as u64,
                        "redis reconnect failed, backing off"
                    );
                    tokio::time::sleep(delay).await;
                    return Ok(Vec::new());
                }
            }
        }

        // Best-effort ack. On failure, drop the connection so the next call
        // reconnects; unacked IDs will be redelivered via the PEL.
        if let Err(e) = self.ack_pending().await {
            let delay = self.backoff.next_delay();
            tracing::warn!(
                error = %e,
                delay_ms = delay.as_millis() as u64,
                "redis XACK failed, dropping connection and backing off"
            );
            self.conn = None;
            tokio::time::sleep(delay).await;
            return Ok(Vec::new());
        }

        let opts = StreamReadOptions::default()
            .count(self.config.batch_size)
            .block(self.config.block_ms)
            .group(&self.config.group, &self.config.consumer);

        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| AeonError::connection("redis conn lost between reconnect and read"))?;
        let reply: StreamReadReply = match conn
            .xread_options(&[&self.config.stream_key], &[">"], &opts)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let delay = self.backoff.next_delay();
                tracing::warn!(
                    error = %e,
                    delay_ms = delay.as_millis() as u64,
                    "redis XREADGROUP failed, dropping connection and backing off"
                );
                self.conn = None;
                tokio::time::sleep(delay).await;
                return Ok(Vec::new());
            }
        };

        // Successful read — reset backoff.
        self.backoff.reset();

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

                let event_id = self
                    .uuid_gen
                    .lock()
                    .map_err(|_| AeonError::connection("UUID generator mutex poisoned"))?
                    .next_uuid();
                let mut event = Event::new(
                    event_id,
                    0,
                    Arc::clone(&self.config.source_name),
                    PartitionId::new(0),
                    payload_bytes,
                );
                event = event.with_source_ts(now);
                // B2: encode Redis stream entry ID "<ms>-<seq>" into a
                // monotonic i64 for checkpoint persistence. High 48 bits
                // hold the ms timestamp, low 16 bits hold the intra-ms seq.
                // Actual resume is via consumer-group PEL (XACK); this
                // offset is for observability and checkpoint ordering.
                if let Some(offset) = parse_stream_id_to_i64(&entry.id) {
                    event = event.with_source_offset(offset);
                }
                events.push(event);

                self.pending_ack_ids.push(entry.id.clone());
            }
        }

        Ok(events)
    }
}

/// Pack a Redis stream entry ID `"<ms>-<seq>"` into a monotonic `i64`.
/// Returns `None` if the ID is malformed.
///
/// Layout: `[ms: high 48 bits][seq: low 16 bits]`. The bit layout is
/// internal — consumers should treat the value as an opaque monotonic
/// ordering key for checkpoint purposes. `seq` saturates at 0xFFFF; in
/// the pathological case of >65k entries at the same ms the ordering
/// between late entries is lost but the checkpoint remains monotonic.
fn parse_stream_id_to_i64(id: &str) -> Option<i64> {
    let (ms_s, seq_s) = id.split_once('-')?;
    let ms: u64 = ms_s.parse().ok()?;
    let seq: u64 = seq_s.parse().ok()?;
    let packed = (ms << 16) | (seq.min(0xFFFF));
    // Clear the sign bit to keep the result non-negative as an i64.
    Some((packed & 0x7FFF_FFFF_FFFF_FFFF) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stream_id_round_trips_ordering() {
        let a = parse_stream_id_to_i64("1713456789123-0").unwrap();
        let b = parse_stream_id_to_i64("1713456789123-1").unwrap();
        let c = parse_stream_id_to_i64("1713456789124-0").unwrap();
        assert!(a < b, "seq monotonic within same ms");
        assert!(b < c, "later ms dominates");
    }

    #[test]
    fn parse_stream_id_malformed_returns_none() {
        assert!(parse_stream_id_to_i64("nope").is_none());
        assert!(parse_stream_id_to_i64("123").is_none());
        assert!(parse_stream_id_to_i64("abc-0").is_none());
    }

    #[test]
    fn parse_stream_id_saturates_large_seq() {
        let a = parse_stream_id_to_i64("1-65535").unwrap();
        let b = parse_stream_id_to_i64("1-99999").unwrap();
        assert_eq!(a, b, "seq saturates at 0xFFFF");
    }
}
