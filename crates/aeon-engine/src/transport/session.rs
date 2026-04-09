//! Shared AWPP session lifecycle for T3/T4 processor transports.
//!
//! Provides transport-agnostic abstractions for the AWPP handshake,
//! heartbeat monitoring, batch in-flight tracking, and drain signaling.
//! Both `WebTransportProcessorHost` and `WebSocketProcessorHost` build on this.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use aeon_types::awpp::{
    AcceptedPayload, ControlMessage, DrainPayload, HeartbeatPayload, PipelineAssignment,
    RejectedPayload,
};
use aeon_types::error::AeonError;
use aeon_types::event::Output;
use aeon_types::transport_codec::TransportCodec;

#[cfg(feature = "processor-auth")]
use crate::identity_store::ProcessorIdentityStore;

// ── Control Channel Trait ───────────────────────────────────────────────

/// Transport-agnostic channel for AWPP control messages.
///
/// Sends/receives JSON-encoded `ControlMessage` bytes. The transport layer
/// handles framing (length-prefix for QUIC, text frames for WebSocket).
pub trait ControlChannel: Send + Sync {
    /// Send a JSON-encoded control message.
    fn send_control(
        &self,
        msg: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + '_>>;

    /// Receive a JSON-encoded control message.
    fn recv_control(&self)
    -> Pin<Box<dyn Future<Output = Result<Vec<u8>, AeonError>> + Send + '_>>;
}

// ── Pipeline Resolver ───────────────────────────────────────────────────

/// Resolves requested pipelines into concrete partition assignments.
pub trait PipelineResolver: Send + Sync {
    /// Given a list of requested pipeline names and the processor name,
    /// return the partition assignments (or error if not found/authorized).
    fn resolve(
        &self,
        requested_pipelines: &[String],
        processor_name: &str,
    ) -> Result<Vec<PipelineAssignment>, AeonError>;
}

// ── Session State ───────────────────────────────────────────────────────

/// AWPP session lifecycle states.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Handshaking = 0,
    Active = 1,
    Draining = 2,
    Closed = 3,
}

impl SessionState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Handshaking,
            1 => Self::Active,
            2 => Self::Draining,
            3 => Self::Closed,
            _ => Self::Closed,
        }
    }
}

// ── Batch In-Flight Tracking ────────────────────────────────────────────

/// Default maximum number of concurrently in-flight batches per session.
///
/// Caps the DashMap pending table so a misbehaving or slow processor cannot
/// cause unbounded memory growth on the host. Callers of `start_batch` await
/// a semaphore permit when this limit is hit, exerting backpressure back
/// through the pipeline processor task.
pub const DEFAULT_MAX_INFLIGHT_BATCHES: usize = 1024;

/// A pending batch slot: the oneshot responder plus the semaphore permit
/// that reserves its capacity. The permit is dropped when the entry is
/// removed from the map, releasing the slot for the next `start_batch`.
type PendingSlot = (
    oneshot::Sender<Result<Vec<Output>, AeonError>>,
    OwnedSemaphorePermit,
);

/// Tracks in-flight batch requests awaiting responses.
///
/// Maps `batch_id` → (oneshot sender, semaphore permit). When the response
/// arrives, the transport calls `complete_batch` to resolve the waiting caller
/// and drop the permit (releasing a slot for the next batch).
///
/// The semaphore bounds the number of concurrently outstanding batches per
/// session. Without it, a slow processor would cause the pending DashMap to
/// grow without limit as new batches kept getting enqueued.
pub struct BatchInflight {
    next_id: AtomicU64,
    pending: DashMap<u64, PendingSlot>,
    semaphore: Arc<Semaphore>,
}

impl BatchInflight {
    /// Create a `BatchInflight` with the default max-inflight capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_INFLIGHT_BATCHES)
    }

    /// Create a `BatchInflight` bounded to at most `max_inflight` concurrent batches.
    pub fn with_capacity(max_inflight: usize) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: DashMap::new(),
            semaphore: Arc::new(Semaphore::new(max_inflight.max(1))),
        }
    }

    /// Allocate a batch_id and return the receiver for the eventual response.
    ///
    /// Awaits a semaphore permit first — if the session already has
    /// `max_inflight` batches outstanding, the caller suspends until an
    /// earlier batch completes (or is cancelled). The permit is held inside
    /// the pending map and released automatically when the batch completes
    /// or is cancelled via `complete_batch` / `cancel_all`.
    pub async fn start_batch(&self) -> (u64, oneshot::Receiver<Result<Vec<Output>, AeonError>>) {
        // A closed semaphore is only possible during shutdown. If that
        // happens we fall back to an immediately-cancelled receiver so the
        // caller sees a clean error instead of panicking.
        let permit = match Arc::clone(&self.semaphore).acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(Err(AeonError::connection("session closed")));
                return (0, rx);
            }
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, (tx, permit));
        (id, rx)
    }

    /// Resolve a pending batch with the given result. Returns false if
    /// the batch_id was not found (already completed or timed out).
    ///
    /// Dropping the stored `OwnedSemaphorePermit` releases a slot so the
    /// next `start_batch` caller can proceed.
    pub fn complete_batch(&self, batch_id: u64, result: Result<Vec<Output>, AeonError>) -> bool {
        if let Some((_, (tx, _permit))) = self.pending.remove(&batch_id) {
            let _ = tx.send(result);
            true
        } else {
            false
        }
    }

    /// Number of batches currently in flight.
    pub fn pending_count(&self) -> u32 {
        self.pending.len() as u32
    }

    /// Number of permits currently available — i.e., how many additional
    /// batches can be started before `start_batch` begins to block.
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Cancel all pending batches with a connection-closed error.
    pub fn cancel_all(&self) {
        let keys: Vec<u64> = self.pending.iter().map(|e| *e.key()).collect();
        for key in keys {
            if let Some((_, (tx, _permit))) = self.pending.remove(&key) {
                let _ = tx.send(Err(AeonError::connection("session closed")));
            }
        }
    }
}

impl Default for BatchInflight {
    fn default() -> Self {
        Self::new()
    }
}

// ── AWPP Session ────────────────────────────────────────────────────────

/// State of a single AWPP session with a connected processor.
pub struct AwppSession {
    /// Unique session identifier (UUID v4 string).
    pub session_id: String,
    /// ED25519 key fingerprint of the connected processor.
    pub fingerprint: String,
    /// Processor name from registration.
    pub processor_name: String,
    /// Processor version from registration.
    pub processor_version: String,
    /// Negotiated transport codec.
    pub codec: TransportCodec,
    /// Pipeline + partition assignments.
    pub pipeline_assignments: Vec<PipelineAssignment>,
    /// Whether per-batch ED25519 signing is required.
    pub batch_signing: bool,
    /// Heartbeat interval.
    pub heartbeat_interval: Duration,
    /// Last received heartbeat timestamp (millis).
    last_heartbeat: Arc<AtomicI64>,
    /// Session lifecycle state.
    state: Arc<AtomicU8>,
    /// When this session was established.
    pub connected_at: Instant,
    /// In-flight batch tracker.
    pub batch_inflight: Arc<BatchInflight>,
}

impl AwppSession {
    /// Get the current session state.
    pub fn state(&self) -> SessionState {
        SessionState::from_u8(self.state.load(Ordering::Relaxed))
    }

    /// Set the session state.
    pub fn set_state(&self, state: SessionState) {
        self.state.store(state as u8, Ordering::Relaxed);
    }

    /// Record receipt of a heartbeat from the remote processor.
    pub fn record_heartbeat(&self, timestamp_ms: i64) {
        self.last_heartbeat.store(timestamp_ms, Ordering::Relaxed);
    }

    /// Check whether the session is healthy (received heartbeat within tolerance).
    pub fn is_healthy(&self) -> bool {
        let state = self.state();
        if state == SessionState::Closed || state == SessionState::Handshaking {
            return false;
        }
        let last = self.last_heartbeat.load(Ordering::Relaxed);
        if last == 0 {
            // No heartbeat received yet — still within initial grace period
            return self.connected_at.elapsed() < self.heartbeat_interval * 3;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let elapsed_ms = now_ms - last;
        elapsed_ms < (self.heartbeat_interval.as_millis() as i64 * 2)
    }

    /// Send a drain signal to the processor via the control channel.
    pub async fn send_drain(
        &self,
        control: &dyn ControlChannel,
        reason: &str,
        deadline_ms: u64,
    ) -> Result<(), AeonError> {
        self.set_state(SessionState::Draining);
        let msg = ControlMessage::Drain(DrainPayload {
            reason: reason.into(),
            deadline_ms,
        });
        let json = serde_json::to_vec(&msg).map_err(|e| AeonError::serialization(e.to_string()))?;
        control.send_control(&json).await
    }

    /// Send a heartbeat on the control channel.
    pub async fn send_heartbeat(&self, control: &dyn ControlChannel) -> Result<(), AeonError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let msg = ControlMessage::Heartbeat(HeartbeatPayload {
            timestamp_ms: now_ms,
        });
        let json = serde_json::to_vec(&msg).map_err(|e| AeonError::serialization(e.to_string()))?;
        control.send_control(&json).await
    }

    /// Close the session: cancel all in-flight batches and set state to Closed.
    pub fn close(&self) {
        self.set_state(SessionState::Closed);
        self.batch_inflight.cancel_all();
    }
}

// ── Handshake ───────────────────────────────────────────────────────────

/// Configuration for the AWPP handshake.
pub struct HandshakeConfig {
    pub oauth_required: bool,
    pub heartbeat_interval: Duration,
    pub batch_signing: bool,
    /// Pipeline codec override — if set, this overrides the processor's preference.
    pub pipeline_codec: Option<TransportCodec>,
    /// Maximum number of concurrently in-flight batches per session.
    /// Bounds the pending DashMap so a slow processor cannot cause
    /// unbounded memory growth. `start_batch` suspends on a semaphore
    /// when this limit is hit.
    pub max_inflight_batches: usize,
}

impl Default for HandshakeConfig {
    fn default() -> Self {
        Self {
            oauth_required: false,
            heartbeat_interval: Duration::from_secs(10),
            batch_signing: true,
            pipeline_codec: None,
            max_inflight_batches: DEFAULT_MAX_INFLIGHT_BATCHES,
        }
    }
}

/// Run the server-side AWPP handshake.
///
/// 1. Send Challenge → 2. Receive Registration → 3. Verify → 4. Send Accepted/Rejected
#[cfg(feature = "processor-auth")]
pub async fn handshake(
    control: &dyn ControlChannel,
    identity_store: &ProcessorIdentityStore,
    pipeline_resolver: &dyn PipelineResolver,
    config: &HandshakeConfig,
) -> Result<AwppSession, AeonError> {
    use crate::processor_auth;

    // Step 1: Send Challenge
    let challenge = processor_auth::create_challenge(config.oauth_required);
    let nonce = challenge.nonce.clone();
    let challenge_msg = ControlMessage::Challenge(aeon_types::awpp::ChallengePayload {
        protocol: challenge.protocol,
        nonce: challenge.nonce,
        oauth_required: challenge.oauth_required,
    });
    let challenge_json =
        serde_json::to_vec(&challenge_msg).map_err(|e| AeonError::serialization(e.to_string()))?;
    control.send_control(&challenge_json).await?;

    // Step 2: Receive Registration
    let reg_bytes = control.recv_control().await?;
    let reg_msg: ControlMessage = serde_json::from_slice(&reg_bytes)
        .map_err(|e| AeonError::serialization(format!("invalid registration message: {e}")))?;

    let reg = match reg_msg {
        ControlMessage::Register(r) => r,
        _ => {
            return Err(AeonError::processor(
                "expected 'register' message, got something else",
            ));
        }
    };

    // Step 3: Verify
    // 3a: Look up identity by public key
    let identity = identity_store
        .get_by_public_key(&reg.public_key)
        .ok_or_else(|| {
            let rejected = aeon_types::awpp::Rejected::new(
                aeon_types::awpp::RejectCode::KeyNotFound,
                format!("no identity found for public key {}", &reg.public_key),
            );
            send_rejected_sync(rejected)
        })?;

    // 3b: Verify challenge signature
    let sig_valid = processor_auth::verify_challenge(&identity, &nonce, &reg.challenge_signature)?;
    if !sig_valid {
        let rejected = aeon_types::awpp::Rejected::new(
            aeon_types::awpp::RejectCode::AuthFailed,
            "ED25519 challenge signature verification failed",
        );
        let rejected_msg = ControlMessage::Rejected(RejectedPayload {
            code: rejected.code,
            message: rejected.message,
        });
        let json = serde_json::to_vec(&rejected_msg)
            .map_err(|e| AeonError::serialization(e.to_string()))?;
        control.send_control(&json).await?;
        return Err(AeonError::processor("challenge verification failed"));
    }

    // 3c: Check authorization (pipeline scope, instance limits)
    let current_connections = identity_store.active_connections(&identity.fingerprint);
    if let Err(rejected) = processor_auth::check_authorization(
        &identity,
        &reg.requested_pipelines,
        current_connections,
    ) {
        let rejected_msg = ControlMessage::Rejected(RejectedPayload {
            code: rejected.code,
            message: rejected.message,
        });
        let json = serde_json::to_vec(&rejected_msg)
            .map_err(|e| AeonError::serialization(e.to_string()))?;
        control.send_control(&json).await?;
        return Err(AeonError::processor("authorization check failed"));
    }

    // 3d: Resolve pipeline assignments
    let assignments = pipeline_resolver.resolve(&reg.requested_pipelines, &reg.name)?;

    // 3e: Determine codec (pipeline config overrides processor preference)
    let codec = config.pipeline_codec.unwrap_or(reg.transport_codec);

    // Step 4: Send Accepted
    let session_id = uuid::Uuid::new_v4().to_string();
    let accepted_msg = ControlMessage::Accepted(AcceptedPayload {
        session_id: session_id.clone(),
        pipelines: assignments.clone(),
        wire_format: "binary/v1".into(),
        transport_codec: codec,
        heartbeat_interval_ms: config.heartbeat_interval.as_millis() as u64,
        batch_signing: config.batch_signing,
    });
    let json =
        serde_json::to_vec(&accepted_msg).map_err(|e| AeonError::serialization(e.to_string()))?;
    control.send_control(&json).await?;

    // Register connection in identity store
    identity_store.connect(&identity.fingerprint);

    Ok(AwppSession {
        session_id,
        fingerprint: identity.fingerprint.clone(),
        processor_name: reg.name,
        processor_version: reg.version,
        codec,
        pipeline_assignments: assignments,
        batch_signing: config.batch_signing,
        heartbeat_interval: config.heartbeat_interval,
        last_heartbeat: Arc::new(AtomicI64::new(0)),
        state: Arc::new(AtomicU8::new(SessionState::Active as u8)),
        connected_at: Instant::now(),
        batch_inflight: Arc::new(BatchInflight::with_capacity(config.max_inflight_batches)),
    })
}

/// Helper: create an AeonError from a Rejected message (for early returns).
#[cfg(feature = "processor-auth")]
fn send_rejected_sync(rejected: aeon_types::awpp::Rejected) -> AeonError {
    AeonError::processor(format!("{}: {}", rejected.code, rejected.message))
}

// ── Control Message Helpers ─────────────────────────────────────────────

/// Parse a received control message from JSON bytes.
pub fn parse_control_message(data: &[u8]) -> Result<ControlMessage, AeonError> {
    serde_json::from_slice(data)
        .map_err(|e| AeonError::serialization(format!("invalid control message: {e}")))
}

/// Serialize a control message to JSON bytes.
pub fn serialize_control_message(msg: &ControlMessage) -> Result<Vec<u8>, AeonError> {
    serde_json::to_vec(msg).map_err(|e| AeonError::serialization(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn batch_inflight_roundtrip() {
        let inflight = BatchInflight::new();
        let (id, mut rx) = inflight.start_batch().await;
        assert_eq!(id, 1);
        assert_eq!(inflight.pending_count(), 1);

        let outputs = vec![];
        assert!(inflight.complete_batch(id, Ok(outputs)));
        assert_eq!(inflight.pending_count(), 0);

        let result = rx.try_recv().unwrap();
        assert!(result.is_ok());
    }

    #[test]
    fn batch_inflight_unknown_id_returns_false() {
        let inflight = BatchInflight::new();
        assert!(!inflight.complete_batch(999, Ok(vec![])));
    }

    #[tokio::test]
    async fn batch_inflight_cancel_all() {
        let inflight = BatchInflight::new();
        let (_id1, mut rx1) = inflight.start_batch().await;
        let (_id2, mut rx2) = inflight.start_batch().await;
        assert_eq!(inflight.pending_count(), 2);

        inflight.cancel_all();
        assert_eq!(inflight.pending_count(), 0);

        let r1 = rx1.try_recv().unwrap();
        assert!(r1.is_err());
        let r2 = rx2.try_recv().unwrap();
        assert!(r2.is_err());
    }

    #[tokio::test]
    async fn batch_inflight_ids_are_monotonic() {
        let inflight = BatchInflight::new();
        let (id1, _rx1) = inflight.start_batch().await;
        let (id2, _rx2) = inflight.start_batch().await;
        let (id3, _rx3) = inflight.start_batch().await;
        assert!(id1 < id2);
        assert!(id2 < id3);
    }

    #[tokio::test]
    async fn batch_inflight_bounded_by_capacity() {
        // Capacity of 2 — third start_batch should block until an earlier
        // batch completes, at which point the permit is released.
        let inflight = Arc::new(BatchInflight::with_capacity(2));
        let (id1, _rx1) = inflight.start_batch().await;
        let (_id2, _rx2) = inflight.start_batch().await;
        assert_eq!(inflight.available_permits(), 0);

        // Spawn a task that tries to start a third batch — it should block.
        let inflight_clone = Arc::clone(&inflight);
        let blocked = tokio::spawn(async move { inflight_clone.start_batch().await });

        // Give it a moment to prove it is actually blocked.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!blocked.is_finished());

        // Completing batch 1 releases a permit — the spawned task unblocks.
        assert!(inflight.complete_batch(id1, Ok(vec![])));
        let (id3, _rx3) = blocked.await.unwrap();
        assert_ne!(id3, id1);
        assert_eq!(inflight.pending_count(), 2);
    }

    #[tokio::test]
    async fn batch_inflight_permit_released_on_cancel_all() {
        let inflight = Arc::new(BatchInflight::with_capacity(1));
        let (_id1, _rx1) = inflight.start_batch().await;
        assert_eq!(inflight.available_permits(), 0);

        inflight.cancel_all();
        assert_eq!(inflight.available_permits(), 1);

        // Next start_batch should proceed immediately.
        let (_id2, _rx2) = inflight.start_batch().await;
    }

    #[test]
    fn session_state_transitions() {
        let session = AwppSession {
            session_id: "test".into(),
            fingerprint: "fp".into(),
            processor_name: "proc".into(),
            processor_version: "1.0".into(),
            codec: TransportCodec::default(),
            pipeline_assignments: vec![],
            batch_signing: true,
            heartbeat_interval: Duration::from_secs(10),
            last_heartbeat: Arc::new(AtomicI64::new(0)),
            state: Arc::new(AtomicU8::new(SessionState::Active as u8)),
            connected_at: Instant::now(),
            batch_inflight: Arc::new(BatchInflight::new()),
        };

        assert_eq!(session.state(), SessionState::Active);
        session.set_state(SessionState::Draining);
        assert_eq!(session.state(), SessionState::Draining);
        session.set_state(SessionState::Closed);
        assert_eq!(session.state(), SessionState::Closed);
    }

    #[test]
    fn session_heartbeat_tracking() {
        let session = AwppSession {
            session_id: "test".into(),
            fingerprint: "fp".into(),
            processor_name: "proc".into(),
            processor_version: "1.0".into(),
            codec: TransportCodec::default(),
            pipeline_assignments: vec![],
            batch_signing: true,
            heartbeat_interval: Duration::from_secs(10),
            last_heartbeat: Arc::new(AtomicI64::new(0)),
            state: Arc::new(AtomicU8::new(SessionState::Active as u8)),
            connected_at: Instant::now(),
            batch_inflight: Arc::new(BatchInflight::new()),
        };

        // Initially healthy (within grace period, no heartbeat yet)
        assert!(session.is_healthy());

        // Record a heartbeat with current time
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        session.record_heartbeat(now_ms);
        assert!(session.is_healthy());

        // Record a stale heartbeat (30 seconds ago)
        session.record_heartbeat(now_ms - 30_000);
        assert!(!session.is_healthy());
    }

    #[tokio::test]
    async fn session_close_cancels_inflight() {
        let session = AwppSession {
            session_id: "test".into(),
            fingerprint: "fp".into(),
            processor_name: "proc".into(),
            processor_version: "1.0".into(),
            codec: TransportCodec::default(),
            pipeline_assignments: vec![],
            batch_signing: true,
            heartbeat_interval: Duration::from_secs(10),
            last_heartbeat: Arc::new(AtomicI64::new(0)),
            state: Arc::new(AtomicU8::new(SessionState::Active as u8)),
            connected_at: Instant::now(),
            batch_inflight: Arc::new(BatchInflight::new()),
        };

        let (_id, mut rx) = session.batch_inflight.start_batch().await;
        assert_eq!(session.batch_inflight.pending_count(), 1);

        session.close();
        assert_eq!(session.state(), SessionState::Closed);
        assert_eq!(session.batch_inflight.pending_count(), 0);

        let result = rx.try_recv().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn closed_session_is_not_healthy() {
        let session = AwppSession {
            session_id: "test".into(),
            fingerprint: "fp".into(),
            processor_name: "proc".into(),
            processor_version: "1.0".into(),
            codec: TransportCodec::default(),
            pipeline_assignments: vec![],
            batch_signing: true,
            heartbeat_interval: Duration::from_secs(10),
            last_heartbeat: Arc::new(AtomicI64::new(0)),
            state: Arc::new(AtomicU8::new(SessionState::Closed as u8)),
            connected_at: Instant::now(),
            batch_inflight: Arc::new(BatchInflight::new()),
        };
        assert!(!session.is_healthy());
    }

    #[test]
    fn parse_and_serialize_control_message() {
        let msg = ControlMessage::Heartbeat(HeartbeatPayload {
            timestamp_ms: 1712345678000,
        });
        let bytes = serialize_control_message(&msg).unwrap();
        let parsed = parse_control_message(&bytes).unwrap();
        match parsed {
            ControlMessage::Heartbeat(hb) => assert_eq!(hb.timestamp_ms, 1712345678000),
            _ => panic!("expected heartbeat"),
        }
    }

    #[test]
    fn parse_invalid_control_message() {
        let result = parse_control_message(b"not json");
        assert!(result.is_err());
    }
}
