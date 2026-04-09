//! HTTP Webhook source — axum server accepting POST requests as events.
//!
//! Runs an HTTP server on a configurable address. Each POST request body
//! becomes an Event. Uses the shared push buffer for three-phase backpressure.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, PushBufferTx, push_buffer};
use aeon_types::{AeonError, Event, PartitionId, Source};
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Configuration for `HttpWebhookSource`.
pub struct HttpWebhookSourceConfig {
    /// Address to bind the HTTP server to.
    pub bind_addr: SocketAddr,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Push buffer configuration.
    pub buffer_config: PushBufferConfig,
    /// Timeout waiting for first event in `next_batch()`.
    pub poll_timeout: Duration,
    /// HTTP path to accept webhooks on (e.g., "/webhook").
    pub path: String,
}

impl HttpWebhookSourceConfig {
    /// Create a config for a webhook source.
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self {
            bind_addr,
            source_name: Arc::from("http-webhook"),
            buffer_config: PushBufferConfig::default(),
            poll_timeout: Duration::from_secs(1),
            path: "/webhook".to_string(),
        }
    }

    /// Set the source name used in Event.source.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Set the webhook path.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Set the poll timeout.
    pub fn with_poll_timeout(mut self, timeout: Duration) -> Self {
        self.poll_timeout = timeout;
        self
    }

    /// Set channel capacity for the push buffer.
    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.buffer_config.channel_capacity = capacity;
        self
    }
}

/// Shared state for the axum webhook handler.
struct WebhookState {
    tx: PushBufferTx,
    source_name: Arc<str>,
}

/// HTTP Webhook event source.
///
/// Spawns an axum HTTP server in the background. POST requests to the
/// configured path are buffered and returned via `next_batch()`.
pub struct HttpWebhookSource {
    rx: PushBufferRx,
    poll_timeout: Duration,
    _server_handle: tokio::task::JoinHandle<()>,
}

impl HttpWebhookSource {
    /// Create and start the webhook source.
    ///
    /// Spawns an HTTP server in the background. The server runs until
    /// the source is dropped.
    pub async fn new(config: HttpWebhookSourceConfig) -> Result<Self, AeonError> {
        let (tx, rx) = push_buffer(config.buffer_config);

        let state = Arc::new(WebhookState {
            tx,
            source_name: config.source_name,
        });

        let app = axum::Router::new()
            .route(&config.path, axum::routing::post(webhook_handler))
            .route("/health", axum::routing::get(|| async { "ok" }))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(config.bind_addr)
            .await
            .map_err(|e| {
                AeonError::connection(format!("webhook bind failed on {}: {e}", config.bind_addr))
            })?;

        let addr = listener
            .local_addr()
            .map_err(|e| AeonError::connection(format!("failed to get local addr: {e}")))?;

        tracing::info!(%addr, "HttpWebhookSource listening");

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "webhook server error");
            }
        });

        Ok(Self {
            rx,
            poll_timeout: config.poll_timeout,
            _server_handle: handle,
        })
    }
}

async fn webhook_handler(
    axum::extract::State(state): axum::extract::State<Arc<WebhookState>>,
    body: Bytes,
) -> axum::http::StatusCode {
    if state.tx.is_overloaded() {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE;
    }

    let event = Event::new(
        uuid::Uuid::nil(),
        0,
        Arc::clone(&state.source_name),
        PartitionId::new(0),
        body,
    );

    match state.tx.send(event).await {
        Ok(()) => axum::http::StatusCode::ACCEPTED,
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl Source for HttpWebhookSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_webhook_source_receives_events() {
        let _config = HttpWebhookSourceConfig::new("127.0.0.1:0".parse().unwrap())
            .with_poll_timeout(Duration::from_millis(200));

        // We need to get the actual port, so we bind manually
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (tx, mut rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("test-webhook"),
        });

        let app = axum::Router::new()
            .route("/webhook", axum::routing::post(webhook_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Send a webhook POST
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/webhook"))
            .body("test-payload")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);

        // Read from buffer
        let batch = rx.next_batch(Duration::from_secs(1)).await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].payload.as_ref(), b"test-payload");
    }

    #[tokio::test]
    async fn test_webhook_health_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (tx, _rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("test"),
        });

        let app = axum::Router::new()
            .route("/webhook", axum::routing::post(webhook_handler))
            .route("/health", axum::routing::get(|| async { "ok" }))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{addr}/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
}
