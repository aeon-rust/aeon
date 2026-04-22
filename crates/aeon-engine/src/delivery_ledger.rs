//! Delivery Ledger — per-pipeline in-memory tracking of event delivery status.
//!
//! Uses DashMap for lock-free concurrent access on the hot path.
//! Cost: ~20ns insert (track), ~20ns remove (ack), ~2.8% of hot-path budget.
//!
//! The ledger tracks every output sent through the sink. At checkpoint boundaries,
//! pending (unacked) events are recorded for crash recovery and audit.

use aeon_types::PartitionId;
use dashmap::DashMap;
use std::time::Instant;

/// Delivery state of a tracked output.
#[derive(Debug, Clone)]
pub enum DeliveryState {
    /// Output has been sent to sink but not yet acknowledged.
    Pending { tracked_at: Instant },
    /// Delivery failed — may be retried or routed to DLQ.
    Failed {
        tracked_at: Instant,
        failed_at: Instant,
        reason: String,
        attempts: u32,
    },
}

/// A tracked entry in the delivery ledger.
#[derive(Debug, Clone)]
pub struct LedgerEntry {
    /// Event ID (UUIDv7) that produced this output.
    pub event_id: uuid::Uuid,
    /// Source partition this event came from.
    pub partition: PartitionId,
    /// Source offset for checkpoint recovery.
    pub source_offset: i64,
    /// Current delivery state.
    pub state: DeliveryState,
}

/// Summary of a failed delivery for querying.
#[derive(Debug, Clone)]
pub struct FailedEntry {
    pub event_id: uuid::Uuid,
    pub partition: PartitionId,
    pub source_offset: i64,
    pub reason: String,
    pub attempts: u32,
    pub failed_at: Instant,
}

/// Per-pipeline delivery ledger backed by DashMap.
///
/// Thread-safe, lock-free concurrent access. The hot path uses only
/// `insert` (~20ns) and `remove` (~20ns) operations.
pub struct DeliveryLedger {
    entries: DashMap<uuid::Uuid, LedgerEntry>,
    /// Total events tracked since creation (monotonic counter).
    total_tracked: std::sync::atomic::AtomicU64,
    /// Total events acknowledged since creation.
    total_acked: std::sync::atomic::AtomicU64,
    /// Total events failed since creation.
    total_failed: std::sync::atomic::AtomicU64,
    /// Maximum retry attempts before routing to DLQ.
    max_attempts: u32,
}

impl DeliveryLedger {
    /// Create a new delivery ledger.
    pub fn new(max_attempts: u32) -> Self {
        Self {
            entries: DashMap::new(),
            total_tracked: std::sync::atomic::AtomicU64::new(0),
            total_acked: std::sync::atomic::AtomicU64::new(0),
            total_failed: std::sync::atomic::AtomicU64::new(0),
            max_attempts,
        }
    }

    /// Track an output for delivery. Called on write_batch().
    /// Hot path: ~20ns (DashMap insert).
    pub fn track(&self, event_id: uuid::Uuid, partition: PartitionId, source_offset: i64) {
        self.entries.insert(
            event_id,
            LedgerEntry {
                event_id,
                partition,
                source_offset,
                state: DeliveryState::Pending {
                    tracked_at: Instant::now(),
                },
            },
        );
        self.total_tracked
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Mark an event as successfully delivered. Called on ack.
    /// Hot path: ~20ns (DashMap remove).
    pub fn mark_acked(&self, event_id: &uuid::Uuid) -> bool {
        if self.entries.remove(event_id).is_some() {
            self.total_acked
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Mark a batch of events as acknowledged.
    pub fn mark_batch_acked(&self, event_ids: &[uuid::Uuid]) -> u64 {
        let mut count = 0;
        for id in event_ids {
            if self.mark_acked(id) {
                count += 1;
            }
        }
        count
    }

    /// Mark an event as failed delivery. Increments attempt counter.
    pub fn mark_failed(&self, event_id: &uuid::Uuid, reason: String) -> bool {
        if let Some(mut entry) = self.entries.get_mut(event_id) {
            let tracked_at = match &entry.state {
                DeliveryState::Pending { tracked_at } => *tracked_at,
                DeliveryState::Failed { tracked_at, .. } => *tracked_at,
            };
            let attempts = match &entry.state {
                DeliveryState::Pending { .. } => 1,
                DeliveryState::Failed { attempts, .. } => attempts + 1,
            };
            entry.state = DeliveryState::Failed {
                tracked_at,
                failed_at: Instant::now(),
                reason,
                attempts,
            };
            self.total_failed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Number of currently pending (unacked) events.
    pub fn pending_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.state, DeliveryState::Pending { .. }))
            .count()
    }

    /// Number of currently failed events.
    pub fn failed_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.state, DeliveryState::Failed { .. }))
            .count()
    }

    /// Total entries in the ledger (pending + failed).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ledger is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get all pending event IDs (for checkpoint record).
    pub fn pending_ids(&self) -> Vec<uuid::Uuid> {
        self.entries
            .iter()
            .filter(|e| matches!(e.state, DeliveryState::Pending { .. }))
            .map(|e| *e.key())
            .collect()
    }

    /// Get all failed entries (for retry/DLQ decisions).
    pub fn failed_entries(&self) -> Vec<FailedEntry> {
        self.entries
            .iter()
            .filter_map(|e| match &e.state {
                DeliveryState::Failed {
                    failed_at,
                    reason,
                    attempts,
                    ..
                } => Some(FailedEntry {
                    event_id: e.event_id,
                    partition: e.partition,
                    source_offset: e.source_offset,
                    reason: reason.clone(),
                    attempts: *attempts,
                    failed_at: *failed_at,
                }),
                _ => None,
            })
            .collect()
    }

    /// Get entries that have exceeded max retry attempts (ready for DLQ).
    pub fn dlq_ready(&self) -> Vec<FailedEntry> {
        self.failed_entries()
            .into_iter()
            .filter(|e| e.attempts >= self.max_attempts)
            .collect()
    }

    /// Age of the oldest pending entry. Returns None if no pending entries.
    pub fn oldest_pending_age(&self) -> Option<std::time::Duration> {
        self.entries
            .iter()
            .filter_map(|e| match &e.state {
                DeliveryState::Pending { tracked_at } => Some(tracked_at.elapsed()),
                _ => None,
            })
            .max()
    }

    /// Get per-partition source offsets for checkpoint (min offset per partition
    /// among all pending entries — the safe replay point).
    pub fn checkpoint_offsets(&self) -> std::collections::HashMap<PartitionId, i64> {
        let mut offsets: std::collections::HashMap<PartitionId, i64> =
            std::collections::HashMap::new();
        for entry in self.entries.iter() {
            offsets
                .entry(entry.partition)
                .and_modify(|o| *o = (*o).min(entry.source_offset))
                .or_insert(entry.source_offset);
        }
        offsets
    }

    /// Remove a specific entry (used after routing to DLQ).
    pub fn remove(&self, event_id: &uuid::Uuid) -> bool {
        self.entries.remove(event_id).is_some()
    }

    /// Clear all entries (used after full checkpoint when all acked).
    pub fn clear(&self) {
        self.entries.clear();
    }

    /// Total events tracked since creation.
    pub fn total_tracked(&self) -> u64 {
        self.total_tracked
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total events acknowledged since creation.
    pub fn total_acked(&self) -> u64 {
        self.total_acked.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total events that failed at least once since creation.
    pub fn total_failed(&self) -> u64 {
        self.total_failed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::PartitionId;

    fn uid() -> uuid::Uuid {
        uuid::Uuid::now_v7()
    }

    #[test]
    fn track_and_ack() {
        let ledger = DeliveryLedger::new(3);
        let id = uid();
        ledger.track(id, PartitionId::new(0), 100);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.pending_count(), 1);
        assert!(ledger.mark_acked(&id));
        assert!(ledger.is_empty());
        assert_eq!(ledger.total_tracked(), 1);
        assert_eq!(ledger.total_acked(), 1);
    }

    #[test]
    fn track_and_fail() {
        let ledger = DeliveryLedger::new(3);
        let id = uid();
        ledger.track(id, PartitionId::new(1), 200);
        assert!(ledger.mark_failed(&id, "timeout".into()));
        assert_eq!(ledger.failed_count(), 1);
        assert_eq!(ledger.pending_count(), 0);

        let failed = ledger.failed_entries();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].reason, "timeout");
        assert_eq!(failed[0].attempts, 1);
    }

    #[test]
    fn fail_increments_attempts() {
        let ledger = DeliveryLedger::new(3);
        let id = uid();
        ledger.track(id, PartitionId::new(0), 50);
        ledger.mark_failed(&id, "err1".into());
        ledger.mark_failed(&id, "err2".into());
        ledger.mark_failed(&id, "err3".into());

        let failed = ledger.failed_entries();
        assert_eq!(failed[0].attempts, 3);
        assert_eq!(failed[0].reason, "err3");
    }

    #[test]
    fn dlq_ready_after_max_attempts() {
        let ledger = DeliveryLedger::new(2);
        let id1 = uid();
        let id2 = uid();
        ledger.track(id1, PartitionId::new(0), 1);
        ledger.track(id2, PartitionId::new(0), 2);

        // id1: 2 failures (meets max)
        ledger.mark_failed(&id1, "a".into());
        ledger.mark_failed(&id1, "b".into());
        // id2: 1 failure (below max)
        ledger.mark_failed(&id2, "c".into());

        let ready = ledger.dlq_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].event_id, id1);
    }

    #[test]
    fn batch_ack() {
        let ledger = DeliveryLedger::new(3);
        let ids: Vec<uuid::Uuid> = (0..5).map(|_| uid()).collect();
        for (i, id) in ids.iter().enumerate() {
            ledger.track(*id, PartitionId::new(0), i as i64);
        }
        assert_eq!(ledger.len(), 5);

        let acked = ledger.mark_batch_acked(&ids[0..3]);
        assert_eq!(acked, 3);
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn ack_nonexistent_returns_false() {
        let ledger = DeliveryLedger::new(3);
        assert!(!ledger.mark_acked(&uid()));
    }

    #[test]
    fn fail_nonexistent_returns_false() {
        let ledger = DeliveryLedger::new(3);
        assert!(!ledger.mark_failed(&uid(), "nope".into()));
    }

    #[test]
    fn checkpoint_offsets_tracks_min_per_partition() {
        let ledger = DeliveryLedger::new(3);
        let p0 = PartitionId::new(0);
        let p1 = PartitionId::new(1);

        ledger.track(uid(), p0, 100);
        ledger.track(uid(), p0, 50);
        ledger.track(uid(), p0, 200);
        ledger.track(uid(), p1, 300);
        ledger.track(uid(), p1, 150);

        let offsets = ledger.checkpoint_offsets();
        assert_eq!(offsets[&p0], 50);
        assert_eq!(offsets[&p1], 150);
    }

    #[test]
    fn oldest_pending_age() {
        let ledger = DeliveryLedger::new(3);
        assert!(ledger.oldest_pending_age().is_none());

        ledger.track(uid(), PartitionId::new(0), 0);
        // Age should be very small but non-zero
        let age = ledger.oldest_pending_age().unwrap();
        assert!(age.as_secs() < 1);
    }

    #[test]
    fn remove_entry() {
        let ledger = DeliveryLedger::new(3);
        let id = uid();
        ledger.track(id, PartitionId::new(0), 0);
        assert!(ledger.remove(&id));
        assert!(ledger.is_empty());
        assert!(!ledger.remove(&id));
    }

    #[test]
    fn clear_all() {
        let ledger = DeliveryLedger::new(3);
        for i in 0..10 {
            ledger.track(uid(), PartitionId::new(0), i);
        }
        assert_eq!(ledger.len(), 10);
        ledger.clear();
        assert!(ledger.is_empty());
    }

    #[test]
    fn pending_ids_returns_only_pending() {
        let ledger = DeliveryLedger::new(3);
        let id1 = uid();
        let id2 = uid();
        let id3 = uid();
        ledger.track(id1, PartitionId::new(0), 0);
        ledger.track(id2, PartitionId::new(0), 1);
        ledger.track(id3, PartitionId::new(0), 2);

        ledger.mark_failed(&id2, "fail".into());

        let pending = ledger.pending_ids();
        assert_eq!(pending.len(), 2);
        assert!(pending.contains(&id1));
        assert!(!pending.contains(&id2));
        assert!(pending.contains(&id3));
    }

    #[test]
    fn nil_uuids_collapse_tracking_slots() {
        // B1 regression guard: every source must stamp a unique Event.id.
        // If a source accidentally reverts to Uuid::nil(), N events would
        // collide into a single ledger slot, and ack(nil) would silently
        // cover a batch that was never delivered — breaking EOS accounting.
        //
        // This test asserts the collapse actually happens for nil, so the
        // paired `distinct_uuids_get_distinct_slots` guarantees the fix.
        let ledger = DeliveryLedger::new(3);
        let nil = uuid::Uuid::nil();
        for i in 0..5 {
            ledger.track(nil, PartitionId::new(0), i as i64);
        }
        assert_eq!(
            ledger.len(),
            1,
            "nil UUIDs collapse into one slot — this is why Event.id must be UUIDv7"
        );
        assert!(ledger.mark_acked(&nil));
        assert!(ledger.is_empty());
    }

    #[test]
    fn distinct_uuids_get_distinct_slots() {
        // B1 regression guard (positive case): N distinct UUIDv7 ids
        // produce N tracking slots, and each must be individually acked.
        let ledger = DeliveryLedger::new(3);
        let ids: Vec<uuid::Uuid> = (0..100).map(|_| uid()).collect();
        for (i, id) in ids.iter().enumerate() {
            ledger.track(*id, PartitionId::new(0), i as i64);
        }
        assert_eq!(ledger.len(), 100);
        let acked = ledger.mark_batch_acked(&ids);
        assert_eq!(acked, 100);
        assert!(ledger.is_empty());
    }

    #[test]
    fn counters_are_monotonic() {
        let ledger = DeliveryLedger::new(3);
        let id1 = uid();
        let id2 = uid();

        ledger.track(id1, PartitionId::new(0), 0);
        ledger.track(id2, PartitionId::new(0), 1);
        assert_eq!(ledger.total_tracked(), 2);

        ledger.mark_acked(&id1);
        assert_eq!(ledger.total_acked(), 1);

        ledger.mark_failed(&id2, "err".into());
        assert_eq!(ledger.total_failed(), 1);

        // Clear doesn't reset counters
        ledger.clear();
        assert_eq!(ledger.total_tracked(), 2);
        assert_eq!(ledger.total_acked(), 1);
    }
}
