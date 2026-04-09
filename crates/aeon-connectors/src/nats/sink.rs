//! NATS sink — publishes outputs to a NATS subject.
//!
//! Supports both core NATS (fire-and-forget) and JetStream (persistent)
//! publishing. Default is JetStream for delivery guarantees.
//!
//! Delivery strategies:
//! - **PerEvent**: JetStream publish + await ack per message.
//! - **OrderedBatch** (default): JetStream publish all in order, await all acks at batch end.
//! - **UnorderedBatch**: JetStream publish (enqueue), collect ack futures, await in flush().

use aeon_types::{AeonError, BatchResult, DeliveryStrategy, Output, Sink};
use bytes::Bytes;

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
}

impl NatsSinkConfig {
    /// Create a config for publishing to a NATS subject via JetStream.
    pub fn new(url: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            subject: subject.into(),
            jetstream: true,
            strategy: DeliveryStrategy::default(),
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
}

impl Sink for NatsSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let event_ids: Vec<_> = outputs.iter().filter_map(|o| o.source_event_id).collect();

        if let Some(js) = &self.jetstream {
            match self.config.strategy {
                DeliveryStrategy::PerEvent => {
                    // Publish and await each ack individually.
                    for output in &outputs {
                        let subject = self.config.subject.clone();
                        let payload = Bytes::from(output.payload.to_vec());
                        let ack_future = js.publish(subject, payload).await.map_err(|e| {
                            AeonError::connection(format!("nats jetstream publish failed: {e}"))
                        })?;
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
                        let payload = Bytes::from(output.payload.to_vec());
                        let ack_future = js.publish(subject, payload).await.map_err(|e| {
                            AeonError::connection(format!("nats jetstream publish failed: {e}"))
                        })?;
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
                        let payload = Bytes::from(output.payload.to_vec());
                        let ack_future = js.publish(subject, payload).await.map_err(|e| {
                            AeonError::connection(format!("nats jetstream publish failed: {e}"))
                        })?;
                        self.pending_acks.push(ack_future);
                    }
                    Ok(BatchResult::all_pending(event_ids))
                }
            }
        } else {
            // Core NATS — fire and forget (strategy irrelevant).
            for output in &outputs {
                let subject = self.config.subject.clone();
                let payload = Bytes::from(output.payload.to_vec());
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
