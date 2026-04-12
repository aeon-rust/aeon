//! Inbound QUIC handler — accepts connections, dispatches Raft RPCs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use openraft::Raft;

use crate::raft_config::AeonRaftConfig;
use crate::transport::endpoint::QuicEndpoint;
use crate::transport::framing::{self, MessageType};
use crate::transport::health;
use crate::types::{JoinRequest, JoinResponse, NodeId, RemoveNodeRequest, RemoveNodeResponse};

/// Run the QUIC server accept loop, dispatching incoming RPCs to the Raft node.
pub async fn serve(
    endpoint: Arc<QuicEndpoint>,
    raft: Raft<AeonRaftConfig>,
    shutdown: Arc<AtomicBool>,
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
        tokio::spawn(async move {
            let connection = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("QUIC handshake failed: {e}");
                    return;
                }
            };

            let self_id: NodeId = raft.metrics().borrow().id;
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
                tokio::spawn(handle_stream(raft, self_id, stream));
            }
        });
    }
}

/// Handle a single QUIC bidirectional stream.
async fn handle_stream(
    raft: Raft<AeonRaftConfig>,
    self_id: NodeId,
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
        MessageType::AddNodeRequest => handle_add_node(&raft, &payload, &mut send).await,
        MessageType::RemoveNodeRequest => handle_remove_node(&raft, &payload, &mut send).await,
        MessageType::HealthPing => health::handle_health_ping(self_id, &payload, &mut send).await,
        _ => {
            tracing::warn!("unexpected message type: {:?}", msg_type);
            Ok(())
        }
    };

    if let Err(e) = result {
        tracing::debug!("RPC handler error: {e}");
    }
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
async fn handle_add_node(
    raft: &Raft<AeonRaftConfig>,
    payload: &[u8],
    send: &mut quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let req: JoinRequest =
        bincode::deserialize(payload).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("deserialize JoinRequest: {e}"),
            source: None,
        })?;

    tracing::info!(node_id = req.node_id, addr = %req.addr, "received AddNodeRequest");

    // Check if we are the leader; if not, redirect
    let leader_id = raft.current_leader().await;
    let my_id = raft.metrics().borrow().id;

    if leader_id != Some(my_id) {
        let resp = JoinResponse {
            success: false,
            leader_id,
            message: format!(
                "not the leader; current leader is {:?}",
                leader_id
            ),
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
            leader_id: Some(my_id),
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
            leader_id: Some(my_id),
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

    tracing::info!(node_id = req.node_id, "node promoted to voter — join complete");

    let resp = JoinResponse {
        success: true,
        leader_id: Some(my_id),
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
    payload: &[u8],
    send: &mut quinn::SendStream,
) -> Result<(), aeon_types::AeonError> {
    let req: RemoveNodeRequest =
        bincode::deserialize(payload).map_err(|e| aeon_types::AeonError::Serialization {
            message: format!("deserialize RemoveNodeRequest: {e}"),
            source: None,
        })?;

    tracing::info!(node_id = req.node_id, "received RemoveNodeRequest");

    // Check if we are the leader
    let leader_id = raft.current_leader().await;
    let my_id = raft.metrics().borrow().id;

    if leader_id != Some(my_id) {
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
    if req.node_id == my_id {
        let resp = RemoveNodeResponse {
            success: false,
            leader_id: Some(my_id),
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
            leader_id: Some(my_id),
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
            leader_id: Some(my_id),
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
        leader_id: Some(my_id),
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
