//! Per-partition write gate — primitive for partition-handover cutover.
//!
//! A `WriteGate` is the engine-side seam through which the cluster
//! coordinator (cluster crate) can pause source-side ingestion for a
//! specific (pipeline, partition) tuple, await drain of any in-flight
//! fetch, and read a stable watermark. It backs the `CutoverCoordinator`
//! trait expected by `aeon_cluster::transport::cutover`.
//!
//! Design (matches roadmap "Session A — G2"):
//!
//! - Three-state lifecycle: `Open` → `FreezeRequested` → `Frozen`.
//! - The source-side ingestion task calls [`WriteGate::try_enter`] before
//!   each `next_batch` fetch. While `Open`, this hands back a
//!   [`DrainGuard`] (RAII) that increments an in-flight counter; the guard
//!   decrements on drop and wakes any drain waiter when the counter hits
//!   zero. While `FreezeRequested` or `Frozen`, `try_enter` returns
//!   `None` and the source breaks out of its loop.
//! - The cluster coordinator calls
//!   [`WriteGate::request_freeze_and_drain`] which flips the state to
//!   `FreezeRequested`, then awaits the in-flight counter reaching zero
//!   (or a configurable timeout). On success, the gate transitions to
//!   `Frozen` atomically with the zero-observation, so cutover can read
//!   final source/PoH watermarks without the source restarting a fetch.
//!
//! Concurrency invariant: source must call `try_enter` *before* fetching.
//! Holding the read-lock across the in-flight increment ensures that a
//! racing freeze request blocks until in-flight readers finish; once the
//! freeze is committed, every subsequent `try_enter` returns `None`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;

use aeon_types::partition::PartitionId;

/// Lifecycle state of a [`WriteGate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    /// Default: source is free to fetch new batches.
    Open,
    /// Cutover requested freeze. Source must stop fetching at the next
    /// iteration; in-flight batches drain naturally.
    FreezeRequested,
    /// Source has stopped (in-flight count reached zero under
    /// `FreezeRequested`). Watermarks are stable; cutover may sample.
    Frozen,
}

/// Per-partition write gate. Cheap to clone via `Arc`. Owned by
/// [`WriteGateRegistry`] and looked up by `(pipeline, partition)`.
pub struct WriteGate {
    state: RwLock<GateState>,
    in_flight: AtomicU64,
    notify: Notify,
}

impl WriteGate {
    /// Construct a new gate in `Open` state.
    pub fn new() -> Self {
        Self {
            state: RwLock::new(GateState::Open),
            in_flight: AtomicU64::new(0),
            notify: Notify::new(),
        }
    }

    /// Snapshot current state. Cheap; for tests and operator-facing
    /// `aeon cluster status --watch`.
    pub fn state(&self) -> GateState {
        // RwLock read holds for the duration of one cheap copy; never
        // contended on the hot path because the writer (freeze request)
        // is rare. Poisoning is mapped to `Open` defensively — a poisoned
        // gate just means a drain task panicked, and we'd rather keep the
        // pipeline running than wedge it.
        self.state
            .read()
            .map(|s| *s)
            .unwrap_or(GateState::Open)
    }

    /// Source-side: attempt to enter the fetch path. Returns `Some(guard)`
    /// while `Open`; returns `None` once `FreezeRequested` or `Frozen`.
    /// The guard must be dropped after the fetch completes (or the source
    /// task itself terminates) so a concurrent drain can observe zero.
    pub fn try_enter(self: &Arc<Self>) -> Option<DrainGuard> {
        // Take the read lock for the duration of the increment. A racing
        // `request_freeze` blocks on the write lock until we release —
        // either we commit the increment (drain will wait for our guard
        // drop) or we observe non-Open and bail.
        let state = self.state.read().ok()?;
        if !matches!(*state, GateState::Open) {
            return None;
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        drop(state);
        Some(DrainGuard {
            gate: Arc::clone(self),
        })
    }

    /// Whether a freeze has been requested. Cheaper than `state()` for
    /// the source loop's per-iteration check (used as an early-out hint;
    /// the authoritative check is `try_enter`).
    pub fn is_freeze_requested(&self) -> bool {
        !matches!(self.state(), GateState::Open)
    }

    /// Cluster-coordinator side: flip to `FreezeRequested`, then await
    /// in-flight drain to zero. On success, transition to `Frozen` and
    /// return `Ok(())`. Returns `Err(DrainError::Timeout)` if the in-flight
    /// counter does not reach zero within `drain_timeout`. Idempotent
    /// when already `Frozen` (returns `Ok` immediately).
    pub async fn request_freeze_and_drain(
        &self,
        drain_timeout: Duration,
    ) -> Result<(), DrainError> {
        // Idempotency fast-path.
        if matches!(self.state(), GateState::Frozen) {
            return Ok(());
        }

        // Flip Open→FreezeRequested under write lock so try_enter readers
        // either committed before us (in_flight reflects them) or will
        // observe the new state and bail.
        {
            let mut s = self
                .state
                .write()
                .map_err(|_| DrainError::PoisonedState)?;
            if matches!(*s, GateState::Open) {
                *s = GateState::FreezeRequested;
            }
        }
        // Wake any source loop parked waiting for state changes.
        self.notify.notify_waiters();

        let drain = async {
            loop {
                if self.in_flight.load(Ordering::Acquire) == 0 {
                    // Atomically transition to Frozen. Re-check under
                    // write lock to defend against a concurrent reopen.
                    let mut s = self
                        .state
                        .write()
                        .map_err(|_| DrainError::PoisonedState)?;
                    if matches!(*s, GateState::FreezeRequested) {
                        *s = GateState::Frozen;
                    }
                    return Ok::<(), DrainError>(());
                }
                // Park until the next guard drop (or freeze flip) wakes us.
                self.notify.notified().await;
            }
        };

        match timeout(drain_timeout, drain).await {
            Ok(res) => res,
            Err(_) => Err(DrainError::Timeout),
        }
    }

    /// Reset the gate to `Open`. Used on transfer abort or test reset.
    /// Must not be called while a `request_freeze_and_drain` future is
    /// still pending — caller responsibility (the cluster driver
    /// guarantees this by serialising freeze/abort per partition).
    pub fn reopen(&self) {
        if let Ok(mut s) = self.state.write() {
            *s = GateState::Open;
        }
        self.notify.notify_waiters();
    }

    /// Test-only: current in-flight count. Useful for assertions on race
    /// behavior; not exposed to production callers.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Acquire)
    }
}

impl Default for WriteGate {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard returned by [`WriteGate::try_enter`]. Decrements the
/// in-flight counter on drop and wakes a parked drain waiter when the
/// counter transitions to zero.
pub struct DrainGuard {
    gate: Arc<WriteGate>,
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        let prev = self.gate.in_flight.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Last in-flight just released — wake any drain waiter.
            // notify_waiters is cheap when there are no waiters.
            self.gate.notify.notify_waiters();
        }
    }
}

/// Error returned by [`WriteGate::request_freeze_and_drain`].
#[derive(Debug)]
pub enum DrainError {
    /// In-flight counter did not reach zero within the drain timeout.
    Timeout,
    /// The gate's state `RwLock` was poisoned by a panicked writer.
    PoisonedState,
}

impl std::fmt::Display for DrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => f.write_str("write-gate drain timed out"),
            Self::PoisonedState => f.write_str("write-gate state lock poisoned"),
        }
    }
}

impl std::error::Error for DrainError {}

/// Process-wide registry of [`WriteGate`]s, keyed by `(pipeline,
/// partition)`. One instance lives on `PipelineSupervisor`; the cluster
/// `CutoverCoordinator` impl looks up gates here.
///
/// Lookup is `O(1)` via `HashMap`; locking is a `std::sync::RwLock`
/// since registry mutations (start/stop pipeline) are rare and reads
/// (cutover, source loop) are not on the per-event hot path.
pub struct WriteGateRegistry {
    inner: RwLock<HashMap<(String, PartitionId), Arc<WriteGate>>>,
}

impl WriteGateRegistry {
    /// New empty registry.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Get the gate for `(pipeline, partition)`, creating one if absent.
    /// Called by `PipelineSupervisor::start` so each partition pipeline
    /// has a gate ready for the cluster coordinator to find.
    pub fn get_or_create(&self, pipeline: &str, partition: PartitionId) -> Arc<WriteGate> {
        // Fast path: read lock, hit existing entry.
        if let Ok(map) = self.inner.read() {
            if let Some(g) = map.get(&(pipeline.to_string(), partition)) {
                return Arc::clone(g);
            }
        }
        // Slow path: take write lock and re-check.
        match self.inner.write() {
            Ok(mut map) => map
                .entry((pipeline.to_string(), partition))
                .or_insert_with(|| Arc::new(WriteGate::new()))
                .clone(),
            // Poisoned: return a fresh detached gate so the caller still
            // gets a usable handle; cleanup is left to operator restart.
            Err(_) => Arc::new(WriteGate::new()),
        }
    }

    /// Get the gate for `(pipeline, partition)` if it exists. Used by the
    /// cluster `CutoverCoordinator` impl — returns `None` if the partition
    /// is not running here, which the coordinator maps to a transport-level
    /// error.
    pub fn get(&self, pipeline: &str, partition: PartitionId) -> Option<Arc<WriteGate>> {
        self.inner
            .read()
            .ok()?
            .get(&(pipeline.to_string(), partition))
            .cloned()
    }

    /// Drop all gates belonging to `pipeline`. Called by
    /// `PipelineSupervisor::stop` so a restarted pipeline starts with
    /// fresh gates (avoids state bleed across restarts).
    pub fn remove_pipeline(&self, pipeline: &str) {
        if let Ok(mut map) = self.inner.write() {
            map.retain(|(p, _), _| p != pipeline);
        }
    }

    /// Test-only: number of gates currently registered.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.read().map(|m| m.len()).unwrap_or(0)
    }
}

impl Default for WriteGateRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn open_gate_admits_try_enter() {
        let g = Arc::new(WriteGate::new());
        let guard = g.try_enter();
        assert!(guard.is_some());
        assert_eq!(g.state(), GateState::Open);
        assert_eq!(g.in_flight(), 1);
        drop(guard);
        assert_eq!(g.in_flight(), 0);
    }

    #[tokio::test]
    async fn freeze_with_no_inflight_returns_immediately() {
        let g = Arc::new(WriteGate::new());
        g.request_freeze_and_drain(Duration::from_millis(100))
            .await
            .expect("drain ok");
        assert_eq!(g.state(), GateState::Frozen);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn freeze_waits_for_inflight_drain() {
        let g = Arc::new(WriteGate::new());
        let guard = g.try_enter().expect("open");
        assert_eq!(g.in_flight(), 1);

        let g_drain = Arc::clone(&g);
        let drain_task = tokio::spawn(async move {
            g_drain
                .request_freeze_and_drain(Duration::from_secs(2))
                .await
        });

        // Hold the guard for ~50 ms, then release.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Drain task must still be waiting.
        assert_eq!(g.state(), GateState::FreezeRequested);
        drop(guard);

        let res = drain_task.await.expect("join");
        assert!(res.is_ok());
        assert_eq!(g.state(), GateState::Frozen);
        assert_eq!(g.in_flight(), 0);
    }

    #[tokio::test]
    async fn try_enter_returns_none_after_freeze() {
        let g = Arc::new(WriteGate::new());
        g.request_freeze_and_drain(Duration::from_millis(100))
            .await
            .expect("drain");
        assert!(g.try_enter().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_timeout_returns_err() {
        let g = Arc::new(WriteGate::new());
        // Take a guard and never drop it.
        let _guard = g.try_enter().expect("open");
        let res = g
            .request_freeze_and_drain(Duration::from_millis(50))
            .await;
        assert!(matches!(res, Err(DrainError::Timeout)));
        // State remains FreezeRequested — caller must reopen() if it
        // wants to abort.
        assert_eq!(g.state(), GateState::FreezeRequested);
    }

    #[tokio::test]
    async fn reopen_resets_state_and_admits_again() {
        let g = Arc::new(WriteGate::new());
        g.request_freeze_and_drain(Duration::from_millis(100))
            .await
            .expect("drain");
        assert_eq!(g.state(), GateState::Frozen);
        g.reopen();
        assert_eq!(g.state(), GateState::Open);
        assert!(g.try_enter().is_some());
    }

    #[tokio::test]
    async fn double_freeze_is_idempotent() {
        let g = Arc::new(WriteGate::new());
        g.request_freeze_and_drain(Duration::from_millis(50))
            .await
            .expect("first drain");
        // Second call short-circuits on Frozen fast-path.
        g.request_freeze_and_drain(Duration::from_millis(50))
            .await
            .expect("second drain");
        assert_eq!(g.state(), GateState::Frozen);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_drains_both_succeed() {
        let g = Arc::new(WriteGate::new());
        let g1 = Arc::clone(&g);
        let g2 = Arc::clone(&g);
        let t1 = tokio::spawn(async move {
            g1.request_freeze_and_drain(Duration::from_millis(500)).await
        });
        let t2 = tokio::spawn(async move {
            g2.request_freeze_and_drain(Duration::from_millis(500)).await
        });
        assert!(t1.await.expect("join1").is_ok());
        assert!(t2.await.expect("join2").is_ok());
        assert_eq!(g.state(), GateState::Frozen);
    }

    #[tokio::test]
    async fn registry_lookup_is_per_partition() {
        let reg = WriteGateRegistry::new();
        let g0 = reg.get_or_create("p1", PartitionId::new(0));
        let g1 = reg.get_or_create("p1", PartitionId::new(1));
        // Different partitions get independent gates.
        assert!(!Arc::ptr_eq(&g0, &g1));
        // Same key returns same Arc.
        let g0_again = reg.get_or_create("p1", PartitionId::new(0));
        assert!(Arc::ptr_eq(&g0, &g0_again));
        // get() returns the same handle when present.
        let g0_get = reg.get("p1", PartitionId::new(0)).expect("present");
        assert!(Arc::ptr_eq(&g0, &g0_get));
        // Missing entries return None without creating.
        assert!(reg.get("p1", PartitionId::new(99)).is_none());
        assert_eq!(reg.len(), 2);
    }

    #[tokio::test]
    async fn registry_remove_pipeline_drops_only_that_pipeline() {
        let reg = WriteGateRegistry::new();
        let _ = reg.get_or_create("p1", PartitionId::new(0));
        let _ = reg.get_or_create("p1", PartitionId::new(1));
        let _ = reg.get_or_create("p2", PartitionId::new(0));
        assert_eq!(reg.len(), 3);
        reg.remove_pipeline("p1");
        assert_eq!(reg.len(), 1);
        assert!(reg.get("p1", PartitionId::new(0)).is_none());
        assert!(reg.get("p2", PartitionId::new(0)).is_some());
    }

    /// Multi-partition isolation: freezing partition 0 must not affect
    /// the source loop for partition 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sibling_partition_is_unaffected_by_freeze() {
        let reg = WriteGateRegistry::new();
        let g0 = reg.get_or_create("p", PartitionId::new(0));
        let g1 = reg.get_or_create("p", PartitionId::new(1));

        g0.request_freeze_and_drain(Duration::from_millis(100))
            .await
            .expect("drain p0");

        assert_eq!(g0.state(), GateState::Frozen);
        assert!(g0.try_enter().is_none());
        // Sibling partition: still Open, still admits.
        assert_eq!(g1.state(), GateState::Open);
        assert!(g1.try_enter().is_some());
    }

    /// Race: freeze requested while a guard is mid-flight. The drain
    /// must see in_flight=1, wait for drop, then transition Frozen.
    /// After Frozen, a fresh try_enter must return None even though it
    /// could in principle race the writer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn freeze_during_inflight_then_no_new_entries() {
        let g = Arc::new(WriteGate::new());
        let guard = g.try_enter().expect("first enter");

        let g_freeze = Arc::clone(&g);
        let freeze_task = tokio::spawn(async move {
            g_freeze
                .request_freeze_and_drain(Duration::from_secs(1))
                .await
        });

        // Give freeze a moment to flip state.
        tokio::time::sleep(Duration::from_millis(20)).await;
        // While in FreezeRequested, no new entries.
        assert!(g.try_enter().is_none());
        // Release the original guard; freeze can complete.
        drop(guard);
        assert!(freeze_task.await.expect("join").is_ok());
        assert_eq!(g.state(), GateState::Frozen);
        assert!(g.try_enter().is_none());
    }
}
