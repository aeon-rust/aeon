//! Inbound QUIC handler — accepts connections, dispatches Raft RPCs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use openraft::Raft;

use crate::raft_config::AeonRaftConfig;
use crate::transport::cutover::{CutoverCoordinator, serve_partition_cutover_with_request};
use crate::transport::endpoint::QuicEndpoint;
use crate::transport::framing::{self, MessageType};
use crate::transport::health;
use crate::transport::partition_transfer::{
    PartitionTransferProvider, serve_partition_transfer_with_request,
};
use crate::transport::poh_transfer::{PohChainProvider, serve_poh_chain_transfer_with_request};
use crate::types::{
    JoinRequest, JoinResponse, NodeId, PartitionCutoverRequest, PartitionCutoverResponse,
    PartitionTransferEnd, PartitionTransferRequest, PohChainTransferRequest,
    PohChainTransferResponse, ProposeForwardRequest, ProposeForwardResponse, RemoveNodeRequest,
    RemoveNodeResponse,
};

/// Shared provider slots for source-side CL-6 handlers. The three slots
/// (segment / PoH / cutover) are populated **post-bootstrap** by the
/// engine layer via `ClusterNode::install_{segment,poh,cutover}_provider`,
/// because [`ClusterNode::bootstrap_multi`] must start the QUIC accept
/// loop before `aeon-cli` has built the engine-side installers.
///
/// The serve loop reads each slot **per-request** (cold-path `RwLock`
/// read) so providers become active as soon as they are installed,
/// without restarting the accept task. Cheaply cloneable — every field
/// is `Arc<RwLock<Option<Arc<...>>>>`.
///
/// CL-6c.4 (engine-side write-freeze wiring): the cutover slot is the
/// hook by which the target-node `PartitionTransferDriver` reaches the
/// source-node `WriteGate` over QUIC. Before this slot is populated,
/// every inbound `PartitionCutoverRequest` replies with a "no
/// coordinator configured" failure and the transfer aborts.
#[derive(Clone, Default)]
pub struct SourceProviderSlots {
    segment: Arc<tokio::sync::RwLock<Option<Arc<dyn PartitionTransferProvider>>>>,
    poh: Arc<tokio::sync::RwLock<Option<Arc<dyn PohChainProvider>>>>,
    cutover: Arc<tokio::sync::RwLock<Option<Arc<dyn CutoverCoordinator>>>>,
}

impl SourceProviderSlots {
    /// Empty slots — all three providers default to `None`. The serve
    /// loop replies with a failure frame on any incoming request until a
    /// provider is installed via the corresponding `install_*` method.
    pub fn new_empty() -> Self {
        Self::default()
    }

    /// Seed the slots with fixed provider handles. Primarily a test seam
    /// — production callers construct empty slots and install providers
    /// post-bootstrap.
    pub fn from_fixed(
        segment: Option<Arc<dyn PartitionTransferProvider>>,
        poh: Option<Arc<dyn PohChainProvider>>,
        cutover: Option<Arc<dyn CutoverCoordinator>>,
    ) -> Self {
        Self {
            segment: Arc::new(tokio::sync::RwLock::new(segment)),
            poh: Arc::new(tokio::sync::RwLock::new(poh)),
            cutover: Arc::new(tokio::sync::RwLock::new(cutover)),
        }
    }

    pub async fn install_segment_provider(&self, p: Arc<dyn PartitionTransferProvider>) {
        *self.segment.write().await = Some(p);
    }

    pub async fn install_poh_provider(&self, p: Arc<dyn PohChainProvider>) {
        *self.poh.write().await = Some(p);
    }

    pub async fn install_cutover_coordinator(&self, c: Arc<dyn CutoverCoordinator>) {
        *self.cutover.write().await = Some(c);
    }

    async fn current_segment(&self) -> Option<Arc<dyn PartitionTransferProvider>> {
        self.segment.read().await.clone()
    }

    async fn current_poh(&self) -> Option<Arc<dyn PohChainProvider>> {
        self.poh.read().await.clone()
    }

    async fn current_cutover(&self) -> Option<Arc<dyn CutoverCoordinator>> {
        self.cutover.read().await.clone()
    }
}

/// Slot-based variant of [`serve`]. Reads providers from [`SourceProviderSlots`]
/// per stream, so providers installed after the accept loop starts become
/// active without a restart. Used by `ClusterNode` bootstrap paths;
/// prefer [`serve`] for tests that wire a fixed set of providers.
pub async fn serve_with_slots(
    endpoint: Arc<QuicEndpoint>,
    raft: Raft<AeonRaftConfig>,
    self_id: NodeId,
    shutdown: Arc<AtomicBool>,
    slots: SourceProviderSlots,
) {
    while !shutdown.load(Ordering::Relaxed) {
        let incoming = tokio::select! {
            incoming = endpoint.accept() => {
                match incoming {
                    Some(i) => i,
                    None => break, // Endpoint closed
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => continue,
        };

        let raft = raft.clone();
        let slots = slots.clone();
        tokio::spawn(async move {
            let connection = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("QUIC handshake failed: {e}");
                    return;
                }
            };

            loop {
                let stream = match connection.accept_bi().await {
                    Ok(s) => s,
                    Err(quinn::ConnectionError::ApplicationClosed(_)) => break,
                    Err(e) => {
                        tracing::debug!("QUIC stream accept error: {e}");
                        break;
                    }
                };

                let raft = raft.clone();
                let slots = slots.clone();
                tokio::spawn(handle_stream_slots(raft, self_id, slots, stream));
            }
        });
    }
}

/// Per-stream handler that resolves providers from shared slots at
/// dispatch time, rather than capturing them at spawn time (which
/// [`handle_stream`] does). Keeps late-installation correct.
async fn handle_stream_slots(
    raft: Raft<AeonRaftConfig>,
    self_id: NodeId,
    slots: SourceProviderSlots,
    (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
) {
    let (msg_type, payload) = match framing::read_frame(&mut recv).await {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!("failed to read frame: {e}");
            return;
        }
    };

    let result = match msg_type {
        MessageType::AppendEntries => handle_append_entries(&raft, &payload, &mut send).await,
        MessageType::Vote => handle_vote(&raft, &payload, &mut send).await,
        MessageType::InstallSnapshot => handle_install_snapshot(&raft, &payload, &mut send).await,
        MessageType::FullSnapshot => handle_full_snapshot(&raft, &payload, &mut send).await,
        MessageType::AddNodeRequest => handle_add_node(&raft, self_id, &payload, &mut send).await,
        MessageType::RemoveNodeRequest => {
            handle_remove_node(&raft, self_id, &payload, &mut send).await
        }
        MessageType::HealthPing => health::handle_health_ping(self_id, &payload, &mut send).await,
        MessageType::PartitionTransferRequest => {
            let provider = slots.current_segment().await;
            handle_partition_transfer(provider.as_deref(), payload, send).await
        }
        MessageType::PohChainTransferRequest => {
            let provider = slots.current_poh().await;
            handle_poh_chain_transfer(provider.as_deref(), payload, send).await
        }
        MessageType::PartitionCutoverRequest => {
            let coord = slots.current_cutover().await;
            handle_partition_cutover(coord.as_deref(), payload, send).await
        }
        MessageType::ProposeForwardRequest => {
            handle_propose_forward(&raft, &payload, &mut send).await
        }
        _ => {
            tracing::warn!("unexpected message type: {:?}", msg_type);
            Ok(())
        }
    };

    if let Err(e) = result {
        tracing::debug!("RPC handler error: {e}");
    }
}

/// Run the QUIC server accept loop, dispatching incoming RPCs to the Raft node.
///
/// `self_id` is the authoritative node id for this process — the value that
/// was passed to `Raft::new()` via `ClusterConfig::node_id`, and the same
/// value that `GET /api/v1/cluster/status` reports. Using this explicitly
/// (rather than re-reading `raft.metrics().borrow().id`) closes G15: the
/// watch-channel id can diverge from the configured node id during openraft
/// initialization / metric-update races, which made `handle_add_node` reject
/// valid joins on the actual Raft leader.
///
/// `transfer_provider` is plugged in by the engine to service
/// `PartitionTransferRequest` streams (CL-6a). `poh_provider` services
/// `PohChainTransferRequest` streams (CL-6b). `cutover_coordinator`
/// services `PartitionCutoverRequest` streams (CL-6c) — the hook that
/// drains and freezes a partition on the source node when the target
/// initiates handover. Pass `None` in contexts that don't own a live L2
/// body store / PoH chain / partition-write path (tests, raft-only
/// benches) — the relevant requests will receive a failure response.
///
/// This variant captures the providers at spawn time — late installation
/// after the loop starts is invisible here. For production where the
/// engine wires providers post-bootstrap, use [`serve_with_slots`].
pub async fn serve(
    endpoint: Arc<QuicEndpoint>,
    raft: Raft<AeonRaftConfig>,
    self_id: NodeId,
    shutdown: Arc<AtomicBool>,
    transfer_provider: Option<Arc<dyn PartitionTransferProvider>>,
    poh_provider: Option<Arc<dyn PohChainProvider>>,
    cutover_coordinator: Option<Arc<dyn CutoverCoordinator>>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        let incoming = tokio::select! {
            incoming = endpoint.accept() => {
                match incoming {
                    Some(i) => i,
                    None => break, // Endpoint closed
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => continue,
        };

        let raft = raft.clone();
        let transfer_provider = transfer_provider.clone();
        let poh_provider = poh_provider.clone();
        let cutover_coordinator = cutover_coordinator.clone();
        tokio::spawn(async move {
            let connection = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("QUIC handshake failed: {e}");
                    return;
                }
            };

            loop {
                let stream = match connection.accept_bi().await {
                    Ok(s) => s,
                    Err(quinn::ConnectionError::ApplicationClosed(_)) => break,
                    Err(e) => {
                        tracing::debug!("QUIC stream accept error: {e}");
                        break;
                    }
                };

                let raft = raft.clone();
                let transfer_provider = transfer_provider.clone();
                let poh_provider = poh_provider.clone();
                let cutover_coordinator = cutover_coordinator.clone();
                tokio::spawn(handle_stream(
                    raft,
                    self_id,
                    transfer_provider,
                    poh_provider,
                    cutover_coordinator,
                    stream,
                ));
            }
        });
    }
}

/// Handle a single QUIC bidirectional stream.
async fn handle_stream(
    raft: Raft<AeonRaftConfig>,
    self_id: NodeId,
    transfer_provider: Option<Arc<dyn PartitionTransferProvider>>,
    poh_provider: Option<Arc<dyn PohChainProvider>>,
    cutover_coordinator: Option<Arc<dyn CutoverCoordinator>>,
    (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
) {
    // Peek the first frame's type so partition-transfer streams (which
    // start with a request frame + continue reading from the same recv)
    // can re-use the open stream without a second read_frame call.
    let (msg_type, payload) = match framing::read_frame(&mut recv).await {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!("failed to read frame: {e}");
            return;
        }
    };

    let result = match msg_type {
        MessageType::AppendEntries => handle_append_entries(&raft, &payload, &mut send).await,
        MessageType::Vote => handle_vote(&raft, &payload, &mut send).await,
        MessageType::InstallSnapshot => handle_install_snapshot(&raft, &payload, &mut send).await,
        MessageType::FullSnapshot => handle_full_snapshot(&raft, &payload, &mut send).await,
        MessageType::AddNodeRequest => handle_add_node(&raft, self_id, &payload, &mut send).await,
        MessageType::RemoveNodeRequest => {
            handle_remove_node(&raft, self_id, &payload, &mut send).await
        }
        MessageType::HealthPing => health::handle_health_ping(self_id, &payload, &mut send).await,
        MessageType::PartitionTransferRequest => {
            handle_partition_transfer(transfer_provider.as_deref(), payload, send).await
        }
        MessageType::PohChainTransferRequest => {
            handle_poh_chain_transfer(poh_provider.as_deref(), payload, send).await
        }
        MessageType::PartitionCutoverRequest => {
            handle_partition_cutover(cutover_coordinator.as_deref(), payload, send).await
        }
        MessageType::ProposeForwardRequest => {
            handle_propose_forward(&raft, &payload, &mut send).await
        }
        _ => {
            tracing::warn!("unexpected message type: {:?}", msg_type);
            Ok(())
        }
    };

    if let Err(e) = result {
        tracing::debug!("RPC handler error: {e}");
    }
}

/// Dispatch a `PartitionTransferRequest`.
///
/// The request frame was already consumed by `handle_stream`; deserialize
/// it and hand off to `serve_partition_transfer_with_request`. If no
/// provider is wired (raft-only contexts like tests/benches), reply with
/// a failure end frame so the client surfaces a meaningful error.
async fn handle_partition_transfer(
    provider: Option<&dyn PartitionTransferProvider>,
    request_payload: Vec<u8>,
    mut send: quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let provider = match provider {
        Some(p) => p,
        None => {
            let end = PartitionTransferEnd {
                success: false,
                message: "node has no partition-transfer provider configured".to_string(),
            };
            let b = bincode::serialize(&end).map_err(|e| aeon_types::AeonError::Serialization {
                message: format!("serialize PartitionTransferEnd: {e}"),
                source: None,
            })?;
            framing::write_frame(&mut send, MessageType::PartitionTransferEndFrame, &b).await?;
            let _ = send.finish();
            return Ok(());
        }
    };

    let req: PartitionTransferRequest = bincode::deserialize(&request_payload).map_err(|e| {
        aeon_types::AeonError::Serialization {
            message: format!("deserialize PartitionTransferRequest: {e}"),
            source: None,
        }
    })?;
    serve_partition_transfer_with_request(provider, send, &req).await
}

/// Dispatch a `PohChainTransferRequest`.
///
/// Same shape as `handle_partition_transfer`: the request frame was
/// already consumed by `handle_stream`; deserialize it and hand off to
/// `serve_poh_chain_transfer_with_request`. If no provider is wired,
/// reply with `success = false` so the client surfaces a meaningful
/// error instead of hanging on a half-closed stream.
async fn handle_poh_chain_transfer(
    provider: Option<&dyn PohChainProvider>,
    request_payload: Vec<u8>,
    mut send: quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let provider = match provider {
        Some(p) => p,
        None => {
            let resp = PohChainTransferResponse {
                success: false,
                state_bytes: Vec::new(),
                message: "node has no poh-chain provider configured".to_string(),
            };
            let b =
                bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
                    message: format!("serialize PohChainTransferResponse: {e}"),
                    source: None,
                })?;
            framing::write_frame(&mut send, MessageType::PohChainTransferResponse, &b).await?;
            let _ = send.finish();
            return Ok(());
        }
    };

    let req: PohChainTransferRequest = bincode::deserialize(&request_payload).map_err(|e| {
        aeon_types::AeonError::Serialization {
            message: format!("deserialize PohChainTransferRequest: {e}"),
            source: None,
        }
    })?;
    serve_poh_chain_transfer_with_request(provider, send, &req).await
}

/// Dispatch a `PartitionCutoverRequest`.
///
/// Same shape as the partition/PoH transfer handlers: the request frame
/// was already consumed by `handle_stream`; deserialize it and hand off
/// to `serve_partition_cutover_with_request`. If no coordinator is
/// wired, reply with `success = false` so the client surfaces a
/// meaningful error.
async fn handle_partition_cutover(
    coordinator: Option<&dyn CutoverCoordinator>,
    request_payload: Vec<u8>,
    mut send: quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let coordinator = match coordinator {
        Some(c) => c,
        None => {
            let resp = PartitionCutoverResponse {
                success: false,
                final_source_offset: -1,
                final_poh_sequence: 0,
                message: "node has no cutover coordinator configured".to_string(),
            };
            let b =
                bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
                    message: format!("serialize PartitionCutoverResponse: {e}"),
                    source: None,
                })?;
            framing::write_frame(&mut send, MessageType::PartitionCutoverResponse, &b).await?;
            let _ = send.finish();
            return Ok(());
        }
    };

    let req: PartitionCutoverRequest = bincode::deserialize(&request_payload).map_err(|e| {
        aeon_types::AeonError::Serialization {
            message: format!("deserialize PartitionCutoverRequest: {e}"),
            source: None,
        }
    })?;
    serve_partition_cutover_with_request(coordinator, send, &req).await
}

async fn handle_append_entries(
    raft: &Raft<AeonRaftConfig>,
    payload: &[u8],
    send: &mut quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let req: openraft::raft::AppendEntriesRequest<AeonRaftConfig> =
        bincode::deserialize(payload).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("deserialize AppendEntries: {e}"),
            source: None,
        })?;

    let resp = raft
        .append_entries(req)
        .await
        .map_err(|e| aeon_types::AeonError::Cluster {
            message: format!("AppendEntries failed: {e}"),
            source: None,
        })?;

    let resp_bytes =
        bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("serialize AppendEntriesResponse: {e}"),
            source: None,
        })?;

    framing::write_frame(send, MessageType::AppendEntriesResponse, &resp_bytes).await?;
    let _ = send.finish();
    Ok(())
}

async fn handle_vote(
    raft: &Raft<AeonRaftConfig>,
    payload: &[u8],
    send: &mut quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let req: openraft::raft::VoteRequest<u64> =
        bincode::deserialize(payload).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("deserialize Vote: {e}"),
            source: None,
        })?;

    let resp = raft
        .vote(req)
        .await
        .map_err(|e| aeon_types::AeonError::Cluster {
            message: format!("Vote failed: {e}"),
            source: None,
        })?;

    let resp_bytes =
        bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("serialize VoteResponse: {e}"),
            source: None,
        })?;

    framing::write_frame(send, MessageType::VoteResponse, &resp_bytes).await?;
    let _ = send.finish();
    Ok(())
}

async fn handle_install_snapshot(
    raft: &Raft<AeonRaftConfig>,
    payload: &[u8],
    send: &mut quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let req: openraft::raft::InstallSnapshotRequest<AeonRaftConfig> = bincode::deserialize(payload)
        .map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("deserialize InstallSnapshot: {e}"),
            source: None,
        })?;

    let resp = raft
        .install_snapshot(req)
        .await
        .map_err(|e| aeon_types::AeonError::Cluster {
            message: format!("InstallSnapshot failed: {e}"),
            source: None,
        })?;

    let resp_bytes =
        bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("serialize InstallSnapshotResponse: {e}"),
            source: None,
        })?;

    framing::write_frame(send, MessageType::InstallSnapshotResponse, &resp_bytes).await?;
    let _ = send.finish();
    Ok(())
}

async fn handle_full_snapshot(
    raft: &Raft<AeonRaftConfig>,
    payload: &[u8],
    send: &mut quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    use std::io::Cursor;

    let (vote, meta, data): (
        openraft::Vote<u64>,
        openraft::storage::SnapshotMeta<u64, crate::types::NodeAddress>,
        Vec<u8>,
    ) = bincode::deserialize(payload).map_err(|e| aeon_types::AeonError::Serialization {
        message: format!("deserialize FullSnapshot: {e}"),
        source: None,
    })?;

    let snapshot = openraft::storage::Snapshot {
        meta,
        snapshot: Box::new(Cursor::new(data)),
    };

    let resp = raft
        .install_full_snapshot(vote, snapshot)
        .await
        .map_err(|e| aeon_types::AeonError::Cluster {
            message: format!("FullSnapshot install failed: {e}"),
            source: None,
        })?;

    let resp_bytes =
        bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("serialize SnapshotResponse: {e}"),
            source: None,
        })?;

    framing::write_frame(send, MessageType::FullSnapshotResponse, &resp_bytes).await?;
    let _ = send.finish();
    Ok(())
}

/// Handle a join request — add the requesting node as learner then promote to voter.
///
/// `self_id` is the configured `ClusterConfig::node_id` (same value REST
/// `/api/v1/cluster/status` reports). G15: we used to compare against
/// `raft.metrics().borrow().id`, but on a multi-node cluster that value can
/// lag or differ from the configured id, causing the leader itself to
/// reject valid joins with `"not the leader"` — breaking STS scale-up.
async fn handle_add_node(
    raft: &Raft<AeonRaftConfig>,
    self_id: NodeId,
    payload: &[u8],
    send: &mut quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let req: JoinRequest =
        bincode::deserialize(payload).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("deserialize JoinRequest: {e}"),
            source: None,
        })?;

    tracing::info!(node_id = req.node_id, addr = %req.addr, "received AddNodeRequest");

    let leader_id = raft.current_leader().await;

    if leader_id != Some(self_id) {
        let metrics_id = raft.metrics().borrow().id;
        if metrics_id != self_id {
            tracing::warn!(
                configured_id = self_id,
                metrics_id,
                leader_id = ?leader_id,
                "G15 diagnostic: raft.metrics().id diverges from ClusterConfig::node_id on AddNode reject"
            );
        }
        let resp = JoinResponse {
            success: false,
            leader_id,
            message: format!("not the leader; current leader is {:?}", leader_id),
        };
        let resp_bytes =
            bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
                message: format!("serialize JoinResponse: {e}"),
                source: None,
            })?;
        framing::write_frame(send, MessageType::AddNodeResponse, &resp_bytes).await?;
        let _ = send.finish();
        return Ok(());
    }

    // Step 1: Add as learner (blocking = true → waits for log catch-up)
    if let Err(e) = raft.add_learner(req.node_id, req.addr.clone(), true).await {
        let resp = JoinResponse {
            success: false,
            leader_id: Some(self_id),
            message: format!("failed to add learner: {e}"),
        };
        let resp_bytes =
            bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
                message: format!("serialize JoinResponse: {e}"),
                source: None,
            })?;
        framing::write_frame(send, MessageType::AddNodeResponse, &resp_bytes).await?;
        let _ = send.finish();
        return Ok(());
    }

    tracing::info!(node_id = req.node_id, "learner added, promoting to voter");

    // Step 2: Promote to voter
    let mut voters = std::collections::BTreeSet::new();
    voters.insert(req.node_id);
    if let Err(e) = raft
        .change_membership(openraft::ChangeMembers::AddVoterIds(voters), false)
        .await
    {
        let resp = JoinResponse {
            success: false,
            leader_id: Some(self_id),
            message: format!("learner added but promotion failed: {e}"),
        };
        let resp_bytes =
            bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
                message: format!("serialize JoinResponse: {e}"),
                source: None,
            })?;
        framing::write_frame(send, MessageType::AddNodeResponse, &resp_bytes).await?;
        let _ = send.finish();
        return Ok(());
    }

    tracing::info!(
        node_id = req.node_id,
        "node promoted to voter — join complete"
    );

    let resp = JoinResponse {
        success: true,
        leader_id: Some(self_id),
        message: "node added and promoted to voter".to_string(),
    };
    let resp_bytes =
        bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("serialize JoinResponse: {e}"),
            source: None,
        })?;
    framing::write_frame(send, MessageType::AddNodeResponse, &resp_bytes).await?;
    let _ = send.finish();
    Ok(())
}

/// Handle a remove-node request — demote from voter then remove from cluster.
async fn handle_remove_node(
    raft: &Raft<AeonRaftConfig>,
    self_id: NodeId,
    payload: &[u8],
    send: &mut quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let req: RemoveNodeRequest =
        bincode::deserialize(payload).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("deserialize RemoveNodeRequest: {e}"),
            source: None,
        })?;

    tracing::info!(node_id = req.node_id, "received RemoveNodeRequest");

    let leader_id = raft.current_leader().await;

    if leader_id != Some(self_id) {
        let metrics_id = raft.metrics().borrow().id;
        if metrics_id != self_id {
            tracing::warn!(
                configured_id = self_id,
                metrics_id,
                leader_id = ?leader_id,
                "G15 diagnostic: raft.metrics().id diverges from ClusterConfig::node_id on RemoveNode reject"
            );
        }
        let resp = RemoveNodeResponse {
            success: false,
            leader_id,
            message: format!("not the leader; current leader is {:?}", leader_id),
        };
        let resp_bytes =
            bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
                message: format!("serialize RemoveNodeResponse: {e}"),
                source: None,
            })?;
        framing::write_frame(send, MessageType::RemoveNodeResponse, &resp_bytes).await?;
        let _ = send.finish();
        return Ok(());
    }

    // Cannot remove self (leader) — transfer leadership first
    if req.node_id == self_id {
        let resp = RemoveNodeResponse {
            success: false,
            leader_id: Some(self_id),
            message: "cannot remove the leader node; transfer leadership first".to_string(),
        };
        let resp_bytes =
            bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
                message: format!("serialize RemoveNodeResponse: {e}"),
                source: None,
            })?;
        framing::write_frame(send, MessageType::RemoveNodeResponse, &resp_bytes).await?;
        let _ = send.finish();
        return Ok(());
    }

    // Step 1: Demote from voter
    let mut to_remove = std::collections::BTreeSet::new();
    to_remove.insert(req.node_id);
    if let Err(e) = raft
        .change_membership(
            openraft::ChangeMembers::RemoveVoters(to_remove.clone()),
            false,
        )
        .await
    {
        let resp = RemoveNodeResponse {
            success: false,
            leader_id: Some(self_id),
            message: format!("failed to demote voter: {e}"),
        };
        let resp_bytes =
            bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
                message: format!("serialize RemoveNodeResponse: {e}"),
                source: None,
            })?;
        framing::write_frame(send, MessageType::RemoveNodeResponse, &resp_bytes).await?;
        let _ = send.finish();
        return Ok(());
    }

    // Step 2: Remove from cluster entirely
    if let Err(e) = raft
        .change_membership(openraft::ChangeMembers::RemoveNodes(to_remove), false)
        .await
    {
        let resp = RemoveNodeResponse {
            success: false,
            leader_id: Some(self_id),
            message: format!("voter demoted but node removal failed: {e}"),
        };
        let resp_bytes =
            bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
                message: format!("serialize RemoveNodeResponse: {e}"),
                source: None,
            })?;
        framing::write_frame(send, MessageType::RemoveNodeResponse, &resp_bytes).await?;
        let _ = send.finish();
        return Ok(());
    }

    tracing::info!(node_id = req.node_id, "node removed from cluster");

    let resp = RemoveNodeResponse {
        success: true,
        leader_id: Some(self_id),
        message: "node removed from cluster".to_string(),
    };
    let resp_bytes =
        bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("serialize RemoveNodeResponse: {e}"),
            source: None,
        })?;
    framing::write_frame(send, MessageType::RemoveNodeResponse, &resp_bytes).await?;
    let _ = send.finish();
    Ok(())
}

/// Handle a forwarded Raft proposal.
///
/// The follower sent us a bincode-serialized `ClusterRequest`; we
/// deserialize it, run `raft.client_write` locally, and reply with the
/// bincode-serialized `ClusterResponse`. If propose fails (this node is
/// also no longer the leader, or openraft surfaces some other error)
/// we set `success = false` and stuff the error into `message` so the
/// caller can decide whether to retry against the new leader.
async fn handle_propose_forward(
    raft: &Raft<AeonRaftConfig>,
    payload: &[u8],
    send: &mut quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let req: ProposeForwardRequest =
        bincode::deserialize(payload).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("deserialize ProposeForwardRequest: {e}"),
            source: None,
        })?;

    let cluster_req: crate::types::ClusterRequest = bincode::deserialize(&req.request_bytes)
        .map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("deserialize forwarded ClusterRequest payload: {e}"),
            source: None,
        })?;

    let resp = match raft.client_write(cluster_req).await {
        Ok(write_resp) => {
            let bytes = bincode::serialize(&write_resp.data).map_err(|e| {
                aeon_types::AeonError::Serialization {
                    message: format!("serialize forwarded ClusterResponse: {e}"),
                    source: None,
                }
            })?;
            ProposeForwardResponse {
                success: true,
                response_bytes: bytes,
                message: String::new(),
            }
        }
        Err(e) => {
            // Includes the case where this node also stopped being the
            // leader between dispatch and propose. Surface the openraft
            // error verbatim — the caller can grep for "ForwardToLeader"
            // and re-resolve if needed.
            ProposeForwardResponse {
                success: false,
                response_bytes: Vec::new(),
                message: format!("client_write failed: {e}"),
            }
        }
    };

    let resp_bytes =
        bincode::serialize(&resp).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("serialize ProposeForwardResponse: {e}"),
            source: None,
        })?;
    framing::write_frame(send, MessageType::ProposeForwardResponse, &resp_bytes).await?;
    let _ = send.finish();
    Ok(())
}
