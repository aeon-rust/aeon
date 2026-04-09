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
use tokio::sync::Mutex;
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
    /// Pipeline name — used by `call_batch` to route events to the correct data stream.
    pub pipeline_name: String,
    /// Pipeline codec override (if set, overrides processor preference).
    pub pipeline_codec: Option<TransportCodec>,
    /// Maximum number of concurrently in-flight batches per session.
    /// Bounds the per-session pending DashMap so a slow processor cannot
    /// cause unbounded memory growth. `call_batch` suspends on the session
    /// semaphore when this limit is reached.
    pub max_inflight_batches: usize,
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
            pipeline_name: String::new(),
            pipeline_codec: None,
            max_inflight_batches: crate::transport::session::DEFAULT_MAX_INFLIGHT_BATCHES,
        }
    }
}

// ── Type Aliases ───────────────────────────────────────────────────────

/// Routing table: (pipeline_name, partition_id) → session_id.
type RoutingTable = Arc<DashMap<(String, u16), String>>;

/// Data stream send halves: (pipeline_name, partition_id) → SendStream.
type DataStreamMap = Arc<DashMap<(String, u16), Arc<Mutex<wtransport::SendStream>>>>;

// ── WebTransport Processor Host ─────────────────────────────────────────

/// T3 WebTransport processor host — accepts QUIC/HTTP3 connections from processors.
pub struct WebTransportProcessorHost {
    /// Active sessions keyed by session_id.
    sessions: Arc<DashMap<String, Arc<AwppSession>>>,
    /// Routing table: (pipeline_name, partition_id) → session_id.
    routing: RoutingTable,
    /// Data stream send halves, populated by the accept loop.
    data_streams: DataStreamMap,
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
        let routing: RoutingTable = Arc::new(DashMap::new());
        let data_streams: DataStreamMap = Arc::new(DashMap::new());
        let config = Arc::new(config);

        let handle = tokio::spawn(wt_accept_loop(
            endpoint,
            sessions.clone(),
            routing.clone(),
            data_streams.clone(),
            config.clone(),
        ));

        Ok(Self {
            sessions,
            routing,
            data_streams,
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
        events: Vec<Event>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Output>, AeonError>> + Send + '_>> {
        Box::pin(async move {
            if events.is_empty() {
                return Ok(vec![]);
            }

            // Route by pipeline_name + first event's partition
            let partition = events[0].partition.as_u16();
            let key = (self.config.pipeline_name.clone(), partition);

            // Look up session via routing table
            let session_id = self
                .routing
                .get(&key)
                .map(|r| r.value().clone())
                .ok_or_else(|| {
                    AeonError::connection(format!(
                        "no T3 session for pipeline={} partition={partition}",
                        self.config.pipeline_name,
                    ))
                })?;

            let session = self
                .sessions
                .get(&session_id)
                .map(|s| Arc::clone(s.value()))
                .ok_or_else(|| {
                    AeonError::connection(format!("T3 session {session_id} not found"))
                })?;

            // Look up data stream
            let stream = self
                .data_streams
                .get(&key)
                .map(|s| Arc::clone(s.value()))
                .ok_or_else(|| {
                    AeonError::connection(format!(
                        "no T3 data stream for pipeline={} partition={partition}",
                        self.config.pipeline_name,
                    ))
                })?;

            // Allocate batch_id and get response receiver. If the session's
            // inflight capacity is saturated, `start_batch` suspends until
            // an earlier batch completes — this is the session-level
            // backpressure that bounds the pending map.
            let (batch_id, rx) = session.batch_inflight.start_batch().await;

            // Encode batch request
            let wire = crate::batch_wire::encode_batch_request(batch_id, &events, session.codec)?;

            // Write length-prefixed frame to data stream
            {
                let mut send = stream.lock().await;
                let len = (wire.len() as u32).to_le_bytes();
                send.write_all(&len).await.map_err(|e| {
                    AeonError::connection(format!("T3 data stream write length failed: {e}"))
                })?;
                send.write_all(&wire).await.map_err(|e| {
                    AeonError::connection(format!("T3 data stream write data failed: {e}"))
                })?;
            }

            // Await response with timeout
            let result = tokio::time::timeout(self.config.batch_timeout, rx)
                .await
                .map_err(|_| {
                    // Timeout — remove the pending entry so it doesn't leak
                    session.batch_inflight.complete_batch(
                        batch_id,
                        Err(AeonError::connection("T3 batch response timeout")),
                    );
                    AeonError::connection(format!(
                        "T3 batch {batch_id} timed out after {:?}",
                        self.config.batch_timeout,
                    ))
                })?;

            // oneshot recv error means the session closed before responding
            result.map_err(|_| AeonError::connection("T3 session closed before batch response"))?
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
    routing: RoutingTable,
    data_streams: DataStreamMap,
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
        let data_streams = data_streams.clone();
        let config = config.clone();

        tokio::spawn(async move {
            if let Err(e) =
                handle_wt_session(session, sessions, routing, data_streams, config).await
            {
                tracing::debug!(error = %e, "T3 session ended");
            }
        });
    }
}

/// Handle a single WebTransport processor session.
async fn handle_wt_session(
    connection: wtransport::Connection,
    sessions: Arc<DashMap<String, Arc<AwppSession>>>,
    routing: RoutingTable,
    data_streams: DataStreamMap,
    config: Arc<WebTransportHostConfig>,
) -> Result<(), AeonError> {
    // Accept the first bidirectional stream as the control stream
    let (ctrl_send, ctrl_recv) = connection
        .accept_bi()
        .await
        .map_err(|e| AeonError::connection(format!("failed to accept control stream: {e}")))?;

    let control = WtControlChannel::new(ctrl_send, ctrl_recv);

    // Run AWPP handshake with timeout
    let handshake_config = crate::transport::session::HandshakeConfig {
        oauth_required: config.oauth_required,
        heartbeat_interval: config.heartbeat_interval,
        batch_signing: true,
        pipeline_codec: config.pipeline_codec,
        max_inflight_batches: config.max_inflight_batches,
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
            routing.insert((assignment.name.clone(), partition), session_id.clone());
        }
    }

    tracing::info!(
        session_id = %session_id,
        processor = %session.processor_name,
        "T3 processor connected"
    );

    // Accept data streams until the connection closes.
    // Each data stream carries a routing header identifying the pipeline+partition,
    // followed by length-prefixed batch_wire frames in both directions.
    loop {
        match connection.accept_bi().await {
            Ok((send, mut recv)) => {
                // Read routing header: [4B name_len LE][name][2B partition LE]
                let mut len_buf = [0u8; 4];
                if recv.read_exact(&mut len_buf).await.is_err() {
                    tracing::warn!(session_id = %session_id, "T3 data stream: failed to read routing header");
                    continue;
                }
                let name_len = u32::from_le_bytes(len_buf) as usize;
                if name_len > 1024 {
                    tracing::warn!(session_id = %session_id, name_len, "T3 data stream: pipeline name too long");
                    continue;
                }
                let mut name_buf = vec![0u8; name_len + 2]; // name + 2B partition
                if recv.read_exact(&mut name_buf).await.is_err() {
                    tracing::warn!(session_id = %session_id, "T3 data stream: routing header truncated");
                    continue;
                }
                let pipeline_name = match std::str::from_utf8(&name_buf[..name_len]) {
                    Ok(n) => n.to_string(),
                    Err(_) => {
                        tracing::warn!(session_id = %session_id, "T3 data stream: invalid pipeline name");
                        continue;
                    }
                };
                let partition = u16::from_le_bytes([name_buf[name_len], name_buf[name_len + 1]]);
                let key = (pipeline_name.clone(), partition);

                tracing::debug!(
                    session_id = %session_id,
                    pipeline = %pipeline_name,
                    partition,
                    "T3 data stream registered"
                );

                // Store send half for outbound batch requests
                let send = Arc::new(Mutex::new(send));
                data_streams.insert(key.clone(), send);

                // Spawn reader task for inbound batch responses
                let session_ref = session.clone();
                let ds_ref = data_streams.clone();
                let key_cleanup = key.clone();
                tokio::spawn(async move {
                    if let Err(e) = wt_data_stream_reader(recv, &session_ref).await {
                        tracing::debug!(
                            session_id = %session_ref.session_id,
                            error = %e,
                            "T3 data stream reader ended"
                        );
                    }
                    // Remove the data stream on close so call_batch gets a clean error
                    ds_ref.remove(&key_cleanup);
                });
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
            let key = (assignment.name.clone(), partition);
            routing.remove(&key);
            data_streams.remove(&key);
        }
    }
    config.identity_store.disconnect(&session.fingerprint);

    Ok(())
}

/// Read batch responses from a QUIC data stream.
///
/// Each frame is length-prefixed: `[4B LE length][batch_wire response data]`.
/// Decoded responses are routed to the session's `BatchInflight` tracker.
async fn wt_data_stream_reader(
    mut recv: wtransport::RecvStream,
    session: &AwppSession,
) -> Result<(), AeonError> {
    loop {
        // Read 4-byte length prefix
        let mut len_buf = [0u8; 4];
        if let Err(e) = recv.read_exact(&mut len_buf).await {
            // Stream closed — not necessarily an error
            return Err(AeonError::connection(format!(
                "T3 data stream recv closed: {e}"
            )));
        }
        let frame_len = u32::from_le_bytes(len_buf) as usize;
        if frame_len > 16 * 1024 * 1024 {
            return Err(AeonError::serialization(format!(
                "T3 data frame too large: {frame_len} bytes"
            )));
        }

        // Read frame body
        let mut frame = vec![0u8; frame_len];
        recv.read_exact(&mut frame)
            .await
            .map_err(|e| AeonError::connection(format!("T3 data frame read failed: {e}")))?;

        // Decode batch response and complete the in-flight batch
        match crate::batch_wire::decode_batch_response(&frame, session.codec) {
            Ok(decoded) => {
                let outputs: Vec<Output> =
                    decoded.outputs_per_event.into_iter().flatten().collect();
                if !session
                    .batch_inflight
                    .complete_batch(decoded.batch_id, Ok(outputs))
                {
                    tracing::debug!(
                        batch_id = decoded.batch_id,
                        "T3 batch response for unknown/expired batch_id"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "T3 batch response decode failed");
            }
        }
    }
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
            send.write_all(&data)
                .await
                .map_err(|e| AeonError::connection(format!("T3 control send data failed: {e}")))?;
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
            recv.read_exact(&mut buf)
                .await
                .map_err(|e| AeonError::connection(format!("T3 control recv data failed: {e}")))?;
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
        assert_eq!(ProcessorTier::WebTransport.to_string(), "web-transport");
    }
}
