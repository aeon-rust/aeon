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
use crate::websocket::mtls::build_mtls_client_config_arc;
use aeon_types::{
    AeonError, Backoff, BackoffPolicy, CoreLocalUuidGenerator, Event, OutboundAuthMode,
    OutboundAuthSigner, OutboundSignContext, PartitionId, Source, SourceKind, SsrfPolicy,
    redact_uri,
};
use bytes::Bytes;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http;

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
    /// S7: SSRF guard. Checked at initial connect AND on every reconnect
    /// so a mid-life DNS change (classic rebinding) can't quietly move
    /// the source onto an internal endpoint.
    pub ssrf_policy: SsrfPolicy,
    /// S10: outbound auth signer. HTTP-style modes (Bearer / Basic /
    /// ApiKey / HmacSign) inject headers on the WebSocket handshake.
    /// `Mtls` drives a custom rustls `Connector` built once at startup
    /// and reused across reconnects. `BrokerNative` does not apply to
    /// WebSocket and is warned-and-ignored.
    pub auth: Option<Arc<OutboundAuthSigner>>,
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
            ssrf_policy: SsrfPolicy::production(),
            auth: None,
        }
    }

    /// Attach an outbound auth signer (S10).
    pub fn with_auth(mut self, signer: Arc<OutboundAuthSigner>) -> Self {
        self.auth = Some(signer);
        self
    }

    /// Override the reconnect backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Override the SSRF policy.
    pub fn with_ssrf_policy(mut self, policy: SsrfPolicy) -> Self {
        self.ssrf_policy = policy;
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
        // S7: pre-flight SSRF check — reject bad URLs synchronously, before
        // DNS / socket. Re-checked on each reconnect below to defeat
        // rebinding (the guard re-resolves, so a mid-flight DNS swap onto
        // 127.0.0.1 or 169.254/16 is caught).
        config.ssrf_policy.check_url(&config.url)?;

        // S10: pre-parse URL path once for HMAC signing (handshake body is
        // empty, method is GET). Static modes (Bearer/Basic/ApiKey) don't
        // use the path but build the request uniformly.
        let path = extract_path(&config.url);

        // S10: branch on signer mode up front.
        // - Mtls: build a rustls ClientConfig once, Arc-clone it across
        //   reconnects so we never re-parse PEMs on the backoff path.
        // - BrokerNative: meaningless for WebSocket — warn-and-ignore.
        // - HTTP-style modes: handled by `build_ws_request` as headers.
        let mtls_tls_config: Option<Arc<rustls::ClientConfig>> = match config.auth.as_deref() {
            Some(signer) if matches!(signer.mode(), OutboundAuthMode::Mtls) => {
                let cert_pem = signer.mtls_cert_pem().ok_or_else(|| {
                    AeonError::config("websocket mTLS: signer missing cert pem")
                })?;
                let key_pem = signer.mtls_key_pem().ok_or_else(|| {
                    AeonError::config("websocket mTLS: signer missing key pem")
                })?;
                Some(build_mtls_client_config_arc(cert_pem, key_pem)?)
            }
            Some(signer) if matches!(signer.mode(), OutboundAuthMode::BrokerNative) => {
                tracing::warn!(
                    "WebSocketSource: broker_native auth mode is not applicable to WebSocket; ignored"
                );
                None
            }
            _ => None,
        };

        // Initial connect — fail fast on bad URL / unreachable host so the
        // pipeline operator sees misconfiguration immediately rather than
        // watching the backoff loop grind forever.
        let request = build_ws_request(&config.url, config.auth.as_ref(), &path)?;
        let connect_result = if let Some(cfg) = mtls_tls_config.as_ref() {
            tokio_tungstenite::connect_async_tls_with_config(
                request,
                None,
                false,
                Some(Connector::Rustls(Arc::clone(cfg))),
            )
            .await
        } else {
            tokio_tungstenite::connect_async(request).await
        };
        let (ws_stream, _) = connect_result.map_err(|e| {
            AeonError::connection(format!(
                "websocket connect failed: {}: {e}",
                redact_uri(&config.url)
            ))
        })?;

        tracing::info!(url = %redact_uri(&config.url), "WebSocketSource connected");

        let (tx, rx) = push_buffer(config.buffer_config);
        let source_name = config.source_name;
        let url = config.url;
        // Pre-redacted copy for logging — avoids recomputing at every
        // reconnect, and keeps the userinfo out of the reader task scope.
        let url_for_log: String = redact_uri(&url).into_owned();
        let ssrf_policy = config.ssrf_policy;
        let auth = config.auth;
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
                    url = %url_for_log,
                    delay_ms = delay.as_millis() as u64,
                    "websocket disconnected, reconnecting after backoff"
                );
                tokio::time::sleep(delay).await;

                // Anti-rebinding: re-run the SSRF guard every reconnect so
                // a DNS record that flipped to a private IP between the
                // initial connect and now fails closed here.
                if let Err(e) = ssrf_policy.check_url(&url) {
                    tracing::error!(
                        url = %url_for_log,
                        error = %e,
                        "websocket reconnect refused by SSRF policy — terminating reader task"
                    );
                    return;
                }

                let request = match build_ws_request(&url, auth.as_ref(), &path) {
                    Ok(r) => r,
                    Err(e) => {
                        // Sign failures on reconnect (e.g. HMAC ctx error) are
                        // transient — warn and retry on the next backoff tick.
                        tracing::warn!(
                            url = %url_for_log,
                            error = %e,
                            "websocket reconnect auth sign failed"
                        );
                        continue;
                    }
                };
                let reconnect_result = if let Some(cfg) = mtls_tls_config.as_ref() {
                    tokio_tungstenite::connect_async_tls_with_config(
                        request,
                        None,
                        false,
                        Some(Connector::Rustls(Arc::clone(cfg))),
                    )
                    .await
                } else {
                    tokio_tungstenite::connect_async(request).await
                };
                match reconnect_result {
                    Ok((stream, _)) => {
                        tracing::info!(url = %url_for_log, "websocket reconnected");
                        backoff.reset();
                        if !run_ws_reader(stream, &tx, &source_name, &mut uuid_gen).await {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(url = %url_for_log, error = %e, "websocket reconnect failed");
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

    fn source_kind(&self) -> SourceKind {
        SourceKind::Push
    }
}

/// Extract the path (including query) from a ws/wss URL for HMAC canonicalization.
/// Falls back to "/" if the URL is malformed — the handshake will fail downstream
/// anyway, so the sign step doesn't need to second-guess URL validity.
fn extract_path(url: &str) -> String {
    // ws://host:port/path?query — find the third '/' (after scheme://) or default
    let after_scheme = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => return "/".to_string(),
    };
    match after_scheme.find('/') {
        Some(i) => after_scheme[i..].to_string(),
        None => "/".to_string(),
    }
}

/// Build the WebSocket handshake request, attaching S10 auth headers when a
/// signer is configured. HTTP-applicable modes (Bearer/Basic/ApiKey/HmacSign)
/// contribute headers; `None`/`Mtls`/`BrokerNative` return an empty set and
/// the request is unmodified.
fn build_ws_request(
    url: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
    path: &str,
) -> Result<http::Request<()>, AeonError> {
    let mut request: http::Request<()> = url.into_client_request().map_err(|e| {
        AeonError::connection(format!(
            "websocket request build failed: {}: {e}",
            redact_uri(url)
        ))
    })?;
    if let Some(s) = signer {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let ctx = OutboundSignContext {
            method: "GET",
            path,
            body: b"",
            now_unix,
        };
        let headers = s
            .http_headers(&ctx)
            .map_err(|e| AeonError::connection(format!("websocket auth sign failed: {e}")))?;
        for (k, v) in headers {
            let name = http::HeaderName::try_from(k)
                .map_err(|e| AeonError::config(format!("websocket auth header name: {e}")))?;
            let value = http::HeaderValue::try_from(v)
                .map_err(|e| AeonError::config(format!("websocket auth header value: {e}")))?;
            request.headers_mut().insert(name, value);
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{
        BasicConfig, BearerConfig, HmacAlgorithm, HmacSignConfig, OutboundApiKeyConfig,
        OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner, OutboundMtlsConfig,
    };

    fn fixture_cert_and_key() -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn extract_path_handles_common_cases() {
        assert_eq!(extract_path("ws://host:8080/ws"), "/ws");
        assert_eq!(extract_path("wss://host/ws?x=1"), "/ws?x=1");
        assert_eq!(extract_path("ws://host:8080"), "/");
        assert_eq!(extract_path("garbage"), "/");
    }

    fn make_signer(config: OutboundAuthConfig) -> Arc<OutboundAuthSigner> {
        Arc::new(OutboundAuthSigner::build(config).unwrap())
    }

    #[test]
    fn build_request_without_signer_has_no_auth_headers() {
        let req = build_ws_request("ws://127.0.0.1:8080/ws", None, "/ws").unwrap();
        assert!(req.headers().get("authorization").is_none());
    }

    #[test]
    fn build_request_bearer_injects_authorization() {
        let signer = make_signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Bearer,
            bearer: Some(BearerConfig {
                token: "tkn-abc".to_string(),
            }),
            ..Default::default()
        });
        let req = build_ws_request("ws://127.0.0.1:8080/ws", Some(&signer), "/ws").unwrap();
        assert_eq!(
            req.headers().get("authorization").unwrap().as_bytes(),
            b"Bearer tkn-abc"
        );
    }

    #[test]
    fn build_request_basic_injects_authorization() {
        let signer = make_signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Basic,
            basic: Some(BasicConfig {
                username: "u".to_string(),
                password: "p".to_string(),
            }),
            ..Default::default()
        });
        let req = build_ws_request("ws://127.0.0.1:8080/ws", Some(&signer), "/ws").unwrap();
        let v = req.headers().get("authorization").unwrap();
        assert!(v.to_str().unwrap().starts_with("Basic "));
    }

    #[test]
    fn build_request_api_key_injects_custom_header() {
        let signer = make_signer(OutboundAuthConfig {
            mode: OutboundAuthMode::ApiKey,
            api_key: Some(OutboundApiKeyConfig {
                header_name: "X-Aeon-Key".to_string(),
                key: "k-xyz".to_string(),
            }),
            ..Default::default()
        });
        let req = build_ws_request("ws://127.0.0.1:8080/ws", Some(&signer), "/ws").unwrap();
        assert_eq!(req.headers().get("x-aeon-key").unwrap().as_bytes(), b"k-xyz");
        // And no Authorization fallback on api_key mode.
        assert!(req.headers().get("authorization").is_none());
    }

    #[tokio::test]
    async fn new_mtls_with_unreachable_port_fails_with_connection_error() {
        // Ensures the mTLS branch parses PEMs at startup without erroring out
        // as a config problem — a real connect attempt must reach the socket
        // layer (port 1 is "reserved + never listening" on most hosts).
        let (cert_pem, key_pem) = fixture_cert_and_key();
        let signer = Arc::new(
            OutboundAuthSigner::build(OutboundAuthConfig {
                mode: OutboundAuthMode::Mtls,
                mtls: Some(OutboundMtlsConfig { cert_pem, key_pem }),
                ..Default::default()
            })
            .unwrap(),
        );
        let cfg = WebSocketSourceConfig::new("wss://127.0.0.1:1/ws")
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests())
            .with_poll_timeout(Duration::from_millis(50))
            .with_auth(signer);
        let err = match WebSocketSource::new(cfg).await {
            Ok(_) => panic!("expected connect failure on unreachable port"),
            Err(e) => e,
        };
        // We expect a connection-class error (TCP refused / TLS failed),
        // NOT a Config error. A Config error would mean the PEM parser
        // rejected our fixture — the point is to prove we got past parsing.
        let msg = format!("{err:?}");
        assert!(
            !msg.to_lowercase().contains("missing cert pem")
                && !msg.to_lowercase().contains("missing key pem"),
            "unexpected config-class error from mTLS PEM plumbing: {msg}"
        );
    }

    #[test]
    fn build_request_hmac_sign_injects_timestamp_and_signature() {
        let signer = make_signer(OutboundAuthConfig {
            mode: OutboundAuthMode::HmacSign,
            hmac_sign: Some(HmacSignConfig {
                algorithm: HmacAlgorithm::HmacSha256,
                secret: "s3cret".to_string(),
                timestamp_header: "X-Aeon-Ts".to_string(),
                signature_header: "X-Aeon-Sig".to_string(),
            }),
            ..Default::default()
        });
        let req = build_ws_request("ws://127.0.0.1:8080/ws", Some(&signer), "/ws").unwrap();
        assert!(req.headers().get("x-aeon-ts").is_some());
        assert!(req.headers().get("x-aeon-sig").is_some());
    }

}
