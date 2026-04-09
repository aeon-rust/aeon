//! QUIC raw source — accepts connections from external QUIC clients.
//!
//! Push-source: a background task accepts connections, opens bidirectional
//! streams, reads length-prefixed messages, and pushes them into a PushBuffer.
//! Protocol: `[length: u32 LE][payload]` per message on each stream.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, push_buffer};
use aeon_types::{AeonError, Event, PartitionId, Source};
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Configuration for `QuicSource`.
pub struct QuicSourceConfig {
    /// Address to bind the QUIC endpoint to.
    pub bind_addr: SocketAddr,
    /// Quinn server configuration (TLS).
    pub server_config: quinn::ServerConfig,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Push buffer configuration.
    pub buffer_config: PushBufferConfig,
    /// Timeout waiting for first event in `next_batch()`.
    pub poll_timeout: Duration,
}

impl QuicSourceConfig {
    /// Create a config for a QUIC source.
    pub fn new(bind_addr: SocketAddr, server_config: quinn::ServerConfig) -> Self {
        Self {
            bind_addr,
            server_config,
            source_name: Arc::from("quic"),
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

/// QUIC raw event source.
///
/// Binds a QUIC endpoint and spawns a background accept loop.
/// Each incoming bidirectional stream reads length-prefixed messages
/// and pushes them as events into the push buffer.
pub struct QuicSource {
    rx: PushBufferRx,
    poll_timeout: Duration,
    _accept_handle: tokio::task::JoinHandle<()>,
    local_addr: SocketAddr,
}

impl QuicSource {
    /// Bind and start the QUIC listener.
    pub fn new(config: QuicSourceConfig) -> Result<Self, AeonError> {
        let endpoint =
            quinn::Endpoint::server(config.server_config, config.bind_addr).map_err(|e| {
                AeonError::connection(format!("quic bind failed on {}: {e}", config.bind_addr))
            })?;

        let local_addr = endpoint
            .local_addr()
            .map_err(|e| AeonError::connection(format!("failed to get quic local addr: {e}")))?;

        tracing::info!(%local_addr, "QuicSource listening");

        let (tx, rx) = push_buffer(config.buffer_config);
        let source_name = config.source_name;

        let handle = tokio::spawn(quic_accept_loop(endpoint, tx, source_name));

        Ok(Self {
            rx,
            poll_timeout: config.poll_timeout,
            _accept_handle: handle,
            local_addr,
        })
    }

    /// The actual bound address (useful when binding to port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

async fn quic_accept_loop(
    endpoint: quinn::Endpoint,
    tx: crate::push_buffer::PushBufferTx,
    source_name: Arc<str>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let tx = tx.clone();
        let source_name = Arc::clone(&source_name);

        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(e) => {
                    tracing::warn!(error = %e, "quic connection failed");
                    return;
                }
            };

            tracing::debug!(
                remote = %conn.remote_address(),
                "quic connection accepted"
            );

            // Accept bidirectional streams from this connection
            loop {
                let stream = match conn.accept_bi().await {
                    Ok(stream) => stream,
                    Err(quinn::ConnectionError::ApplicationClosed(_)) => break,
                    Err(e) => {
                        tracing::debug!(error = %e, "quic stream accept ended");
                        break;
                    }
                };

                let tx = tx.clone();
                let source_name = Arc::clone(&source_name);

                tokio::spawn(async move {
                    if let Err(e) = handle_stream(stream, tx, source_name).await {
                        tracing::debug!(error = %e, "quic stream handler error");
                    }
                });
            }
        });
    }
}

/// Read length-prefixed messages from a bidirectional QUIC stream.
async fn handle_stream(
    (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
    tx: crate::push_buffer::PushBufferTx,
    source_name: Arc<str>,
) -> Result<(), AeonError> {
    loop {
        // Phase 3: if overloaded, send backpressure signal
        if tx.is_overloaded() {
            // Send a 0-length frame as backpressure signal
            send.write_all(&0u32.to_le_bytes()).await.map_err(|e| {
                AeonError::connection(format!("quic write backpressure signal failed: {e}"))
            })?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        // Read length prefix (4 bytes LE)
        let mut len_buf = [0u8; 4];
        match recv.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(_)) => break, // Stream closed
            Err(e) => {
                return Err(AeonError::connection(format!(
                    "quic read length failed: {e}"
                )));
            }
        }

        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 {
            continue; // Keep-alive / empty frame
        }

        // Read payload
        let mut payload_buf = vec![0u8; len];
        recv.read_exact(&mut payload_buf)
            .await
            .map_err(|e| AeonError::connection(format!("quic read payload failed: {e}")))?;

        let event = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::clone(&source_name),
            PartitionId::new(0),
            Bytes::from(payload_buf),
        );

        if tx.send(event).await.is_err() {
            break; // Buffer closed
        }
    }

    Ok(())
}

impl Source for QuicSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_quic_source_binds() {
        let (server_config, _client_config) = crate::quic::tls::dev_quic_configs();
        let config = QuicSourceConfig::new("127.0.0.1:0".parse().unwrap(), server_config);
        let source = QuicSource::new(config).unwrap();
        assert_ne!(source.local_addr().port(), 0);
    }

    #[tokio::test]
    async fn test_quic_source_receives_messages() {
        let (server_config, client_config) = crate::quic::tls::dev_quic_configs();
        let config = QuicSourceConfig::new("127.0.0.1:0".parse().unwrap(), server_config)
            .with_poll_timeout(Duration::from_millis(500));
        let mut source = QuicSource::new(config).unwrap();
        let addr = source.local_addr();

        // Connect client
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(client_config);

        let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();

        let (mut send, _recv) = conn.open_bi().await.unwrap();

        // Send a length-prefixed message
        let payload = b"hello-quic";
        send.write_all(&(payload.len() as u32).to_le_bytes())
            .await
            .unwrap();
        send.write_all(payload).await.unwrap();
        send.finish().unwrap();

        // Read from source
        let batch = source.next_batch().await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].payload.as_ref(), b"hello-quic");
    }
}
