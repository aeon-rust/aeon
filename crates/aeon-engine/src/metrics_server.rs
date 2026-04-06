//! Minimal Prometheus metrics endpoint.
//!
//! Serves `PipelineMetrics` as Prometheus text format on `/metrics`.
//! Uses raw tokio TCP — no HTTP framework dependency needed for 3 counters.

use crate::pipeline::PipelineMetrics;
use aeon_types::AeonError;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Optional TLS certificate expiry, set by the caller.
/// Value is seconds since Unix epoch. 0 means not configured.
pub struct MetricsConfig {
    /// TLS certificate expiry (seconds since epoch). 0 = not configured.
    pub tls_cert_expiry_secs: std::sync::atomic::AtomicI64,
}

impl MetricsConfig {
    pub fn new() -> Self {
        Self {
            tls_cert_expiry_secs: std::sync::atomic::AtomicI64::new(0),
        }
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Format pipeline metrics as Prometheus exposition format.
fn format_prometheus(metrics: &PipelineMetrics, config: &MetricsConfig) -> String {
    let received = metrics.events_received.load(Ordering::Relaxed);
    let processed = metrics.events_processed.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    let mut out = format!(
        "# HELP aeon_events_received_total Total events received from sources\n\
         # TYPE aeon_events_received_total counter\n\
         aeon_events_received_total {received}\n\
         # HELP aeon_events_processed_total Total events processed by processors\n\
         # TYPE aeon_events_processed_total counter\n\
         aeon_events_processed_total {processed}\n\
         # HELP aeon_outputs_sent_total Total outputs sent to sinks\n\
         # TYPE aeon_outputs_sent_total counter\n\
         aeon_outputs_sent_total {sent}\n"
    );

    let cert_expiry = config.tls_cert_expiry_secs.load(Ordering::Relaxed);
    if cert_expiry > 0 {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let days_remaining = (cert_expiry - now_secs) / 86400;

        out.push_str(&format!(
            "# HELP aeon_tls_cert_expiry_seconds TLS certificate expiry (Unix epoch seconds)\n\
             # TYPE aeon_tls_cert_expiry_seconds gauge\n\
             aeon_tls_cert_expiry_seconds {cert_expiry}\n\
             # HELP aeon_tls_cert_days_remaining Days until TLS certificate expires\n\
             # TYPE aeon_tls_cert_days_remaining gauge\n\
             aeon_tls_cert_days_remaining {days_remaining}\n"
        ));
    }

    out
}

/// Serve pipeline metrics at the given address on `/metrics`.
///
/// Runs until the shutdown flag is set. Only responds to `GET /metrics` —
/// all other paths return 404.
///
/// This is intentionally minimal: raw TCP with hand-crafted HTTP responses.
/// A full HTTP framework (axum/hyper) will be used when the observability
/// crate is built out in later phases.
pub async fn serve_metrics(
    addr: std::net::SocketAddr,
    metrics: Arc<PipelineMetrics>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), AeonError> {
    serve_metrics_with_config(addr, metrics, Arc::new(MetricsConfig::new()), shutdown).await
}

/// Serve metrics with optional TLS certificate expiry configuration.
pub async fn serve_metrics_with_config(
    addr: std::net::SocketAddr,
    metrics: Arc<PipelineMetrics>,
    config: Arc<MetricsConfig>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), AeonError> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| AeonError::connection(format!("metrics bind failed: {e}")))?;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Accept with a timeout so we can check shutdown periodically
        let accept = tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept());

        let (mut stream, _) = match accept.await {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                return Err(AeonError::connection(format!("metrics accept error: {e}")));
            }
            Err(_) => continue, // timeout — check shutdown flag
        };

        // Read the request (we only need the first line)
        let mut buf = [0u8; 1024];
        let n = match stream.read(&mut buf).await {
            Ok(n) if n > 0 => n,
            _ => continue,
        };

        let request = String::from_utf8_lossy(&buf[..n]);
        let first_line = request.lines().next().unwrap_or("");

        let response = if first_line.starts_with("GET /metrics") {
            let body = format_prometheus(&metrics, &config);
            format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            )
        } else {
            let body = "Not Found\n";
            format!(
                "HTTP/1.1 404 Not Found\r\n\
                 Content-Type: text/plain\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            )
        };

        let _ = stream.write_all(response.as_bytes()).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::PipelineMetrics;

    #[test]
    fn prometheus_format_counters() {
        let metrics = PipelineMetrics::new();
        metrics.events_received.store(100, Ordering::Relaxed);
        metrics.events_processed.store(95, Ordering::Relaxed);
        metrics.outputs_sent.store(90, Ordering::Relaxed);
        let config = MetricsConfig::new();

        let output = format_prometheus(&metrics, &config);

        assert!(output.contains("aeon_events_received_total 100"));
        assert!(output.contains("aeon_events_processed_total 95"));
        assert!(output.contains("aeon_outputs_sent_total 90"));
        assert!(output.contains("# TYPE aeon_events_received_total counter"));
    }

    #[test]
    fn prometheus_format_zeros() {
        let metrics = PipelineMetrics::new();
        let config = MetricsConfig::new();
        let output = format_prometheus(&metrics, &config);

        assert!(output.contains("aeon_events_received_total 0"));
        assert!(output.contains("aeon_events_processed_total 0"));
        assert!(output.contains("aeon_outputs_sent_total 0"));
        // No TLS metrics when not configured
        assert!(!output.contains("aeon_tls_cert"));
    }

    #[test]
    fn prometheus_format_with_cert_expiry() {
        let metrics = PipelineMetrics::new();
        let config = MetricsConfig::new();
        // Set a cert expiry ~30 days from now
        let future_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 30 * 86400;
        config
            .tls_cert_expiry_secs
            .store(future_secs, Ordering::Relaxed);

        let output = format_prometheus(&metrics, &config);

        assert!(output.contains("aeon_tls_cert_expiry_seconds"));
        assert!(output.contains("aeon_tls_cert_days_remaining"));
        // Should be approximately 30 days (29 or 30 depending on timing)
        assert!(
            output.contains("aeon_tls_cert_days_remaining 29")
                || output.contains("aeon_tls_cert_days_remaining 30")
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_serves_get() {
        let metrics = Arc::new(PipelineMetrics::new());
        metrics.events_received.store(42, Ordering::Relaxed);
        metrics.events_processed.store(42, Ordering::Relaxed);
        metrics.outputs_sent.store(42, Ordering::Relaxed);

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Bind to a random port
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();

        // We can't easily reuse the listener, so we'll test format_prometheus directly
        // and test the server separately by spawning it.
        drop(listener);

        let metrics_clone = Arc::clone(&metrics);
        let shutdown_clone = Arc::clone(&shutdown);

        let server =
            tokio::spawn(
                async move { serve_metrics(bound_addr, metrics_clone, shutdown_clone).await },
            );

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect and send GET /metrics
        let mut stream = tokio::net::TcpStream::connect(bound_addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response);

        assert!(response_str.contains("200 OK"));
        assert!(response_str.contains("aeon_events_received_total 42"));

        // Shutdown
        shutdown.store(true, Ordering::Relaxed);

        // Connect once more to unblock the accept loop
        let _ = tokio::net::TcpStream::connect(bound_addr).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
    }

    #[tokio::test]
    async fn metrics_endpoint_404_for_other_paths() {
        let metrics = Arc::new(PipelineMetrics::new());
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let bound_addr = listener.local_addr().unwrap();
        drop(listener);

        let metrics_clone = Arc::clone(&metrics);
        let shutdown_clone = Arc::clone(&shutdown);

        let server =
            tokio::spawn(
                async move { serve_metrics(bound_addr, metrics_clone, shutdown_clone).await },
            );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = tokio::net::TcpStream::connect(bound_addr).await.unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response);

        assert!(response_str.contains("404 Not Found"));

        shutdown.store(true, Ordering::Relaxed);
        let _ = tokio::net::TcpStream::connect(bound_addr).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
    }
}
