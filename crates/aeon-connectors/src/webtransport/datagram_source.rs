//! WebTransport Datagram source — unreliable, unordered delivery.
//!
//! Reads datagrams from WebTransport sessions. Datagrams may be
//! dropped, reordered, or duplicated. Requires explicit `accept_loss: true`
//! in the configuration to acknowledge lossy semantics.
//!
//! Use cases: real-time telemetry, sensor data, game state where
//! occasional loss is acceptable and low latency is critical.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, push_buffer};
use aeon_types::{AeonError, Event, PartitionId, Source};
use std::sync::Arc;
use std::time::Duration;

/// Configuration for `WebTransportDatagramSource`.
pub struct WebTransportDatagramSourceConfig {
    /// wtransport server configuration.
    pub server_config: wtransport::ServerConfig,
    /// Must be `true` to acknowledge lossy delivery semantics.
    /// The source will refuse to start if this is `false`.
    pub accept_loss: bool,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Push buffer configuration.
    pub buffer_config: PushBufferConfig,
    /// Timeout waiting for first event in `next_batch()`.
    pub poll_timeout: Duration,
}

impl WebTransportDatagramSourceConfig {
    /// Create a config for a datagram source.
    ///
    /// `accept_loss` must be `true` — this is an explicit opt-in to lossy delivery.
    pub fn new(server_config: wtransport::ServerConfig, accept_loss: bool) -> Self {
        Self {
            server_config,
            accept_loss,
            source_name: Arc::from("webtransport-dgram"),
            buffer_config: PushBufferConfig::default(),
            poll_timeout: Duration::from_secs(1),
        }
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Set the poll timeout.
    pub fn with_poll_timeout(mut self, timeout: Duration) -> Self {
        self.poll_timeout = timeout;
        self
    }
}

/// WebTransport Datagram event source.
///
/// **WARNING**: Datagrams are unreliable. Events may be dropped, reordered,
/// or duplicated. Only use when occasional data loss is acceptable.
///
/// Requires `accept_loss: true` in the config to construct.
pub struct WebTransportDatagramSource {
    rx: PushBufferRx,
    poll_timeout: Duration,
    _accept_handle: tokio::task::JoinHandle<()>,
}

impl WebTransportDatagramSource {
    /// Bind and start the datagram source.
    ///
    /// Returns `Err` if `accept_loss` is not set to `true`.
    pub async fn new(config: WebTransportDatagramSourceConfig) -> Result<Self, AeonError> {
        if !config.accept_loss {
            return Err(AeonError::config(
                "WebTransportDatagramSource requires accept_loss: true — \
                 datagrams are unreliable and may be dropped, reordered, or duplicated"
                    .to_string(),
            ));
        }

        let endpoint = wtransport::Endpoint::server(config.server_config).map_err(|e| {
            AeonError::connection(format!("webtransport datagram bind failed: {e}"))
        })?;

        tracing::info!("WebTransportDatagramSource listening (lossy mode)");

        let (tx, rx) = push_buffer(config.buffer_config);
        let source_name = config.source_name;

        let handle = tokio::spawn(datagram_accept_loop(endpoint, tx, source_name));

        Ok(Self {
            rx,
            poll_timeout: config.poll_timeout,
            _accept_handle: handle,
        })
    }
}

async fn datagram_accept_loop(
    endpoint: wtransport::Endpoint<wtransport::endpoint::endpoint_side::Server>,
    tx: crate::push_buffer::PushBufferTx,
    source_name: Arc<str>,
) {
    loop {
        let incoming = endpoint.accept().await;

        let session_request = match incoming.await {
            Ok(req) => req,
            Err(e) => {
                tracing::warn!(error = %e, "webtransport dgram session request failed");
                continue;
            }
        };

        let session = match session_request.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "webtransport dgram session accept failed");
                continue;
            }
        };

        let tx = tx.clone();
        let source_name = Arc::clone(&source_name);

        tokio::spawn(async move {
            loop {
                match session.receive_datagram().await {
                    Ok(datagram) => {
                        if tx.is_overloaded() {
                            // Intentionally drop — datagrams are lossy by nature
                            continue;
                        }

                        // FT-11: datagram.payload() returns bytes::Bytes directly.
                        let payload = datagram.payload();
                        let event = Event::new(
                            uuid::Uuid::nil(),
                            0,
                            Arc::clone(&source_name),
                            PartitionId::new(0),
                            payload,
                        );

                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "webtransport dgram recv ended");
                        break;
                    }
                }
            }
        });
    }
}

impl Source for WebTransportDatagramSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_datagram_source_rejects_without_accept_loss() {
        let identity = wtransport::Identity::self_signed(["localhost"]).unwrap();
        let server_config = wtransport::ServerConfig::builder()
            .with_bind_default(0)
            .with_identity(identity)
            .build();

        let config = WebTransportDatagramSourceConfig::new(server_config, false);
        let result = WebTransportDatagramSource::new(config).await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        let err_msg = format!("{err}");
        assert!(err_msg.contains("accept_loss"));
    }
}
