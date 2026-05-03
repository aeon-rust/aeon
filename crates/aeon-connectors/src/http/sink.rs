//! HTTP Sink — POST outputs to an external HTTP endpoint.
//!
//! Each output payload is sent as an HTTP POST request body.
//! Supports configurable URL, headers, timeout, and batch-as-JSON mode.
//! Enables Aeon → serverless fan-out (Lambda, Cloud Functions, webhooks).

use aeon_types::{
    AeonError, BatchResult, OutboundAuthMode, OutboundAuthSigner, OutboundSignContext, Output,
    Sink, SinkAckCallback, SsrfPolicy, redact_uri,
};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Configuration for `HttpSink`.
pub struct HttpSinkConfig {
    /// URL to POST outputs to.
    pub url: String,
    /// HTTP request timeout.
    pub timeout: Duration,
    /// Optional headers to include in requests.
    pub headers: Vec<(String, String)>,
    /// S7: SSRF guard. The URL is checked against this policy at sink
    /// creation time — a denied URL fails fast instead of turning every
    /// POST into an observable denial.
    pub ssrf_policy: SsrfPolicy,
    /// S10: optional outbound auth signer. Exactly one mode per connector:
    /// Bearer / Basic / ApiKey / HmacSign inject per-request headers;
    /// Mtls installs a client identity on the reqwest client at build time;
    /// BrokerNative is not applicable to HTTP (warned and ignored);
    /// None is a no-op.
    pub auth: Option<Arc<OutboundAuthSigner>>,
}

impl HttpSinkConfig {
    /// Create an HTTP sink config.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout: Duration::from_secs(30),
            headers: Vec::new(),
            ssrf_policy: SsrfPolicy::production(),
            auth: None,
        }
    }

    /// Set request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Add a header to include in requests.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Override the SSRF policy.
    pub fn with_ssrf_policy(mut self, policy: SsrfPolicy) -> Self {
        self.ssrf_policy = policy;
        self
    }

    /// S10: attach an outbound auth signer. The signer's mode determines
    /// whether headers are injected per-request (Bearer / Basic / ApiKey /
    /// HmacSign) or whether a client identity is installed on the reqwest
    /// client at construction time (Mtls).
    pub fn with_auth(mut self, signer: Arc<OutboundAuthSigner>) -> Self {
        self.auth = Some(signer);
        self
    }
}

/// HTTP output sink.
///
/// Each output is sent as an HTTP POST with the payload as body.
/// Content-Type defaults to `application/octet-stream` unless overridden
/// by a header in the config.
pub struct HttpSink {
    config: HttpSinkConfig,
    client: reqwest::Client,
    /// Pre-parsed URL path, fed to the S10 [`OutboundSignContext`] so we
    /// don't re-parse the URL for every POST. Empty string when the URL
    /// has no path component.
    path: String,
    delivered: u64,
    /// Engine-installed callback fired when POSTs return 2xx. Drives the
    /// `outputs_acked_total` companion metric so operators can see the gap
    /// between `write_batch()` calls and receiver-confirmed delivery.
    ack_callback: Option<SinkAckCallback>,
}

impl HttpSink {
    /// Create a new HTTP sink. Validates the destination URL against
    /// [`SsrfPolicy`] before opening the reqwest client — a denied URL
    /// surfaces as `AeonError::Config` at startup instead of silently
    /// dialing an internal endpoint on every batch.
    pub fn new(config: HttpSinkConfig) -> Result<Self, AeonError> {
        config.ssrf_policy.check_url(&config.url)?;

        let parsed = reqwest::Url::parse(&config.url)
            .map_err(|e| AeonError::config(format!("http sink url parse failed: {e}")))?;
        let path = parsed.path().to_string();

        let mut builder = reqwest::Client::builder().timeout(config.timeout);

        // S10: mTLS mode installs a client identity on the reqwest client
        // at build time — it can't be expressed as a per-request header.
        // BrokerNative is not applicable to HTTP; warn so operators catch
        // the misconfiguration without failing the pipeline.
        if let Some(signer) = &config.auth {
            match signer.mode() {
                OutboundAuthMode::Mtls => {
                    let cert = signer.mtls_cert_pem().unwrap_or_default();
                    let key = signer.mtls_key_pem().unwrap_or_default();
                    let mut pem = Vec::with_capacity(cert.len() + key.len() + 1);
                    pem.extend_from_slice(cert);
                    if !cert.ends_with(b"\n") {
                        pem.push(b'\n');
                    }
                    pem.extend_from_slice(key);
                    let identity = reqwest::Identity::from_pem(&pem)
                        .map_err(|e| AeonError::config(format!("http sink mtls identity: {e}")))?;
                    builder = builder.identity(identity);
                }
                OutboundAuthMode::BrokerNative => {
                    tracing::warn!(
                        "HttpSink: broker_native auth mode is not applicable to HTTP; ignored"
                    );
                }
                _ => {}
            }
        }

        let client = builder
            .build()
            .map_err(|e| AeonError::connection(format!("http client build failed: {e}")))?;

        tracing::info!(
            url = %redact_uri(&config.url),
            auth_mode = config.auth.as_ref().map(|s| s.mode().tag()).unwrap_or("unset"),
            "HttpSink created"
        );

        Ok(Self {
            config,
            client,
            path,
            delivered: 0,
            ack_callback: None,
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    fn fire_ack(&self, n: usize) {
        if n == 0 {
            return;
        }
        if let Some(cb) = self.ack_callback.as_ref() {
            cb(n);
        }
    }
}

impl Sink for HttpSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();
        let mut acked = 0usize;

        for output in &outputs {
            let mut request = self.client.post(&self.config.url);

            // Apply configured headers
            for (key, value) in &self.config.headers {
                request = request.header(key.as_str(), value.as_str());
            }

            // Apply output-specific headers
            for (key, value) in &output.headers {
                request = request.header(key.as_ref(), value.as_ref());
            }

            // S10: merge outbound auth headers. `http_headers` returns an
            // empty Vec for modes that don't contribute HTTP headers
            // (None / Mtls / BrokerNative), so we can call it uniformly.
            if let Some(signer) = &self.config.auth {
                let now_unix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let ctx = OutboundSignContext {
                    method: "POST",
                    path: &self.path,
                    body: &output.payload,
                    now_unix,
                };
                let auth_headers = signer.http_headers(&ctx).map_err(|e| {
                    AeonError::connection(format!("http sink auth sign failed: {e}"))
                })?;
                for (k, v) in auth_headers {
                    request = request.header(k, v);
                }
            }

            let response = request
                .body(output.payload.clone())
                .send()
                .await
                .map_err(|e| {
                    AeonError::connection(format!(
                        "http sink POST failed: {}: {e}",
                        redact_uri(&self.config.url)
                    ))
                })?;

            if !response.status().is_success() {
                // Fire any partial acks accumulated before this failure so the
                // metric doesn't under-report when the batch error-exits.
                self.fire_ack(acked);
                return Err(AeonError::connection(format!(
                    "http sink POST returned {}: {}",
                    response.status(),
                    redact_uri(&self.config.url),
                )));
            }

            self.delivered += 1;
            acked += 1;
        }

        self.fire_ack(acked);
        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        // HTTP POST is synchronous per-request; nothing to flush.
        Ok(())
    }

    fn on_ack_callback(&mut self, cb: SinkAckCallback) {
        self.ack_callback = Some(cb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_http_sink_posts_data() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let received = Arc::new(AtomicUsize::new(0));
        let received_clone = Arc::clone(&received);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = axum::Router::new().route(
            "/ingest",
            axum::routing::post(move |body: Bytes| {
                let received = Arc::clone(&received_clone);
                async move {
                    assert!(!body.is_empty());
                    received.fetch_add(1, Ordering::Relaxed);
                    "ok"
                }
            }),
        );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpSinkConfig::new(format!("http://{addr}/ingest"))
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests());
        let mut sink = HttpSink::new(config).unwrap();

        let outputs: Vec<Output> = (0..3)
            .map(|i| Output {
                destination: Arc::from("test"),
                key: None,
                payload: Bytes::from(format!("payload-{i}")),
                headers: Default::default(),
                source_ts: None,
                source_event_id: None,
                source_partition: None,
                source_offset: None,
                l2_seq: None,
            })
            .collect();

        let result = sink.write_batch(outputs).await.unwrap();
        assert_eq!(result.delivered.len(), 0); // no source_event_ids set
        assert_eq!(sink.delivered(), 3);
        assert_eq!(received.load(std::sync::atomic::Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn test_http_sink_propagates_error_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = axum::Router::new().route(
            "/fail",
            axum::routing::post(|| async {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "error")
            }),
        );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpSinkConfig::new(format!("http://{addr}/fail"))
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests());
        let mut sink = HttpSink::new(config).unwrap();

        let outputs = vec![Output {
            destination: Arc::from("test"),
            key: None,
            payload: Bytes::from("data"),
            headers: Default::default(),
            source_ts: None,
            source_event_id: None,
            source_partition: None,
            source_offset: None,
            l2_seq: None,
        }];

        let result = sink.write_batch(outputs).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_rejects_imds_url_at_creation() {
        // Production-default policy — caller didn't opt out. 169.254.169.254
        // is on the always-denied list, so new() must fail before any
        // socket is opened.
        let config = HttpSinkConfig::new("http://169.254.169.254/latest/meta-data/");
        let Err(err) = HttpSink::new(config) else {
            panic!("expected SSRF rejection, but new() succeeded");
        };
        assert!(matches!(err, AeonError::Config { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn ssrf_rejects_loopback_under_production_policy() {
        // Production default denies 127/8 — pointing a sink at
        // http://127.0.0.1:4471/... must fail closed.
        let config = HttpSinkConfig::new("http://127.0.0.1:1/whatever");
        let Err(err) = HttpSink::new(config) else {
            panic!("expected SSRF rejection, but new() succeeded");
        };
        assert!(matches!(err, AeonError::Config { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn auth_bearer_injects_authorization_header() {
        use aeon_types::{BearerConfig, OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let seen_bearer = Arc::new(AtomicUsize::new(0));
        let seen_clone = Arc::clone(&seen_bearer);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = axum::Router::new().route(
            "/auth",
            axum::routing::post(move |headers: axum::http::HeaderMap, _body: Bytes| {
                let seen = Arc::clone(&seen_clone);
                async move {
                    if headers.get("authorization").map(|v| v.as_bytes())
                        == Some(b"Bearer tkn-xyz".as_slice())
                    {
                        seen.fetch_add(1, Ordering::Relaxed);
                    }
                    "ok"
                }
            }),
        );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let signer = Arc::new(
            OutboundAuthSigner::build(OutboundAuthConfig {
                mode: OutboundAuthMode::Bearer,
                bearer: Some(BearerConfig {
                    token: "tkn-xyz".to_string(),
                }),
                ..Default::default()
            })
            .unwrap(),
        );

        let config = HttpSinkConfig::new(format!("http://{addr}/auth"))
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests())
            .with_auth(signer);
        let mut sink = HttpSink::new(config).unwrap();

        let outputs = vec![Output {
            destination: Arc::from("t"),
            key: None,
            payload: Bytes::from("data"),
            headers: Default::default(),
            source_ts: None,
            source_event_id: None,
            source_partition: None,
            source_offset: None,
            l2_seq: None,
        }];

        sink.write_batch(outputs).await.unwrap();
        assert_eq!(seen_bearer.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn auth_hmac_sign_injects_timestamp_and_signature_headers() {
        use aeon_types::{
            HmacSignConfig, OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        let sig_seen = Arc::new(AtomicUsize::new(0));
        let ts_seen = Arc::new(AtomicUsize::new(0));
        let sig_clone = Arc::clone(&sig_seen);
        let ts_clone = Arc::clone(&ts_seen);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = axum::Router::new().route(
            "/hmac",
            axum::routing::post(move |headers: axum::http::HeaderMap, _body: Bytes| {
                let sig = Arc::clone(&sig_clone);
                let ts = Arc::clone(&ts_clone);
                async move {
                    if let Some(v) = headers.get("x-aeon-signature") {
                        let s = v.to_str().unwrap_or("");
                        if !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit()) {
                            sig.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if let Some(v) = headers.get("x-aeon-timestamp") {
                        let s = v.to_str().unwrap_or("");
                        if s.parse::<i64>().is_ok() {
                            ts.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    "ok"
                }
            }),
        );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let signer = Arc::new(
            OutboundAuthSigner::build(OutboundAuthConfig {
                mode: OutboundAuthMode::HmacSign,
                hmac_sign: Some(HmacSignConfig {
                    signature_header: "X-Aeon-Signature".into(),
                    timestamp_header: "X-Aeon-Timestamp".into(),
                    secret: "shh".into(),
                    algorithm: aeon_types::HmacAlgorithm::HmacSha256,
                }),
                ..Default::default()
            })
            .unwrap(),
        );

        let config = HttpSinkConfig::new(format!("http://{addr}/hmac"))
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests())
            .with_auth(signer);
        let mut sink = HttpSink::new(config).unwrap();

        let outputs = vec![Output {
            destination: Arc::from("t"),
            key: None,
            payload: Bytes::from("payload-body"),
            headers: Default::default(),
            source_ts: None,
            source_event_id: None,
            source_partition: None,
            source_offset: None,
            l2_seq: None,
        }];

        sink.write_batch(outputs).await.unwrap();
        assert_eq!(sig_seen.load(Ordering::Relaxed), 1);
        assert_eq!(ts_seen.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn auth_none_injects_no_auth_headers() {
        use aeon_types::{OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let seen_no_auth = Arc::new(AtomicUsize::new(0));
        let seen_clone = Arc::clone(&seen_no_auth);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = axum::Router::new().route(
            "/noauth",
            axum::routing::post(move |headers: axum::http::HeaderMap, _body: Bytes| {
                let seen = Arc::clone(&seen_clone);
                async move {
                    if headers.get("authorization").is_none()
                        && headers.get("x-aeon-api-key").is_none()
                        && headers.get("x-aeon-signature").is_none()
                    {
                        seen.fetch_add(1, Ordering::Relaxed);
                    }
                    "ok"
                }
            }),
        );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let signer = Arc::new(
            OutboundAuthSigner::build(OutboundAuthConfig {
                mode: OutboundAuthMode::None,
                ..Default::default()
            })
            .unwrap(),
        );

        let config = HttpSinkConfig::new(format!("http://{addr}/noauth"))
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests())
            .with_auth(signer);
        let mut sink = HttpSink::new(config).unwrap();

        let outputs = vec![Output {
            destination: Arc::from("t"),
            key: None,
            payload: Bytes::from("x"),
            headers: Default::default(),
            source_ts: None,
            source_event_id: None,
            source_partition: None,
            source_offset: None,
            l2_seq: None,
        }];

        sink.write_batch(outputs).await.unwrap();
        assert_eq!(seen_no_auth.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn auth_api_key_injects_custom_header() {
        use aeon_types::{
            OutboundApiKeyConfig, OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        let seen = Arc::new(AtomicUsize::new(0));
        let seen_clone = Arc::clone(&seen);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = axum::Router::new().route(
            "/api",
            axum::routing::post(move |headers: axum::http::HeaderMap, _body: Bytes| {
                let seen = Arc::clone(&seen_clone);
                async move {
                    if headers.get("x-upstream-token").map(|v| v.as_bytes())
                        == Some(b"k-123".as_slice())
                    {
                        seen.fetch_add(1, Ordering::Relaxed);
                    }
                    "ok"
                }
            }),
        );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let signer = Arc::new(
            OutboundAuthSigner::build(OutboundAuthConfig {
                mode: OutboundAuthMode::ApiKey,
                api_key: Some(OutboundApiKeyConfig {
                    header_name: "X-Upstream-Token".into(),
                    key: "k-123".into(),
                }),
                ..Default::default()
            })
            .unwrap(),
        );

        let config = HttpSinkConfig::new(format!("http://{addr}/api"))
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests())
            .with_auth(signer);
        let mut sink = HttpSink::new(config).unwrap();

        let outputs = vec![Output {
            destination: Arc::from("t"),
            key: None,
            payload: Bytes::from("x"),
            headers: Default::default(),
            source_ts: None,
            source_event_id: None,
            source_partition: None,
            source_offset: None,
            l2_seq: None,
        }];

        sink.write_batch(outputs).await.unwrap();
        assert_eq!(seen.load(Ordering::Relaxed), 1);
    }
}
