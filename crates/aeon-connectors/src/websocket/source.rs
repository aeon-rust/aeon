//! WebSocket source — connects to a WS server and receives messages.
//!
//! Push-source: a background task reads from the WebSocket and pushes
//! events into a PushBuffer. `next_batch()` drains the buffer.
//!
//! **Backpressure**: `PushBufferTx::send` blocks (awaits) when the channel
//! is full, which in turn suspends the `read.next()` loop below. Because
//! the reader task is the only thing driving the tungstenite sink, not
//! calling `read.next()` stops draining the underlying TCP socket, and
//! the OS-level TCP window fills — propagating backpressure to the remote
//! peer without needing an explicit protocol-level signal. Messages are
//! never dropped.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, PushBufferTx, push_buffer};
use aeon_types::{AeonError, Backoff, BackoffPolicy, CoreLocalUuidGenerator, Event, PartitionId, Source};
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
    /// TR-3 reconnect backoff. On Close or socket error, the reader task
    /// drops the connection, sleeps for the current backoff delay, and
    /// reconnects. Resets on a successful reconnect. Initial `new()` still
    /// fails fast so misconfiguration surfaces on startup.
    pub backoff: BackoffPolicy,
}

impl WebSocketSourceConfig {
    /// Create a config for a WebSocket source.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            source_name: Arc::from("websocket"),
            buffer_config: PushBufferConfig::default(),
            poll_timeout: Duration::from_secs(1),
            backoff: BackoffPolicy::default(),
        }
    }

    /// Override the reconnect backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
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
    ///
    /// TR-3: the initial connect is validated synchronously (misconfiguration
    /// surfaces on startup). After that, the reader task owns a reconnect
    /// loop — on Close/error it drops the socket, sleeps for the backoff
    /// delay, and reconnects; resets the backoff on successful reconnect.
    pub async fn new(config: WebSocketSourceConfig) -> Result<Self, AeonError> {
        // Initial connect — fail fast on bad URL / unreachable host so the
        // pipeline operator sees misconfiguration immediately rather than
        // watching the backoff loop grind forever.
        let (ws_stream, _) = tokio_tungstenite::connect_async(&config.url)
            .await
            .map_err(|e| {
                AeonError::connection(format!("websocket connect failed: {}: {e}", config.url))
            })?;

        tracing::info!(url = %config.url, "WebSocketSource connected");

        let (tx, rx) = push_buffer(config.buffer_config);
        let source_name = config.source_name;
        let url = config.url;
        let mut backoff = Backoff::new(config.backoff);
        let mut uuid_gen = CoreLocalUuidGenerator::new(0);

        let handle = tokio::spawn(async move {
            // Drive the initial connection first, then fall into the
            // reconnect loop on drop.
            if !run_ws_reader(ws_stream, &tx, &source_name, &mut uuid_gen).await {
                return; // Buffer closed — caller dropped the source.
            }

            loop {
                let delay = backoff.next_delay();
                tracing::warn!(
                    %url,
                    delay_ms = delay.as_millis() as u64,
                    "websocket disconnected, reconnecting after backoff"
                );
                tokio::time::sleep(delay).await;

                match tokio_tungstenite::connect_async(&url).await {
                    Ok((stream, _)) => {
                        tracing::info!(%url, "websocket reconnected");
                        backoff.reset();
                        if !run_ws_reader(stream, &tx, &source_name, &mut uuid_gen).await {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%url, error = %e, "websocket reconnect failed");
                        // Loop back; next iteration picks up the next delay.
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

/// Drive a single WebSocket connection until it closes or errors.
///
/// Returns `true` if the loop exited because the remote closed / errored
/// (the outer task should reconnect), `false` if the downstream push buffer
/// was closed (the source was dropped — stop entirely).
async fn run_ws_reader<S>(
    ws_stream: tokio_tungstenite::WebSocketStream<S>,
    tx: &PushBufferTx,
    source_name: &Arc<str>,
    uuid_gen: &mut CoreLocalUuidGenerator,
) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::StreamExt;
    let (_, mut read) = ws_stream.split();

    while let Some(msg_result) = read.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                let event = Event::new(
                    uuid_gen.next_uuid(),
                    0,
                    Arc::clone(source_name),
                    PartitionId::new(0),
                    // FT-11: Utf8Bytes: AsRef<Bytes> — clone is refcount-only.
                    AsRef::<Bytes>::as_ref(&text).clone(),
                );
                // tx.send blocks when the push buffer is full; that
                // back-pressures the outer read.next() loop and
                // eventually the TCP socket.
                if tx.send(event).await.is_err() {
                    return false; // Buffer closed
                }
            }
            Ok(Message::Binary(data)) => {
                let event = Event::new(
                    uuid_gen.next_uuid(),
                    0,
                    Arc::clone(source_name),
                    PartitionId::new(0),
                    data,
                );
                if tx.send(event).await.is_err() {
                    return false;
                }
            }
            Ok(Message::Close(_)) => {
                tracing::info!("websocket closed by server");
                return true;
            }
            Ok(_) => {} // Ping/Pong handled by tungstenite
            Err(e) => {
                tracing::error!(error = %e, "websocket read error");
                return true;
            }
        }
    }
    true
}

impl Source for WebSocketSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }
}
