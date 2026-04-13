//! NATS sink — publishes outputs to a NATS subject.
//!
//! Supports both core NATS (fire-and-forget) and JetStream (persistent)
//! publishing. Default is JetStream for delivery guarantees.
//!
//! Delivery strategies:
//! - **PerEvent**: JetStream publish + await ack per message.
//! - **OrderedBatch** (default): JetStream publish all in order, await all acks at batch end.
//! - **UnorderedBatch**: JetStream publish (enqueue), collect ack futures, await in flush().

use aeon_types::{AeonError, BatchResult, DeliveryStrategy, IdempotentSink, Output, Sink};
use async_nats::jetstream::context::Publish;
use uuid::Uuid;

/// Configuration for `NatsSink`.
pub struct NatsSinkConfig {
    /// NATS server URL (e.g., "nats://localhost:4222").
    pub url: String,
    /// Default subject to publish to.
    pub subject: String,
    /// Whether to use JetStream publish (persistent) vs core NATS (fire-and-forget).
    pub jetstream: bool,
    /// Delivery strategy — controls how write_batch handles JetStream acks.
    pub strategy: DeliveryStrategy,
    /// EO-3: When true, publishes set the `Nats-Msg-Id` header to the
    /// output's `source_event_id` (UUID). JetStream streams configured with
    /// a non-zero `duplicate_window` will reject duplicate msg-ids server-side
    /// — this is the native JetStream EOS primitive.
    ///
    /// Requires:
    /// - JetStream mode (ignored for core NATS)
    /// - The target stream has a `duplicate_window` configured (NATS default 2m)
    /// - Outputs carry `source_event_id` (the engine propagates this by default)
    ///
    /// Off by default; opt in via `with_dedup()`.
    pub dedup: bool,
}

impl NatsSinkConfig {
    /// Create a config for publishing to a NATS subject via JetStream.
    pub fn new(url: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            subject: subject.into(),
            jetstream: true,
            strategy: DeliveryStrategy::default(),
            dedup: false,
        }
    }

    /// Use core NATS publish (no persistence guarantees).
    pub fn with_core_nats(mut self) -> Self {
        self.jetstream = false;
        self
    }

    /// Set the delivery strategy.
    pub fn with_strategy(mut self, strategy: DeliveryStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// EO-3: Enable JetStream server-side deduplication via `Nats-Msg-Id`.
    ///
    /// Each published message carries its `source_event_id` as the msg-id;
    /// JetStream rejects duplicates within the stream's `duplicate_window`.
    /// See [`NatsSinkConfig::dedup`] for requirements.
    pub fn with_dedup(mut self) -> Self {
        self.dedup = true;
        self
    }
}

/// NATS output sink.
///
/// Publishes each output to the configured subject. With JetStream enabled
/// (default), waits for server acknowledgment for at-least-once delivery.
///
/// - **PerEvent**: publishes and awaits each ack individually.
/// - **OrderedBatch**: publishes all in order, awaits all acks at batch end.
/// - **UnorderedBatch**: publishes and stores ack futures, await in `flush()`.
pub struct NatsSink {
    client: async_nats::Client,
    jetstream: Option<async_nats::jetstream::Context>,
    config: NatsSinkConfig,
    delivered: u64,
    /// Pending JetStream ack futures (UnorderedBatch mode only).
    pending_acks: Vec<async_nats::jetstream::context::PublishAckFuture>,
}

impl NatsSink {
    /// Connect to NATS.
    pub async fn new(config: NatsSinkConfig) -> Result<Self, AeonError> {
        let client = async_nats::connect(&config.url)
            .await
            .map_err(|e| AeonError::connection(format!("nats connect failed: {e}")))?;

        let jetstream = if config.jetstream {
            Some(async_nats::jetstream::new(client.clone()))
        } else {
            None
        };

        tracing::info!(
            subject = %config.subject,
            jetstream = config.jetstream,
            strategy = ?config.strategy,
            "NatsSink connected"
        );

        Ok(Self {
            client,
            jetstream,
            config,
            delivered: 0,
            pending_acks: Vec::new(),
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Publish one output to JetStream, attaching `Nats-Msg-Id` for dedup
    /// when `dedup` is enabled and the output carries a `source_event_id`.
    ///
    /// Factored out so the three `DeliveryStrategy` arms share one publish
    /// path — keeps the msg-id branch from being duplicated three times.
    async fn js_publish_one(
        js: &async_nats::jetstream::Context,
        subject: String,
        output: &Output,
        dedup: bool,
    ) -> Result<async_nats::jetstream::context::PublishAckFuture, AeonError> {
        // FT-11: zero-copy — async_nats uses the same bytes::Bytes,
        // so .clone() is a refcount bump, not a data copy.
        let payload = output.payload.clone();
        let msg_id = if dedup {
            output.source_event_id.map(|id| id.to_string())
        } else {
            None
        };
        match msg_id {
            Some(id) => js
                .send_publish(subject, Publish::build().payload(payload).message_id(id))
                .await
                .map_err(|e| {
                    AeonError::connection(format!("nats jetstream publish (msg-id) failed: {e}"))
                }),
            None => js
                .publish(subject, payload)
                .await
                .map_err(|e| AeonError::connection(format!("nats jetstream publish failed: {e}"))),
        }
    }
}

impl Sink for NatsSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let event_ids: Vec<_> = outputs.iter().filter_map(|o| o.source_event_id).collect();

        if let Some(js) = &self.jetstream {
            let dedup = self.config.dedup;
            match self.config.strategy {
                DeliveryStrategy::PerEvent => {
                    // Publish and await each ack individually.
                    for output in &outputs {
                        let subject = self.config.subject.clone();
                        let ack_future = Self::js_publish_one(js, subject, output, dedup).await?;
                        ack_future.await.map_err(|e| {
                            AeonError::connection(format!("nats jetstream ack failed: {e}"))
                        })?;
                        self.delivered += 1;
                    }
                    Ok(BatchResult::all_delivered(event_ids))
                }
                DeliveryStrategy::OrderedBatch => {
                    // Publish all in order, collect ack futures, await all at batch end.
                    let mut ack_futures = Vec::with_capacity(outputs.len());
                    for output in &outputs {
                        let subject = self.config.subject.clone();
                        let ack_future = Self::js_publish_one(js, subject, output, dedup).await?;
                        ack_futures.push(ack_future);
                    }

                    // Await all acks at batch boundary.
                    for ack_future in ack_futures {
                        ack_future.await.map_err(|e| {
                            AeonError::connection(format!("nats jetstream ack failed: {e}"))
                        })?;
                        self.delivered += 1;
                    }
                    Ok(BatchResult::all_delivered(event_ids))
                }
                DeliveryStrategy::UnorderedBatch => {
                    // Store ack futures — flush() will collect acks.
                    for output in &outputs {
                        let subject = self.config.subject.clone();
                        let ack_future = Self::js_publish_one(js, subject, output, dedup).await?;
                        self.pending_acks.push(ack_future);
                    }
                    Ok(BatchResult::all_pending(event_ids))
                }
            }
        } else {
            // Core NATS — fire and forget (strategy irrelevant).
            for output in &outputs {
                let subject = self.config.subject.clone();
                // FT-11: zero-copy clone (refcount-only).
                let payload = output.payload.clone();
                self.client
                    .publish(subject, payload)
                    .await
                    .map_err(|e| AeonError::connection(format!("nats publish failed: {e}")))?;
                self.delivered += 1;
            }
            Ok(BatchResult::all_delivered(event_ids))
        }
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        // Await all pending JetStream ack futures (UnorderedBatch mode).
        if !self.pending_acks.is_empty() {
            let acks = std::mem::take(&mut self.pending_acks);
            for ack in acks {
                ack.await.map_err(|e| {
                    AeonError::connection(format!("nats jetstream ack failed: {e}"))
                })?;
                self.delivered += 1;
            }
        }

        self.client
            .flush()
            .await
            .map_err(|e| AeonError::connection(format!("nats flush failed: {e}")))
    }
}

/// EO-3: `IdempotentSink` on `NatsSink` — JetStream server-side dedup.
///
/// Dedup is provided by JetStream's `duplicate_window`: messages published with
/// the same `Nats-Msg-Id` within the window are silently rejected by the
/// server. When [`NatsSinkConfig::dedup`] is enabled, `write_batch` sets the
/// msg-id to each output's `source_event_id` so replayed or re-sent events
/// are deduplicated at the stream.
///
/// There is no public JetStream API to query "has msg-id X been delivered?" —
/// dedup state lives inside the stream's internal window. `has_seen` therefore
/// returns `Ok(false)` unconditionally; the engine doesn't need to consult the
/// sink because the server rejects duplicates on publish. Mirrors the
/// `KafkaSink` design where dedup is server-scoped, not event-scoped.
impl IdempotentSink for NatsSink {
    async fn has_seen(&self, _event_id: &Uuid) -> Result<bool, AeonError> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_dedup_off() {
        let cfg = NatsSinkConfig::new("nats://localhost:4222", "events");
        assert!(!cfg.dedup);
    }

    #[test]
    fn with_dedup_enables_flag() {
        let cfg = NatsSinkConfig::new("nats://localhost:4222", "events").with_dedup();
        assert!(cfg.dedup);
    }

    /// Compile-time proof that `NatsSink` satisfies `IdempotentSink`.
    #[allow(dead_code)]
    fn _assert_idempotent_sink<T: IdempotentSink>() {}
    #[allow(dead_code)]
    fn _assert_nats_sink_is_idempotent() {
        _assert_idempotent_sink::<NatsSink>();
    }
}
