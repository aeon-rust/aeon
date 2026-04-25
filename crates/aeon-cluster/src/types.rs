//! Core cluster types: node identity, partition ownership, Raft log entries.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use aeon_types::{AeonError, PartitionId, RegistryCommand, RegistryResponse};
use serde::{Deserialize, Serialize};

/// Unique identifier for a cluster node.
pub type NodeId = u64;

/// Network address of a cluster node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeAddress {
    pub host: String,
    pub port: u16,
}

impl NodeAddress {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// Parse into a SocketAddr (resolves DNS if needed at call site).
    pub fn to_socket_addr(&self) -> Result<SocketAddr, std::net::AddrParseError> {
        format!("{}:{}", self.host, self.port).parse()
    }
}

impl fmt::Display for NodeAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

impl FromStr for NodeAddress {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (host, port_str) = s
            .rsplit_once(':')
            .ok_or_else(|| format!("missing ':port' in address: {s}"))?;
        let port: u16 = port_str
            .parse()
            .map_err(|e| format!("invalid port '{port_str}': {e}"))?;
        if host.is_empty() {
            return Err("empty host in address".to_string());
        }
        Ok(Self::new(host, port))
    }
}

/// Outcome of a caller-initiated partition transfer attempt.
///
/// Returned by [`crate::ClusterNode::propose_partition_transfer`] so REST and
/// CLI callers can map each business-logic outcome to a distinct HTTP status
/// or exit code without inspecting free-form error strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferStatus {
    /// `BeginTransfer` was accepted and the partition is now in the
    /// `Transferring { source, target }` state on the committed Raft log.
    Accepted { source: NodeId, target: NodeId },
    /// This node is not the Raft leader. Callers should retry against
    /// `current_leader` (if present) or discover the leader via cluster status.
    NotLeader { current_leader: Option<NodeId> },
    /// Requested partition is not present in the partition table.
    UnknownPartition,
    /// The partition already has a transfer in flight.
    AlreadyTransferring { source: NodeId, target: NodeId },
    /// Target equals current owner — nothing to do.
    NoChange { owner: NodeId },
    /// The state-machine rejected the `BeginTransfer` (e.g. racy apply).
    Rejected(String),
}

/// Raft log entry payload — what gets replicated across the cluster.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClusterRequest {
    /// Assign a partition to a node.
    AssignPartition {
        partition: PartitionId,
        node: NodeId,
    },
    /// Begin two-phase transfer of a partition.
    BeginTransfer {
        partition: PartitionId,
        source: NodeId,
        target: NodeId,
    },
    /// Complete a transfer — target now owns the partition.
    CompleteTransfer {
        partition: PartitionId,
        new_owner: NodeId,
    },
    /// Abort an in-progress transfer — revert to source ownership.
    AbortTransfer {
        partition: PartitionId,
        revert_to: NodeId,
    },
    /// Update a cluster-wide configuration key.
    UpdateConfig { key: String, value: String },
    /// Submit checkpoint source offsets for Raft replication.
    /// Persists the per-partition source-anchor offsets in the cluster state
    /// so that any node can resume from the checkpointed position after failover.
    SubmitCheckpoint {
        /// Per-partition source offsets at checkpoint time (partition_id_u16 → offset).
        source_offsets: std::collections::HashMap<u16, i64>,
    },
    /// Rebalance partitions across the given set of active nodes.
    /// The state machine computes moves internally using `compute_rebalance`
    /// and applies them atomically. This ensures the rebalance is deterministic
    /// and identical on all Raft replicas.
    RebalancePartitions { nodes: Vec<NodeId> },
    /// Assign partitions using initial round-robin distribution.
    /// Used during multi-node bootstrap when no partitions are assigned yet.
    InitialAssignment {
        num_partitions: u16,
        nodes: Vec<NodeId>,
    },
    /// Replicate a registry / pipeline-manager command across the cluster.
    ///
    /// `payload` is a bincode-serialized `aeon_types::RegistryCommand`. The
    /// state machine deserializes and hands it to the node's registered
    /// `RegistryApplier` at apply time. Carrying the command as bytes keeps
    /// `ClusterRequest` free of derives that `RegistryCommand`'s nested types
    /// (notably `f64` thresholds) cannot satisfy.
    Registry { payload: Vec<u8> },
}

impl ClusterRequest {
    /// Wrap a `RegistryCommand` into a `ClusterRequest::Registry`.
    ///
    /// Uses JSON encoding rather than bincode: `RegistryCommand` is an
    /// internally-tagged enum (`#[serde(tag = "type")]`) which routes through
    /// `deserialize_any`, and bincode does not support self-describing decode.
    /// The overhead is negligible — Registry entries are control plane,
    /// not hot path.
    pub fn registry(cmd: &RegistryCommand) -> Result<Self, AeonError> {
        let payload = serde_json::to_vec(cmd).map_err(|e| AeonError::Cluster {
            message: format!("RegistryCommand serialize failed: {e}"),
            source: None,
        })?;
        Ok(Self::Registry { payload })
    }
}

impl ClusterResponse {
    /// Wrap a `RegistryResponse` into a `ClusterResponse::Registry`.
    /// See `ClusterRequest::registry` for the JSON-vs-bincode rationale.
    pub fn registry(resp: &RegistryResponse) -> Result<Self, AeonError> {
        let bytes = serde_json::to_vec(resp).map_err(|e| AeonError::Cluster {
            message: format!("RegistryResponse serialize failed: {e}"),
            source: None,
        })?;
        Ok(Self::Registry(bytes))
    }

    /// Decode a `ClusterResponse::Registry` payload back into `RegistryResponse`.
    /// Returns `Err` on non-Registry variants or malformed bytes.
    pub fn into_registry(self) -> Result<RegistryResponse, AeonError> {
        match self {
            Self::Registry(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| AeonError::Cluster {
                    message: format!("RegistryResponse deserialize failed: {e}"),
                    source: None,
                })
            }
            Self::Ok => Ok(RegistryResponse::Ok),
            Self::Error(msg) => Ok(RegistryResponse::Error { message: msg }),
        }
    }
}

/// Response after applying a ClusterRequest to the state machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClusterResponse {
    /// Operation succeeded.
    Ok,
    /// Operation failed with a reason.
    Error(String),
    /// Bincode-serialized `aeon_types::RegistryResponse` from a
    /// `ClusterRequest::Registry` apply.
    Registry(Vec<u8>),
}

// ── Join protocol messages (over QUIC, not through Raft log) ────────

/// Request from a new node to join an existing cluster.
/// Sent to a seed node (which may forward to the leader).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    /// The node ID the joining node wants to use.
    pub node_id: NodeId,
    /// The address the joining node is reachable at (for Raft RPCs).
    pub addr: NodeAddress,
}

/// Response to a join request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinResponse {
    /// Whether the join was accepted.
    pub success: bool,
    /// The current leader's node ID (so the joiner can redirect if needed).
    pub leader_id: Option<NodeId>,
    /// Human-readable error message if `success` is false.
    pub message: String,
}

/// Request to remove a node from the cluster.
/// Sent to the leader node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveNodeRequest {
    /// The node ID to remove from the cluster.
    pub node_id: NodeId,
}

/// Response to a remove-node request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveNodeResponse {
    /// Whether the removal was accepted.
    pub success: bool,
    /// The current leader's node ID (for redirect if this node is not leader).
    pub leader_id: Option<NodeId>,
    /// Human-readable error or status message.
    pub message: String,
}

/// CL-6a partition transfer: client-to-server request naming which
/// `(pipeline, partition)` the target wants to pull. The server responds
/// on the same bidirectional stream with a manifest + chunks + end frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionTransferRequest {
    /// Pipeline name as known to `PipelineL2Registry`.
    pub pipeline: String,
    /// Partition id (maps to the `p{partition:05}` directory).
    pub partition: PartitionId,
}

/// Terminal frame on the transfer stream. Tells the receiver whether the
/// source was able to stream every manifested segment. If
/// `success == false` the receiver must treat the transfer as aborted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionTransferEnd {
    pub success: bool,
    /// Error context when `success == false`. Empty on happy path.
    pub message: String,
}

/// CL-6b PoH chain transfer: client-to-server request naming which
/// `(pipeline, partition)` PoH chain the target wants a snapshot of.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PohChainTransferRequest {
    pub pipeline: String,
    pub partition: PartitionId,
}

/// Response to a `PohChainTransferRequest` — single-shot because a
/// `PohChainState` is small (current_hash + sequence + MMR, typically
/// well under 16 KiB). The `state_bytes` payload is the bincode-serialized
/// form of `aeon_crypto::poh::PohChainState`; aeon-cluster treats it as
/// opaque so we don't need a crypto dependency at the transport layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PohChainTransferResponse {
    pub success: bool,
    /// bincode-serialized `PohChainState`. Empty on failure.
    pub state_bytes: Vec<u8>,
    /// Error context when `success == false`. Empty on happy path.
    pub message: String,
}

/// CL-6c partition cutover: target → source request to stop accepting
/// writes for `(pipeline, partition)` and hand back the final offsets at
/// the moment of freeze. Sent after a successful bulk sync + PoH chain
/// transfer, immediately before the caller drives the Raft ownership
/// flip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionCutoverRequest {
    pub pipeline: String,
    pub partition: PartitionId,
}

/// Response to a `PartitionCutoverRequest`. On success, the source
/// guarantees it will not accept further writes for the partition until
/// the caller completes the Raft ownership flip (or explicitly aborts).
/// `final_source_offset` and `final_poh_sequence` are the watermarks the
/// target must have caught up to before resuming writes on its side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionCutoverResponse {
    pub success: bool,
    /// Source-anchor offset (e.g. Kafka offset) at freeze. `-1` means
    /// "no meaningful offset" (non-Kafka source or first cutover).
    pub final_source_offset: i64,
    /// PoH chain sequence at freeze. The target's PoH chain must have
    /// `sequence >= final_poh_sequence` to be a valid continuation.
    pub final_poh_sequence: u64,
    /// Error context when `success == false`. Empty on happy path.
    pub message: String,
}

/// Forwarded Raft proposal: a follower wraps a `ClusterRequest` it
/// would have called `raft.client_write` on directly and ships it to
/// the current leader. The leader runs `client_write` locally and
/// returns the resulting `ClusterResponse` (or an error message if
/// the leader's own propose fails).
///
/// `request_bytes` is the bincode-serialized `ClusterRequest` because
/// `aeon-cluster` types live below the state machine in the dependency
/// graph and pulling the enum across module boundaries adds friction.
/// Callers serialize at the call site; the leader-side dispatcher
/// deserializes back to a `ClusterRequest` before the propose call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposeForwardRequest {
    /// Bincode-serialized `ClusterRequest`.
    pub request_bytes: Vec<u8>,
}

/// Response to a `ProposeForwardRequest`. On `success == true`,
/// `response_bytes` is the bincode-serialized `ClusterResponse`
/// produced by `raft.client_write`. On `success == false`, `message`
/// describes the failure (leader changed, propose timeout, etc.) and
/// `response_bytes` is empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposeForwardResponse {
    pub success: bool,
    /// Bincode-serialized `ClusterResponse`. Empty on failure.
    pub response_bytes: Vec<u8>,
    /// Error context when `success == false`. Empty on happy path.
    pub message: String,
}

/// Ownership state of a single partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartitionOwnership {
    /// Partition is owned by a single node.
    Owned(NodeId),
    /// Partition is being transferred from source to target.
    Transferring { source: NodeId, target: NodeId },
}

impl PartitionOwnership {
    /// The node currently responsible for processing this partition.
    pub fn active_node(&self) -> NodeId {
        match self {
            Self::Owned(n) => *n,
            Self::Transferring { source, .. } => *source,
        }
    }

    pub fn is_transferring(&self) -> bool {
        matches!(self, Self::Transferring { .. })
    }
}

/// Maps every partition to its ownership state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PartitionTable {
    assignments: HashMap<PartitionId, PartitionOwnership>,
}

impl PartitionTable {
    pub fn new() -> Self {
        Self {
            assignments: HashMap::new(),
        }
    }

    /// Create a table with all partitions assigned to a single node.
    pub fn single_node(num_partitions: u16, node_id: NodeId) -> Self {
        let mut assignments = HashMap::with_capacity(num_partitions as usize);
        for i in 0..num_partitions {
            assignments.insert(PartitionId::new(i), PartitionOwnership::Owned(node_id));
        }
        Self { assignments }
    }

    pub fn get(&self, partition: PartitionId) -> Option<&PartitionOwnership> {
        self.assignments.get(&partition)
    }

    pub fn assign(&mut self, partition: PartitionId, node: NodeId) {
        self.assignments
            .insert(partition, PartitionOwnership::Owned(node));
    }

    pub fn begin_transfer(&mut self, partition: PartitionId, source: NodeId, target: NodeId) {
        self.assignments.insert(
            partition,
            PartitionOwnership::Transferring { source, target },
        );
    }

    pub fn complete_transfer(&mut self, partition: PartitionId, new_owner: NodeId) {
        self.assignments
            .insert(partition, PartitionOwnership::Owned(new_owner));
    }

    /// Get all partitions owned by (or actively served by) a given node.
    pub fn partitions_for_node(&self, node_id: NodeId) -> Vec<PartitionId> {
        self.assignments
            .iter()
            .filter(|(_, ownership)| ownership.active_node() == node_id)
            .map(|(p, _)| *p)
            .collect()
    }

    /// Get all unique active node IDs.
    pub fn active_nodes(&self) -> Vec<NodeId> {
        let mut nodes: Vec<NodeId> = self.assignments.values().map(|o| o.active_node()).collect();
        nodes.sort_unstable();
        nodes.dedup();
        nodes
    }

    pub fn num_partitions(&self) -> usize {
        self.assignments.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&PartitionId, &PartitionOwnership)> {
        self.assignments.iter()
    }

    /// Count partitions per node (active, not counting transfer targets).
    pub fn partition_counts(&self) -> HashMap<NodeId, usize> {
        let mut counts = HashMap::new();
        for ownership in self.assignments.values() {
            *counts.entry(ownership.active_node()).or_insert(0) += 1;
        }
        counts
    }
}

impl Default for PartitionTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_address_display_and_parse() {
        let addr = NodeAddress::new("10.0.0.1", 4433);
        assert_eq!(addr.to_string(), "10.0.0.1:4433");

        let parsed: NodeAddress = "10.0.0.1:4433".parse().unwrap();
        assert_eq!(parsed, addr);
    }

    #[test]
    fn node_address_parse_errors() {
        assert!("noport".parse::<NodeAddress>().is_err());
        assert!(":4433".parse::<NodeAddress>().is_err()); // empty host
        assert!("host:abc".parse::<NodeAddress>().is_err()); // invalid port
    }

    #[test]
    fn node_address_to_socket_addr() {
        let addr = NodeAddress::new("127.0.0.1", 4433);
        let sa = addr.to_socket_addr().unwrap();
        assert_eq!(sa, "127.0.0.1:4433".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn partition_ownership_active_node() {
        let owned = PartitionOwnership::Owned(1);
        assert_eq!(owned.active_node(), 1);
        assert!(!owned.is_transferring());

        let xfer = PartitionOwnership::Transferring {
            source: 1,
            target: 2,
        };
        assert_eq!(xfer.active_node(), 1);
        assert!(xfer.is_transferring());
    }

    #[test]
    fn partition_table_single_node() {
        let table = PartitionTable::single_node(4, 1);
        assert_eq!(table.num_partitions(), 4);
        for i in 0..4 {
            assert_eq!(
                table.get(PartitionId::new(i)),
                Some(&PartitionOwnership::Owned(1))
            );
        }
        assert_eq!(table.partitions_for_node(1).len(), 4);
        assert_eq!(table.partitions_for_node(2).len(), 0);
    }

    #[test]
    fn partition_table_assign_and_transfer() {
        let mut table = PartitionTable::single_node(4, 1);

        // Assign P2 to node 2
        table.assign(PartitionId::new(2), 2);
        assert_eq!(
            table.get(PartitionId::new(2)),
            Some(&PartitionOwnership::Owned(2))
        );

        // Begin transfer P0: node 1 → node 3
        table.begin_transfer(PartitionId::new(0), 1, 3);
        assert_eq!(
            table.get(PartitionId::new(0)),
            Some(&PartitionOwnership::Transferring {
                source: 1,
                target: 3
            })
        );
        // Active node is still source during transfer
        assert!(table.partitions_for_node(1).contains(&PartitionId::new(0)));

        // Complete transfer
        table.complete_transfer(PartitionId::new(0), 3);
        assert_eq!(
            table.get(PartitionId::new(0)),
            Some(&PartitionOwnership::Owned(3))
        );
    }

    #[test]
    fn partition_table_counts() {
        let mut table = PartitionTable::single_node(6, 1);
        table.assign(PartitionId::new(2), 2);
        table.assign(PartitionId::new(4), 2);

        let counts = table.partition_counts();
        assert_eq!(counts[&1], 4);
        assert_eq!(counts[&2], 2);
    }

    #[test]
    fn partition_table_active_nodes() {
        let mut table = PartitionTable::single_node(4, 1);
        table.assign(PartitionId::new(2), 3);
        let mut nodes = table.active_nodes();
        nodes.sort();
        assert_eq!(nodes, vec![1, 3]);
    }

    #[test]
    fn cluster_request_serde_roundtrip() {
        let requests = vec![
            ClusterRequest::AssignPartition {
                partition: PartitionId::new(5),
                node: 42,
            },
            ClusterRequest::BeginTransfer {
                partition: PartitionId::new(3),
                source: 1,
                target: 2,
            },
            ClusterRequest::CompleteTransfer {
                partition: PartitionId::new(3),
                new_owner: 2,
            },
            ClusterRequest::AbortTransfer {
                partition: PartitionId::new(3),
                revert_to: 1,
            },
            ClusterRequest::UpdateConfig {
                key: "batch_size".to_string(),
                value: "2048".to_string(),
            },
        ];

        for req in &requests {
            let bytes = bincode::serialize(req).unwrap();
            let decoded: ClusterRequest = bincode::deserialize(&bytes).unwrap();
            assert_eq!(&decoded, req);
        }
    }

    #[test]
    fn node_address_serde_roundtrip() {
        let addr = NodeAddress::new("10.0.0.1", 4433);
        let bytes = bincode::serialize(&addr).unwrap();
        let decoded: NodeAddress = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded, addr);
    }
}
