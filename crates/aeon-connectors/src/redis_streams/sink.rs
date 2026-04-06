//! Redis Streams sink — XADD producer.
//!
//! Each output is added to a Redis Stream as an entry with a "data" field
//! containing the output payload.

use aeon_types::{AeonError, BatchResult, Output, Sink};
use redis::AsyncCommands;
use redis::aio::MultiplexedConnection;

/// Configuration for `RedisSink`.
pub struct RedisSinkConfig {
    /// Redis connection URL (e.g., "redis://localhost:6379").
    pub url: String,
    /// Redis Stream key to write to.
    pub stream_key: String,
    /// Maximum stream length (MAXLEN ~). 0 = unbounded.
    pub max_len: usize,
}

impl RedisSinkConfig {
    /// Create a config for writing to a Redis Stream.
    pub fn new(url: impl Into<String>, stream_key: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            stream_key: stream_key.into(),
            max_len: 0,
        }
    }

    /// Set approximate maximum stream length (trimming with MAXLEN ~).
    pub fn with_max_len(mut self, max_len: usize) -> Self {
        self.max_len = max_len;
        self
    }
}

/// Redis Streams output sink.
///
/// Uses XADD to append entries. Each output's payload is stored as the
/// "data" field. Output headers are stored as additional fields.
pub struct RedisSink {
    conn: MultiplexedConnection,
    config: RedisSinkConfig,
    delivered: u64,
}

impl RedisSink {
    /// Connect to Redis.
    pub async fn new(config: RedisSinkConfig) -> Result<Self, AeonError> {
        let client = redis::Client::open(config.url.as_str())
            .map_err(|e| AeonError::connection(format!("redis client create failed: {e}")))?;

        let conn = client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| AeonError::connection(format!("redis connect failed: {e}")))?;

        tracing::info!(stream = %config.stream_key, "RedisSink connected");

        Ok(Self {
            conn,
            config,
            delivered: 0,
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}

impl Sink for RedisSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let ids: Vec<_> = outputs.iter().filter_map(|o| o.source_event_id).collect();
        for output in &outputs {
            let payload_str = String::from_utf8_lossy(output.payload.as_ref());

            // Build field-value pairs: always include "data" field
            let mut fields: Vec<(&str, &str)> = vec![("data", payload_str.as_ref())];

            // Add headers as additional fields
            let header_strings: Vec<(String, String)> = output
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();

            for (k, v) in &header_strings {
                fields.push((k.as_str(), v.as_str()));
            }

            if self.config.max_len > 0 {
                let _: String = redis::cmd("XADD")
                    .arg(&self.config.stream_key)
                    .arg("MAXLEN")
                    .arg("~")
                    .arg(self.config.max_len)
                    .arg("*")
                    .arg(&fields)
                    .query_async(&mut self.conn)
                    .await
                    .map_err(|e| AeonError::connection(format!("redis XADD failed: {e}")))?;
            } else {
                let _: String = self
                    .conn
                    .xadd(&self.config.stream_key, "*", &fields)
                    .await
                    .map_err(|e| AeonError::connection(format!("redis XADD failed: {e}")))?;
            }

            self.delivered += 1;
        }

        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        // Redis commands are synchronous at the protocol level — no buffering to flush
        Ok(())
    }
}
