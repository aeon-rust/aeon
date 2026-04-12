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
use aeon_types::event::{Event, Output};
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

/// A pending batch slot: the oneshot responder, the original event batch
/// (retained so it can be replayed on reconnect — TR-1), and the semaphore
/// permit that reserves its capacity. The permit is dropped when the entry
/// is removed from the map, releasing the slot for the next `start_batch`.
///
/// Events are carried as `Arc<Vec<Event>>` so retention is a refcount bump
/// rather than a payload copy — the transport encoder already has the
/// events by reference, and the replay path needs the same allocation.
type PendingSlot = (
    oneshot::Sender<Result<Vec<Output>, AeonError>>,
    Arc<Vec<Event>>,
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
    /// The `events` handle is retained inside the pending map so disconnect
    /// handlers can drain the in-flight batches for replay (TR-1) rather
    /// than losing them. Callers should pass the same `Arc<Vec<Event>>` they
    /// serialize onto the wire — retention is a refcount bump, not a copy.
    ///
    /// Awaits a semaphore permit first — if the session already has
    /// `max_inflight` batches outstanding, the caller suspends until an
    /// earlier batch completes (or is cancelled). The permit is held inside
    /// the pending map and released automatically when the batch completes
    /// or is cancelled via `complete_batch` / `cancel_all` / `drain_for_replay`.
    pub async fn start_batch(
        &self,
        events: Arc<Vec<Event>>,
    ) -> (u64, oneshot::Receiver<Result<Vec<Output>, AeonError>>) {
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
        self.pending.insert(id, (tx, events, permit));
        (id, rx)
    }

    /// Resolve a pending batch with the given result. Returns false if
    /// the batch_id was not found (already completed or timed out).
    ///
    /// Dropping the stored `OwnedSemaphorePermit` releases a slot so the
    /// next `start_batch` caller can proceed.
    pub fn complete_batch(&self, batch_id: u64, result: Result<Vec<Output>, AeonError>) -> bool {
        if let Some((_, (tx, _events, _permit))) = self.pending.remove(&batch_id) {
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

    /// Sum of event counts across all in-flight batches. Used by metrics and
    /// disconnect-handling logs so operators see exactly how many events were
    /// retained for replay (or lost, if replay is disabled).
    pub fn inflight_event_count(&self) -> u64 {
        self.pending
            .iter()
            .map(|e| e.value().1.len() as u64)
            .sum()
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
            if let Some((_, (tx, _events, _permit))) = self.pending.remove(&key) {
                let _ = tx.send(Err(AeonError::connection("session closed")));
            }
        }
    }

    /// Drain all in-flight batches for replay (TR-1). Removes every pending
    /// entry and returns `(batch_id, events, oneshot_sender)` tuples — the
    /// oneshot senders are **not** signalled, so the pipeline callers remain
    /// parked awaiting their responses. A reconnect orchestrator can then
    /// re-submit the events on a new session and forward the response via
    /// the returned `oneshot::Sender` when it arrives.
    ///
    /// Batches returned in ascending `batch_id` order so replay preserves
    /// original submission order. Drop the `Sender` to fail the waiting
    /// caller if replay is abandoned (the oneshot closes with
    /// `RecvError::Closed`).
    #[must_use = "dropped senders fail the waiting pipeline callers — wire them to replay or to final error"]
    pub fn drain_for_replay(&self) -> Vec<InflightBatch> {
        let mut keys: Vec<u64> = self.pending.iter().map(|e| *e.key()).collect();
        keys.sort_unstable();
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some((_, (tx, events, permit))) = self.pending.remove(&key) {
                out.push(InflightBatch {
                    batch_id: key,
                    events,
                    responder: tx,
                    _permit: permit,
                });
            }
        }
        out
    }
}

/// A batch drained from `BatchInflight::drain_for_replay`. Holds the original
/// events and the oneshot responder so a reconnect orchestrator can re-submit
/// the batch on a new session and forward the response to the waiting caller.
///
/// The `_permit` field keeps the original session's permit alive until this
/// struct is dropped — dropping it releases the slot on the **old** session's
/// semaphore, which is harmless because that session is closing anyway.
pub struct InflightBatch {
    pub batch_id: u64,
    pub events: Arc<Vec<Event>>,
    pub responder: oneshot::Sender<Result<Vec<Output>, AeonError>>,
    _permit: OwnedSemaphorePermit,
}

// ── Replay Orchestrator (TR-1 Stage 2) ──────────────────────────────────

/// Replay bucket key: `(fingerprint, pipeline, partition)`.
///
/// Batches are stashed per identity-pipeline-partition so a reconnect
/// arriving with the same ED25519 fingerprint and covering the same
/// partition picks up exactly the batches that were mid-flight when the
/// prior session dropped.
pub type ReplayKey = (String, String, u16);

struct ReplayBucket {
    batches: Vec<InflightBatch>,
    expires_at: Instant,
}

/// Host-level orchestrator for TR-1 replay-on-reconnect.
///
/// When a processor session disconnects with in-flight batches, the host
/// calls [`ReplayOrchestrator::stash`]. The batches live in identity-keyed
/// buckets for `replay_window`. If a new session arrives with the same
/// fingerprint and is assigned the same pipeline+partition, the host
/// calls [`ReplayOrchestrator::take`] to re-submit them on the new session
/// — the original pipeline callers remain parked on their oneshot receivers
/// and get the replayed response transparently.
///
/// If the replay window elapses without a matching reconnect, a background
/// sweep fails each stashed batch's oneshot with a `connection` error so
/// pipeline callers unblock.
pub struct ReplayOrchestrator {
    buckets: Arc<DashMap<ReplayKey, ReplayBucket>>,
    replay_window: Duration,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl ReplayOrchestrator {
    /// Create a new orchestrator and spawn its expiry-sweep task.
    ///
    /// `replay_window` is how long a disconnected processor has to
    /// reconnect before its stashed batches are failed. Typical value is
    /// 30s — long enough to ride out a transient network glitch, short
    /// enough that a permanently-dead processor doesn't park pipeline
    /// callers forever.
    pub fn new(replay_window: Duration) -> Arc<Self> {
        let buckets: Arc<DashMap<ReplayKey, ReplayBucket>> = Arc::new(DashMap::new());
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let orchestrator = Arc::new(Self {
            buckets: Arc::clone(&buckets),
            replay_window,
            shutdown: Arc::clone(&shutdown),
        });

        // Sweep cadence: quarter of the window, clamped to [100ms, 5s].
        // Short enough that expired buckets don't linger, long enough that
        // idle hosts don't burn CPU on empty scans.
        let interval = replay_window
            .checked_div(4)
            .unwrap_or(Duration::from_secs(1))
            .max(Duration::from_millis(100))
            .min(Duration::from_secs(5));

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let now = Instant::now();
                let expired: Vec<ReplayKey> = buckets
                    .iter()
                    .filter(|e| e.value().expires_at <= now)
                    .map(|e| e.key().clone())
                    .collect();
                for key in expired {
                    if let Some((_, bucket)) = buckets.remove(&key) {
                        tracing::warn!(
                            fingerprint = %key.0,
                            pipeline = %key.1,
                            partition = key.2,
                            batches = bucket.batches.len(),
                            "TR-1 replay window expired — failing stashed batches"
                        );
                        for b in bucket.batches {
                            let _ = b.responder.send(Err(AeonError::connection(
                                "TR-1 replay window expired before processor reconnected",
                            )));
                        }
                    }
                }
            }
        });

        orchestrator
    }

    /// Configured replay window.
    pub fn replay_window(&self) -> Duration {
        self.replay_window
    }

    /// Stash in-flight batches from a disconnected session. Returns the
    /// number of batches stashed.
    ///
    /// Drains `session.batch_inflight` and bucketizes by (fingerprint,
    /// pipeline, partition). Partition comes from `events[0].partition`
    /// in each batch; pipeline is the session's assignment that contains
    /// that partition. A batch whose events list is empty, or whose
    /// partition isn't in any assignment, falls into the empty-pipeline
    /// bucket and will only be reclaimed by a matching reconnect if the
    /// caller also supplies empty strings — in practice this only happens
    /// in tests.
    pub fn stash(&self, session: &AwppSession) -> usize {
        let drained = session.batch_inflight.drain_for_replay();
        let expires_at = Instant::now() + self.replay_window;
        let count = drained.len();
        for batch in drained {
            let partition = batch
                .events
                .first()
                .map(|e| e.partition.as_u16())
                .unwrap_or(0);
            let pipeline = session
                .pipeline_assignments
                .iter()
                .find(|a| a.partitions.contains(&partition))
                .map(|a| a.name.clone())
                .unwrap_or_default();
            let key = (session.fingerprint.clone(), pipeline, partition);
            let mut entry = self.buckets.entry(key).or_insert_with(|| ReplayBucket {
                batches: Vec::new(),
                expires_at,
            });
            entry.batches.push(batch);
            // Refresh expiry so late-arriving batches don't inherit a stale deadline.
            entry.expires_at = expires_at;
        }
        count
    }

    /// Remove and return stashed batches matching `(fingerprint,
    /// pipeline, partition)`. Caller re-submits them on the new session.
    ///
    /// Returns `None` if no bucket matches, or if the bucket matches but
    /// the replay window has already elapsed — in the latter case the
    /// stashed responders are failed before the bucket is dropped so
    /// pipeline callers unblock.
    pub fn take(
        &self,
        fingerprint: &str,
        pipeline: &str,
        partition: u16,
    ) -> Option<Vec<InflightBatch>> {
        let key = (fingerprint.to_string(), pipeline.to_string(), partition);
        let (_, bucket) = self.buckets.remove(&key)?;
        if Instant::now() >= bucket.expires_at {
            for b in bucket.batches {
                let _ = b.responder.send(Err(AeonError::connection(
                    "TR-1 replay window expired before processor reconnected",
                )));
            }
            return None;
        }
        Some(bucket.batches)
    }

    /// Number of stashed buckets currently held.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Sum of stashed batches across all buckets.
    pub fn stashed_batch_count(&self) -> usize {
        self.buckets.iter().map(|e| e.value().batches.len()).sum()
    }
}

impl Drop for ReplayOrchestrator {
    fn drop(&mut self) {
        // Stop the sweep task on the next tick.
        self.shutdown.store(true, Ordering::Relaxed);
        // Fail any remaining buckets so waiters don't leak past host shutdown.
        let keys: Vec<ReplayKey> = self.buckets.iter().map(|e| e.key().clone()).collect();
        for key in keys {
            if let Some((_, bucket)) = self.buckets.remove(&key) {
                for b in bucket.batches {
                    let _ = b.responder.send(Err(AeonError::connection(
                        "replay orchestrator shut down",
                    )));
                }
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

    fn empty_events() -> Arc<Vec<Event>> {
        Arc::new(Vec::new())
    }

    fn sample_events(count: usize) -> Arc<Vec<Event>> {
        use aeon_types::PartitionId;
        use bytes::Bytes;
        let src: Arc<str> = Arc::from("t");
        let vec: Vec<Event> = (0..count)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i as i64,
                    Arc::clone(&src),
                    PartitionId::new(0),
                    Bytes::from_static(b"x"),
                )
            })
            .collect();
        Arc::new(vec)
    }

    #[tokio::test]
    async fn batch_inflight_roundtrip() {
        let inflight = BatchInflight::new();
        let (id, mut rx) = inflight.start_batch(empty_events()).await;
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
        let (_id1, mut rx1) = inflight.start_batch(empty_events()).await;
        let (_id2, mut rx2) = inflight.start_batch(empty_events()).await;
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
        let (id1, _rx1) = inflight.start_batch(empty_events()).await;
        let (id2, _rx2) = inflight.start_batch(empty_events()).await;
        let (id3, _rx3) = inflight.start_batch(empty_events()).await;
        assert!(id1 < id2);
        assert!(id2 < id3);
    }

    #[tokio::test]
    async fn batch_inflight_bounded_by_capacity() {
        // Capacity of 2 — third start_batch should block until an earlier
        // batch completes, at which point the permit is released.
        let inflight = Arc::new(BatchInflight::with_capacity(2));
        let (id1, _rx1) = inflight.start_batch(empty_events()).await;
        let (_id2, _rx2) = inflight.start_batch(empty_events()).await;
        assert_eq!(inflight.available_permits(), 0);

        // Spawn a task that tries to start a third batch — it should block.
        let inflight_clone = Arc::clone(&inflight);
        let blocked =
            tokio::spawn(async move { inflight_clone.start_batch(empty_events()).await });

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
        let (_id1, _rx1) = inflight.start_batch(empty_events()).await;
        assert_eq!(inflight.available_permits(), 0);

        inflight.cancel_all();
        assert_eq!(inflight.available_permits(), 1);

        // Next start_batch should proceed immediately.
        let (_id2, _rx2) = inflight.start_batch(empty_events()).await;
    }

    #[tokio::test]
    async fn drain_for_replay_returns_events_and_senders_in_order() {
        let inflight = BatchInflight::new();
        let e1 = sample_events(3);
        let e2 = sample_events(5);
        let e3 = sample_events(2);
        let (id1, _rx1) = inflight.start_batch(Arc::clone(&e1)).await;
        let (id2, _rx2) = inflight.start_batch(Arc::clone(&e2)).await;
        let (id3, _rx3) = inflight.start_batch(Arc::clone(&e3)).await;
        assert_eq!(inflight.inflight_event_count(), 10);

        let drained = inflight.drain_for_replay();
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].batch_id, id1);
        assert_eq!(drained[1].batch_id, id2);
        assert_eq!(drained[2].batch_id, id3);
        assert_eq!(drained[0].events.len(), 3);
        assert_eq!(drained[1].events.len(), 5);
        assert_eq!(drained[2].events.len(), 2);
        assert_eq!(inflight.pending_count(), 0);
        assert_eq!(inflight.inflight_event_count(), 0);
    }

    #[tokio::test]
    async fn drain_for_replay_keeps_waiters_parked() {
        // Unlike cancel_all, draining must NOT complete the oneshot — the
        // orchestrator holds the responder and only fires it once the replay
        // result is known.
        let inflight = BatchInflight::new();
        let (_id, mut rx) = inflight.start_batch(sample_events(1)).await;

        let drained = inflight.drain_for_replay();
        assert_eq!(drained.len(), 1);
        // The oneshot must still be open — nothing has been sent on it.
        assert!(rx.try_recv().is_err()); // Empty but not closed.

        // When the drained batch's responder is eventually fired by the
        // replay orchestrator, the waiter receives it.
        let outputs = vec![];
        let _ = drained.into_iter().next().unwrap().responder.send(Ok(outputs));
        let result = rx.try_recv().expect("responder fired");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn drain_for_replay_releases_permits_on_drop() {
        let inflight = Arc::new(BatchInflight::with_capacity(2));
        let (_id1, _rx1) = inflight.start_batch(sample_events(1)).await;
        let (_id2, _rx2) = inflight.start_batch(sample_events(1)).await;
        assert_eq!(inflight.available_permits(), 0);

        let drained = inflight.drain_for_replay();
        // Permits still held by the drained batches until they're dropped.
        assert_eq!(inflight.available_permits(), 0);
        drop(drained);
        assert_eq!(inflight.available_permits(), 2);
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

        let (_id, mut rx) = session.batch_inflight.start_batch(empty_events()).await;
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

    // ── ReplayOrchestrator (TR-1 Stage 2) ───────────────────────────────

    fn make_session(fingerprint: &str, pipeline: &str, partitions: Vec<u16>) -> AwppSession {
        AwppSession {
            session_id: format!("s-{fingerprint}"),
            fingerprint: fingerprint.into(),
            processor_name: "proc".into(),
            processor_version: "1.0".into(),
            codec: TransportCodec::default(),
            pipeline_assignments: vec![PipelineAssignment {
                name: pipeline.into(),
                partitions,
                batch_size: 100,
            }],
            batch_signing: true,
            heartbeat_interval: Duration::from_secs(10),
            last_heartbeat: Arc::new(AtomicI64::new(0)),
            state: Arc::new(AtomicU8::new(SessionState::Active as u8)),
            connected_at: Instant::now(),
            batch_inflight: Arc::new(BatchInflight::new()),
        }
    }

    fn events_on_partition(count: usize, partition: u16) -> Arc<Vec<Event>> {
        use aeon_types::PartitionId;
        use bytes::Bytes;
        let src: Arc<str> = Arc::from("t");
        let vec: Vec<Event> = (0..count)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i as i64,
                    Arc::clone(&src),
                    PartitionId::new(partition),
                    Bytes::from_static(b"x"),
                )
            })
            .collect();
        Arc::new(vec)
    }

    #[tokio::test]
    async fn replay_orchestrator_stash_and_take_roundtrip() {
        let orch = ReplayOrchestrator::new(Duration::from_secs(5));
        let sess = make_session("fp1", "p1", vec![0, 1]);

        // Submit two batches on partition 0 and one on partition 1.
        let (_id1, mut rx1) = sess
            .batch_inflight
            .start_batch(events_on_partition(2, 0))
            .await;
        let (_id2, _rx2) = sess
            .batch_inflight
            .start_batch(events_on_partition(3, 0))
            .await;
        let (_id3, _rx3) = sess
            .batch_inflight
            .start_batch(events_on_partition(1, 1))
            .await;

        assert_eq!(orch.stash(&sess), 3);
        assert_eq!(orch.bucket_count(), 2);
        assert_eq!(orch.stashed_batch_count(), 3);

        // Reconnect for partition 0 — picks up the two stashed batches.
        let p0 = orch.take("fp1", "p1", 0).expect("partition 0 bucket");
        assert_eq!(p0.len(), 2);
        assert_eq!(orch.bucket_count(), 1);

        // Caller hasn't fired the senders yet — original waiter still parked.
        assert!(rx1.try_recv().is_err());

        // Once the replay resolves, firing the stashed responder unblocks the waiter.
        let mut drained = p0.into_iter();
        let first = drained.next().unwrap();
        let _ = first.responder.send(Ok(vec![]));
        assert!(rx1.try_recv().expect("responder fired").is_ok());

        // Partition 1 bucket also reclaimable.
        let p1 = orch.take("fp1", "p1", 1).expect("partition 1 bucket");
        assert_eq!(p1.len(), 1);
        assert_eq!(orch.bucket_count(), 0);
    }

    #[tokio::test]
    async fn replay_orchestrator_take_wrong_identity_returns_none() {
        let orch = ReplayOrchestrator::new(Duration::from_secs(5));
        let sess = make_session("fpA", "p1", vec![0]);
        let (_id, _rx) = sess
            .batch_inflight
            .start_batch(events_on_partition(1, 0))
            .await;
        orch.stash(&sess);

        assert!(orch.take("fpB", "p1", 0).is_none());
        assert!(orch.take("fpA", "other", 0).is_none());
        assert!(orch.take("fpA", "p1", 99).is_none());
        // Original bucket still there for the right reconnect.
        assert_eq!(orch.bucket_count(), 1);
    }

    #[tokio::test]
    async fn replay_orchestrator_expiry_fails_waiters() {
        // Tiny window + tiny sweep cadence to keep the test fast.
        let orch = ReplayOrchestrator::new(Duration::from_millis(200));
        let sess = make_session("fp1", "p1", vec![0]);
        let (_id, mut rx) = sess
            .batch_inflight
            .start_batch(events_on_partition(1, 0))
            .await;
        orch.stash(&sess);
        assert_eq!(orch.bucket_count(), 1);

        // Wait longer than the window + one sweep interval (~50ms clamped to 100ms).
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(orch.bucket_count(), 0);
        let result = rx.try_recv().expect("expiry fires the responder");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn replay_orchestrator_drop_fails_remaining_waiters() {
        let sess = make_session("fp1", "p1", vec![0]);
        let (_id, mut rx) = sess
            .batch_inflight
            .start_batch(events_on_partition(1, 0))
            .await;
        {
            let orch = ReplayOrchestrator::new(Duration::from_secs(60));
            orch.stash(&sess);
            // Drop the only Arc → Drop impl fires.
        }
        // Give the drop a tick to run.
        tokio::task::yield_now().await;
        let result = rx.try_recv().expect("drop fires the responder");
        assert!(result.is_err());
    }
}
