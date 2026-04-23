//! S6.8 — time-triggered erasure compaction policy.
//!
//! The GDPR right-to-erasure SLA is 30 days (Art. 17). Aeon's default
//! compaction strategy is size-triggered — a segment is rewritten
//! when it fills up. On an idle partition that alone cannot satisfy
//! the SLA, so the engine layers a **time** trigger on top:
//!
//! - every pending tombstone carries an `accepted_at_nanos` stamp
//!   (S6.2),
//! - [`ErasurePolicy::evaluate`] scans `list_pending()` and computes
//!   the oldest pending tombstone's age,
//! - if that age exceeds [`ErasureConfig::max_delay_hours`] (default
//!   24h per `docs/aeon-dev-notes.txt` §6.1.g), the returned
//!   [`ErasureBacklog`] carries `force_compaction = true` — the
//!   signal the compactor consumes to sweep affected segments even
//!   if they are not yet full.
//!
//! # Metric surface
//!
//! The backlog struct doubles as the data source for two metrics
//! listed in 6.1.g:
//!
//! - `aeon_erasure_tombstones_pending` ← [`ErasureBacklog::pending_count`]
//! - `aeon_erasure_oldest_tombstone_age_seconds` ←
//!   [`ErasureBacklog::oldest_age_nanos`] / 1e9
//!
//! Emitting the metrics is the observability crate's job; this module
//! just surfaces the numbers.
//!
//! # Scope
//!
//! This module evaluates the trigger. The compactor that actually
//! rewrites segments is a separate piece (tracked under the
//! durability/GC work). Separating the two keeps the policy
//! testable without a real L2 store and lets operators plug in a
//! different compactor without duplicating the SLA math.

use std::time::Duration;

use aeon_types::{AeonError, ErasureConfig};

use crate::erasure_store::ErasureStore;

/// Evaluate whether the erasure backlog has breached its SLA.
#[derive(Debug, Clone, Copy)]
pub struct ErasurePolicy {
    max_delay_nanos: u64,
}

/// Snapshot of the pending-erasure backlog at a single point in
/// time. Used both by the compactor trigger and by the metrics
/// endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErasureBacklog {
    /// Number of tombstones in `Pending` state.
    pub pending_count: usize,
    /// `accepted_at_nanos` of the oldest pending tombstone.
    /// `None` when nothing is pending.
    pub oldest_accepted_at_nanos: Option<i64>,
    /// Age of the oldest pending tombstone, in nanoseconds.
    /// `None` when nothing is pending. Saturates at `0` when the
    /// tombstone's stamp is in the future (clock-skew safe).
    pub oldest_age_nanos: Option<u64>,
    /// `true` when the oldest pending tombstone has exceeded the
    /// configured max-delay threshold. The compactor must run a
    /// sweep of affected segments even if none are yet full.
    pub force_compaction: bool,
}

impl ErasurePolicy {
    /// Construct from an explicit max-delay duration. Used in tests
    /// that want sub-hour precision.
    pub fn new(max_delay: Duration) -> Self {
        // `as_nanos()` returns `u128`; clamp to u64::MAX for any
        // practically-absurd input. 2^64 ns is ~584 years — a value
        // that large effectively disables the trigger, which is
        // the intended semantic for a config error.
        let max_delay_nanos = u64::try_from(max_delay.as_nanos()).unwrap_or(u64::MAX);
        Self { max_delay_nanos }
    }

    /// Construct from the manifest config. `ErasureConfig::max_delay_hours`
    /// is a `u32`; `u32 * 3600 * 1_000_000_000` fits comfortably in `u64`
    /// for any human-plausible input.
    pub fn from_config(cfg: &ErasureConfig) -> Self {
        // `u64::from(u32) * 3600 * 1e9` cannot overflow: u32::MAX * 3600 *
        // 1e9 ≈ 1.5e22, which fits in u64 (1.8e19)? No — u32::MAX≈4.3e9, so
        // that product is 1.5e22, which overflows. Saturate to u64::MAX.
        let hours = u64::from(cfg.max_delay_hours);
        let max_delay_nanos = hours
            .checked_mul(3_600)
            .and_then(|sec| sec.checked_mul(1_000_000_000))
            .unwrap_or(u64::MAX);
        Self { max_delay_nanos }
    }

    /// The configured max delay, in nanoseconds. Exposed for tests
    /// and for metric emission ("SLA is N seconds").
    pub fn max_delay_nanos(self) -> u64 {
        self.max_delay_nanos
    }

    /// Walk the store's pending tombstones, compute the backlog
    /// snapshot, and decide whether a forced compaction is due.
    ///
    /// `now_nanos` is the current Unix-epoch nanosecond stamp. It is
    /// passed in (rather than read from `SystemTime` here) so tests
    /// and deterministic replays can pin the clock.
    pub fn evaluate(
        &self,
        store: &dyn ErasureStore,
        now_nanos: i64,
    ) -> Result<ErasureBacklog, AeonError> {
        let pending = store.list_pending()?;
        let pending_count = pending.len();
        let Some(oldest) = pending.iter().min_by_key(|t| t.accepted_at_nanos) else {
            return Ok(ErasureBacklog {
                pending_count: 0,
                oldest_accepted_at_nanos: None,
                oldest_age_nanos: None,
                force_compaction: false,
            });
        };

        let age_nanos = if now_nanos >= oldest.accepted_at_nanos {
            // Safe: difference of two non-negative i64 values where the
            // minuend is the larger is always non-negative, and fits in
            // u64 because i64::MAX < u64::MAX.
            let diff = now_nanos.saturating_sub(oldest.accepted_at_nanos);
            u64::try_from(diff).unwrap_or(u64::MAX)
        } else {
            // Clock skew: oldest.accepted_at_nanos > now. Treat as
            // age-zero rather than as a negative number. A pending
            // tombstone cannot meaningfully be older than "right now".
            0
        };

        let force_compaction = age_nanos >= self.max_delay_nanos;
        Ok(ErasureBacklog {
            pending_count,
            oldest_accepted_at_nanos: Some(oldest.accepted_at_nanos),
            oldest_age_nanos: Some(age_nanos),
            force_compaction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{ErasureRequest, ErasureSelector, ErasureTombstone, TombstoneState};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    /// In-memory `ErasureStore` for policy tests. Mirrors the trait
    /// semantics without an L3 dependency.
    struct MemStore {
        inner: Mutex<Vec<ErasureTombstone>>,
    }

    impl MemStore {
        fn new() -> Self {
            Self {
                inner: Mutex::new(Vec::new()),
            }
        }
    }

    impl ErasureStore for MemStore {
        fn append(&self, t: &ErasureTombstone) -> Result<(), AeonError> {
            self.inner.lock().unwrap().push(t.clone());
            Ok(())
        }
        fn list_all(&self) -> Result<Vec<ErasureTombstone>, AeonError> {
            Ok(self.inner.lock().unwrap().clone())
        }
        fn list_pending(&self) -> Result<Vec<ErasureTombstone>, AeonError> {
            Ok(self
                .inner
                .lock()
                .unwrap()
                .iter()
                .filter(|t| matches!(t.state, TombstoneState::Pending))
                .cloned()
                .collect())
        }
        fn get(&self, id: &Uuid) -> Result<Option<ErasureTombstone>, AeonError> {
            Ok(self
                .inner
                .lock()
                .unwrap()
                .iter()
                .find(|t| &t.id == id)
                .cloned())
        }
        fn mark_state(&self, id: &Uuid, state: TombstoneState) -> Result<(), AeonError> {
            let mut g = self.inner.lock().unwrap();
            for t in g.iter_mut() {
                if &t.id == id {
                    t.state = state;
                    return Ok(());
                }
            }
            Err(AeonError::state(format!("tombstone {id} not found")))
        }
        fn pending_count(&self) -> Result<usize, AeonError> {
            Ok(self.list_pending()?.len())
        }
    }

    fn tombstone_at(accepted_at_nanos: i64) -> ErasureTombstone {
        let req = ErasureRequest {
            pipeline: Arc::from("pipe"),
            selector: ErasureSelector::parse("tenant-a/user-1").unwrap(),
            reason: None,
            soft_delete: None,
        };
        req.into_tombstone(accepted_at_nanos, Uuid::now_v7())
    }

    #[test]
    fn from_config_maps_hours_to_nanos() {
        let p = ErasurePolicy::from_config(&ErasureConfig { max_delay_hours: 1 });
        assert_eq!(p.max_delay_nanos(), 3_600 * 1_000_000_000);
        let p24 = ErasurePolicy::from_config(&ErasureConfig { max_delay_hours: 24 });
        assert_eq!(p24.max_delay_nanos(), 24 * 3_600 * 1_000_000_000);
    }

    #[test]
    fn from_config_saturates_on_absurd_hours() {
        // A user pasting u32::MAX would overflow u64 if we multiplied
        // naively. Policy must saturate rather than panic or wrap.
        let p = ErasurePolicy::from_config(&ErasureConfig {
            max_delay_hours: u32::MAX,
        });
        assert_eq!(p.max_delay_nanos(), u64::MAX);
    }

    #[test]
    fn evaluate_empty_store_reports_zero_backlog() {
        let store = MemStore::new();
        let p = ErasurePolicy::from_config(&ErasureConfig::default());
        let b = p.evaluate(&store, 1_000_000_000).unwrap();
        assert_eq!(b.pending_count, 0);
        assert!(b.oldest_accepted_at_nanos.is_none());
        assert!(b.oldest_age_nanos.is_none());
        assert!(!b.force_compaction);
    }

    #[test]
    fn evaluate_picks_oldest_pending() {
        let store = MemStore::new();
        store.append(&tombstone_at(5_000_000_000)).unwrap();
        store.append(&tombstone_at(1_000_000_000)).unwrap(); // oldest
        store.append(&tombstone_at(9_000_000_000)).unwrap();

        let p = ErasurePolicy::from_config(&ErasureConfig { max_delay_hours: 24 });
        let b = p.evaluate(&store, 10_000_000_000).unwrap();
        assert_eq!(b.pending_count, 3);
        assert_eq!(b.oldest_accepted_at_nanos, Some(1_000_000_000));
        assert_eq!(b.oldest_age_nanos, Some(9_000_000_000));
        assert!(!b.force_compaction, "9s < 24h, no force trigger");
    }

    #[test]
    fn evaluate_ignores_non_pending_tombstones() {
        let store = MemStore::new();
        // Older tombstone, but already Completed — must not drive the SLA.
        let mut old = tombstone_at(1_000_000_000);
        old.state = TombstoneState::Completed;
        store.append(&old).unwrap();
        // Younger tombstone, still Pending.
        store.append(&tombstone_at(8_000_000_000)).unwrap();

        let p = ErasurePolicy::from_config(&ErasureConfig::default());
        let b = p.evaluate(&store, 9_000_000_000).unwrap();
        assert_eq!(b.pending_count, 1);
        assert_eq!(b.oldest_accepted_at_nanos, Some(8_000_000_000));
    }

    #[test]
    fn force_compaction_fires_when_age_exceeds_threshold() {
        let store = MemStore::new();
        // Pending tombstone 2h old, policy = 1h → must force.
        let two_hours_nanos = 2 * 3_600 * 1_000_000_000_i64;
        store.append(&tombstone_at(0)).unwrap();
        let p = ErasurePolicy::from_config(&ErasureConfig { max_delay_hours: 1 });
        let b = p.evaluate(&store, two_hours_nanos).unwrap();
        assert!(b.force_compaction);
        assert_eq!(b.oldest_age_nanos, Some(two_hours_nanos as u64));
    }

    #[test]
    fn force_compaction_quiescent_below_threshold() {
        let store = MemStore::new();
        // Pending tombstone 30m old, policy = 1h → no force.
        let thirty_min_nanos = 30 * 60 * 1_000_000_000_i64;
        store.append(&tombstone_at(0)).unwrap();
        let p = ErasurePolicy::from_config(&ErasureConfig { max_delay_hours: 1 });
        let b = p.evaluate(&store, thirty_min_nanos).unwrap();
        assert!(!b.force_compaction);
    }

    #[test]
    fn force_compaction_boundary_exact_age() {
        // age == threshold → trigger. The SLA is "no older than N
        // hours", so the boundary must be inclusive.
        let store = MemStore::new();
        store.append(&tombstone_at(0)).unwrap();
        let one_hour_nanos = 3_600 * 1_000_000_000_i64;
        let p = ErasurePolicy::from_config(&ErasureConfig { max_delay_hours: 1 });
        let b = p.evaluate(&store, one_hour_nanos).unwrap();
        assert!(b.force_compaction);
    }

    #[test]
    fn clock_skew_clamps_age_to_zero() {
        // Tombstone timestamp is AHEAD of `now`. This can happen with
        // NTP jitter / leader-switch. Policy must not underflow or
        // saturate; it must treat the age as 0 and stay quiescent.
        let store = MemStore::new();
        store.append(&tombstone_at(10_000_000_000)).unwrap();
        let p = ErasurePolicy::from_config(&ErasureConfig::default());
        let b = p.evaluate(&store, 5_000_000_000).unwrap();
        assert_eq!(b.oldest_age_nanos, Some(0));
        assert!(!b.force_compaction);
    }

    #[test]
    fn default_policy_threshold_is_24h() {
        let p = ErasurePolicy::from_config(&ErasureConfig::default());
        assert_eq!(p.max_delay_nanos(), 24 * 3_600 * 1_000_000_000);
    }
}
