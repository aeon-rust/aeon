//! WebTransport Streams sink — reliable, ordered delivery.
//!
//! Connects to a WebTransport server and sends outputs as
//! length-prefixed messages on bidirectional streams.

use aeon_types::{AeonError, BatchResult, Output, Sink};

/// Configuration for `WebTransportSink`.
pub struct WebTransportSinkConfig {
    /// WebTransport URL to connect to (e.g., "https://localhost:4472").
    pub url: String,
    /// wtransport client configuration.
    pub client_config: wtransport::ClientConfig,
}

impl WebTransportSinkConfig {
    /// Create a config for connecting to a WebTransport server.
    pub fn new(url: impl Into<String>, client_config: wtransport::ClientConfig) -> Self {
        Self {
            url: url.into(),
            client_config,
        }
    }
}

/// WebTransport Streams output sink.
///
/// Maintains a persistent WebTransport session. Opens a new bidirectional
/// stream for each batch and sends length-prefixed messages.
pub struct WebTransportSink {
    connection: wtransport::Connection,
    delivered: u64,
}

impl WebTransportSink {
    /// Connect to the WebTransport server.
    pub async fn new(config: WebTransportSinkConfig) -> Result<Self, AeonError> {
        let endpoint = wtransport::Endpoint::client(config.client_config).map_err(|e| {
            AeonError::connection(format!("webtransport client endpoint failed: {e}"))
        })?;

        let connection = endpoint.connect(&config.url).await.map_err(|e| {
            AeonError::connection(format!(
                "webtransport connect failed to {}: {e}",
                config.url
            ))
        })?;

        tracing::info!(url = %config.url, "WebTransportSink connected");

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

impl Sink for WebTransportSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let ids: Vec<_> = outputs.iter().filter_map(|o| o.source_event_id).collect();
        let opening =
            self.connection.open_bi().await.map_err(|e| {
                AeonError::connection(format!("webtransport open stream failed: {e}"))
            })?;
        let (mut send, _recv) = opening
            .await
            .map_err(|e| AeonError::connection(format!("webtransport stream open failed: {e}")))?;

        for output in &outputs {
            let len = output.payload.len() as u32;
            send.write_all(&len.to_le_bytes()).await.map_err(|e| {
                AeonError::connection(format!("webtransport write length failed: {e}"))
            })?;
            send.write_all(output.payload.as_ref()).await.map_err(|e| {
                AeonError::connection(format!("webtransport write payload failed: {e}"))
            })?;
            self.delivered += 1;
        }

        send.finish().await.map_err(|e| {
            AeonError::connection(format!("webtransport finish stream failed: {e}"))
        })?;

        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        Ok(())
    }
}
