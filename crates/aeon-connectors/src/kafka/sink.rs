//! Kafka/Redpanda sink — batch produce with FutureProducer.
//!
//! Supports three delivery strategies:
//! - **PerEvent**: Each output is enqueued and its delivery future awaited individually.
//! - **OrderedBatch** (default): All outputs in `write_batch()` are enqueued in order,
//!   then all delivery futures awaited together before returning.
//! - **UnorderedBatch**: `write_batch()` enqueues outputs into rdkafka's internal buffer
//!   and returns immediately. `flush()` calls `producer.flush()` to await all
//!   pending deliveries. Higher throughput, downstream sorts by UUIDv7.

use aeon_types::{
    AeonError, BatchResult, DeliveryStrategy, IdempotentSink, OutboundAuthSigner, Output, Sink,
    SinkAckCallback, SinkEosTier, TransactionalSink,
};
use futures_util::future::join_all;
use rdkafka::config::ClientConfig;
use rdkafka::message::OwnedHeaders;
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

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
    /// When `Some`, the sink uses Kafka transactions for exactly-once semantics
    /// (EOS) at the producer level: each `write_batch()` call runs under a
    /// `begin_transaction` / `commit_transaction` envelope; failures trigger
    /// `abort_transaction` so partial batches never become visible to
    /// `isolation.level=read_committed` consumers.
    ///
    /// The `transactional.id` **must be stable across process restarts** and
    /// **unique per producer instance** — Kafka uses it to fence zombie
    /// producers (epoch bump on every `init_transactions`). Enabling this
    /// forces `enable.idempotence=true`.
    ///
    /// This is what powers the `IdempotentSink` impl on `KafkaSink`: dedup
    /// is provided producer-side (idempotent sequence numbers + transactional
    /// fencing), not via per-event-id lookups.
    pub transactional_id: Option<String>,
    /// S10: outbound auth signer. `BrokerNative` passes SASL/SSL knobs
    /// directly to rdkafka; `Mtls` sets `security.protocol=ssl` +
    /// `ssl.{certificate,key}.pem`. HTTP-style modes are warned-and-
    /// ignored (not meaningful for the Kafka protocol).
    pub auth: Option<Arc<OutboundAuthSigner>>,
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
            transactional_id: None,
            auth: None,
        }
    }

    /// Attach an outbound auth signer (S10). See [`crate::kafka::auth`]
    /// for the per-mode translation onto rdkafka config knobs.
    pub fn with_auth(mut self, signer: Arc<OutboundAuthSigner>) -> Self {
        self.auth = Some(signer);
        self
    }

    /// Enable Kafka transactions (EOS) with a stable, unique `transactional.id`.
    ///
    /// See [`KafkaSinkConfig::transactional_id`] for semantics. Implicitly
    /// forces `enable.idempotence=true`.
    pub fn with_transactional_id(mut self, id: impl Into<String>) -> Self {
        self.transactional_id = Some(id.into());
        self
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
    /// True when the producer was initialised for transactional writes
    /// (`transactional.id` set + `init_transactions` succeeded).
    transactional: bool,
    /// EO-2: true when an outer `TransactionalSink::begin()` is active.
    ///
    /// While this is set, `Sink::write_batch()` skips its own per-batch
    /// `begin_transaction` / `commit_transaction` envelope because the
    /// pipeline is driving a longer-lived transaction that will be closed
    /// by the matching `commit()` or `abort()` call. This is the switch
    /// that lets the same producer serve both the EO-1 per-batch model
    /// and the EO-2 two-phase-L3 model.
    in_outer_txn: bool,
    /// G4: optional ack callback installed by the engine to drive the
    /// `outputs_acked_total` companion metric. Fired with the count of
    /// newly broker-confirmed deliveries — once per `produce_inner()`
    /// outcome for `PerEvent`/`OrderedBatch`, and once per `flush()` for
    /// `UnorderedBatch` (where librdkafka's background thread acks).
    ack_callback: Option<SinkAckCallback>,
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
        // even with multiple in-flight requests. Transactions also require it.
        let transactional = config.transactional_id.is_some();
        if config.strategy.preserves_order() || transactional {
            client_config.set("enable.idempotence", "true");
        }
        if let Some(ref tid) = config.transactional_id {
            client_config.set("transactional.id", tid);
        }

        // S10: outbound auth (BrokerNative / Mtls) before user overrides so
        // the user escape-hatch (`config_overrides`) always wins for debugging.
        super::auth::apply_outbound_auth(&mut client_config, config.auth.as_ref());

        // Apply user overrides
        for (k, v) in &config.config_overrides {
            client_config.set(k, v);
        }

        let producer: FutureProducer = client_config
            .create()
            .map_err(|e| AeonError::connection(format!("kafka producer create failed: {e}")))?;

        // Transactions must be initialised before any send. This fences any
        // zombie producer sharing the same transactional.id by bumping the
        // producer epoch.
        if transactional {
            producer
                .init_transactions(config.flush_timeout)
                .map_err(|e| {
                    AeonError::connection(format!("kafka init_transactions failed: {e}"))
                })?;
        }

        tracing::info!(
            topic = %config.default_topic,
            strategy = ?config.strategy,
            transactional,
            "KafkaSink created"
        );

        Ok(Self {
            producer,
            config,
            delivered: 0,
            pending: 0,
            transactional,
            in_outer_txn: false,
            ack_callback: None,
        })
    }

    /// Fire the engine-installed ack callback if one is present. Called
    /// from inside `produce_inner` and `flush` whenever `delivered` is
    /// advanced. Helper centralises the `Option` check so the produce arms
    /// stay readable.
    fn fire_ack(&self, n: usize) {
        if n == 0 {
            return;
        }
        if let Some(cb) = self.ack_callback.as_ref() {
            cb(n);
        }
    }

    /// Returns `true` when this sink was constructed with a `transactional_id`
    /// and the rdkafka producer successfully initialised transactions.
    pub fn is_transactional(&self) -> bool {
        self.transactional
    }

    /// Number of outputs successfully delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Number of outputs enqueued but not yet confirmed (UnorderedBatch).
    pub fn pending(&self) -> u64 {
        self.pending
    }

    /// Produce path for `write_batch()` — factored out so the transactional
    /// envelope in `Sink::write_batch` can commit/abort around it without
    /// duplicating the three `DeliveryStrategy` arms.
    async fn produce_inner(
        &mut self,
        outputs: &[Output],
        count: usize,
        event_ids: Vec<Uuid>,
    ) -> Result<BatchResult, AeonError> {
        // Enqueue all outputs into rdkafka's internal producer queue.
        // FutureProducer.send() copies data into librdkafka's buffer and returns
        // a future for the delivery confirmation.
        let mut futures = Vec::with_capacity(count);

        for output in outputs {
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
                // All futures already awaited above. Each successful await
                // bumped `delivered` by 1 — fire the ack callback once with
                // the full count (cheaper than N callback calls).
                self.fire_ack(count);
                Ok(BatchResult::all_delivered(event_ids))
            }
            DeliveryStrategy::OrderedBatch => {
                // Await all delivery futures concurrently at the batch boundary.
                // Sequential `for future in futures { future.await }` costs one
                // scheduler yield per await (~1ms on Windows), which collapses
                // throughput to ~700 events/sec regardless of batch size.
                // `join_all` polls all futures together and wakes once on
                // completion — the batch finishes in O(max round-trip), not
                // O(batch_size × scheduler_yield).
                // Idempotent producer guarantees ordering even with pipelining.
                let results = join_all(futures).await;
                let mut errors = Vec::new();
                let mut newly_acked: usize = 0;
                for result in results {
                    match result {
                        Ok(_) => {
                            self.delivered += 1;
                            newly_acked += 1;
                        }
                        Err((e, _)) => {
                            errors.push(format!("{e}"));
                        }
                    }
                }

                // Fire ack callback for the partial-or-full successful set.
                // Even on error we want operators to see the partial deliveries
                // that did land — `acked` reflects broker truth, not batch outcome.
                self.fire_ack(newly_acked);

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
                // polling thread. flush() will wait for all pending deliveries —
                // the ack callback fires there, not here, because at this point
                // nothing is broker-confirmed yet.
                //
                // NOTE: UnorderedBatch + transactional is a degenerate combination —
                // the transaction would commit before futures resolve, defeating
                // the EOS guarantee. `KafkaSink::new` does not currently reject
                // this pairing; callers that need EOS must use PerEvent or
                // OrderedBatch.
                self.pending += count as u64;
                Ok(BatchResult::all_pending(event_ids))
            }
        }
    }
}

/// `IdempotentSink` on `KafkaSink` — producer-side EOS.
///
/// Dedup for Kafka is provided by the **idempotent producer** (sequence numbers
/// + producer epoch) and, when a `transactional_id` is configured, by
///   **Kafka transactions** fenced on the same `transactional.id`. Consumers
///   read deduplicated output by setting `isolation.level=read_committed`.
///
/// There is no efficient per-event-id lookup API on a Kafka producer — the
/// broker has no "have you seen this UUID?" primitive. `has_seen()` therefore
/// returns `Ok(false)` unconditionally: the contract is that the engine does
/// not need to consult the sink to avoid duplicates, because the sink already
/// guarantees that duplicate produces (same `(producer_id, sequence)`) are
/// rejected by the broker.
///
/// This matches the industry convention for Kafka EOS: dedup is
/// producer-scoped, not event-scoped.
impl IdempotentSink for KafkaSink {
    async fn has_seen(&self, _event_id: &Uuid) -> Result<bool, AeonError> {
        Ok(false)
    }
}

impl Sink for KafkaSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let count = outputs.len();

        // Transactional envelope: begin before the first send so that any
        // produce error can be turned into an `abort_transaction` and nothing
        // ever becomes visible to `read_committed` consumers.
        //
        // EO-2: when `in_outer_txn` is set, the pipeline has already called
        // `TransactionalSink::begin()` and will close the transaction with
        // `commit()` / `abort()`. Skipping the per-batch begin/commit lets
        // the outer transaction span this produce along with the pending-L3
        // write and any other batches before the checkpoint boundary.
        let manage_txn = self.transactional && !self.in_outer_txn;
        if manage_txn {
            self.producer.begin_transaction().map_err(|e| {
                AeonError::connection(format!("kafka begin_transaction failed: {e}"))
            })?;
        }

        // Collect event IDs for BatchResult tracking.
        let event_ids: Vec<_> = outputs.iter().filter_map(|o| o.source_event_id).collect();

        // The actual produce path — wrapped so a transactional envelope can
        // turn any error into `abort_transaction` without 3× duplicated code.
        let outcome = self.produce_inner(&outputs, count, event_ids.clone()).await;

        if manage_txn {
            match &outcome {
                Ok(_) => {
                    // Commit the transaction. A commit failure after successful
                    // produces is still a batch failure: the data is not visible
                    // to `read_committed` consumers until commit returns Ok.
                    self.producer
                        .commit_transaction(self.config.flush_timeout)
                        .map_err(|e| {
                            AeonError::connection(format!("kafka commit_transaction failed: {e}"))
                        })?;
                }
                Err(_) => {
                    // Best-effort abort; swallow the abort error and surface
                    // the original produce error to the caller.
                    if let Err(abort_err) =
                        self.producer.abort_transaction(self.config.flush_timeout)
                    {
                        tracing::warn!(
                            error = %abort_err,
                            "kafka abort_transaction failed after produce error"
                        );
                    }
                }
            }
        }

        outcome
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        // Wait for all messages in rdkafka's internal queue to be delivered.
        // In PerEvent/OrderedBatch, pending is always 0 (futures awaited in write_batch).
        // In UnorderedBatch, this is the checkpoint boundary where acks are collected.
        self.producer
            .flush(self.config.flush_timeout)
            .map_err(|e| AeonError::connection(format!("kafka producer flush failed: {e}")))?;

        // All pending messages are now broker-confirmed.
        let newly_acked = self.pending;
        self.delivered += newly_acked;
        self.pending = 0;
        // UnorderedBatch ack point: fire the engine callback now that
        // librdkafka's background thread has confirmed every queued send.
        self.fire_ack(newly_acked as usize);
        Ok(())
    }

    fn on_ack_callback(&mut self, cb: SinkAckCallback) {
        self.ack_callback = Some(cb);
    }
}

/// EO-2: `TransactionalSink` for Kafka — tier T2 (`TransactionalStream`).
///
/// Drives the Kafka producer-epoch transaction lifecycle from outside the
/// per-batch envelope. The pipeline flush path is expected to:
///
/// ```text
///   sink.begin().await?;             // open an outer transaction
///   sink.write_batch(batch).await?;  // one or more produce batches
///   sink.write_batch(batch).await?;  // (per-batch envelope is skipped)
///   // write Pending L3 record here
///   sink.commit().await?;            // flip to Committed once ack returns
///   // write Committed L3 record here
/// ```
///
/// On recovery, the engine may instead call `sink.abort().await?` to discard
/// any in-flight outer transaction before replaying from the prior committed
/// checkpoint. The idempotent-producer sequence fencing guarantees that a
/// replay after abort produces exactly the same sequence numbers the broker
/// has already rejected, so there are no zombie writes.
///
/// Requires `transactional_id` to be configured — without it
/// (`is_transactional() == false`), `begin()` returns an error rather than
/// silently degrading to at-least-once, so the engine can detect the
/// misconfiguration at pipeline start instead of at first crash.
impl TransactionalSink for KafkaSink {
    fn eos_tier(&self) -> SinkEosTier {
        SinkEosTier::TransactionalStream
    }

    async fn begin(&mut self) -> Result<(), AeonError> {
        if !self.transactional {
            return Err(AeonError::config(
                "KafkaSink::begin requires transactional_id to be configured; \
                 call KafkaSinkConfig::with_transactional_id() before constructing the sink",
            ));
        }
        if self.in_outer_txn {
            // Idempotent — repeated begins from the same generation are a
            // no-op. A doubled begin on the rdkafka side would return
            // `ERR_STATE` ("Operation not valid in state"), which we avoid
            // by tracking state here.
            return Ok(());
        }
        self.producer
            .begin_transaction()
            .map_err(|e| AeonError::connection(format!("kafka begin_transaction failed: {e}")))?;
        self.in_outer_txn = true;
        Ok(())
    }

    async fn commit(&mut self) -> Result<(), AeonError> {
        if !self.in_outer_txn {
            // No outer transaction in flight — nothing to commit. This is
            // the normal case when the sink is used in the at-least-once
            // (non-EO-2) path; the per-batch envelope has already committed.
            return Ok(());
        }
        self.producer
            .commit_transaction(self.config.flush_timeout)
            .map_err(|e| AeonError::connection(format!("kafka commit_transaction failed: {e}")))?;
        self.in_outer_txn = false;
        Ok(())
    }

    async fn abort(&mut self) -> Result<(), AeonError> {
        if !self.in_outer_txn {
            return Ok(());
        }
        // Clear the flag first so that a retry from the caller doesn't try
        // to abort an already-aborted transaction. rdkafka would return
        // `ERR_STATE` on the second abort; pre-clearing mirrors how we
        // handle re-entrant begin().
        self.in_outer_txn = false;
        self.producer
            .abort_transaction(self.config.flush_timeout)
            .map_err(|e| AeonError::connection(format!("kafka abort_transaction failed: {e}")))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn config_defaults_non_transactional() {
        let cfg = KafkaSinkConfig::new("localhost:9092", "test-topic");
        assert!(cfg.transactional_id.is_none());
    }

    #[test]
    fn with_transactional_id_sets_field() {
        let cfg = KafkaSinkConfig::new("localhost:9092", "test-topic")
            .with_transactional_id("aeon-sink-node-1");
        assert_eq!(cfg.transactional_id.as_deref(), Some("aeon-sink-node-1"));
    }

    #[test]
    fn ack_callback_starts_unset_and_installs_via_trait() {
        // KafkaSink::new with a non-transactional config does not contact the
        // broker (rdkafka is lazy), so this constructs cleanly even without a
        // running cluster — fine for verifying the callback wiring slot.
        let cfg = KafkaSinkConfig::new("localhost:9092", "ack-cb-test");
        let mut sink = KafkaSink::new(cfg).expect("non-txn KafkaSink construction is local");
        assert!(sink.ack_callback.is_none(), "callback must start unset");

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_cb = Arc::clone(&counter);
        let cb: SinkAckCallback = Arc::new(move |n| {
            counter_for_cb.fetch_add(n, Ordering::Relaxed);
        });
        Sink::on_ack_callback(&mut sink, cb);
        assert!(sink.ack_callback.is_some(), "callback must be stored");

        // fire_ack with N forwards N to the stored callback.
        sink.fire_ack(42);
        assert_eq!(counter.load(Ordering::Relaxed), 42);

        // fire_ack(0) is a no-op — short-circuited before the closure call.
        sink.fire_ack(0);
        assert_eq!(counter.load(Ordering::Relaxed), 42);
    }

    #[test]
    fn fire_ack_without_callback_is_a_no_op() {
        let cfg = KafkaSinkConfig::new("localhost:9092", "no-cb-test");
        let sink = KafkaSink::new(cfg).expect("non-txn KafkaSink construction is local");
        // No callback installed — must not panic on fire_ack.
        sink.fire_ack(0);
        sink.fire_ack(7);
    }

    /// Compile-time proof that `KafkaSink` satisfies `IdempotentSink`.
    #[allow(dead_code)]
    fn _assert_idempotent_sink<T: IdempotentSink>() {}
    #[allow(dead_code)]
    fn _assert_kafka_sink_is_idempotent() {
        _assert_idempotent_sink::<KafkaSink>();
    }

    /// EO-2: compile-time proof that `KafkaSink` satisfies `TransactionalSink`.
    #[allow(dead_code)]
    fn _assert_transactional_sink<T: TransactionalSink>() {}
    #[allow(dead_code)]
    fn _assert_kafka_sink_is_transactional() {
        _assert_transactional_sink::<KafkaSink>();
    }
}
