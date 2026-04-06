//! QUIC raw sink — connects to a QUIC server and sends outputs.
//!
//! Each output is sent as a length-prefixed message on a bidirectional stream.
//! Protocol: `[length: u32 LE][payload]` per message.

use aeon_types::{AeonError, BatchResult, Output, Sink};
use std::net::SocketAddr;

/// Configuration for `QuicSink`.
pub struct QuicSinkConfig {
    /// Remote QUIC server address to connect to.
    pub remote_addr: SocketAddr,
    /// Server name for TLS SNI.
    pub server_name: String,
    /// Quinn client configuration (TLS).
    pub client_config: quinn::ClientConfig,
}

impl QuicSinkConfig {
    /// Create a config for connecting to a QUIC server.
    pub fn new(
        remote_addr: SocketAddr,
        server_name: impl Into<String>,
        client_config: quinn::ClientConfig,
    ) -> Self {
        Self {
            remote_addr,
            server_name: server_name.into(),
            client_config,
        }
    }
}

/// QUIC raw output sink.
///
/// Maintains a persistent QUIC connection. Opens a new bidirectional stream
/// for each batch and sends length-prefixed messages.
pub struct QuicSink {
    connection: quinn::Connection,
    delivered: u64,
}

impl QuicSink {
    /// Connect to the QUIC server.
    pub async fn new(config: QuicSinkConfig) -> Result<Self, AeonError> {
        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
            .map_err(|e| AeonError::connection(format!("quic client bind failed: {e}")))?;
        endpoint.set_default_client_config(config.client_config);

        let connection = endpoint
            .connect(config.remote_addr, &config.server_name)
            .map_err(|e| AeonError::connection(format!("quic connect setup failed: {e}")))?
            .await
            .map_err(|e| {
                AeonError::connection(format!(
                    "quic connect failed to {}: {e}",
                    config.remote_addr
                ))
            })?;

        tracing::info!(
            remote = %config.remote_addr,
            "QuicSink connected"
        );

        Ok(Self {
            connection,
            delivered: 0,
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}

impl Sink for QuicSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let ids: Vec<_> = outputs.iter().filter_map(|o| o.source_event_id).collect();
        // Open a new bidirectional stream for this batch
        let (mut send, _recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|e| AeonError::connection(format!("quic open stream failed: {e}")))?;

        for output in &outputs {
            let len = output.payload.len() as u32;
            send.write_all(&len.to_le_bytes())
                .await
                .map_err(|e| AeonError::connection(format!("quic write length failed: {e}")))?;
            send.write_all(output.payload.as_ref())
                .await
                .map_err(|e| AeonError::connection(format!("quic write payload failed: {e}")))?;
            self.delivered += 1;
        }

        send.finish()
            .map_err(|e| AeonError::connection(format!("quic finish stream failed: {e}")))?;

        // Wait for the stream to be fully acknowledged
        send.stopped().await.ok(); // Best-effort wait

        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        // QUIC handles reliability at the transport level
        // Streams auto-flush on finish
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::source::{QuicSource, QuicSourceConfig};
    use aeon_types::Source;
    use std::time::Duration;

    #[tokio::test]
    async fn test_quic_source_sink_roundtrip() {
        let (server_config, client_config) = crate::quic::tls::dev_quic_configs();
        let source_config = QuicSourceConfig::new("127.0.0.1:0".parse().unwrap(), server_config)
            .with_poll_timeout(Duration::from_millis(500));
        let mut source = QuicSource::new(source_config).unwrap();
        let addr = source.local_addr();

        let sink_config = QuicSinkConfig::new(addr, "localhost", client_config);
        let mut sink = QuicSink::new(sink_config).await.unwrap();

        // Send outputs through sink
        let outputs = vec![
            Output::new(std::sync::Arc::from("test"), bytes::Bytes::from("msg1")),
            Output::new(std::sync::Arc::from("test"), bytes::Bytes::from("msg2")),
        ];
        sink.write_batch(outputs).await.unwrap();

        // Small delay for network transit
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Read from source
        let batch = source.next_batch().await.unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].payload.as_ref(), b"msg1");
        assert_eq!(batch[1].payload.as_ref(), b"msg2");
        assert_eq!(sink.delivered(), 2);
    }
}
