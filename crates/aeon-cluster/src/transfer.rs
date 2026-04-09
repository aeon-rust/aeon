//! Two-phase partition transfer state machine.
//!
//! Phase 1 (Bulk Sync): L3 state data transfer in the background (stubbed — no L3 yet).
//! Phase 2 (Cutover): Brief pause, L1 snapshot + source-anchor offset transfer.
//!
//! Each phase transition is committed via Raft to ensure crash-safety.

use serde::{Deserialize, Serialize};

use aeon_types::PartitionId;

use crate::types::NodeId;

/// The state of a partition transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferState {
    /// No transfer in progress.
    Idle,
    /// Phase 1: Bulk data sync (background, partition keeps running).
    BulkSync {
        source: NodeId,
        target: NodeId,
        /// Bytes transferred so far.
        progress: u64,
        /// Total bytes to transfer (0 = unknown).
        total: u64,
    },
    /// Phase 2: Cutover (partition paused, final delta transfer).
    Cutover { source: NodeId, target: NodeId },
    /// Transfer completed successfully.
    Complete { new_owner: NodeId },
    /// Transfer was aborted.
    Aborted { reverted_to: NodeId, reason: String },
}

impl TransferState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::BulkSync { .. } | Self::Cutover { .. })
    }
}

/// Tracks transfer state for a single partition.
#[derive(Debug)]
pub struct TransferTracker {
    pub partition: PartitionId,
    pub state: TransferState,
}

impl TransferTracker {
    pub fn new(partition: PartitionId) -> Self {
        Self {
            partition,
            state: TransferState::Idle,
        }
    }

    /// Transition: Idle → BulkSync
    pub fn begin(&mut self, source: NodeId, target: NodeId) -> Result<(), String> {
        match &self.state {
            TransferState::Idle => {
                self.state = TransferState::BulkSync {
                    source,
                    target,
                    progress: 0,
                    total: 0,
                };
                Ok(())
            }
            _ => Err(format!(
                "cannot begin transfer: partition {:?} is in state {:?}",
                self.partition, self.state
            )),
        }
    }

    /// Transition: BulkSync → Cutover
    pub fn begin_cutover(&mut self) -> Result<(), String> {
        match &self.state {
            TransferState::BulkSync { source, target, .. } => {
                self.state = TransferState::Cutover {
                    source: *source,
                    target: *target,
                };
                Ok(())
            }
            _ => Err(format!(
                "cannot cutover: partition {:?} is in state {:?}",
                self.partition, self.state
            )),
        }
    }

    /// Transition: Cutover → Complete
    pub fn complete(&mut self) -> Result<NodeId, String> {
        match &self.state {
            TransferState::Cutover { target, .. } => {
                let new_owner = *target;
                self.state = TransferState::Complete { new_owner };
                Ok(new_owner)
            }
            _ => Err(format!(
                "cannot complete: partition {:?} is in state {:?}",
                self.partition, self.state
            )),
        }
    }

    /// Transition: any active state → Aborted
    pub fn abort(&mut self, reason: String) -> Result<NodeId, String> {
        match &self.state {
            TransferState::BulkSync { source, .. } | TransferState::Cutover { source, .. } => {
                let reverted = *source;
                self.state = TransferState::Aborted {
                    reverted_to: reverted,
                    reason,
                };
                Ok(reverted)
            }
            _ => Err(format!(
                "cannot abort: partition {:?} is in state {:?}",
                self.partition, self.state
            )),
        }
    }

    /// Update bulk sync progress.
    pub fn update_progress(&mut self, bytes: u64, total: u64) {
        if let TransferState::BulkSync {
            progress, total: t, ..
        } = &mut self.state
        {
            *progress = bytes;
            *t = total;
        }
    }

    /// Reset to Idle (after complete or abort).
    pub fn reset(&mut self) {
        self.state = TransferState::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_happy_path() {
        let mut t = TransferTracker::new(PartitionId::new(0));
        assert_eq!(t.state, TransferState::Idle);

        t.begin(1, 2).unwrap();
        assert!(matches!(
            t.state,
            TransferState::BulkSync {
                source: 1,
                target: 2,
                ..
            }
        ));

        t.update_progress(1000, 5000);
        if let TransferState::BulkSync {
            progress, total, ..
        } = &t.state
        {
            assert_eq!(*progress, 1000);
            assert_eq!(*total, 5000);
        }

        t.begin_cutover().unwrap();
        assert!(matches!(
            t.state,
            TransferState::Cutover {
                source: 1,
                target: 2
            }
        ));

        let owner = t.complete().unwrap();
        assert_eq!(owner, 2);
        assert!(matches!(t.state, TransferState::Complete { new_owner: 2 }));
    }

    #[test]
    fn transfer_abort_from_bulk_sync() {
        let mut t = TransferTracker::new(PartitionId::new(0));
        t.begin(1, 2).unwrap();

        let reverted = t.abort("test abort".to_string()).unwrap();
        assert_eq!(reverted, 1);
        assert!(matches!(
            t.state,
            TransferState::Aborted { reverted_to: 1, .. }
        ));
    }

    #[test]
    fn transfer_abort_from_cutover() {
        let mut t = TransferTracker::new(PartitionId::new(0));
        t.begin(1, 2).unwrap();
        t.begin_cutover().unwrap();

        let reverted = t.abort("network failure".to_string()).unwrap();
        assert_eq!(reverted, 1);
    }

    #[test]
    fn transfer_cannot_begin_twice() {
        let mut t = TransferTracker::new(PartitionId::new(0));
        t.begin(1, 2).unwrap();
        assert!(t.begin(1, 3).is_err());
    }

    #[test]
    fn transfer_cannot_cutover_from_idle() {
        let mut t = TransferTracker::new(PartitionId::new(0));
        assert!(t.begin_cutover().is_err());
    }

    #[test]
    fn transfer_cannot_complete_from_bulk_sync() {
        let mut t = TransferTracker::new(PartitionId::new(0));
        t.begin(1, 2).unwrap();
        assert!(t.complete().is_err());
    }

    #[test]
    fn transfer_cannot_abort_from_idle() {
        let mut t = TransferTracker::new(PartitionId::new(0));
        assert!(t.abort("test".to_string()).is_err());
    }

    #[test]
    fn transfer_reset() {
        let mut t = TransferTracker::new(PartitionId::new(0));
        t.begin(1, 2).unwrap();
        t.begin_cutover().unwrap();
        t.complete().unwrap();
        t.reset();
        assert_eq!(t.state, TransferState::Idle);
    }

    #[test]
    fn transfer_is_active() {
        let mut t = TransferTracker::new(PartitionId::new(0));
        assert!(!t.state.is_active());

        t.begin(1, 2).unwrap();
        assert!(t.state.is_active());

        t.begin_cutover().unwrap();
        assert!(t.state.is_active());

        t.complete().unwrap();
        assert!(!t.state.is_active());
    }
}
