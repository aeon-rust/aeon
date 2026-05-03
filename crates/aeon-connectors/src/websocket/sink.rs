//! WebSocket sink — connects to a WS server and sends outputs as messages.
//!
//! Each output payload is sent as a WebSocket binary message.
//! The connection is maintained across `write_batch()` calls.

use crate::websocket::mtls::build_mtls_connector;
use aeon_types::{
    AeonError, BatchResult, OutboundAuthMode, OutboundAuthSigner, OutboundSignContext, Output,
    Sink, SsrfPolicy, redact_uri,
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http;

/// Configuration for `WebSocketSink`.
pub struct WebSocketSinkConfig {
    /// WebSocket URL to connect to.
    pub url: String,
    /// S7: SSRF guard. The URL is checked against this policy before dial.
    pub ssrf_policy: SsrfPolicy,
    /// S10: outbound auth signer. HTTP-style modes (Bearer / Basic /
    /// ApiKey / HmacSign) inject headers on the WebSocket handshake.
    /// `Mtls` drives a custom rustls `Connector` — the signer's
    /// cert/key are presented during the TLS handshake; the remote
    /// server is verified against the public `webpki-roots` store.
    /// `BrokerNative` does not apply to WebSocket and is warned-
    /// and-ignored.
    pub auth: Option<Arc<OutboundAuthSigner>>,
}

impl WebSocketSinkConfig {
    /// Create a config for a WebSocket sink.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ssrf_policy: SsrfPolicy::production(),
            auth: None,
        }
    }

    /// Override the SSRF policy.
    pub fn with_ssrf_policy(mut self, policy: SsrfPolicy) -> Self {
        self.ssrf_policy = policy;
        self
    }

    /// Attach an outbound auth signer (S10).
    pub fn with_auth(mut self, signer: Arc<OutboundAuthSigner>) -> Self {
        self.auth = Some(signer);
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

        // Branch on signer mode up front:
        // - Mtls drives a custom rustls connector (below).
        // - BrokerNative is meaningless for WebSocket — warn once.
        // - HTTP-style modes are handled by `build_ws_request`.
        let mtls_connector = match config.auth.as_deref() {
            Some(signer) if matches!(signer.mode(), OutboundAuthMode::Mtls) => {
                let cert_pem = signer
                    .mtls_cert_pem()
                    .ok_or_else(|| AeonError::config("websocket mTLS: signer missing cert pem"))?;
                let key_pem = signer
                    .mtls_key_pem()
                    .ok_or_else(|| AeonError::config("websocket mTLS: signer missing key pem"))?;
                Some(build_mtls_connector(cert_pem, key_pem)?)
            }
            Some(signer) if matches!(signer.mode(), OutboundAuthMode::BrokerNative) => {
                tracing::warn!(
                    "WebSocketSink: broker_native auth mode is not applicable to WebSocket; ignored"
                );
                None
            }
            _ => None,
        };

        let path = extract_path(&config.url);
        let request = build_ws_request(&config.url, config.auth.as_ref(), &path)?;

        // Use `connect_async_tls_with_config` when we have a custom
        // connector so the mTLS identity is presented on handshake.
        // The default path (no connector) still works for ws:// plaintext
        // and wss:// with public webpki roots.
        let connect_result = if mtls_connector.is_some() {
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, mtls_connector)
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

/// Extract the path (including query) from a ws/wss URL for HMAC canonicalization.
fn extract_path(url: &str) -> String {
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
/// signer is configured.
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
    use aeon_types::{BearerConfig, HmacAlgorithm, HmacSignConfig, OutboundAuthConfig};

    fn make_signer(config: OutboundAuthConfig) -> Arc<OutboundAuthSigner> {
        Arc::new(OutboundAuthSigner::build(config).unwrap())
    }

    #[test]
    fn extract_path_handles_common_cases() {
        assert_eq!(extract_path("ws://host:8080/sink"), "/sink");
        assert_eq!(extract_path("wss://host/s?t=1"), "/s?t=1");
        assert_eq!(extract_path("ws://host"), "/");
    }

    #[test]
    fn build_request_without_signer_has_no_auth_headers() {
        let req = build_ws_request("ws://127.0.0.1:8080/sink", None, "/sink").unwrap();
        assert!(req.headers().get("authorization").is_none());
    }

    #[test]
    fn build_request_bearer_injects_authorization() {
        let signer = make_signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Bearer,
            bearer: Some(BearerConfig {
                token: "tkn-sink".to_string(),
            }),
            ..Default::default()
        });
        let req = build_ws_request("ws://127.0.0.1:8080/sink", Some(&signer), "/sink").unwrap();
        assert_eq!(
            req.headers().get("authorization").unwrap().as_bytes(),
            b"Bearer tkn-sink"
        );
    }

    #[test]
    fn build_request_hmac_sign_injects_timestamp_and_signature() {
        let signer = make_signer(OutboundAuthConfig {
            mode: OutboundAuthMode::HmacSign,
            hmac_sign: Some(HmacSignConfig {
                algorithm: HmacAlgorithm::HmacSha256,
                secret: "s".to_string(),
                timestamp_header: "X-Ts".to_string(),
                signature_header: "X-Sig".to_string(),
            }),
            ..Default::default()
        });
        let req = build_ws_request("ws://127.0.0.1:8080/sink", Some(&signer), "/sink").unwrap();
        assert!(req.headers().get("x-ts").is_some());
        assert!(req.headers().get("x-sig").is_some());
    }
}
