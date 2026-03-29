//! Health and readiness HTTP endpoints.
//!
//! Extends the raw TCP approach from the metrics server.
//! - `GET /health` — liveness probe, always returns 200 if the server is running
//! - `GET /ready` — readiness probe, returns 200 when pipeline is running, 503 otherwise
//! - `GET /metrics` — Prometheus metrics (delegated to metrics formatting)
//!
//! This replaces the standalone metrics server with a combined health+metrics server.

use crate::pipeline::PipelineMetrics;
use aeon_types::AeonError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Pipeline readiness state.
///
/// Set to `true` when the pipeline is fully initialized and processing events.
/// Set to `false` during startup, shutdown, or error states.
#[derive(Debug)]
pub struct HealthState {
    /// Whether the pipeline is ready to accept traffic.
    pub ready: AtomicBool,
}

impl HealthState {
    pub fn new() -> Self {
        Self {
            ready: AtomicBool::new(false),
        }
    }

    /// Mark the pipeline as ready.
    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::Relaxed);
    }

    /// Mark the pipeline as not ready (e.g., during shutdown).
    pub fn set_not_ready(&self) {
        self.ready.store(false, Ordering::Relaxed);
    }

    /// Check if the pipeline is ready.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

/// Format pipeline metrics as Prometheus exposition format.
fn format_prometheus(metrics: &PipelineMetrics) -> String {
    let received = metrics.events_received.load(Ordering::Relaxed);
    let processed = metrics.events_processed.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    format!(
        "# HELP aeon_events_received_total Total events received from sources\n\
         # TYPE aeon_events_received_total counter\n\
         aeon_events_received_total {received}\n\
         # HELP aeon_events_processed_total Total events processed by processors\n\
         # TYPE aeon_events_processed_total counter\n\
         aeon_events_processed_total {processed}\n\
         # HELP aeon_outputs_sent_total Total outputs sent to sinks\n\
         # TYPE aeon_outputs_sent_total counter\n\
         aeon_outputs_sent_total {sent}\n"
    )
}

/// Build an HTTP response with the given status, content type, and body.
fn http_response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

/// Serve health, readiness, and metrics endpoints.
///
/// Routes:
/// - `GET /health` → 200 OK `{"status":"ok"}`
/// - `GET /ready` → 200 OK or 503 Service Unavailable
/// - `GET /metrics` → Prometheus text format
/// - Everything else → 404
pub async fn serve_health(
    addr: std::net::SocketAddr,
    metrics: Arc<PipelineMetrics>,
    health: Arc<HealthState>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), AeonError> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| AeonError::connection(format!("health server bind failed: {e}")))?;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let accept = tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept());

        let (mut stream, _) = match accept.await {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                return Err(AeonError::connection(format!(
                    "health server accept error: {e}"
                )));
            }
            Err(_) => continue,
        };

        let mut buf = [0u8; 1024];
        let n = match stream.read(&mut buf).await {
            Ok(n) if n > 0 => n,
            _ => continue,
        };

        let request = String::from_utf8_lossy(&buf[..n]);
        let first_line = request.lines().next().unwrap_or("");

        let response = if first_line.starts_with("GET /health") {
            http_response("200 OK", "application/json", "{\"status\":\"ok\"}")
        } else if first_line.starts_with("GET /ready") {
            if health.is_ready() {
                http_response("200 OK", "application/json", "{\"status\":\"ready\"}")
            } else {
                http_response(
                    "503 Service Unavailable",
                    "application/json",
                    "{\"status\":\"not_ready\"}",
                )
            }
        } else if first_line.starts_with("GET /metrics") {
            let body = format_prometheus(&metrics);
            http_response("200 OK", "text/plain; version=0.0.4; charset=utf-8", &body)
        } else {
            http_response("404 Not Found", "text/plain", "Not Found\n")
        };

        let _ = stream.write_all(response.as_bytes()).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_state_defaults_to_not_ready() {
        let state = HealthState::new();
        assert!(!state.is_ready());
    }

    #[test]
    fn health_state_set_ready() {
        let state = HealthState::new();
        state.set_ready();
        assert!(state.is_ready());
    }

    #[test]
    fn health_state_set_not_ready() {
        let state = HealthState::new();
        state.set_ready();
        state.set_not_ready();
        assert!(!state.is_ready());
    }

    #[test]
    fn http_response_format() {
        let resp = http_response("200 OK", "text/plain", "hello");
        assert!(resp.contains("HTTP/1.1 200 OK"));
        assert!(resp.contains("Content-Length: 5"));
        assert!(resp.contains("hello"));
    }

    #[test]
    fn prometheus_format() {
        let metrics = PipelineMetrics::new();
        metrics.events_received.store(10, Ordering::Relaxed);
        let output = format_prometheus(&metrics);
        assert!(output.contains("aeon_events_received_total 10"));
    }

    #[tokio::test]
    async fn health_endpoint_returns_200() {
        let metrics = Arc::new(PipelineMetrics::new());
        let health = Arc::new(HealthState::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();
        drop(listener);

        let m = Arc::clone(&metrics);
        let h = Arc::clone(&health);
        let s = Arc::clone(&shutdown);
        let server = tokio::spawn(async move { serve_health(bound_addr, m, h, s).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Test /health
        let mut stream = tokio::net::TcpStream::connect(bound_addr).await.unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("\"status\":\"ok\""));

        shutdown.store(true, Ordering::Relaxed);
        let _ = tokio::net::TcpStream::connect(bound_addr).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
    }

    #[tokio::test]
    async fn ready_endpoint_503_when_not_ready() {
        let metrics = Arc::new(PipelineMetrics::new());
        let health = Arc::new(HealthState::new()); // not ready
        let shutdown = Arc::new(AtomicBool::new(false));

        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();
        drop(listener);

        let m = Arc::clone(&metrics);
        let h = Arc::clone(&health);
        let s = Arc::clone(&shutdown);
        let server = tokio::spawn(async move { serve_health(bound_addr, m, h, s).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = tokio::net::TcpStream::connect(bound_addr).await.unwrap();
        stream
            .write_all(b"GET /ready HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("503 Service Unavailable"));
        assert!(response_str.contains("\"status\":\"not_ready\""));

        shutdown.store(true, Ordering::Relaxed);
        let _ = tokio::net::TcpStream::connect(bound_addr).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
    }

    #[tokio::test]
    async fn ready_endpoint_200_when_ready() {
        let metrics = Arc::new(PipelineMetrics::new());
        let health = Arc::new(HealthState::new());
        health.set_ready(); // mark as ready
        let shutdown = Arc::new(AtomicBool::new(false));

        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();
        drop(listener);

        let m = Arc::clone(&metrics);
        let h = Arc::clone(&health);
        let s = Arc::clone(&shutdown);
        let server = tokio::spawn(async move { serve_health(bound_addr, m, h, s).await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = tokio::net::TcpStream::connect(bound_addr).await.unwrap();
        stream
            .write_all(b"GET /ready HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response);
        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("\"status\":\"ready\""));

        shutdown.store(true, Ordering::Relaxed);
        let _ = tokio::net::TcpStream::connect(bound_addr).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
    }
}
