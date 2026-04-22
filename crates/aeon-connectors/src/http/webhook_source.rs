//! HTTP Webhook source — axum server accepting POST requests as events.
//!
//! Runs an HTTP server on a configurable address. Each POST request body
//! becomes an Event. Uses the shared push buffer for three-phase backpressure.
//!
//! ## S9: Inbound authentication
//!
//! An optional [`InboundAuthVerifier`] can be attached via
//! [`HttpWebhookSourceConfig::with_auth`]. When present, every request is
//! verified in configured-mode order before any event is enqueued:
//!
//! - **`ip_allowlist`** uses the peer socket address surfaced by axum's
//!   `ConnectInfo<SocketAddr>` extractor.
//! - **`api_key`** + **`hmac`** read from `HeaderMap`.
//! - **`mtls`** expects client-cert subjects to be supplied by the TLS
//!   terminator (a future S9.2b lift; the current TCP-only listener
//!   configuration will reject any request that requires mTLS with
//!   [`AuthRejection::MtlsMissing`]).
//!
//! Rejections map to HTTP 401 (`ApiKey*`/`Hmac*`), 403
//! (`IpNotAllowed`/`Mtls*`), with the reason tag emitted in a `warn` log.
//! Bodies are read into a bounded `Bytes` buffer before the verifier runs
//! — HMAC signing needs the full body anyway, and axum's default extraction
//! already materialises it.

use crate::push_buffer::{PushBufferConfig, PushBufferRx, PushBufferTx, push_buffer};
use aeon_types::{
    AeonError, AuthContext, AuthRejection, Event, InboundAuthVerifier, PartitionId, Source,
    SourceKind,
};
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
    /// Optional S9 inbound auth verifier. When `None`, any request that
    /// reaches the configured path is accepted — intended only for
    /// in-VPC / loopback / trusted-network deployments.
    pub auth: Option<Arc<InboundAuthVerifier>>,
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
            auth: None,
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

    /// Attach an S9 inbound auth verifier. All modes configured on the
    /// verifier run in declaration order before each event is enqueued.
    pub fn with_auth(mut self, verifier: Arc<InboundAuthVerifier>) -> Self {
        self.auth = Some(verifier);
        self
    }
}

/// Shared state for the axum webhook handler.
struct WebhookState {
    tx: PushBufferTx,
    source_name: Arc<str>,
    auth: Option<Arc<InboundAuthVerifier>>,
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
            auth: config.auth,
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
            if let Err(e) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
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
    axum::extract::ConnectInfo(remote): axum::extract::ConnectInfo<SocketAddr>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> axum::http::StatusCode {
    // S9: run inbound auth before any work. Reject early so unauthenticated
    // clients never exercise the push-buffer overload check.
    if let Some(verifier) = &state.auth {
        let method_str = method.as_str();
        let path_str = uri.path();
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Materialise headers into the verifier's (&str, &[u8]) shape.
        // Bounded by the incoming header count — axum caps this by default.
        let headers_pairs: Vec<(&str, &[u8])> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_bytes()))
            .collect();

        let ctx = AuthContext {
            peer_ip: remote.ip(),
            method: method_str,
            path: path_str,
            body: body.as_ref(),
            headers: &headers_pairs,
            now_unix,
            client_cert_subjects: None,
        };

        if let Err(rejection) = verifier.verify(&ctx) {
            let source = state.source_name.as_ref();
            log_rejection(source, &rejection, remote.ip());
            return status_for(&rejection);
        }
    }

    if state.tx.is_overloaded() {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE;
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);

    let event = Event::new(
        uuid::Uuid::now_v7(),
        timestamp,
        Arc::clone(&state.source_name),
        PartitionId::new(0),
        body,
    );

    match state.tx.send(event).await {
        Ok(()) => axum::http::StatusCode::ACCEPTED,
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn status_for(rejection: &AuthRejection) -> axum::http::StatusCode {
    use AuthRejection::*;
    match rejection {
        // Credentials missing or malformed → 401 so client knows to retry
        // with auth material.
        ApiKeyMissing
        | ApiKeyInvalid
        | HmacSignatureMissing
        | HmacTimestampMissing
        | HmacTimestampMalformed
        | HmacClockSkew { .. }
        | HmacInvalid
        | HmacMalformed => axum::http::StatusCode::UNAUTHORIZED,
        // Identity-based rejection → 403 (caller is who they say but
        // isn't allowed).
        IpNotAllowed { .. }
        | MtlsMissing
        | MtlsSubjectNotAllowed { .. }
        | MtlsSubjectAllowListEmpty => axum::http::StatusCode::FORBIDDEN,
    }
}

fn log_rejection(source: &str, rejection: &AuthRejection, peer: std::net::IpAddr) {
    // Redact peer IP last octet (S2 rule). We use the rejection's own
    // redactor when it surfaces its own IP, falling back to manual
    // redaction of the socket peer for non-IP rejections.
    let redacted = rejection
        .redacted_peer_ip()
        .unwrap_or_else(|| redact_ip(peer));
    tracing::warn!(
        target: "aeon::inbound_auth",
        source = %source,
        reason = %rejection.reason_tag(),
        peer = %redacted,
        "inbound request rejected"
    );
}

fn redact_ip(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v) => {
            let o = v.octets();
            format!("{}.{}.{}.x", o[0], o[1], o[2])
        }
        std::net::IpAddr::V6(v) => {
            let s = v.segments();
            format!(
                "{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:x:x",
                s[0], s[1], s[2], s[3], s[4], s[5]
            )
        }
    }
}

impl Source for HttpWebhookSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        self.rx.next_batch(self.poll_timeout).await
    }

    fn source_kind(&self) -> SourceKind {
        SourceKind::Push
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{
        ApiKeyConfig, HmacAlgorithm, HmacConfig, InboundAuthConfig, InboundAuthMode,
        IpAllowlistConfig, MtlsConfig,
    };

    async fn start_server(state: Arc<WebhookState>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/webhook", axum::routing::post(webhook_handler))
            .route("/health", axum::routing::get(|| async { "ok" }))
            .with_state(state);
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn test_webhook_source_receives_events() {
        let (tx, mut rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("test-webhook"),
            auth: None,
        });
        let addr = start_server(state).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/webhook"))
            .body("test-payload")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);

        let batch = rx.next_batch(Duration::from_secs(1)).await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].payload.as_ref(), b"test-payload");
    }

    #[tokio::test]
    async fn test_webhook_health_endpoint() {
        let (tx, _rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("test"),
            auth: None,
        });
        let addr = start_server(state).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{addr}/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // ── S9 auth tests ──────────────────────────────────────────────

    fn verifier(cfg: InboundAuthConfig) -> Arc<InboundAuthVerifier> {
        Arc::new(InboundAuthVerifier::build(cfg).unwrap())
    }

    #[tokio::test]
    async fn auth_rejects_when_ip_not_in_allowlist() {
        // Loopback-based test: allowlist does NOT include 127.0.0.0/8.
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::IpAllowlist],
            ip_allowlist: Some(IpAllowlistConfig {
                cidrs: vec!["10.0.0.0/8".parse().unwrap()],
            }),
            ..Default::default()
        };
        let (tx, _rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("t"),
            auth: Some(verifier(cfg)),
        });
        let addr = start_server(state).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/webhook"))
            .body("p")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }

    #[tokio::test]
    async fn auth_accepts_when_ip_in_allowlist() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::IpAllowlist],
            ip_allowlist: Some(IpAllowlistConfig {
                cidrs: vec!["127.0.0.0/8".parse().unwrap()],
            }),
            ..Default::default()
        };
        let (tx, mut rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("t"),
            auth: Some(verifier(cfg)),
        });
        let addr = start_server(state).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/webhook"))
            .body("p")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        let batch = rx.next_batch(Duration::from_secs(1)).await.unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[tokio::test]
    async fn auth_rejects_missing_api_key() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::ApiKey],
            api_key: Some(ApiKeyConfig {
                header_name: "X-Aeon-Api-Key".into(),
                keys: vec!["secret".into()],
            }),
            ..Default::default()
        };
        let (tx, _rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("t"),
            auth: Some(verifier(cfg)),
        });
        let addr = start_server(state).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/webhook"))
            .body("p")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn auth_rejects_wrong_api_key() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::ApiKey],
            api_key: Some(ApiKeyConfig {
                header_name: "X-Aeon-Api-Key".into(),
                keys: vec!["correct".into()],
            }),
            ..Default::default()
        };
        let (tx, _rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("t"),
            auth: Some(verifier(cfg)),
        });
        let addr = start_server(state).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/webhook"))
            .header("X-Aeon-Api-Key", "wrong")
            .body("p")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn auth_accepts_valid_api_key() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::ApiKey],
            api_key: Some(ApiKeyConfig {
                header_name: "X-Aeon-Api-Key".into(),
                keys: vec!["secret".into()],
            }),
            ..Default::default()
        };
        let (tx, mut rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("t"),
            auth: Some(verifier(cfg)),
        });
        let addr = start_server(state).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/webhook"))
            .header("X-Aeon-Api-Key", "secret")
            .body("p")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        let batch = rx.next_batch(Duration::from_secs(1)).await.unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[tokio::test]
    async fn auth_accepts_valid_hmac_signature() {
        use aeon_types::auth::sign_request;
        let secret = "shh";
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "X-Aeon-Signature".into(),
                timestamp_header: "X-Aeon-Timestamp".into(),
                secrets: vec![secret.into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 300,
            }),
            ..Default::default()
        };
        let (tx, mut rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("t"),
            auth: Some(verifier(cfg)),
        });
        let addr = start_server(state).await;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let ts_str = now.to_string();
        let body = "payload-bytes";
        let sig = sign_request(
            HmacAlgorithm::HmacSha256,
            secret.as_bytes(),
            b"POST",
            b"/webhook",
            ts_str.as_bytes(),
            body.as_bytes(),
        )
        .unwrap();

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/webhook"))
            .header("X-Aeon-Timestamp", &ts_str)
            .header("X-Aeon-Signature", &sig)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        let batch = rx.next_batch(Duration::from_secs(1)).await.unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[tokio::test]
    async fn auth_rejects_hmac_over_tampered_body() {
        use aeon_types::auth::sign_request;
        let secret = "shh";
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "X-Aeon-Signature".into(),
                timestamp_header: "X-Aeon-Timestamp".into(),
                secrets: vec![secret.into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 300,
            }),
            ..Default::default()
        };
        let (tx, _rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("t"),
            auth: Some(verifier(cfg)),
        });
        let addr = start_server(state).await;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let ts_str = now.to_string();
        let sig = sign_request(
            HmacAlgorithm::HmacSha256,
            secret.as_bytes(),
            b"POST",
            b"/webhook",
            ts_str.as_bytes(),
            b"original",
        )
        .unwrap();

        // Send DIFFERENT body — signature won't match.
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/webhook"))
            .header("X-Aeon-Timestamp", &ts_str)
            .header("X-Aeon-Signature", &sig)
            .body("tampered")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn auth_rejects_when_mtls_required_but_no_cert() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Mtls],
            mtls: Some(MtlsConfig {
                subject_allowlist: vec!["CN=any".into()],
            }),
            ..Default::default()
        };
        let (tx, _rx) = push_buffer(PushBufferConfig::default());
        let state = Arc::new(WebhookState {
            tx,
            source_name: Arc::from("t"),
            auth: Some(verifier(cfg)),
        });
        let addr = start_server(state).await;

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/webhook"))
            .body("p")
            .send()
            .await
            .unwrap();
        // Plain HTTP (no TLS, no client cert) → MtlsMissing → 403.
        assert_eq!(resp.status(), 403);
    }
}
