//! NATS sink — publishes outputs to a NATS subject.
//!
//! Supports both core NATS (fire-and-forget) and JetStream (persistent)
//! publishing. Default is JetStream for delivery guarantees.

use aeon_types::{AeonError, Output, Sink};
use bytes::Bytes;

/// Configuration for `NatsSink`.
pub struct NatsSinkConfig {
    /// NATS server URL (e.g., "nats://localhost:4222").
    pub url: String,
    /// Default subject to publish to.
    pub subject: String,
    /// Whether to use JetStream publish (persistent) vs core NATS (fire-and-forget).
    pub jetstream: bool,
}

impl NatsSinkConfig {
    /// Create a config for publishing to a NATS subject via JetStream.
    pub fn new(url: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            subject: subject.into(),
            jetstream: true,
        }
    }

    /// Use core NATS publish (no persistence guarantees).
    pub fn with_core_nats(mut self) -> Self {
        self.jetstream = false;
        self
    }
}

/// NATS output sink.
///
/// Publishes each output to the configured subject. With JetStream enabled
/// (default), waits for server acknowledgment for at-least-once delivery.
pub struct NatsSink {
    client: async_nats::Client,
    jetstream: Option<async_nats::jetstream::Context>,
    config: NatsSinkConfig,
    delivered: u64,
}

impl NatsSink {
    /// Connect to NATS.
    pub async fn new(config: NatsSinkConfig) -> Result<Self, AeonError> {
        let client = async_nats::connect(&config.url).await.map_err(|e| {
            AeonError::connection(format!("nats connect failed: {e}"))
        })?;

        let jetstream = if config.jetstream {
            Some(async_nats::jetstream::new(client.clone()))
        } else {
            None
        };

        tracing::info!(
            subject = %config.subject,
            jetstream = config.jetstream,
            "NatsSink connected"
        );

        Ok(Self {
            client,
            jetstream,
            config,
            delivered: 0,
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}

impl Sink for NatsSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<(), AeonError> {
        for output in &outputs {
            let subject = self.config.subject.clone();
            let payload = Bytes::from(output.payload.to_vec());

            if let Some(js) = &self.jetstream {
                // JetStream publish — waits for ack
                js.publish(subject, payload)
                    .await
                    .map_err(|e| {
                        AeonError::connection(format!("nats jetstream publish failed: {e}"))
                    })?
                    .await
                    .map_err(|e| {
                        AeonError::connection(format!("nats jetstream ack failed: {e}"))
                    })?;
            } else {
                // Core NATS — fire and forget
                self.client
                    .publish(subject, payload)
                    .await
                    .map_err(|e| {
                        AeonError::connection(format!("nats publish failed: {e}"))
                    })?;
            }

            self.delivered += 1;
        }

        Ok(())
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        self.client.flush().await.map_err(|e| {
            AeonError::connection(format!("nats flush failed: {e}"))
        })
    }
}
