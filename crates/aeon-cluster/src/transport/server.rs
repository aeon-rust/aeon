//! Inbound QUIC handler — accepts connections, dispatches Raft RPCs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use openraft::Raft;

use crate::raft_config::AeonRaftConfig;
use crate::transport::endpoint::QuicEndpoint;
use crate::transport::framing::{self, MessageType};

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
                tokio::spawn(handle_stream(raft, stream));
            }
        });
    }
}

/// Handle a single QUIC bidirectional stream.
async fn handle_stream(
    raft: Raft<AeonRaftConfig>,
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
