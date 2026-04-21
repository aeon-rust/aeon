//! Cluster-backed [`PartitionOwnershipResolver`] implementation.
//!
//! Reads this node's owned partitions from the replicated cluster state
//! (`SharedClusterState`) at the moment the supervisor starts a pipeline.
//! The G1 fix from DOKS Session A: without this, `KafkaSourceFactory`
//! silently defaulted an empty `partitions` list to `[0]` and the node
//! under-read the topic even when Raft had assigned it a larger slice.
//!
//! This module is feature-gated behind `cluster`, matching the optional
//! `aeon-cluster` dependency — single-node builds do not pay for it.

use std::future::Future;
use std::pin::Pin;

use aeon_cluster::store::SharedClusterState;
use aeon_cluster::types::NodeId;
use aeon_types::PartitionId;
use tokio::sync::watch;

use crate::connector_registry::PartitionOwnershipResolver;

/// Reads owned partitions from the cluster state machine. Holds a
/// cloneable `SharedClusterState` handle (an `Arc<RwLock<ClusterSnapshot>>`)
/// so repeat queries see the latest committed ownership without extra
/// synchronisation.
pub struct ClusterPartitionOwnership {
    shared_state: SharedClusterState,
    my_id: NodeId,
    /// P5: optional receiver for the cluster's per-node owned-partitions
    /// watch. When present, subscribers (pipeline source loop) observe
    /// every committed ownership change without polling. Stored as a
    /// `Receiver` so each `watch()` call hands out a fresh subscription.
    owned_partitions_rx: Option<watch::Receiver<Vec<PartitionId>>>,
}

impl ClusterPartitionOwnership {
    pub fn new(shared_state: SharedClusterState, my_id: NodeId) -> Self {
        Self {
            shared_state,
            my_id,
            owned_partitions_rx: None,
        }
    }

    /// P5: attach the cluster-side owned-partitions watch receiver. Callers
    /// typically obtain this from `ClusterNode::watch_owned_partitions()`
    /// immediately after bootstrap. Chainable so `cmd_serve` can build the
    /// resolver in one expression.
    pub fn with_watch(
        mut self,
        rx: watch::Receiver<Vec<PartitionId>>,
    ) -> Self {
        self.owned_partitions_rx = Some(rx);
        self
    }
}

impl PartitionOwnershipResolver for ClusterPartitionOwnership {
    fn owned_partitions<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Option<Vec<u16>>> + Send + 'a>> {
        Box::pin(async move {
            let snapshot = self.shared_state.read().await;
            let owned: Vec<u16> = snapshot
                .partition_table
                .partitions_for_node(self.my_id)
                .into_iter()
                .map(|p| p.as_u16())
                .collect();
            // Distinguish "resolver is present but knows nothing yet"
            // (empty vec) from "no ownership info at all" (None) so the
            // supervisor can keep the factory's own fallback semantics.
            if owned.is_empty() { None } else { Some(owned) }
        })
    }

    fn watch(&self) -> Option<watch::Receiver<Vec<u16>>> {
        // Adapt the cluster-side `Vec<PartitionId>` watch into the
        // engine-facing `Vec<u16>` watch expected by the pipeline source
        // loop. Rather than carry a pre-mapped receiver (cluster-side
        // sender only knows `PartitionId`), spawn a one-shot relay task
        // the first time we hand out a subscription — the relay stays
        // alive as long as any subscriber keeps its handle.
        //
        // Cheap: relay is a `while watch.changed()` loop — no per-event
        // allocation beyond the re-shaped `Vec<u16>`. Returning `None`
        // means the caller should fall back to the polling resolver.
        let mut upstream = self.owned_partitions_rx.as_ref()?.clone();
        let initial: Vec<u16> = upstream
            .borrow()
            .iter()
            .map(|p| p.as_u16())
            .collect();
        let (tx, rx) = watch::channel(initial);
        tokio::spawn(async move {
            while upstream.changed().await.is_ok() {
                let mapped: Vec<u16> = upstream
                    .borrow_and_update()
                    .iter()
                    .map(|p| p.as_u16())
                    .collect();
                if tx.send(mapped).is_err() {
                    break;
                }
            }
        });
        Some(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_cluster::snapshot::ClusterSnapshot;
    use aeon_types::partition::PartitionId;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn new_state() -> SharedClusterState {
        Arc::new(RwLock::new(ClusterSnapshot::default()))
    }

    #[tokio::test]
    async fn empty_table_returns_none() {
        let state = new_state();
        let r = ClusterPartitionOwnership::new(state, 1);
        assert!(r.owned_partitions().await.is_none());
    }

    #[tokio::test]
    async fn returns_only_partitions_owned_by_this_node() {
        let state = new_state();
        {
            let mut s = state.write().await;
            s.partition_table.assign(PartitionId::new(0), 1);
            s.partition_table.assign(PartitionId::new(1), 2);
            s.partition_table.assign(PartitionId::new(2), 1);
            s.partition_table.assign(PartitionId::new(3), 3);
        }
        let r = ClusterPartitionOwnership::new(Arc::clone(&state), 1);
        let mut owned = r.owned_partitions().await.expect("has ownership");
        owned.sort();
        assert_eq!(owned, vec![0, 2]);

        let r2 = ClusterPartitionOwnership::new(state, 2);
        assert_eq!(r2.owned_partitions().await, Some(vec![1]));
    }

    #[tokio::test]
    async fn node_with_no_partitions_returns_none_not_empty_vec() {
        let state = new_state();
        {
            let mut s = state.write().await;
            s.partition_table.assign(PartitionId::new(0), 2);
        }
        // Node 1 owns nothing — must be None so the supervisor falls
        // back to the factory's own default rather than passing an
        // empty slice (which the source would reject).
        let r = ClusterPartitionOwnership::new(state, 1);
        assert!(r.owned_partitions().await.is_none());
    }
}
