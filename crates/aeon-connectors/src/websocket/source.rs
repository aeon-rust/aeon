//! WebSocket source — connects to a WS server and receives messages.
//!
//! Push-source: a background task reads from the WebSocket and pushes
//! events into a PushBuffer. `next_batch()` drains the buffer.
//! Phase 3 backpressure: when overloaded, the background reader pauses.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, push_buffer};
use aeon_types::{AeonError, Event, PartitionId, Source};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// Configuration for `WebSocketSource`.
pub struct WebSocketSourceConfig {
    /// WebSocket URL to connect to (e.g., "ws://localhost:8080/ws").
    pub url: String,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Push buffer configuration.
    pub buffer_config: PushBufferConfig,
    /// Timeout waiting for first event in `next_batch()`.
    pub poll_timeout: Duration,
}

impl WebSocketSourceConfig {
    /// Create a config for a WebSocket source.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            source_name: Arc::from("websocket"),
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

    /// Set channel capacity.
    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.buffer_config.channel_capacity = capacity;
        self
    }
}

/// WebSocket event source.
///
/// Spawns a background task that reads messages from the WebSocket connection
/// and pushes them into the push buffer. `next_batch()` drains events.
pub struct WebSocketSource {
    rx: PushBufferRx,
    poll_timeout: Duration,
    _reader_handle: tokio::task::JoinHandle<()>,
}

impl WebSocketSource {
    /// Connect to the WebSocket server and start receiving.
    pub async fn new(config: WebSocketSourceConfig) -> Result<Self, AeonError> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(&config.url)
            .await
            .map_err(|e| {
                AeonError::connection(format!("websocket connect failed: {}: {e}", config.url))
            })?;

        tracing::info!(url = %config.url, "WebSocketSource connected");

        let (tx, rx) = push_buffer(config.buffer_config);
        let source_name = config.source_name;

        let handle = tokio::spawn(async move {
            use futures_util::StreamExt;
            let (_, mut read) = ws_stream.split();

            while let Some(msg_result) = read.next().await {
                match msg_result {
                    Ok(Message::Text(text)) => {
                        // Phase 3: if overloaded, skip (could also pause reading)
                        if tx.is_overloaded() {
                            tracing::warn!("websocket source overloaded, dropping message");
                            continue;
                        }
                        let event = Event::new(
                            uuid::Uuid::nil(),
                            0,
                            Arc::clone(&source_name),
                            PartitionId::new(0),
                            Bytes::from(text.as_bytes().to_vec()),
                        );
                        if tx.send(event).await.is_err() {
                            break; // Buffer closed
                        }
                    }
                    Ok(Message::Binary(data)) => {
                        if tx.is_overloaded() {
                            tracing::warn!("websocket source overloaded, dropping message");
                            continue;
                        }
                        let event = Event::new(
                            uuid::Uuid::nil(),
                            0,
                            Arc::clone(&source_name),
                            PartitionId::new(0),
                            data,
                        );
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => {
                        tracing::info!("websocket closed by server");
                        break;
                    }
                    Ok(_) => {} // Ping/Pong handled by tungstenite
                    Err(e) => {
                        tracing::error!(error = %e, "websocket read error");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            rx,
            poll_timeout: config.poll_timeout,
            _reader_handle: handle,
        })
    }
}

impl Source for WebSocketSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }
}
