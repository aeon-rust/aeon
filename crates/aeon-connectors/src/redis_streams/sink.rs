//! Redis Streams sink — XADD producer.
//!
//! Each output is added to a Redis Stream as an entry with a "data" field
//! containing the output payload.
//!
//! Supports three delivery strategies (matches Kafka sink contract):
//! - **PerEvent**: each output is sent as a standalone XADD, awaited individually.
//!   One round-trip per output — lowest throughput, strongest per-message guarantee.
//! - **OrderedBatch** (default): all outputs in a `write_batch()` call are bundled
//!   into a single `redis::pipe()` pipeline and executed as one round-trip. Redis
//!   is single-threaded and executes pipelined commands in order, so ordering is
//!   preserved automatically. This is the Redis Streams equivalent of the Kafka
//!   sink's `join_all` batch-boundary fix — it amortizes the round-trip cost over
//!   the whole batch.
//! - **UnorderedBatch**: pipeline execution is spawned as a detached task and the
//!   `JoinHandle` is stashed. `write_batch()` returns immediately with
//!   `BatchResult::all_pending`. `flush()` awaits all stashed tasks and credits
//!   them. Used by the pipeline sink task at flush intervals.

use aeon_types::{AeonError, BatchResult, DeliveryStrategy, Output, Sink};
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
    /// Delivery strategy — controls how write_batch batches XADD calls.
    pub strategy: DeliveryStrategy,
}

impl RedisSinkConfig {
    /// Create a config for writing to a Redis Stream.
    pub fn new(url: impl Into<String>, stream_key: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            stream_key: stream_key.into(),
            max_len: 0,
            strategy: DeliveryStrategy::default(),
        }
    }

    /// Set approximate maximum stream length (trimming with MAXLEN ~).
    pub fn with_max_len(mut self, max_len: usize) -> Self {
        self.max_len = max_len;
        self
    }

    /// Set the delivery strategy.
    pub fn with_strategy(mut self, strategy: DeliveryStrategy) -> Self {
        self.strategy = strategy;
        self
    }
}

/// Redis Streams output sink.
///
/// Uses XADD to append entries. Each output's payload is stored as the
/// "data" field. Output headers are stored as additional fields.
///
/// See the module-level docs for the three-strategy behavior of `write_batch`.
pub struct RedisSink {
    conn: MultiplexedConnection,
    config: RedisSinkConfig,
    /// Count of successfully delivered outputs.
    delivered: u64,
    /// UnorderedBatch: spawned pipeline tasks awaited at `flush()`.
    pending_tasks: Vec<tokio::task::JoinHandle<Result<(), AeonError>>>,
    /// Count of outputs enqueued but not yet confirmed (UnorderedBatch).
    pending: u64,
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

        tracing::info!(
            stream = %config.stream_key,
            strategy = ?config.strategy,
            "RedisSink connected"
        );

        Ok(Self {
            conn,
            config,
            delivered: 0,
            pending_tasks: Vec::new(),
            pending: 0,
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Number of outputs enqueued but not yet confirmed (UnorderedBatch).
    pub fn pending(&self) -> u64 {
        self.pending
    }
}

/// Build a pipeline of XADDs for a set of outputs. Shared by OrderedBatch and
/// UnorderedBatch strategies — the only difference between those two is whether
/// the pipeline is awaited inline or on a background task.
fn build_pipeline(stream_key: &str, max_len: usize, outputs: &[Output]) -> redis::Pipeline {
    let mut pipe = redis::pipe();
    for output in outputs {
        let payload_str = String::from_utf8_lossy(output.payload.as_ref()).into_owned();
        let mut fields: Vec<(String, String)> = Vec::with_capacity(1 + output.headers.len());
        fields.push(("data".to_string(), payload_str));
        for (k, v) in &output.headers {
            fields.push((k.to_string(), v.to_string()));
        }

        let mut cmd = redis::cmd("XADD");
        cmd.arg(stream_key);
        if max_len > 0 {
            cmd.arg("MAXLEN").arg("~").arg(max_len);
        }
        cmd.arg("*");
        for (k, v) in &fields {
            cmd.arg(k.as_str()).arg(v.as_str());
        }
        pipe.add_command(cmd);
    }
    pipe
}

impl Sink for RedisSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let count = outputs.len();
        let event_ids: Vec<_> = outputs.iter().filter_map(|o| o.source_event_id).collect();

        match self.config.strategy {
            DeliveryStrategy::PerEvent => {
                for output in &outputs {
                    let payload_str = String::from_utf8_lossy(output.payload.as_ref());
                    let mut fields: Vec<(&str, &str)> = vec![("data", payload_str.as_ref())];
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
                            .map_err(|e| {
                                AeonError::connection(format!("redis XADD failed: {e}"))
                            })?;
                    } else {
                        let _: String = self
                            .conn
                            .xadd(&self.config.stream_key, "*", &fields)
                            .await
                            .map_err(|e| {
                                AeonError::connection(format!("redis XADD failed: {e}"))
                            })?;
                    }
                    self.delivered += 1;
                }
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::OrderedBatch => {
                // Single pipelined round-trip. Redis is single-threaded and
                // executes pipelined commands in order, so ordering is preserved.
                let pipe = build_pipeline(&self.config.stream_key, self.config.max_len, &outputs);
                let _: Vec<String> = pipe.query_async(&mut self.conn).await.map_err(|e| {
                    AeonError::connection(format!("redis XADD pipeline failed: {e}"))
                })?;
                self.delivered += count as u64;
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::UnorderedBatch => {
                // Spawn the pipeline execution on a background task so
                // `write_batch()` returns immediately without waiting for the
                // round-trip. The `MultiplexedConnection` is clonable and
                // internally multiplexes concurrent commands onto the same
                // TCP connection, so spawning many concurrent tasks is safe.
                // `flush()` awaits all stashed tasks and credits their outputs.
                let pipe = build_pipeline(&self.config.stream_key, self.config.max_len, &outputs);
                let mut conn = self.conn.clone();
                let handle = tokio::spawn(async move {
                    let _: Vec<String> = pipe.query_async(&mut conn).await.map_err(|e| {
                        AeonError::connection(format!("redis XADD pipeline failed: {e}"))
                    })?;
                    Ok(())
                });
                self.pending_tasks.push(handle);
                self.pending += count as u64;
                Ok(BatchResult::all_pending(event_ids))
            }
        }
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        if self.pending_tasks.is_empty() {
            return Ok(());
        }
        let tasks = std::mem::take(&mut self.pending_tasks);
        for handle in tasks {
            handle.await.map_err(|e| {
                AeonError::connection(format!("redis sink task join failed: {e}"))
            })??;
        }
        self.delivered += self.pending;
        self.pending = 0;
        Ok(())
    }
}
