//! WebTransport Streams sink — reliable, ordered delivery.
//!
//! Connects to a WebTransport server and sends outputs as
//! length-prefixed messages on bidirectional streams.

use aeon_types::{
    AeonError, BatchResult, OutboundAuthMode, OutboundAuthSigner, OutboundSignContext, Output,
    Sink, SsrfPolicy, redact_uri,
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use wtransport::endpoint::ConnectOptions;

/// Configuration for `WebTransportSink`.
pub struct WebTransportSinkConfig {
    /// WebTransport URL to connect to (e.g., "https://localhost:4472").
    pub url: String,
    /// wtransport client configuration.
    pub client_config: wtransport::ClientConfig,
    /// S7: SSRF guard. Checked at connection time.
    pub ssrf_policy: SsrfPolicy,
    /// S10: outbound auth signer. HTTP-style modes (Bearer / Basic /
    /// ApiKey / HmacSign) inject headers on the HTTP/3 CONNECT request
    /// that opens the WebTransport session. `Mtls` mode requires the
    /// client certificate be baked into `client_config` — use
    /// [`crate::webtransport::mtls_client_config_from_signer`] to build
    /// one from the signer, then pass it to [`WebTransportSinkConfig::new`].
    /// `BrokerNative` does not apply to WebTransport and is warned-and-ignored.
    pub auth: Option<Arc<OutboundAuthSigner>>,
}

impl WebTransportSinkConfig {
    /// Create a config for connecting to a WebTransport server.
    pub fn new(url: impl Into<String>, client_config: wtransport::ClientConfig) -> Self {
        Self {
            url: url.into(),
            client_config,
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

/// WebTransport Streams output sink.
///
/// Maintains a persistent WebTransport session. Opens a new bidirectional
/// stream for each batch and sends length-prefixed messages.
pub struct WebTransportSink {
    connection: wtransport::Connection,
    delivered: u64,
}

impl WebTransportSink {
    /// Connect to the WebTransport server. The URL is validated against
    /// the [`SsrfPolicy`] before the QUIC handshake is initiated.
    pub async fn new(config: WebTransportSinkConfig) -> Result<Self, AeonError> {
        config.ssrf_policy.check_url(&config.url)?;

        // S10: warn-once on modes that don't translate to CONNECT headers.
        if let Some(signer) = &config.auth {
            match signer.mode() {
                OutboundAuthMode::Mtls => tracing::info!(
                    "WebTransportSink: mTLS signer present — client_config must have been \
                     built via webtransport::mtls_client_config_from_signer for the cert \
                     to be presented; signer cert/key are advisory at this layer"
                ),
                OutboundAuthMode::BrokerNative => tracing::warn!(
                    "WebTransportSink: broker_native auth mode is not applicable to WebTransport; ignored"
                ),
                _ => {}
            }
        }

        let options = build_connect_options(&config.url, config.auth.as_ref())?;

        let endpoint = wtransport::Endpoint::client(config.client_config).map_err(|e| {
            AeonError::connection(format!("webtransport client endpoint failed: {e}"))
        })?;

        let connection = endpoint.connect(options).await.map_err(|e| {
            AeonError::connection(format!(
                "webtransport connect failed to {}: {e}",
                redact_uri(&config.url)
            ))
        })?;

        tracing::info!(url = %redact_uri(&config.url), "WebTransportSink connected");

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

/// Extract the path (including query) from an https URL for HMAC
/// canonicalization. WebTransport URLs are always `https://`.
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

/// Build `ConnectOptions` for the WebTransport session, attaching S10 auth
/// headers when a signer is configured. HTTP-style auth modes serialize
/// through the HTTP/3 CONNECT headers; `Mtls` / `BrokerNative` return no
/// headers and are warned on at the call site.
fn build_connect_options(
    url: &str,
    signer: Option<&Arc<OutboundAuthSigner>>,
) -> Result<ConnectOptions, AeonError> {
    let mut builder = ConnectOptions::builder(url);
    if let Some(s) = signer {
        let path = extract_path(url);
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let ctx = OutboundSignContext {
            method: "CONNECT",
            path: &path,
            body: b"",
            now_unix,
        };
        let headers = s
            .http_headers(&ctx)
            .map_err(|e| AeonError::connection(format!("webtransport auth sign failed: {e}")))?;
        for (k, v) in headers {
            builder = builder.add_header(k, v);
        }
    }
    Ok(builder.build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{
        BasicConfig, BearerConfig, BrokerNativeConfig, HmacAlgorithm, HmacSignConfig,
        OutboundApiKeyConfig, OutboundAuthConfig, OutboundMtlsConfig,
    };
    use std::collections::BTreeMap;

    fn signer(config: OutboundAuthConfig) -> Arc<OutboundAuthSigner> {
        Arc::new(OutboundAuthSigner::build(config).unwrap())
    }

    fn header(opts: &ConnectOptions, name: &str) -> Option<String> {
        opts.additional_headers().get(name).cloned()
    }

    #[test]
    fn extract_path_handles_common_cases() {
        assert_eq!(extract_path("https://host:4472/wt"), "/wt");
        assert_eq!(extract_path("https://host/wt?t=1"), "/wt?t=1");
        assert_eq!(extract_path("https://host"), "/");
        assert_eq!(extract_path("garbage"), "/");
    }

    #[test]
    fn no_signer_adds_no_headers() {
        let opts = build_connect_options("https://127.0.0.1:4472/wt", None).unwrap();
        assert!(opts.additional_headers().is_empty());
        assert_eq!(opts.url(), "https://127.0.0.1:4472/wt");
    }

    #[test]
    fn bearer_injects_authorization_header() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Bearer,
            bearer: Some(BearerConfig {
                token: "tok-123".to_string(),
            }),
            ..Default::default()
        });
        let opts = build_connect_options("https://127.0.0.1:4472/wt", Some(&s)).unwrap();
        assert_eq!(
            header(&opts, "Authorization").as_deref(),
            Some("Bearer tok-123")
        );
    }

    #[test]
    fn basic_injects_authorization_header() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Basic,
            basic: Some(BasicConfig {
                username: "u".to_string(),
                password: "p".to_string(),
            }),
            ..Default::default()
        });
        let opts = build_connect_options("https://127.0.0.1:4472/wt", Some(&s)).unwrap();
        let val = header(&opts, "Authorization").expect("authorization header present");
        assert!(val.starts_with("Basic "), "got: {val}");
    }

    #[test]
    fn api_key_injects_custom_header() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::ApiKey,
            api_key: Some(OutboundApiKeyConfig {
                header_name: "X-Aeon-Key".to_string(),
                key: "abc".to_string(),
            }),
            ..Default::default()
        });
        let opts = build_connect_options("https://127.0.0.1:4472/wt", Some(&s)).unwrap();
        assert_eq!(header(&opts, "X-Aeon-Key").as_deref(), Some("abc"));
    }

    #[test]
    fn hmac_injects_signature_and_timestamp() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::HmacSign,
            hmac_sign: Some(HmacSignConfig {
                signature_header: "X-Aeon-Signature".to_string(),
                timestamp_header: "X-Aeon-Timestamp".to_string(),
                secret: "shhh".to_string(),
                algorithm: HmacAlgorithm::HmacSha256,
            }),
            ..Default::default()
        });
        let opts = build_connect_options("https://127.0.0.1:4472/wt", Some(&s)).unwrap();
        assert!(header(&opts, "X-Aeon-Signature").is_some());
        assert!(header(&opts, "X-Aeon-Timestamp").is_some());
    }

    #[test]
    fn mtls_mode_adds_no_headers() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::Mtls,
            mtls: Some(OutboundMtlsConfig {
                cert_pem: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
                    .to_string(),
                key_pem: "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n"
                    .to_string(),
            }),
            ..Default::default()
        });
        let opts = build_connect_options("https://127.0.0.1:4472/wt", Some(&s)).unwrap();
        assert!(opts.additional_headers().is_empty());
    }

    #[test]
    fn broker_native_mode_adds_no_headers() {
        let mut values = BTreeMap::new();
        values.insert("anything".to_string(), "x".to_string());
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::BrokerNative,
            broker_native: Some(BrokerNativeConfig { values }),
            ..Default::default()
        });
        let opts = build_connect_options("https://127.0.0.1:4472/wt", Some(&s)).unwrap();
        assert!(opts.additional_headers().is_empty());
    }

    #[test]
    fn none_mode_adds_no_headers() {
        let s = signer(OutboundAuthConfig {
            mode: OutboundAuthMode::None,
            ..Default::default()
        });
        let opts = build_connect_options("https://127.0.0.1:4472/wt", Some(&s)).unwrap();
        assert!(opts.additional_headers().is_empty());
    }
}
