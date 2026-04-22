//! WebSocket sink — connects to a WS server and sends outputs as messages.
//!
//! Each output payload is sent as a WebSocket binary message.
//! The connection is maintained across `write_batch()` calls.

use aeon_types::{AeonError, BatchResult, Output, Sink, SsrfPolicy, redact_uri};
use tokio_tungstenite::tungstenite::Message;

/// Configuration for `WebSocketSink`.
pub struct WebSocketSinkConfig {
    /// WebSocket URL to connect to.
    pub url: String,
    /// S7: SSRF guard. The URL is checked against this policy before dial.
    pub ssrf_policy: SsrfPolicy,
}

impl WebSocketSinkConfig {
    /// Create a config for a WebSocket sink.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ssrf_policy: SsrfPolicy::production(),
        }
    }

    /// Override the SSRF policy.
    pub fn with_ssrf_policy(mut self, policy: SsrfPolicy) -> Self {
        self.ssrf_policy = policy;
        self
    }
}

/// WebSocket output sink.
///
/// Maintains a persistent WebSocket connection. Each output is sent
/// as a binary message. Reconnection is not automatic — errors are
/// propagated to the engine for retry logic.
pub struct WebSocketSink {
    writer: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    delivered: u64,
}

impl WebSocketSink {
    /// Connect to the WebSocket server. The URL is validated against the
    /// [`SsrfPolicy`] before the TCP SYN is sent, so a denied target fails
    /// at startup.
    pub async fn new(config: WebSocketSinkConfig) -> Result<Self, AeonError> {
        config.ssrf_policy.check_url(&config.url)?;

        let (ws_stream, _) = tokio_tungstenite::connect_async(&config.url)
            .await
            .map_err(|e| {
                AeonError::connection(format!(
                    "websocket connect failed: {}: {e}",
                    redact_uri(&config.url)
                ))
            })?;

        tracing::info!(url = %redact_uri(&config.url), "WebSocketSink connected");

        use futures_util::StreamExt;
        let (writer, _reader) = ws_stream.split();

        Ok(Self {
            writer,
            delivered: 0,
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}

impl Sink for WebSocketSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        use futures_util::SinkExt;

        let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();
        for output in &outputs {
            // FT-11: Message::Binary holds bytes::Bytes — clone is refcount-only.
            let msg = Message::Binary(output.payload.clone());
            self.writer
                .send(msg)
                .await
                .map_err(|e| AeonError::connection(format!("websocket send failed: {e}")))?;
            self.delivered += 1;
        }

        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        use futures_util::SinkExt;

        self.writer
            .flush()
            .await
            .map_err(|e| AeonError::connection(format!("websocket flush failed: {e}")))
    }
}
