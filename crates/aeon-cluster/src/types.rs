//! Core cluster types: node identity, partition ownership, Raft log entries.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use aeon_types::PartitionId;
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
}

/// Response after applying a ClusterRequest to the state machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClusterResponse {
    /// Operation succeeded.
    Ok,
    /// Operation failed with a reason.
    Error(String),
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
