//! HTTP Polling source — periodically fetches data from an HTTP endpoint.
//!
//! Poll-source: `next_batch()` issues an HTTP GET and returns the response
//! body as a single Event. No backpressure buffer needed — the polling
//! interval provides natural flow control. Classified as `SourceKind::Poll`
//! because HTTP endpoints have no durable replay position: without L2
//! event-body persistence under EO-2, events pulled since the last
//! checkpoint vanish on crash.

use aeon_types::{
    AeonError, Backoff, BackoffPolicy, CoreLocalUuidGenerator, Event, OutboundAuthMode,
    OutboundAuthSigner, OutboundSignContext, PartitionId, Source, SourceKind, SsrfPolicy,
    redact_uri,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Configuration for `HttpPollingSource`.
pub struct HttpPollingSourceConfig {
    /// URL to poll.
    pub url: String,
    /// Polling interval between requests.
    pub interval: Duration,
    /// HTTP request timeout.
    pub timeout: Duration,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Optional headers to include in requests.
    pub headers: Vec<(String, String)>,
    /// Reconnect backoff policy — TR-3 applies when the endpoint is down /
    /// returns 5xx / body read fails. On error, `next_batch` sleeps for the
    /// current backoff delay (capped at `max_ms`) and returns an empty batch
    /// instead of propagating the Err, so the pipeline stays alive through
    /// transient outages. Reset on the first successful poll after failure.
    pub backoff: BackoffPolicy,
    /// S7: SSRF guard. The URL is checked once at source creation.
    pub ssrf_policy: SsrfPolicy,
    /// S10: optional outbound auth signer. Applied per GET request.
    /// Mtls mode installs an identity on the reqwest client at build
    /// time; header-bearing modes inject on every poll.
    pub auth: Option<Arc<OutboundAuthSigner>>,
}

impl HttpPollingSourceConfig {
    /// Create a polling source config.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(30),
            source_name: Arc::from("http-poll"),
            headers: Vec::new(),
            backoff: BackoffPolicy::default(),
            ssrf_policy: SsrfPolicy::production(),
            auth: None,
        }
    }

    /// Set polling interval.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Set request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Add a header to include in requests.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Override the reconnect backoff policy (defaults to `BackoffPolicy::default()`).
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Override the SSRF policy.
    pub fn with_ssrf_policy(mut self, policy: SsrfPolicy) -> Self {
        self.ssrf_policy = policy;
        self
    }

    /// S10: attach an outbound auth signer.
    pub fn with_auth(mut self, signer: Arc<OutboundAuthSigner>) -> Self {
        self.auth = Some(signer);
        self
    }
}

/// HTTP Polling event source.
///
/// Each `next_batch()` call waits for the polling interval, then issues
/// an HTTP GET to the configured URL. The response body becomes a single Event.
/// Empty responses are skipped (empty batch returned).
pub struct HttpPollingSource {
    config: HttpPollingSourceConfig,
    client: reqwest::Client,
    /// Pre-parsed URL path — fed to the S10 [`OutboundSignContext`] so we
    /// don't re-parse the URL on every poll.
    path: String,
    last_poll: Option<tokio::time::Instant>,
    /// TR-3: backoff state — advanced on every failure, reset on success.
    backoff: Backoff,
    /// Per-source UUIDv7 generator (SPSC pool, ~1-2ns per UUID).
    /// Mutex for Sync (Source: Send + Sync), only accessed in next_batch.
    uuid_gen: Mutex<CoreLocalUuidGenerator>,
}

impl HttpPollingSource {
    /// Create a new polling source. Validates the URL against
    /// [`SsrfPolicy`] before any HTTP I/O — a denied URL surfaces as
    /// `AeonError::Config` at startup.
    pub fn new(config: HttpPollingSourceConfig) -> Result<Self, AeonError> {
        config.ssrf_policy.check_url(&config.url)?;

        let parsed = reqwest::Url::parse(&config.url)
            .map_err(|e| AeonError::config(format!("http poll url parse failed: {e}")))?;
        let path = parsed.path().to_string();

        let mut builder = reqwest::Client::builder().timeout(config.timeout);

        // S10: mTLS installs a client identity. BrokerNative isn't
        // meaningful for HTTP and is warned.
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
                        .map_err(|e| AeonError::config(format!("http poll mtls identity: {e}")))?;
                    builder = builder.identity(identity);
                }
                OutboundAuthMode::BrokerNative => {
                    tracing::warn!(
                        "HttpPollingSource: broker_native auth mode is not applicable to HTTP; ignored"
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
            interval = ?config.interval,
            auth_mode = config.auth.as_ref().map(|s| s.mode().tag()).unwrap_or("unset"),
            "HttpPollingSource created"
        );

        let backoff = Backoff::new(config.backoff);
        let uuid_gen = CoreLocalUuidGenerator::new(0);
        Ok(Self {
            config,
            client,
            path,
            last_poll: None,
            backoff,
            uuid_gen: Mutex::new(uuid_gen),
        })
    }
}

impl Source for HttpPollingSource {
    fn source_kind(&self) -> SourceKind {
        SourceKind::Poll
    }

    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // Wait for interval since last poll
        if let Some(last) = self.last_poll {
            let elapsed = last.elapsed();
            if elapsed < self.config.interval {
                tokio::time::sleep(self.config.interval - elapsed).await;
            }
        }

        self.last_poll = Some(tokio::time::Instant::now());

        // Build request
        let mut request = self.client.get(&self.config.url);
        for (key, value) in &self.config.headers {
            request = request.header(key.as_str(), value.as_str());
        }

        // S10: outbound auth headers. HMAC preimage uses an empty body
        // for GET (matches the inbound verifier expectation).
        if let Some(signer) = &self.config.auth {
            let now_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let ctx = OutboundSignContext {
                method: "GET",
                path: &self.path,
                body: b"",
                now_unix,
            };
            let auth_headers = match signer.http_headers(&ctx) {
                Ok(h) => h,
                Err(e) => {
                    let delay = self.backoff.next_delay();
                    tracing::warn!(
                        url = %redact_uri(&self.config.url),
                        error = %e,
                        delay_ms = delay.as_millis() as u64,
                        "http poll auth sign failed, backing off"
                    );
                    tokio::time::sleep(delay).await;
                    return Ok(Vec::new());
                }
            };
            for (k, v) in auth_headers {
                request = request.header(k, v);
            }
        }

        // TR-3: on any transient error, back off and return an empty batch
        // instead of propagating Err. Returning Err would kill the pipeline
        // task; keeping it alive matches the behaviour of every other
        // reconnecting source (NATS, MQTT, Postgres CDC, MySQL, RabbitMQ,
        // MongoDB CDC).
        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                let delay = self.backoff.next_delay();
                tracing::warn!(
                    url = %redact_uri(&self.config.url),
                    error = %e,
                    delay_ms = delay.as_millis() as u64,
                    "http poll request failed, backing off"
                );
                tokio::time::sleep(delay).await;
                return Ok(Vec::new());
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let delay = self.backoff.next_delay();
            tracing::warn!(
                url = %redact_uri(&self.config.url),
                %status,
                delay_ms = delay.as_millis() as u64,
                "http poll returned non-success status, backing off"
            );
            tokio::time::sleep(delay).await;
            return Ok(Vec::new());
        }

        let body = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                let delay = self.backoff.next_delay();
                tracing::warn!(
                    url = %redact_uri(&self.config.url),
                    error = %e,
                    delay_ms = delay.as_millis() as u64,
                    "http poll body read failed, backing off"
                );
                tokio::time::sleep(delay).await;
                return Ok(Vec::new());
            }
        };

        // Success — reset backoff so the next failure starts fresh from
        // `initial_ms` rather than inheriting the previous outage's growth.
        self.backoff.reset();

        if body.is_empty() {
            return Ok(Vec::new());
        }

        let event_id = self
            .uuid_gen
            .lock()
            .map_err(|_| AeonError::connection("UUID generator mutex poisoned"))?
            .next_uuid();

        let event = Event::new(
            event_id,
            0,
            Arc::clone(&self.config.source_name),
            PartitionId::new(0),
            // FT-11: reqwest Response.bytes() returns bytes::Bytes directly.
            body,
        );

        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_polling_source_fetches_data() {
        // Start a mock server
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = axum::Router::new().route(
            "/data",
            axum::routing::get(|| async { "poll-response-data" }),
        );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpPollingSourceConfig::new(format!("http://{addr}/data"))
            .with_interval(Duration::from_millis(10))
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests());
        let mut source = HttpPollingSource::new(config).unwrap();

        let batch = source.next_batch().await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].payload.as_ref(), b"poll-response-data");
    }

    #[tokio::test]
    async fn test_polling_source_empty_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = axum::Router::new().route("/empty", axum::routing::get(|| async { "" }));

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = HttpPollingSourceConfig::new(format!("http://{addr}/empty"))
            .with_interval(Duration::from_millis(10))
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests());
        let mut source = HttpPollingSource::new(config).unwrap();

        let batch = source.next_batch().await.unwrap();
        assert!(batch.is_empty());
    }

    #[tokio::test]
    async fn ssrf_rejects_imds_at_creation() {
        let config = HttpPollingSourceConfig::new(
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
        );
        let Err(err) = HttpPollingSource::new(config) else {
            panic!("expected SSRF rejection, but new() succeeded");
        };
        assert!(matches!(err, AeonError::Config { .. }), "got: {err:?}");
    }

    #[tokio::test]
    async fn auth_bearer_injects_authorization_on_poll() {
        use aeon_types::{BearerConfig, OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let seen = Arc::new(AtomicUsize::new(0));
        let seen_clone = Arc::clone(&seen);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = axum::Router::new().route(
            "/auth",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let seen = Arc::clone(&seen_clone);
                async move {
                    if headers.get("authorization").map(|v| v.as_bytes())
                        == Some(b"Bearer poll-token".as_slice())
                    {
                        seen.fetch_add(1, Ordering::Relaxed);
                    }
                    "data"
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
                    token: "poll-token".into(),
                }),
                ..Default::default()
            })
            .unwrap(),
        );

        let config = HttpPollingSourceConfig::new(format!("http://{addr}/auth"))
            .with_interval(Duration::from_millis(10))
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests())
            .with_auth(signer);
        let mut source = HttpPollingSource::new(config).unwrap();

        let batch = source.next_batch().await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(seen.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn auth_hmac_sign_injects_timestamp_and_signature_on_poll() {
        use aeon_types::{
            HmacAlgorithm, HmacSignConfig, OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner,
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
            axum::routing::get(move |headers: axum::http::HeaderMap| {
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
                    "data"
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
                    algorithm: HmacAlgorithm::HmacSha256,
                }),
                ..Default::default()
            })
            .unwrap(),
        );

        let config = HttpPollingSourceConfig::new(format!("http://{addr}/hmac"))
            .with_interval(Duration::from_millis(10))
            .with_ssrf_policy(SsrfPolicy::permissive_for_tests())
            .with_auth(signer);
        let mut source = HttpPollingSource::new(config).unwrap();

        let batch = source.next_batch().await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(sig_seen.load(Ordering::Relaxed), 1);
        assert_eq!(ts_seen.load(Ordering::Relaxed), 1);
    }
}
