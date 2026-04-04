//! Cluster snapshot — serializable state for Raft snapshot mechanism.

use std::collections::HashMap;

use aeon_types::PartitionId;
use serde::{Deserialize, Serialize};

use crate::types::{NodeId, PartitionTable};

/// Complete cluster state, used for Raft snapshots.
///
/// This is intentionally small (kilobytes) — event data never enters the Raft log.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterSnapshot {
    /// Which node owns which partitions.
    pub partition_table: PartitionTable,
    /// Last applied Raft log index.
    pub last_applied_log_index: u64,
    /// Last applied Raft log term.
    pub last_applied_log_term: u64,
    /// Per-partition source-anchor offsets (for Kafka consumer restart).
    pub source_anchor_offsets: HashMap<PartitionId, u64>,
    /// Global PoH checkpoint hash (placeholder for Phase 9, 64 bytes = SHA-512).
    pub poh_checkpoint: Option<Vec<u8>>,
    /// Cluster-wide configuration overrides.
    pub config_overrides: HashMap<String, String>,
}

impl ClusterSnapshot {
    /// Create an empty snapshot for a fresh single-node cluster.
    pub fn new_single_node(num_partitions: u16, node_id: NodeId) -> Self {
        Self {
            partition_table: PartitionTable::single_node(num_partitions, node_id),
            last_applied_log_index: 0,
            last_applied_log_term: 0,
            source_anchor_offsets: HashMap::new(),
            poh_checkpoint: None,
            config_overrides: HashMap::new(),
        }
    }

    /// Serialize to bincode bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    /// Deserialize from bincode bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_serde_roundtrip() {
        let mut snap = ClusterSnapshot::new_single_node(16, 1);
        snap.last_applied_log_index = 42;
        snap.last_applied_log_term = 3;
        snap.source_anchor_offsets
            .insert(PartitionId::new(0), 12345);
        snap.config_overrides
            .insert("batch_size".into(), "2048".into());

        let bytes = snap.to_bytes().unwrap();
        let decoded = ClusterSnapshot::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, snap);
    }

    #[test]
    fn snapshot_single_node_has_all_partitions() {
        let snap = ClusterSnapshot::new_single_node(8, 1);
        assert_eq!(snap.partition_table.num_partitions(), 8);
        assert_eq!(snap.partition_table.partitions_for_node(1).len(), 8);
    }

    #[test]
    fn snapshot_with_poh_checkpoint() {
        let mut snap = ClusterSnapshot::default();
        let checkpoint = vec![0xABu8; 64];
        snap.poh_checkpoint = Some(checkpoint.clone());

        let bytes = snap.to_bytes().unwrap();
        let decoded = ClusterSnapshot::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.poh_checkpoint.unwrap(), checkpoint);
    }

    #[test]
    fn snapshot_default_is_empty() {
        let snap = ClusterSnapshot::default();
        assert_eq!(snap.partition_table.num_partitions(), 0);
        assert_eq!(snap.last_applied_log_index, 0);
        assert!(snap.poh_checkpoint.is_none());
    }
}
