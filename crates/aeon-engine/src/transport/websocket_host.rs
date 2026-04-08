//! T4 WebSocket Processor Host — axum WebSocket server for out-of-process processors.
//!
//! Accepts WebSocket connections on the existing REST API port (4471) at
//! `/api/v1/processors/connect`. Runs the AWPP handshake over text frames
//! and exchanges batch data over binary frames with a routing header.
//!
//! Feature-gated behind `websocket-host` (implies `rest-api` + `processor-auth`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use tokio::sync::Mutex;

use aeon_types::error::AeonError;
use aeon_types::event::{Event, Output};
use aeon_types::processor_transport::{ProcessorHealth, ProcessorInfo, ProcessorTier};
use aeon_types::traits::ProcessorTransport;
use aeon_types::transport_codec::TransportCodec;

use crate::identity_store::ProcessorIdentityStore;
use crate::transport::session::{AwppSession, ControlChannel, PipelineResolver};

// ── Configuration ───────────────────────────────────────────────────────

/// Configuration for the WebSocket processor host.
pub struct WebSocketHostConfig {
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
    /// Pipeline name — used by `call_batch` to route events to the correct session.
    pub pipeline_name: String,
    /// Pipeline codec override (if set, overrides processor preference).
    pub pipeline_codec: Option<TransportCodec>,
}

impl WebSocketHostConfig {
    /// Create a new config with required fields and sensible defaults.
    pub fn new(
        identity_store: Arc<ProcessorIdentityStore>,
        pipeline_resolver: Arc<dyn PipelineResolver>,
    ) -> Self {
        Self {
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
        }
    }
}

// ── WebSocket Processor Host ────────────────────────────────────────────

/// T4 WebSocket processor host — accepts WebSocket connections from processors.
///
/// Integrates with the axum REST API server. Call [`WebSocketProcessorHost::handle_upgrade`]
/// from the WebSocket upgrade handler to process a new connection.
pub struct WebSocketProcessorHost {
    /// Active sessions keyed by session_id.
    sessions: Arc<DashMap<String, Arc<AwppSession>>>,
    /// Routing table: (pipeline_name, partition_id) → session_id.
    routing: Arc<DashMap<(String, u16), String>>,
    /// WebSocket handles: session_id → shared socket for sending frames.
    sockets: Arc<DashMap<String, Arc<WsSharedSocket>>>,
    /// Host configuration.
    config: Arc<WebSocketHostConfig>,
    /// When the host was created.
    created_at: Instant,
}

impl WebSocketProcessorHost {
    /// Create a new WebSocket processor host.
    pub fn new(config: WebSocketHostConfig) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            routing: Arc::new(DashMap::new()),
            sockets: Arc::new(DashMap::new()),
            config: Arc::new(config),
            created_at: Instant::now(),
        }
    }

    /// Handle a WebSocket upgrade — runs the AWPP handshake and manages
    /// the session lifecycle. Call this from the axum upgrade handler.
    pub async fn handle_upgrade(&self, socket: WebSocket) -> Result<(), AeonError> {
        let (shared, recv_rx) = WsSharedSocket::new(socket);

        let control = WsControlChannel {
            socket: shared.clone(),
            recv_rx: Mutex::new(recv_rx),
        };

        // Run AWPP handshake with timeout
        let handshake_config = crate::transport::session::HandshakeConfig {
            oauth_required: self.config.oauth_required,
            heartbeat_interval: self.config.heartbeat_interval,
            batch_signing: true,
            pipeline_codec: self.config.pipeline_codec,
        };

        let awpp = tokio::time::timeout(
            self.config.handshake_timeout,
            crate::transport::session::handshake(
                &control,
                &self.config.identity_store,
                &*self.config.pipeline_resolver,
                &handshake_config,
            ),
        )
        .await
        .map_err(|_| AeonError::connection("T4 handshake timeout"))??;

        let session = Arc::new(awpp);
        let session_id = session.session_id.clone();
        self.sessions.insert(session_id.clone(), session.clone());
        self.sockets.insert(session_id.clone(), shared.clone());

        // Update routing table
        for assignment in &session.pipeline_assignments {
            for &partition in &assignment.partitions {
                self.routing
                    .insert((assignment.name.clone(), partition), session_id.clone());
            }
        }

        tracing::info!(
            session_id = %session_id,
            processor = %session.processor_name,
            "T4 processor connected via WebSocket"
        );

        // Extract recv_rx from control channel for the read loop
        let mut recv_rx = control.recv_rx.into_inner();

        // Read loop — demultiplex text (control) and binary (data) frames
        let result = ws_read_loop(&mut recv_rx, &session).await;

        // Cleanup
        session.close();
        self.sessions.remove(&session_id);
        self.sockets.remove(&session_id);
        for assignment in &session.pipeline_assignments {
            for &partition in &assignment.partitions {
                self.routing.remove(&(assignment.name.clone(), partition));
            }
        }
        self.config.identity_store.disconnect(&session.fingerprint);

        tracing::debug!(session_id = %session_id, "T4 processor disconnected");

        result
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

impl ProcessorTransport for WebSocketProcessorHost {
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
                        "no T4 session for pipeline={} partition={partition}",
                        self.config.pipeline_name,
                    ))
                })?;

            let session = self
                .sessions
                .get(&session_id)
                .map(|s| Arc::clone(s.value()))
                .ok_or_else(|| {
                    AeonError::connection(format!("T4 session {session_id} not found"))
                })?;

            // Look up WebSocket handle
            let socket = self
                .sockets
                .get(&session_id)
                .map(|s| Arc::clone(s.value()))
                .ok_or_else(|| {
                    AeonError::connection(format!("T4 socket for session {session_id} not found"))
                })?;

            // Allocate batch_id and get response receiver
            let (batch_id, rx) = session.batch_inflight.start_batch();

            // Encode batch request, then wrap in data frame with routing header
            let wire = crate::batch_wire::encode_batch_request(batch_id, &events, session.codec)?;
            let frame = build_ws_data_frame(&self.config.pipeline_name, partition, &wire);

            // Send as binary WebSocket frame
            socket.send(Message::Binary(frame.into())).await?;

            // Await response with timeout
            let result = tokio::time::timeout(self.config.batch_timeout, rx)
                .await
                .map_err(|_| {
                    session.batch_inflight.complete_batch(
                        batch_id,
                        Err(AeonError::connection("T4 batch response timeout")),
                    );
                    AeonError::connection(format!(
                        "T4 batch {batch_id} timed out after {:?}",
                        self.config.batch_timeout,
                    ))
                })?;

            result.map_err(|_| AeonError::connection("T4 session closed before batch response"))?
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
            tier: ProcessorTier::WebSocket,
            capabilities: vec!["batch".into()],
        }
    }
}

// ── Shared WebSocket ────────────────────────────────────────────────────

/// Channel-based shared WebSocket — avoids mutex contention between the
/// read loop (which blocks on recv) and send callers (call_batch, heartbeat).
///
/// Architecture:
/// - A single owner task runs the I/O loop: reads from the socket and
///   processes `send_rx` commands in `tokio::select!`.
/// - Callers enqueue sends via `send_tx` (non-blocking).
/// - Received frames are forwarded through `recv_tx` for the read loop.
struct WsSharedSocket {
    send_tx: tokio::sync::mpsc::Sender<Message>,
}

impl WsSharedSocket {
    /// Create a new shared socket. Spawns a background I/O pump task.
    /// Returns (shared_socket, recv_rx for the read loop).
    fn new(
        socket: WebSocket,
    ) -> (
        Arc<Self>,
        tokio::sync::mpsc::Receiver<Result<Message, AeonError>>,
    ) {
        let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<Message>(256);
        let (recv_tx, recv_rx) =
            tokio::sync::mpsc::channel::<Result<Message, AeonError>>(256);

        // I/O pump: owns the WebSocket, multiplexes send/recv without contention.
        tokio::spawn(async move {
            let mut socket = socket;
            loop {
                tokio::select! {
                    // Outbound: dequeue a message to send.
                    Some(msg) = send_rx.recv() => {
                        if socket.send(msg).await.is_err() {
                            break;
                        }
                    }
                    // Inbound: read from the WebSocket.
                    msg = socket.recv() => {
                        match msg {
                            Some(Ok(frame)) => {
                                if recv_tx.send(Ok(frame)).await.is_err() {
                                    break; // read loop dropped
                                }
                            }
                            Some(Err(e)) => {
                                let _ = recv_tx.send(Err(AeonError::connection(
                                    format!("T4 WebSocket recv failed: {e}"),
                                ))).await;
                                break;
                            }
                            None => {
                                break; // connection closed
                            }
                        }
                    }
                }
            }
        });

        (Arc::new(Self { send_tx }), recv_rx)
    }

    async fn send(&self, msg: Message) -> Result<(), AeonError> {
        self.send_tx
            .send(msg)
            .await
            .map_err(|_| AeonError::connection("T4 WebSocket send channel closed"))
    }
}

// ── WebSocket Read Loop ─────────────────────────────────────────────────

/// Demultiplex incoming WebSocket frames from the recv channel.
///
/// - **Text frames**: AWPP control messages (heartbeat, drain, error).
/// - **Binary frames**: Batch response data with routing header.
async fn ws_read_loop(
    recv_rx: &mut tokio::sync::mpsc::Receiver<Result<Message, AeonError>>,
    session: &AwppSession,
) -> Result<(), AeonError> {
    while let Some(result) = recv_rx.recv().await {
        let msg = result?;

        match msg {
            Message::Text(text) => {
                let bytes = text.as_bytes();
                match crate::transport::session::parse_control_message(bytes) {
                    Ok(aeon_types::awpp::ControlMessage::Heartbeat(hb)) => {
                        session.record_heartbeat(hb.timestamp_ms);
                    }
                    Ok(aeon_types::awpp::ControlMessage::Error(err)) => {
                        tracing::warn!(
                            code = err.code,
                            message = %err.message,
                            "T4 processor reported error"
                        );
                    }
                    Ok(other) => {
                        tracing::debug!(
                            msg_type = ?std::mem::discriminant(&other),
                            "T4 unexpected control message"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "T4 malformed control message");
                    }
                }
            }
            Message::Binary(data) => {
                if let Err(e) = handle_ws_data_frame(&data, session) {
                    tracing::warn!(error = %e, "T4 data frame error");
                }
            }
            Message::Ping(_) | Message::Pong(_) => {
                // axum handles ping/pong automatically
            }
            Message::Close(_) => {
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Parse and handle a binary data frame (batch response).
///
/// Frame format: `[4B name_len LE][name][2B partition LE][batch_wire data]`
fn handle_ws_data_frame(data: &[u8], session: &AwppSession) -> Result<(), AeonError> {
    let (_name, _partition, offset) = parse_ws_routing_header(data)?;
    let batch_data = &data[offset..];

    let decoded = crate::batch_wire::decode_batch_response(batch_data, session.codec)?;

    let outputs: Vec<Output> = decoded.outputs_per_event.into_iter().flatten().collect();
    session
        .batch_inflight
        .complete_batch(decoded.batch_id, Ok(outputs));

    Ok(())
}

// ── Data Frame Helpers ──────────────────────────────────────────────────

/// Build a binary data frame for sending a batch request.
///
/// Frame format: `[4B name_len LE][name][2B partition LE][batch_wire data]`
pub fn build_ws_data_frame(pipeline_name: &str, partition: u16, batch_wire_data: &[u8]) -> Vec<u8> {
    let name_bytes = pipeline_name.as_bytes();
    let name_len = (name_bytes.len() as u32).to_le_bytes();
    let part_bytes = partition.to_le_bytes();

    let mut frame = Vec::with_capacity(4 + name_bytes.len() + 2 + batch_wire_data.len());
    frame.extend_from_slice(&name_len);
    frame.extend_from_slice(name_bytes);
    frame.extend_from_slice(&part_bytes);
    frame.extend_from_slice(batch_wire_data);
    frame
}

/// Parse the routing header from a binary data frame, returning
/// `(pipeline_name, partition, batch_data_offset)`.
pub fn parse_ws_routing_header(data: &[u8]) -> Result<(&str, u16, usize), AeonError> {
    if data.len() < 7 {
        return Err(AeonError::serialization("T4 data frame too short"));
    }
    let name_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if data.len() < 4 + name_len + 2 {
        return Err(AeonError::serialization("T4 data frame name truncated"));
    }
    let name = std::str::from_utf8(&data[4..4 + name_len])
        .map_err(|e| AeonError::serialization(format!("T4 invalid pipeline name: {e}")))?;
    let partition = u16::from_le_bytes([data[4 + name_len], data[4 + name_len + 1]]);
    Ok((name, partition, 4 + name_len + 2))
}

// ── Control Channel (WebSocket) ─────────────────────────────────────────

/// Control channel over WebSocket — uses the send channel for outgoing
/// and the recv channel for incoming during handshake.
struct WsControlChannel {
    socket: Arc<WsSharedSocket>,
    recv_rx: Mutex<tokio::sync::mpsc::Receiver<Result<Message, AeonError>>>,
}

impl ControlChannel for WsControlChannel {
    fn send_control(
        &self,
        msg: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + '_>> {
        // Control messages are JSON — safe to interpret as UTF-8.
        let text = String::from_utf8_lossy(msg).into_owned();
        Box::pin(async move { self.socket.send(Message::Text(text.into())).await })
    }

    fn recv_control(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, AeonError>> + Send + '_>> {
        Box::pin(async move {
            let mut rx = self.recv_rx.lock().await;
            loop {
                match rx.recv().await {
                    Some(Ok(Message::Text(text))) => {
                        return Ok(text.as_bytes().to_vec());
                    }
                    Some(Ok(Message::Binary(_))) => {
                        // Skip binary frames during handshake
                        continue;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_))) => {
                        return Err(AeonError::connection(
                            "T4 WebSocket closed during handshake",
                        ));
                    }
                    Some(Err(e)) => return Err(e),
                    None => {
                        return Err(AeonError::connection("T4 WebSocket connection lost"));
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_data_frame_build_parse() {
        let pipeline = "my-pipeline";
        let partition = 7u16;
        let batch_data = b"some-batch-wire-data";

        let frame = build_ws_data_frame(pipeline, partition, batch_data);

        let (name, part, offset) = parse_ws_routing_header(&frame).unwrap();
        assert_eq!(name, pipeline);
        assert_eq!(part, partition);
        assert_eq!(&frame[offset..], batch_data);
    }

    #[test]
    fn ws_data_frame_empty_name() {
        let frame = build_ws_data_frame("", 0, b"data");
        let (name, part, offset) = parse_ws_routing_header(&frame).unwrap();
        assert_eq!(name, "");
        assert_eq!(part, 0);
        assert_eq!(&frame[offset..], b"data");
    }

    #[test]
    fn ws_data_frame_too_short() {
        assert!(parse_ws_routing_header(&[0; 5]).is_err());
    }

    #[test]
    fn ws_data_frame_truncated_name() {
        // name_len says 100 but only 2 bytes of name present
        let mut frame = vec![100, 0, 0, 0, b'a', b'b'];
        frame.extend_from_slice(&[0, 0]); // partition
        assert!(parse_ws_routing_header(&frame).is_err());
    }

    #[test]
    fn ws_host_config_defaults() {
        assert_eq!(ProcessorTier::WebSocket.to_string(), "web-socket");
    }
}
