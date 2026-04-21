//! HTTP Polling source — periodically fetches data from an HTTP endpoint.
//!
//! Pull-source: `next_batch()` issues an HTTP GET and returns the response
//! body as a single Event. No backpressure buffer needed — the polling
//! interval provides natural flow control.

use aeon_types::{AeonError, Backoff, BackoffPolicy, CoreLocalUuidGenerator, Event, PartitionId, Source};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
}

/// HTTP Polling event source.
///
/// Each `next_batch()` call waits for the polling interval, then issues
/// an HTTP GET to the configured URL. The response body becomes a single Event.
/// Empty responses are skipped (empty batch returned).
pub struct HttpPollingSource {
    config: HttpPollingSourceConfig,
    client: reqwest::Client,
    last_poll: Option<tokio::time::Instant>,
    /// TR-3: backoff state — advanced on every failure, reset on success.
    backoff: Backoff,
    /// Per-source UUIDv7 generator (SPSC pool, ~1-2ns per UUID).
    /// Mutex for Sync (Source: Send + Sync), only accessed in next_batch.
    uuid_gen: Mutex<CoreLocalUuidGenerator>,
}

impl HttpPollingSource {
    /// Create a new polling source.
    pub fn new(config: HttpPollingSourceConfig) -> Result<Self, AeonError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| AeonError::connection(format!("http client build failed: {e}")))?;

        tracing::info!(url = %config.url, interval = ?config.interval, "HttpPollingSource created");

        let backoff = Backoff::new(config.backoff);
        let uuid_gen = CoreLocalUuidGenerator::new(0);
        Ok(Self {
            config,
            client,
            last_poll: None,
            backoff,
            uuid_gen: Mutex::new(uuid_gen),
        })
    }
}

impl Source for HttpPollingSource {
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
                    url = %self.config.url,
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
                url = %self.config.url,
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
                    url = %self.config.url,
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
            .with_interval(Duration::from_millis(10));
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
            .with_interval(Duration::from_millis(10));
        let mut source = HttpPollingSource::new(config).unwrap();

        let batch = source.next_batch().await.unwrap();
        assert!(batch.is_empty());
    }
}
