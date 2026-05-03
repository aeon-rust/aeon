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

use aeon_types::{
    AeonError, BatchResult, DeliveryStrategy, IdempotentSink, OutboundAuthSigner, Output, Sink,
    SinkAckCallback,
};
use redis::aio::MultiplexedConnection;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

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
    /// EO-3: Dedup TTL for seen event IDs. When `Some`, each output with a
    /// `source_event_id` emits a `SET <dedup_key> 1 EX <ttl> NX` alongside
    /// its XADD in the same pipelined round-trip; `has_seen` does an EXISTS
    /// on the same key. When `None`, dedup is disabled and `has_seen`
    /// returns `Ok(false)` unconditionally.
    ///
    /// Unlike Kafka/NATS which dedup server-side via producer epoch or
    /// duplicate-window, Redis has no stream-level dedup primitive — so we
    /// track seen IDs ourselves under a keyspace that expires after `ttl`,
    /// bounding memory use. Pick a TTL larger than the maximum expected
    /// replay window (e.g. the replay-orchestrator window from TR-1).
    pub dedup_ttl: Option<Duration>,
    /// Key prefix for dedup records. Full key is `{prefix}{stream_key}:{event_id}`.
    /// Defaults to `"aeon:dedup:"`. Only used when `dedup_ttl` is set.
    pub dedup_key_prefix: String,
    /// S10 outbound auth. See `RedisSourceConfig::auth`.
    pub auth: Option<Arc<OutboundAuthSigner>>,
}

impl RedisSinkConfig {
    /// Create a config for writing to a Redis Stream.
    pub fn new(url: impl Into<String>, stream_key: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            stream_key: stream_key.into(),
            max_len: 0,
            strategy: DeliveryStrategy::default(),
            dedup_ttl: None,
            dedup_key_prefix: "aeon:dedup:".to_string(),
            auth: None,
        }
    }

    /// S10: attach an outbound-auth signer.
    pub fn with_auth(mut self, signer: Arc<OutboundAuthSigner>) -> Self {
        self.auth = Some(signer);
        self
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

    /// EO-3: Enable SETNX-based dedup with the given TTL for seen event IDs.
    /// See [`RedisSinkConfig::dedup_ttl`] for semantics. Pick a TTL larger
    /// than the maximum expected replay window.
    pub fn with_dedup_ttl(mut self, ttl: Duration) -> Self {
        self.dedup_ttl = Some(ttl);
        self
    }

    /// Override the dedup key prefix (default `"aeon:dedup:"`).
    pub fn with_dedup_key_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.dedup_key_prefix = prefix.into();
        self
    }

    /// Build the dedup key for a given event ID under this sink's configured
    /// stream + prefix. Shared by the write path and `has_seen` so the two
    /// never disagree on the keyspace.
    pub(crate) fn dedup_key(&self, event_id: &Uuid) -> String {
        format!("{}{}:{}", self.dedup_key_prefix, self.stream_key, event_id)
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
    /// Engine-installed callback fired when XADD round-trips succeed. Drives
    /// the `outputs_acked_total` companion metric so the operator can see the
    /// gap between `outputs_sent` (write_batch calls) and broker-confirmed
    /// delivery. UnorderedBatch fires on flush, not on enqueue.
    ack_callback: Option<SinkAckCallback>,
}

impl RedisSink {
    /// Connect to Redis.
    pub async fn new(config: RedisSinkConfig) -> Result<Self, AeonError> {
        let client = super::auth::resolve_client(&config.url, config.auth.as_ref())?;

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
            ack_callback: None,
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

    /// Fire the engine-installed ack callback. Shared by PerEvent/OrderedBatch
    /// write paths and the UnorderedBatch flush drain.
    fn fire_ack(&self, n: usize) {
        if n == 0 {
            return;
        }
        if let Some(cb) = self.ack_callback.as_ref() {
            cb(n);
        }
    }
}

/// Build a pipeline of XADDs for a set of outputs. Shared by OrderedBatch and
/// UnorderedBatch strategies — the only difference between those two is whether
/// the pipeline is awaited inline or on a background task.
///
/// When `config.dedup_ttl` is `Some`, each output carrying a `source_event_id`
/// gets a preceding `SET <dedup_key> 1 EX <ttl> NX` command in the same
/// pipeline — one round-trip still, but the dedup record and the stream entry
/// are always either both present or both absent (Redis executes pipelined
/// commands atomically relative to other clients). This is the write-side
/// half of the `IdempotentSink` contract: `has_seen` on the same key will
/// return true on the next attempt.
fn build_pipeline(config: &RedisSinkConfig, outputs: &[Output]) -> redis::Pipeline {
    let mut pipe = redis::pipe();
    for output in outputs {
        // EO-3: mark the event as seen before the XADD. If dedup is off or
        // the output has no `source_event_id`, we skip this step.
        if let (Some(ttl), Some(id)) = (config.dedup_ttl, output.source_event_id) {
            let mut set_cmd = redis::cmd("SET");
            set_cmd
                .arg(config.dedup_key(&id))
                .arg(1)
                .arg("EX")
                .arg(ttl.as_secs().max(1))
                .arg("NX");
            pipe.add_command(set_cmd);
        }

        let payload_str = String::from_utf8_lossy(output.payload.as_ref()).into_owned();
        let mut fields: Vec<(String, String)> = Vec::with_capacity(1 + output.headers.len());
        fields.push(("data".to_string(), payload_str));
        for (k, v) in &output.headers {
            fields.push((k.to_string(), v.to_string()));
        }

        let mut cmd = redis::cmd("XADD");
        cmd.arg(&config.stream_key);
        if config.max_len > 0 {
            cmd.arg("MAXLEN").arg("~").arg(config.max_len);
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
                // Route PerEvent through the same pipeline builder as the
                // batch paths so the dedup SET+XADD stays atomic per-output.
                // Single-element pipeline is still one round-trip.
                for output in &outputs {
                    let pipe = build_pipeline(&self.config, std::slice::from_ref(output));
                    let _: redis::Value = pipe
                        .query_async(&mut self.conn)
                        .await
                        .map_err(|e| AeonError::connection(format!("redis XADD failed: {e}")))?;
                    self.delivered += 1;
                }
                self.fire_ack(count);
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::OrderedBatch => {
                // Single pipelined round-trip. Redis is single-threaded and
                // executes pipelined commands in order, so ordering is preserved.
                let pipe = build_pipeline(&self.config, &outputs);
                let _: redis::Value = pipe.query_async(&mut self.conn).await.map_err(|e| {
                    AeonError::connection(format!("redis XADD pipeline failed: {e}"))
                })?;
                self.delivered += count as u64;
                self.fire_ack(count);
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::UnorderedBatch => {
                // Spawn the pipeline execution on a background task so
                // `write_batch()` returns immediately without waiting for the
                // round-trip. The `MultiplexedConnection` is clonable and
                // internally multiplexes concurrent commands onto the same
                // TCP connection, so spawning many concurrent tasks is safe.
                // `flush()` awaits all stashed tasks and credits their outputs.
                let pipe = build_pipeline(&self.config, &outputs);
                let mut conn = self.conn.clone();
                let handle = tokio::spawn(async move {
                    let _: redis::Value = pipe.query_async(&mut conn).await.map_err(|e| {
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
        let newly_acked = self.pending;
        self.delivered += newly_acked;
        self.pending = 0;
        self.fire_ack(newly_acked as usize);
        Ok(())
    }

    fn on_ack_callback(&mut self, cb: SinkAckCallback) {
        self.ack_callback = Some(cb);
    }
}

/// EO-3: `IdempotentSink` on `RedisSink` — client-side dedup via SETNX.
///
/// Unlike Kafka (producer epoch) or NATS JetStream (stream duplicate window),
/// Redis Streams has no built-in dedup primitive. We build one on top of
/// regular Redis keys: each `write_batch` pipelines a
/// `SET <prefix><stream>:<event_id> 1 EX <ttl> NX` command before the XADD,
/// so the dedup record and the stream entry are created in the same
/// round-trip and subject to the same atomicity guarantees as any Redis
/// pipeline. `has_seen` returns `true` iff that key currently exists.
///
/// Requires `dedup_ttl` to be set on the config — without it, `has_seen`
/// always returns `Ok(false)` (no keyspace to consult) and the write path
/// degrades to a plain XADD.
impl IdempotentSink for RedisSink {
    async fn has_seen(&self, event_id: &Uuid) -> Result<bool, AeonError> {
        if self.config.dedup_ttl.is_none() {
            return Ok(false);
        }
        // MultiplexedConnection is Arc-internal, so this clone is cheap and
        // lets us issue the EXISTS without needing `&mut self`.
        let mut conn = self.conn.clone();
        let key = self.config.dedup_key(event_id);
        let exists: i32 = redis::cmd("EXISTS")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|e| AeonError::connection(format!("redis EXISTS failed: {e}")))?;
        Ok(exists == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_dedup_off() {
        let cfg = RedisSinkConfig::new("redis://localhost:6379", "events");
        assert!(cfg.dedup_ttl.is_none());
        assert_eq!(cfg.dedup_key_prefix, "aeon:dedup:");
    }

    #[test]
    fn with_dedup_ttl_sets_field() {
        let cfg = RedisSinkConfig::new("redis://localhost:6379", "events")
            .with_dedup_ttl(Duration::from_secs(60));
        assert_eq!(cfg.dedup_ttl, Some(Duration::from_secs(60)));
    }

    #[test]
    fn dedup_key_format_stable() {
        let cfg = RedisSinkConfig::new("redis://localhost:6379", "my-stream");
        let id = Uuid::nil();
        assert_eq!(
            cfg.dedup_key(&id),
            "aeon:dedup:my-stream:00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn dedup_key_prefix_override() {
        let cfg =
            RedisSinkConfig::new("redis://localhost:6379", "s").with_dedup_key_prefix("custom:");
        let id = Uuid::nil();
        assert!(cfg.dedup_key(&id).starts_with("custom:s:"));
    }

    /// Compile-time proof that `RedisSink` satisfies `IdempotentSink`.
    #[allow(dead_code)]
    fn _assert_idempotent_sink<T: IdempotentSink>() {}
    #[allow(dead_code)]
    fn _assert_redis_sink_is_idempotent() {
        _assert_idempotent_sink::<RedisSink>();
    }
}
