//! HTTP Sink — POST outputs to an external HTTP endpoint.
//!
//! Each output payload is sent as an HTTP POST request body.
//! Supports configurable URL, headers, timeout, and batch-as-JSON mode.
//! Enables Aeon → serverless fan-out (Lambda, Cloud Functions, webhooks).

use aeon_types::{AeonError, BatchResult, Output, Sink};
use std::time::Duration;

/// Configuration for `HttpSink`.
pub struct HttpSinkConfig {
    /// URL to POST outputs to.
    pub url: String,
    /// HTTP request timeout.
    pub timeout: Duration,
    /// Optional headers to include in requests.
    pub headers: Vec<(String, String)>,
}

impl HttpSinkConfig {
    /// Create an HTTP sink config.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout: Duration::from_secs(30),
            headers: Vec::new(),
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
}

/// HTTP output sink.
///
/// Each output is sent as an HTTP POST with the payload as body.
/// Content-Type defaults to `application/octet-stream` unless overridden
/// by a header in the config.
pub struct HttpSink {
    config: HttpSinkConfig,
    client: reqwest::Client,
    delivered: u64,
}

impl HttpSink {
    /// Create a new HTTP sink.
    pub fn new(config: HttpSinkConfig) -> Result<Self, AeonError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| AeonError::connection(format!("http client build failed: {e}")))?;

        tracing::info!(url = %config.url, "HttpSink created");

        Ok(Self {
            config,
            client,
            delivered: 0,
        })
    }

    /// Number of outputs delivered.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }
}

impl Sink for HttpSink {
    async fn write_batch(&mut self, outputs: Vec<Output>) -> Result<BatchResult, AeonError> {
        let ids = outputs.iter().filter_map(|o| o.source_event_id).collect();

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

            let response = request
                .body(output.payload.clone())
                .send()
                .await
                .map_err(|e| {
                    AeonError::connection(format!(
                        "http sink POST failed: {}: {e}",
                        self.config.url
                    ))
                })?;

            if !response.status().is_success() {
                return Err(AeonError::connection(format!(
                    "http sink POST returned {}: {}",
                    response.status(),
                    self.config.url,
                )));
            }

            self.delivered += 1;
        }

        Ok(BatchResult::all_delivered(ids))
    }

    async fn flush(&mut self) -> Result<(), AeonError> {
        // HTTP POST is synchronous per-request; nothing to flush.
        Ok(())
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

        let config = HttpSinkConfig::new(format!("http://{addr}/ingest"));
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

        let config = HttpSinkConfig::new(format!("http://{addr}/fail"));
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
}
