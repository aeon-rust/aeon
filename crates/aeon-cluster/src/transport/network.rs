//! RaftNetwork implementation over QUIC streams.

use std::sync::Arc;

use openraft::Vote;
use openraft::error::{
    InstallSnapshotError, NetworkError, RPCError, RaftError, ReplicationClosed, StreamingError,
    Unreachable,
};
use openraft::network::RPCOption;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::storage::Snapshot;

use aeon_types::AeonError;

use crate::raft_config::AeonRaftConfig;
use crate::transport::endpoint::QuicEndpoint;
use crate::transport::framing::{self, MessageType};
use crate::types::{JoinRequest, JoinResponse, NodeAddress, NodeId, RemoveNodeRequest, RemoveNodeResponse};

/// Factory that creates QUIC-based RaftNetwork connections.
pub struct QuicNetworkFactory {
    endpoint: Arc<QuicEndpoint>,
}

impl QuicNetworkFactory {
    pub fn new(endpoint: Arc<QuicEndpoint>) -> Self {
        Self { endpoint }
    }
}

impl openraft::RaftNetworkFactory<AeonRaftConfig> for QuicNetworkFactory {
    type Network = QuicNetworkConnection;

    async fn new_client(&mut self, target: NodeId, node: &NodeAddress) -> Self::Network {
        QuicNetworkConnection {
            target,
            target_addr: node.clone(),
            endpoint: Arc::clone(&self.endpoint),
        }
    }
}

/// A QUIC-based RaftNetwork connection to a single remote node.
pub struct QuicNetworkConnection {
    target: NodeId,
    target_addr: NodeAddress,
    endpoint: Arc<QuicEndpoint>,
}

impl QuicNetworkConnection {
    /// Send a bincode-serialized request and receive a response over a QUIC bi-stream.
    async fn rpc<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
        &mut self,
        msg_type: MessageType,
        resp_type: MessageType,
        request: &Req,
    ) -> Result<Resp, NetworkError> {
        let conn = self
            .endpoint
            .connect(self.target, &self.target_addr)
            .await
            .map_err(|e| NetworkError::new(&std::io::Error::other(e.to_string())))?;

        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| {
            NetworkError::new(&std::io::Error::other(format!(
                "failed to open QUIC stream: {e}"
            )))
        })?;

        // Serialize and send request
        let payload = bincode::serialize(request)
            .map_err(|e| NetworkError::new(&std::io::Error::other(e.to_string())))?;

        framing::write_frame(&mut send, msg_type, &payload)
            .await
            .map_err(|e| NetworkError::new(&std::io::Error::other(e.to_string())))?;

        send.finish().map_err(|e| {
            NetworkError::new(&std::io::Error::other(format!(
                "failed to finish QUIC send: {e}"
            )))
        })?;

        // Read response
        let (recv_type, resp_payload) = framing::read_frame(&mut recv)
            .await
            .map_err(|e| NetworkError::new(&std::io::Error::other(e.to_string())))?;

        if recv_type != resp_type {
            return Err(NetworkError::new(&std::io::Error::other(format!(
                "unexpected response type: {:?} (expected {:?})",
                recv_type, resp_type
            ))));
        }

        let response: Resp = bincode::deserialize(&resp_payload)
            .map_err(|e| NetworkError::new(&std::io::Error::other(e.to_string())))?;

        Ok(response)
    }
}

impl openraft::RaftNetwork<AeonRaftConfig> for QuicNetworkConnection {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<AeonRaftConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, NodeAddress, RaftError<u64>>> {
        self.rpc(
            MessageType::AppendEntries,
            MessageType::AppendEntriesResponse,
            &rpc,
        )
        .await
        .map_err(RPCError::Network)
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote<u64>,
        snapshot: Snapshot<AeonRaftConfig>,
        _cancel: impl std::future::Future<Output = ReplicationClosed> + Send + 'static,
        _option: RPCOption,
    ) -> Result<SnapshotResponse<u64>, StreamingError<AeonRaftConfig, openraft::error::Fatal<u64>>>
    {
        // Serialize vote + snapshot meta + snapshot data together
        let snap_data = snapshot.snapshot.into_inner();
        let full = (vote, &snapshot.meta, &snap_data);
        let payload = bincode::serialize(&full).map_err(|e| {
            StreamingError::Unreachable(Unreachable::new(&std::io::Error::other(e.to_string())))
        })?;

        let conn = self
            .endpoint
            .connect(self.target, &self.target_addr)
            .await
            .map_err(|e| {
                StreamingError::Unreachable(Unreachable::new(&std::io::Error::other(e.to_string())))
            })?;

        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| {
            StreamingError::Unreachable(Unreachable::new(&std::io::Error::other(format!(
                "failed to open QUIC stream: {e}"
            ))))
        })?;

        framing::write_frame(&mut send, MessageType::FullSnapshot, &payload)
            .await
            .map_err(|e| {
                StreamingError::Unreachable(Unreachable::new(&std::io::Error::other(e.to_string())))
            })?;

        send.finish().map_err(|e| {
            StreamingError::Unreachable(Unreachable::new(&std::io::Error::other(format!(
                "failed to finish send: {e}"
            ))))
        })?;

        let (_, resp_payload) = framing::read_frame(&mut recv).await.map_err(|e| {
            StreamingError::Unreachable(Unreachable::new(&std::io::Error::other(e.to_string())))
        })?;

        let response: SnapshotResponse<u64> = bincode::deserialize(&resp_payload).map_err(|e| {
            StreamingError::Unreachable(Unreachable::new(&std::io::Error::other(e.to_string())))
        })?;

        Ok(response)
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<AeonRaftConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, NodeAddress, RaftError<u64, InstallSnapshotError>>,
    > {
        self.rpc(
            MessageType::InstallSnapshot,
            MessageType::InstallSnapshotResponse,
            &rpc,
        )
        .await
        .map_err(RPCError::Network)
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, NodeAddress, RaftError<u64>>> {
        self.rpc(MessageType::Vote, MessageType::VoteResponse, &rpc)
            .await
            .map_err(RPCError::Network)
    }
}

// ── Public cluster management RPC helpers ───────────────────────────

/// Send a join request to a seed node and return the response.
///
/// The seed may not be the leader — check `JoinResponse::leader_id` and
/// retry against the actual leader if needed.
pub async fn send_join_request(
    endpoint: &QuicEndpoint,
    _seed_id: NodeId,
    seed_addr: &NodeAddress,
    request: &JoinRequest,
) -> Result<JoinResponse, AeonError> {
    // G16: join RPCs must NOT go through the pooled `connect()` path. Seed
    // ids are unknown at this point (callers pass a placeholder `0`), so
    // pooling would cache the first seed's connection under key 0 and every
    // subsequent seed attempt in the loop would silently reuse it — meaning
    // `retry against aeon-1` actually re-hits aeon-0. The `_seed_id`
    // parameter is kept for callsite-readability; the connection itself is
    // opened uncached so each seed really gets its own QUIC handshake.
    cluster_rpc_uncached(
        endpoint,
        seed_addr,
        MessageType::AddNodeRequest,
        MessageType::AddNodeResponse,
        request,
    )
    .await
}

/// Send a remove-node request to the leader and return the response.
pub async fn send_remove_request(
    endpoint: &QuicEndpoint,
    leader_id: NodeId,
    leader_addr: &NodeAddress,
    request: &RemoveNodeRequest,
) -> Result<RemoveNodeResponse, AeonError> {
    cluster_rpc(
        endpoint,
        leader_id,
        leader_addr,
        MessageType::RemoveNodeRequest,
        MessageType::RemoveNodeResponse,
        request,
    )
    .await
}

/// Generic cluster management RPC over a fresh, uncached QUIC connection.
///
/// G16: the pooled `cluster_rpc` is keyed by `NodeId`, which makes it unsafe
/// for flows (like seed-join) where the caller does not know the target's
/// real id and must fan out across multiple addresses. This variant opens a
/// fresh handshake per call and lets the connection drop when the RPC
/// returns.
async fn cluster_rpc_uncached<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
    endpoint: &QuicEndpoint,
    target_addr: &NodeAddress,
    msg_type: MessageType,
    resp_type: MessageType,
    request: &Req,
) -> Result<Resp, AeonError> {
    let conn = endpoint
        .connect_uncached(target_addr)
        .await
        .map_err(|e| AeonError::Connection {
            message: format!("failed to connect to {target_addr}: {e}"),
            source: None,
            retryable: true,
        })?;

    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| AeonError::Connection {
        message: format!("failed to open QUIC stream to {target_addr}: {e}"),
        source: None,
        retryable: true,
    })?;

    let payload =
        bincode::serialize(request).map_err(|e| AeonError::Serialization {
            message: format!("serialize cluster RPC request: {e}"),
            source: None,
        })?;

    framing::write_frame(&mut send, msg_type, &payload).await?;
    send.finish().map_err(|e| AeonError::Connection {
        message: format!("failed to finish QUIC send: {e}"),
        source: None,
        retryable: false,
    })?;

    let (recv_type, resp_payload) = framing::read_frame(&mut recv).await?;

    if recv_type != resp_type {
        return Err(AeonError::Connection {
            message: format!(
                "unexpected response type: {:?} (expected {:?})",
                recv_type, resp_type
            ),
            source: None,
            retryable: false,
        });
    }

    let response: Resp = bincode::deserialize(&resp_payload).map_err(|e| {
        AeonError::Serialization {
            message: format!("deserialize cluster RPC response: {e}"),
            source: None,
        }
    })?;

    Ok(response)
}

/// Generic cluster management RPC: serialize request → QUIC stream → deserialize response.
async fn cluster_rpc<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
    endpoint: &QuicEndpoint,
    target_id: NodeId,
    target_addr: &NodeAddress,
    msg_type: MessageType,
    resp_type: MessageType,
    request: &Req,
) -> Result<Resp, AeonError> {
    let conn = endpoint
        .connect(target_id, target_addr)
        .await
        .map_err(|e| AeonError::Connection {
            message: format!("failed to connect to node {target_id} at {target_addr}: {e}"),
            source: None,
            retryable: true,
        })?;

    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| AeonError::Connection {
        message: format!("failed to open QUIC stream to node {target_id}: {e}"),
        source: None,
        retryable: true,
    })?;

    let payload =
        bincode::serialize(request).map_err(|e| AeonError::Serialization {
            message: format!("serialize cluster RPC request: {e}"),
            source: None,
        })?;

    framing::write_frame(&mut send, msg_type, &payload).await?;
    send.finish().map_err(|e| AeonError::Connection {
        message: format!("failed to finish QUIC send: {e}"),
        source: None,
        retryable: false,
    })?;

    let (recv_type, resp_payload) = framing::read_frame(&mut recv).await?;

    if recv_type != resp_type {
        return Err(AeonError::Connection {
            message: format!(
                "unexpected response type: {:?} (expected {:?})",
                recv_type, resp_type
            ),
            source: None,
            retryable: false,
        });
    }

    let response: Resp = bincode::deserialize(&resp_payload).map_err(|e| {
        AeonError::Serialization {
            message: format!("deserialize cluster RPC response: {e}"),
            source: None,
        }
    })?;

    Ok(response)
}
