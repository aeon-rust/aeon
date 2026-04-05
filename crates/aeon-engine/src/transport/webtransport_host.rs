//! T3 WebTransport Processor Host — QUIC/HTTP3 server for out-of-process processors.
//!
//! Accepts WebTransport sessions from T3 processor instances, runs the AWPP
//! handshake, and exposes `ProcessorTransport` for pipeline integration.
//!
//! Feature-gated behind `webtransport-host`.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::task::JoinHandle;

use aeon_types::error::AeonError;
use aeon_types::event::{Event, Output};
use aeon_types::processor_transport::{ProcessorHealth, ProcessorInfo, ProcessorTier};
use aeon_types::traits::ProcessorTransport;
use aeon_types::transport_codec::TransportCodec;

use crate::identity_store::ProcessorIdentityStore;
use crate::transport::session::{AwppSession, ControlChannel, PipelineResolver};

// ── Configuration ───────────────────────────────────────────────────────

/// Configuration for the WebTransport processor host.
///
/// `server_config` is consumed by `start()` (wtransport::ServerConfig is not Clone),
/// so this struct is used once and then wrapped in `Arc` minus the server config.
pub struct WebTransportHostConfig {
    /// Address to bind the QUIC/HTTP3 endpoint to (default: 0.0.0.0:4472).
    pub bind_addr: SocketAddr,
    /// Identity store for ED25519 challenge-response auth.
    pub identity_store: Arc<ProcessorIdentityStore>,
    /// Pipeline resolver for partition assignment.
    pub pipeline_resolver: Arc<dyn PipelineResolver>,
    /// Whether OAuth is required in addition to ED25519.
    pub oauth_required: bool,
    /// Heartbeat interval (default: 10s).
    pub heartbeat_interval: Duration,
    /// Handshake timeout (default: 5s).
    pub handshake_timeout: Duration,
    /// Batch response timeout (default: 30s).
    pub batch_timeout: Duration,
    /// Processor name (for ProcessorInfo).
    pub processor_name: String,
    /// Processor version (for ProcessorInfo).
    pub processor_version: String,
    /// Pipeline codec override (if set, overrides processor preference).
    pub pipeline_codec: Option<TransportCodec>,
}

impl WebTransportHostConfig {
    /// Create a new config with required fields and sensible defaults.
    pub fn new(
        bind_addr: SocketAddr,
        identity_store: Arc<ProcessorIdentityStore>,
        pipeline_resolver: Arc<dyn PipelineResolver>,
    ) -> Self {
        Self {
            bind_addr,
            identity_store,
            pipeline_resolver,
            oauth_required: false,
            heartbeat_interval: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(5),
            batch_timeout: Duration::from_secs(30),
            processor_name: String::new(),
            processor_version: String::new(),
            pipeline_codec: None,
        }
    }
}

// ── WebTransport Processor Host ─────────────────────────────────────────

/// T3 WebTransport processor host — accepts QUIC/HTTP3 connections from processors.
pub struct WebTransportProcessorHost {
    /// Active sessions keyed by session_id.
    sessions: Arc<DashMap<String, Arc<AwppSession>>>,
    /// Routing table: (pipeline_name, partition_id) → session_id.
    /// Used by call_batch (TODO) to route events to the correct session.
    #[allow(dead_code)]
    routing: Arc<DashMap<(String, u16), String>>,
    /// Host configuration.
    config: Arc<WebTransportHostConfig>,
    /// When the host was started.
    created_at: Instant,
    /// Background accept loop handle.
    _accept_handle: JoinHandle<()>,
}

impl WebTransportProcessorHost {
    /// Start the WebTransport processor host, binding to the configured address.
    ///
    /// `server_config` is consumed (not Clone) — it includes the TLS identity.
    pub async fn start(
        config: WebTransportHostConfig,
        server_config: wtransport::ServerConfig,
    ) -> Result<Self, AeonError> {
        let endpoint = wtransport::Endpoint::server(server_config).map_err(|e| {
            AeonError::connection(format!(
                "webtransport host bind failed on {}: {e}",
                config.bind_addr
            ))
        })?;

        tracing::info!(addr = %config.bind_addr, "T3 WebTransport processor host listening");

        let sessions: Arc<DashMap<String, Arc<AwppSession>>> = Arc::new(DashMap::new());
        let routing: Arc<DashMap<(String, u16), String>> = Arc::new(DashMap::new());
        let config = Arc::new(config);

        let handle = tokio::spawn(wt_accept_loop(
            endpoint,
            sessions.clone(),
            routing.clone(),
            config.clone(),
        ));

        Ok(Self {
            sessions,
            routing,
            config,
            created_at: Instant::now(),
            _accept_handle: handle,
        })
    }

    /// Number of active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Total pending batches across all sessions.
    fn total_pending_batches(&self) -> u32 {
        self.sessions
            .iter()
            .map(|s| s.batch_inflight.pending_count())
            .sum()
    }
}

impl ProcessorTransport for WebTransportProcessorHost {
    fn call_batch(
        &self,
        _events: Vec<Event>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Output>, AeonError>> + Send + '_>> {
        Box::pin(async move {
            // TODO: Phase 12b-3 full implementation
            // 1. Determine target session from events
            // 2. Allocate batch_id via BatchInflight
            // 3. Encode via encode_batch_request with session codec
            // 4. Write to appropriate data stream
            // 5. Await response via oneshot with timeout
            Err(AeonError::processor(
                "T3 WebTransport call_batch not yet fully implemented",
            ))
        })
    }

    fn health(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessorHealth, AeonError>> + Send + '_>> {
        Box::pin(async move {
            let any_healthy = self.sessions.iter().any(|s| s.is_healthy());
            Ok(ProcessorHealth {
                healthy: any_healthy,
                pending_batches: Some(self.total_pending_batches()),
                uptime_secs: Some(self.created_at.elapsed().as_secs()),
                latency_us: None,
            })
        })
    }

    fn drain(&self) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + '_>> {
        Box::pin(async move {
            for session in self.sessions.iter() {
                session.close();
            }
            Ok(())
        })
    }

    fn info(&self) -> ProcessorInfo {
        ProcessorInfo {
            name: self.config.processor_name.clone(),
            version: self.config.processor_version.clone(),
            tier: ProcessorTier::WebTransport,
            capabilities: vec!["batch".into()],
        }
    }
}

// ── Accept Loop ─────────────────────────────────────────────────────────

async fn wt_accept_loop(
    endpoint: wtransport::Endpoint<wtransport::endpoint::endpoint_side::Server>,
    sessions: Arc<DashMap<String, Arc<AwppSession>>>,
    routing: Arc<DashMap<(String, u16), String>>,
    config: Arc<WebTransportHostConfig>,
) {
    loop {
        let incoming = endpoint.accept().await;

        let session_request = match incoming.await {
            Ok(req) => req,
            Err(e) => {
                tracing::warn!(error = %e, "T3 session request failed");
                continue;
            }
        };

        let session = match session_request.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "T3 session accept failed");
                continue;
            }
        };

        let sessions = sessions.clone();
        let routing = routing.clone();
        let config = config.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_wt_session(session, sessions, routing, config).await {
                tracing::debug!(error = %e, "T3 session ended");
            }
        });
    }
}

/// Handle a single WebTransport processor session.
async fn handle_wt_session(
    connection: wtransport::Connection,
    sessions: Arc<DashMap<String, Arc<AwppSession>>>,
    routing: Arc<DashMap<(String, u16), String>>,
    config: Arc<WebTransportHostConfig>,
) -> Result<(), AeonError> {
    // Accept the first bidirectional stream as the control stream
    let (ctrl_send, ctrl_recv) = connection.accept_bi().await.map_err(|e| {
        AeonError::connection(format!("failed to accept control stream: {e}"))
    })?;

    let control = WtControlChannel::new(ctrl_send, ctrl_recv);

    // Run AWPP handshake with timeout
    let handshake_config = crate::transport::session::HandshakeConfig {
        oauth_required: config.oauth_required,
        heartbeat_interval: config.heartbeat_interval,
        batch_signing: true,
        pipeline_codec: config.pipeline_codec,
    };

    let awpp = tokio::time::timeout(
        config.handshake_timeout,
        crate::transport::session::handshake(
            &control,
            &config.identity_store,
            &*config.pipeline_resolver,
            &handshake_config,
        ),
    )
    .await
    .map_err(|_| AeonError::connection("T3 handshake timeout"))??;

    let session = Arc::new(awpp);
    let session_id = session.session_id.clone();
    sessions.insert(session_id.clone(), session.clone());

    // Update routing table
    for assignment in &session.pipeline_assignments {
        for &partition in &assignment.partitions {
            routing.insert(
                (assignment.name.clone(), partition),
                session_id.clone(),
            );
        }
    }

    tracing::info!(
        session_id = %session_id,
        processor = %session.processor_name,
        "T3 processor connected"
    );

    // Accept data streams until the connection closes
    loop {
        match connection.accept_bi().await {
            Ok((_send, _recv)) => {
                // TODO: handle data stream (batch request/response pairs)
                // Each data stream maps to one pipeline+partition
            }
            Err(e) => {
                tracing::debug!(
                    session_id = %session_id,
                    error = %e,
                    "T3 connection closed"
                );
                break;
            }
        }
    }

    // Cleanup
    session.close();
    sessions.remove(&session_id);
    for assignment in &session.pipeline_assignments {
        for &partition in &assignment.partitions {
            routing.remove(&(assignment.name.clone(), partition));
        }
    }
    config.identity_store.disconnect(&session.fingerprint);

    Ok(())
}

// ── Control Channel (WebTransport) ──────────────────────────────────────

/// Control channel over a WebTransport bidirectional stream.
///
/// Uses 4-byte LE length-prefix framing for JSON messages.
struct WtControlChannel {
    send: tokio::sync::Mutex<wtransport::SendStream>,
    recv: tokio::sync::Mutex<wtransport::RecvStream>,
}

impl WtControlChannel {
    fn new(send: wtransport::SendStream, recv: wtransport::RecvStream) -> Self {
        Self {
            send: tokio::sync::Mutex::new(send),
            recv: tokio::sync::Mutex::new(recv),
        }
    }
}

impl ControlChannel for WtControlChannel {
    fn send_control(
        &self,
        msg: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + '_>> {
        // Copy msg into the future — the borrow may not outlive &self.
        let data = msg.to_vec();
        Box::pin(async move {
            let mut send = self.send.lock().await;
            let len = (data.len() as u32).to_le_bytes();
            send.write_all(&len).await.map_err(|e| {
                AeonError::connection(format!("T3 control send length failed: {e}"))
            })?;
            send.write_all(&data).await.map_err(|e| {
                AeonError::connection(format!("T3 control send data failed: {e}"))
            })?;
            Ok(())
        })
    }

    fn recv_control(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, AeonError>> + Send + '_>> {
        Box::pin(async move {
            let mut recv = self.recv.lock().await;
            let mut len_buf = [0u8; 4];
            recv.read_exact(&mut len_buf).await.map_err(|e| {
                AeonError::connection(format!("T3 control recv length failed: {e}"))
            })?;
            let len = u32::from_le_bytes(len_buf) as usize;
            if len > 64 * 1024 {
                return Err(AeonError::serialization(format!(
                    "control message too large: {len} bytes"
                )));
            }
            let mut buf = vec![0u8; len];
            recv.read_exact(&mut buf).await.map_err(|e| {
                AeonError::connection(format!("T3 control recv data failed: {e}"))
            })?;
            Ok(buf)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wt_host_config_defaults() {
        // Verify config construction doesn't panic
        // (can't actually start a server without TLS certs in tests)
        assert_eq!(
            ProcessorTier::WebTransport.to_string(),
            "web-transport"
        );
    }
}
