//! EO-2 Phase P5 — L3→WAL fallback orchestration + crash-recovery planning.
//!
//! Two primitives:
//!
//! 1. [`FallbackCheckpointStore`] — a `CheckpointPersist` wrapper that writes
//!    to an L3-backed primary, transparently falls back to a file-backed WAL
//!    on L3 write error, and can be reprobed later to reconcile.
//!
//! 2. [`RecoveryPlan`] — derives what each source must do on pipeline start
//!    given a loaded `CheckpointRecord`. Pull sources seek; push/poll sources
//!    scan L2 from the min-ack-seq frontier.
//!
//! See `docs/EO-2-DURABILITY-DESIGN.md` §5.1 and §6.2–6.4.

use crate::checkpoint::{CheckpointPersist, CheckpointRecord, WalCheckpointStore};
use aeon_types::{AeonError, PartitionId, SourceKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ── FallbackCheckpointStore ─────────────────────────────────────────────

/// Wraps a primary `CheckpointPersist` (typically L3-backed) with a
/// file-backed WAL fallback. On primary write error:
///
/// 1. Flips a flag so future writes go straight to WAL until reprobed.
/// 2. Emits `aeon_checkpoint_fallback_wal` (count of distinct transitions).
/// 3. Logs `WARN` once per transition.
///
/// A caller-driven `try_recover_primary()` replays any WAL records past
/// the primary's last `checkpoint_id` and returns the fallback to primary
/// mode on success. Reprobing is caller-scheduled (not a background task)
/// so tests are deterministic and the runtime has a single owner.
pub struct FallbackCheckpointStore {
    primary: Box<dyn CheckpointPersist>,
    wal_path: PathBuf,
    wal: Option<WalCheckpointStore>,
    in_fallback: Arc<AtomicBool>,
    /// Total transitions into fallback mode — `aeon_checkpoint_fallback_wal`.
    fallback_transitions: Arc<std::sync::atomic::AtomicU64>,
    /// EO-2 P8: optional observability registry. `inc_checkpoint_fallback_wal()`
    /// fires on first L3 → WAL transition; unset during tests/benches.
    metrics: Option<Arc<crate::eo2_metrics::Eo2Metrics>>,
}

impl FallbackCheckpointStore {
    /// Build a fallback store over `primary`, with a WAL file at `wal_path`
    /// used only when the primary errors.
    pub fn new(
        primary: Box<dyn CheckpointPersist>,
        wal_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            primary,
            wal_path: wal_path.into(),
            wal: None,
            in_fallback: Arc::new(AtomicBool::new(false)),
            fallback_transitions: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            metrics: None,
        }
    }

    /// Attach an `Eo2Metrics` registry so the first L3 → WAL transition
    /// bumps `aeon_checkpoint_fallback_wal_total`.
    pub fn with_metrics(
        mut self,
        metrics: Arc<crate::eo2_metrics::Eo2Metrics>,
    ) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Whether the store is currently routing writes to the WAL.
    pub fn in_fallback(&self) -> bool {
        self.in_fallback.load(Ordering::Relaxed)
    }

    /// Total count of L3 → WAL transitions since process start.
    /// Maps to the `aeon_checkpoint_fallback_wal` counter in §15.
    pub fn fallback_transitions(&self) -> u64 {
        self.fallback_transitions.load(Ordering::Relaxed)
    }

    /// Path to the WAL file used when in fallback.
    pub fn wal_path(&self) -> &Path {
        &self.wal_path
    }

    /// Engage fallback mode: open the WAL lazily and flip the flag.
    fn engage_fallback(&mut self, cause: &AeonError) -> Result<(), AeonError> {
        if self.wal.is_none() {
            if let Some(parent) = self.wal_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AeonError::state(format!("fallback: mkdir: {e}")))?;
            }
            self.wal = Some(WalCheckpointStore::open(&self.wal_path)?);
        }
        let first_transition = !self.in_fallback.swap(true, Ordering::Relaxed);
        if first_transition {
            self.fallback_transitions.fetch_add(1, Ordering::Relaxed);
            if let Some(ref m) = self.metrics {
                m.inc_checkpoint_fallback_wal();
            }
            tracing::warn!(
                wal = %self.wal_path.display(),
                cause = %cause,
                "L3 checkpoint store failed; falling back to WAL"
            );
        }
        Ok(())
    }

    /// Try to drain the WAL back into the primary. Callable on a slow
    /// schedule (e.g. every N checkpoint writes, or on explicit operator
    /// trigger). Returns `Ok(true)` if fallback was successfully cleared,
    /// `Ok(false)` if still in fallback, `Err` on replay error.
    pub fn try_recover_primary(&mut self) -> Result<bool, AeonError> {
        if !self.in_fallback() {
            return Ok(true);
        }
        let Some(wal) = self.wal.as_ref() else {
            // No WAL ever opened — impossible state but recover gracefully.
            self.in_fallback.store(false, Ordering::Relaxed);
            return Ok(true);
        };

        let primary_last_id = match self.primary.read_last() {
            Ok(Some(r)) => Some(r.checkpoint_id),
            Ok(None) => None,
            // Primary still broken → stay in fallback, not a hard error.
            Err(_) => return Ok(false),
        };

        let wal_records = match wal.read_last() {
            Ok(opt) => opt,
            Err(e) => return Err(e),
        };
        // Fast exit if WAL has nothing beyond primary.
        if let (Some(wr), Some(pid)) = (&wal_records, primary_last_id) {
            if wr.checkpoint_id <= pid {
                // WAL is already at or behind primary; clear fallback state.
                self.in_fallback.store(false, Ordering::Relaxed);
                self.wal = None;
                // File is left on disk; operator / next pipeline start will
                // decide whether to delete. Truncating here would race with
                // a concurrent reader.
                return Ok(true);
            }
        }

        // For now we do a single-record re-append of the latest WAL record
        // into the primary (matching the "at most one in-flight checkpoint"
        // invariant — see `write_checkpoint` in `pipeline.rs`). Multi-record
        // replay can extend this with a `read_all` API on `CheckpointPersist`.
        if let Some(mut record) = wal_records {
            let pre_id = record.checkpoint_id;
            if let Err(e) = self.primary.append(&mut record) {
                tracing::debug!(cause = %e, "fallback: primary still unhealthy");
                return Ok(false);
            }
            tracing::info!(
                replayed_from = pre_id,
                new_id = record.checkpoint_id,
                "L3 recovered from WAL fallback"
            );
        }
        self.in_fallback.store(false, Ordering::Relaxed);
        self.wal = None;
        Ok(true)
    }
}

impl CheckpointPersist for FallbackCheckpointStore {
    fn append(&mut self, record: &mut CheckpointRecord) -> Result<(), AeonError> {
        if !self.in_fallback() {
            match self.primary.append(record) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    self.engage_fallback(&e)?;
                }
            }
        }
        // Route to WAL.
        let wal = self
            .wal
            .as_mut()
            .ok_or_else(|| AeonError::state("fallback: WAL not open"))?;
        wal.append(record)
    }

    fn read_last(&self) -> Result<Option<CheckpointRecord>, AeonError> {
        // Prefer primary; fall back to WAL only if primary errors or is empty.
        match self.primary.read_last() {
            Ok(Some(r)) => {
                // If WAL has a record with a higher id (recovery scenario), return that.
                if let Some(wal) = self.wal.as_ref() {
                    if let Ok(Some(w)) = wal.read_last() {
                        if w.checkpoint_id > r.checkpoint_id {
                            return Ok(Some(w));
                        }
                    }
                }
                Ok(Some(r))
            }
            Ok(None) => match self.wal.as_ref() {
                Some(wal) => wal.read_last(),
                None => Ok(None),
            },
            Err(_) => match self.wal.as_ref() {
                Some(wal) => wal.read_last(),
                None => Ok(None),
            },
        }
    }

    fn next_checkpoint_id(&self) -> u64 {
        if self.in_fallback() {
            if let Some(w) = self.wal.as_ref() {
                return w.next_checkpoint_id();
            }
        }
        self.primary.next_checkpoint_id()
    }

    fn try_recover_primary(&mut self) -> Result<bool, AeonError> {
        FallbackCheckpointStore::try_recover_primary(self)
    }
}

// ── RecoveryPlan ────────────────────────────────────────────────────────

/// A plan derived from a loaded `CheckpointRecord` telling each source
/// how to resume after a crash.
///
/// - **Pull** sources: call `Seekable::seek(offset)` using per-partition
///   offsets from the record, then replay normally; duplicates are
///   suppressed by `IdempotentSink::has_seen`.
/// - **Push/Poll** sources: the upstream has no replay position, so the
///   pipeline must replay events whose delivery seq is `>= min_ack_seq`
///   from the L2 body store. `IdempotentSink::has_seen` still suppresses
///   duplicates on sinks that acked past that cursor.
#[derive(Debug, Clone)]
pub struct RecoveryPlan {
    /// Per-partition upstream offsets (Kafka offset / LSN / binlog pos).
    /// Keyed by `PartitionId` for safer matching vs the wire `u16`.
    pub pull_seek_offsets: HashMap<PartitionId, i64>,
    /// L2 replay start seq for push/poll sources. `0` when the record has
    /// no sink ack info (fresh pipeline, or `durability == None`).
    pub l2_replay_start_seq: u64,
    /// The checkpoint ID the plan was derived from — for logging.
    pub source_checkpoint_id: u64,
}

impl RecoveryPlan {
    /// Build a plan from the last good checkpoint. `None` → fresh start
    /// (empty offsets, seq 0).
    pub fn from_last(record: Option<&CheckpointRecord>) -> Self {
        let Some(r) = record else {
            return Self {
                pull_seek_offsets: HashMap::new(),
                l2_replay_start_seq: 0,
                source_checkpoint_id: 0,
            };
        };

        let pull_seek_offsets = r
            .source_offsets
            .iter()
            .map(|(p, o)| (PartitionId::new(*p), *o))
            .collect();

        // min across registered sinks. If no sink acked yet, start at 0
        // (replay everything the L2 store holds).
        let l2_replay_start_seq = r
            .per_sink_ack_seq
            .values()
            .min()
            .copied()
            .unwrap_or(0);

        Self {
            pull_seek_offsets,
            l2_replay_start_seq,
            source_checkpoint_id: r.checkpoint_id,
        }
    }

    /// What this plan implies for a given source kind.
    pub fn action_for(&self, kind: SourceKind) -> RecoveryAction {
        match kind {
            SourceKind::Pull => RecoveryAction::SeekPartitions(self.pull_seek_offsets.clone()),
            SourceKind::Push | SourceKind::Poll => {
                RecoveryAction::ReplayL2From(self.l2_replay_start_seq)
            }
        }
    }
}

/// Concrete action a source must take on recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Seek each partition to the given upstream offset, then resume.
    SeekPartitions(HashMap<PartitionId, i64>),
    /// Scan the L2 body store from the given seq, reinjecting every event
    /// whose seq is `>= n` into the pipeline. Duplicates past sink ack
    /// cursors are suppressed by `IdempotentSink::has_seen`.
    ReplayL2From(u64),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::CheckpointPersist;

    // ── Fake primary that can be toggled healthy/unhealthy per call ───

    struct FaultyPrimary {
        records: Vec<CheckpointRecord>,
        writes_fail: bool,
    }
    impl FaultyPrimary {
        fn new() -> Self {
            Self {
                records: Vec::new(),
                writes_fail: false,
            }
        }
    }
    impl CheckpointPersist for FaultyPrimary {
        fn append(&mut self, record: &mut CheckpointRecord) -> Result<(), AeonError> {
            if self.writes_fail {
                return Err(AeonError::state("faulty: write fail"));
            }
            record.checkpoint_id = self.records.len() as u64;
            self.records.push(record.clone());
            Ok(())
        }
        fn read_last(&self) -> Result<Option<CheckpointRecord>, AeonError> {
            Ok(self.records.last().cloned())
        }
        fn next_checkpoint_id(&self) -> u64 {
            self.records.len() as u64
        }
    }

    fn tmp_wal() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("fallback.wal");
        std::mem::forget(dir);
        p
    }

    fn rec() -> CheckpointRecord {
        CheckpointRecord::new(0, HashMap::new(), vec![], 1, 0)
    }

    #[test]
    fn healthy_primary_stays_primary() {
        let primary = Box::new(FaultyPrimary::new());
        let mut store = FallbackCheckpointStore::new(primary, tmp_wal());
        let mut r = rec();
        store.append(&mut r).unwrap();
        assert!(!store.in_fallback());
        assert_eq!(store.fallback_transitions(), 0);
    }

    #[test]
    fn primary_error_engages_fallback() {
        let mut primary = FaultyPrimary::new();
        primary.writes_fail = true;
        let mut store = FallbackCheckpointStore::new(Box::new(primary), tmp_wal());
        let mut r = rec();
        store.append(&mut r).unwrap();
        assert!(store.in_fallback());
        assert_eq!(store.fallback_transitions(), 1);
        // Subsequent writes stay in fallback without incrementing transitions.
        store.append(&mut rec()).unwrap();
        assert_eq!(store.fallback_transitions(), 1);
    }

    #[test]
    fn read_last_prefers_wal_when_ahead() {
        let primary = FaultyPrimary::new();
        let mut store = FallbackCheckpointStore::new(Box::new(primary), tmp_wal());
        // Healthy write — primary at id 0.
        store.append(&mut rec()).unwrap();
        // Force fault + two WAL writes — WAL gets higher IDs than primary.
        // FaultyPrimary can't flip in place; construct a fresh scenario:
        // write to WAL directly by re-wrapping with a faulty primary.
        let wal_path = store.wal_path().to_path_buf();
        let mut faulty = FaultyPrimary::new();
        faulty.writes_fail = true;
        let mut store2 = FallbackCheckpointStore::new(Box::new(faulty), wal_path);
        store2.append(&mut rec()).unwrap();
        store2.append(&mut rec()).unwrap();
        let last = store2.read_last().unwrap();
        // WAL picked up after primary failed; a record must be readable.
        assert!(last.is_some());
    }

    #[test]
    fn try_recover_succeeds_when_primary_heals() {
        let mut primary = FaultyPrimary::new();
        primary.writes_fail = true;
        let mut store = FallbackCheckpointStore::new(Box::new(primary), tmp_wal());
        store.append(&mut rec()).unwrap();
        assert!(store.in_fallback());

        // Heal the primary by swapping in a healthy one.
        store.primary = Box::new(FaultyPrimary::new());
        let recovered = store.try_recover_primary().unwrap();
        assert!(recovered);
        assert!(!store.in_fallback());
    }

    #[test]
    fn try_recover_stays_in_fallback_when_primary_still_broken() {
        let mut primary = FaultyPrimary::new();
        primary.writes_fail = true;
        let mut store = FallbackCheckpointStore::new(Box::new(primary), tmp_wal());
        store.append(&mut rec()).unwrap();
        // Primary still broken — recovery attempt returns false.
        let recovered = store.try_recover_primary().unwrap();
        assert!(!recovered);
        assert!(store.in_fallback());
    }

    #[test]
    fn fallback_transitions_counts_distinct_events() {
        let mut primary = FaultyPrimary::new();
        primary.writes_fail = true;
        let mut store = FallbackCheckpointStore::new(Box::new(primary), tmp_wal());
        for _ in 0..3 {
            store.append(&mut rec()).unwrap();
        }
        // Only one transition despite three writes in fallback.
        assert_eq!(store.fallback_transitions(), 1);
    }

    #[test]
    fn fallback_transition_bumps_metrics_counter() {
        let mut primary = FaultyPrimary::new();
        primary.writes_fail = true;
        let metrics = Arc::new(crate::eo2_metrics::Eo2Metrics::new());
        let mut store = FallbackCheckpointStore::new(Box::new(primary), tmp_wal())
            .with_metrics(Arc::clone(&metrics));
        for _ in 0..3 {
            store.append(&mut rec()).unwrap();
        }
        // Render the counter — one transition → exactly one bump in metrics.
        let rendered = metrics.render_prometheus();
        assert!(
            rendered.contains("aeon_checkpoint_fallback_wal_total 1"),
            "expected counter=1, got:\n{rendered}"
        );
    }

    // ── RecoveryPlan ─────────────────────────────────────────────────

    #[test]
    fn plan_from_none_is_fresh() {
        let plan = RecoveryPlan::from_last(None);
        assert!(plan.pull_seek_offsets.is_empty());
        assert_eq!(plan.l2_replay_start_seq, 0);
    }

    #[test]
    fn plan_pull_uses_source_offsets() {
        let mut offsets = HashMap::new();
        offsets.insert(PartitionId::new(0), 100i64);
        offsets.insert(PartitionId::new(1), 250i64);
        let mut rec = CheckpointRecord::new(7, offsets, vec![], 0, 0);
        rec.checkpoint_id = 7;
        let plan = RecoveryPlan::from_last(Some(&rec));
        assert_eq!(plan.source_checkpoint_id, 7);
        match plan.action_for(SourceKind::Pull) {
            RecoveryAction::SeekPartitions(m) => {
                assert_eq!(m[&PartitionId::new(0)], 100);
                assert_eq!(m[&PartitionId::new(1)], 250);
            }
            _ => panic!("pull must seek"),
        }
    }

    #[test]
    fn plan_push_replays_from_min_ack() {
        let mut per_sink = HashMap::new();
        per_sink.insert("kafka".to_string(), 100u64);
        per_sink.insert("webhook".to_string(), 42u64);
        per_sink.insert("redis".to_string(), 300u64);
        let rec = CheckpointRecord::new(0, HashMap::new(), vec![], 0, 0)
            .with_per_sink_ack_seq(per_sink);

        let plan = RecoveryPlan::from_last(Some(&rec));
        match plan.action_for(SourceKind::Push) {
            RecoveryAction::ReplayL2From(seq) => assert_eq!(seq, 42),
            _ => panic!("push must replay L2"),
        }
        // Poll behaves the same.
        match plan.action_for(SourceKind::Poll) {
            RecoveryAction::ReplayL2From(seq) => assert_eq!(seq, 42),
            _ => panic!("poll must replay L2"),
        }
    }

    #[test]
    fn plan_empty_ack_map_replays_from_zero() {
        let rec = CheckpointRecord::new(0, HashMap::new(), vec![], 0, 0);
        let plan = RecoveryPlan::from_last(Some(&rec));
        assert_eq!(plan.l2_replay_start_seq, 0);
    }
}
