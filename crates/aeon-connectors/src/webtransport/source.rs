//! WebTransport Streams source — reliable, ordered delivery.
//!
//! Accepts WebTransport sessions via an HTTP/3 endpoint.
//! Each bidirectional stream carries length-prefixed messages.
//! Push-source with three-phase backpressure.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, push_buffer};
use aeon_types::{AeonError, CoreLocalUuidGenerator, Event, PartitionId, Source};
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use wtransport::ServerConfig;

/// Configuration for `WebTransportSource`.
pub struct WebTransportSourceConfig {
    /// Address to bind the WebTransport endpoint to.
    pub bind_addr: SocketAddr,
    /// wtransport server configuration.
    pub server_config: ServerConfig,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Push buffer configuration.
    pub buffer_config: PushBufferConfig,
    /// Timeout waiting for first event in `next_batch()`.
    pub poll_timeout: Duration,
}

impl WebTransportSourceConfig {
    /// Create a config for a WebTransport streams source.
    pub fn new(bind_addr: SocketAddr, server_config: ServerConfig) -> Self {
        Self {
            bind_addr,
            server_config,
            source_name: Arc::from("webtransport"),
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

/// WebTransport Streams event source.
///
/// Binds an HTTP/3 WebTransport endpoint and accepts sessions.
/// Each bidirectional stream reads length-prefixed binary messages.
/// Reliable and ordered — no data loss under normal operation.
pub struct WebTransportSource {
    rx: PushBufferRx,
    poll_timeout: Duration,
    _accept_handle: tokio::task::JoinHandle<()>,
}

impl WebTransportSource {
    /// Bind and start the WebTransport listener.
    pub async fn new(config: WebTransportSourceConfig) -> Result<Self, AeonError> {
        let endpoint = wtransport::Endpoint::server(config.server_config).map_err(|e| {
            AeonError::connection(format!(
                "webtransport bind failed on {}: {e}",
                config.bind_addr
            ))
        })?;

        tracing::info!(addr = %config.bind_addr, "WebTransportSource listening");

        let (tx, rx) = push_buffer(config.buffer_config);
        let source_name = config.source_name;

        let handle = tokio::spawn(wt_accept_loop(endpoint, tx, source_name));

        Ok(Self {
            rx,
            poll_timeout: config.poll_timeout,
            _accept_handle: handle,
        })
    }
}

async fn wt_accept_loop(
    endpoint: wtransport::Endpoint<wtransport::endpoint::endpoint_side::Server>,
    tx: crate::push_buffer::PushBufferTx,
    source_name: Arc<str>,
) {
    loop {
        let incoming = endpoint.accept().await;

        let session_request = match incoming.await {
            Ok(req) => req,
            Err(e) => {
                tracing::warn!(error = %e, "webtransport session request failed");
                continue;
            }
        };

        let session = match session_request.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "webtransport session accept failed");
                continue;
            }
        };

        let tx = tx.clone();
        let source_name = Arc::clone(&source_name);

        tokio::spawn(async move {
            loop {
                let stream = match session.accept_bi().await {
                    Ok(stream) => stream,
                    Err(e) => {
                        tracing::debug!(error = %e, "webtransport bi-stream accept ended");
                        break;
                    }
                };

                let tx = tx.clone();
                let source_name = Arc::clone(&source_name);

                tokio::spawn(async move {
                    // Per-stream UUID generator — each task owns its own.
                    let id_gen = CoreLocalUuidGenerator::new(0);
                    if let Err(e) = handle_wt_stream(stream, tx, source_name, id_gen).await {
                        tracing::debug!(error = %e, "webtransport stream error");
                    }
                });
            }
        });
    }
}

async fn handle_wt_stream(
    (mut send, mut recv): (wtransport::SendStream, wtransport::RecvStream),
    tx: crate::push_buffer::PushBufferTx,
    source_name: Arc<str>,
    mut id_gen: CoreLocalUuidGenerator,
) -> Result<(), AeonError> {
    loop {
        // Phase 3: backpressure signal
        if tx.is_overloaded() {
            let _ = send.write_all(&0u32.to_le_bytes()).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        // Read length prefix (4 bytes LE)
        let mut len_buf = [0u8; 4];
        match recv.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(_) => break, // Stream closed
        }

        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 {
            continue;
        }

        let mut payload_buf = vec![0u8; len];
        recv.read_exact(&mut payload_buf)
            .await
            .map_err(|e| AeonError::connection(format!("webtransport read payload failed: {e}")))?;

        let event = Event::new(
            id_gen.next_uuid(),
            0,
            Arc::clone(&source_name),
            PartitionId::new(0),
            Bytes::from(payload_buf),
        );

        if tx.send(event).await.is_err() {
            break;
        }
    }

    Ok(())
}

impl Source for WebTransportSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }
}
