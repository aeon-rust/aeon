//! CL-6d — Token-bucket bandwidth limiter for partition transfers.
//!
//! A partition bulk-sync can saturate the link between two cluster
//! nodes, starving the live pipeline traffic that shares the same QUIC
//! connection. `TransferThrottle` caps the source-side chunk-write rate
//! at a configured bytes/sec, with a burst window equal to one second
//! of budget. Default is "unlimited" — the engine is expected to plug
//! in a throttle sized to a fraction (30% per roadmap default) of the
//! observed link capacity.
//!
//! The throttle is deliberately stateless to the partition transfer
//! protocol — it exposes only `acquire(bytes)` which blocks until the
//! token budget allows the write. Callers wrap it around
//! `framing::write_frame` calls in the source's chunk loop.

use std::sync::Mutex;

use tokio::time::{Duration, Instant, sleep};

/// Rate limiter with a one-second burst window.
///
/// `bytes_per_sec == 0` disables the limiter entirely (`acquire` is a
/// no-op), which is the default for tests and for the first cut of
/// CL-6d where the engine hasn't observed link capacity yet.
pub struct TransferThrottle {
    bytes_per_sec: u64,
    state: Mutex<ThrottleState>,
}

struct ThrottleState {
    /// Available budget in bytes. May go negative to represent debt
    /// the limiter will "pay back" by sleeping on the next acquire.
    budget: i64,
    last_refill: Instant,
}

impl TransferThrottle {
    /// Construct a throttle at `bytes_per_sec`. `0` disables limiting.
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec,
            state: Mutex::new(ThrottleState {
                budget: bytes_per_sec as i64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Same as `new` but zero bytes/sec — explicit no-op constructor
    /// that callers can pass without branching.
    pub fn unlimited() -> Self {
        Self::new(0)
    }

    pub fn bytes_per_sec(&self) -> u64 {
        self.bytes_per_sec
    }

    /// Reserve `bytes` of budget, sleeping until the bucket recovers if
    /// the write would push the budget into overdraft. Returns
    /// immediately when the throttle is disabled (`bytes_per_sec == 0`).
    pub async fn acquire(&self, bytes: u64) {
        if self.bytes_per_sec == 0 {
            return;
        }

        // Compute the required sleep under the lock, then release it
        // before awaiting so we don't hold it across the await point.
        let sleep_for = {
            let mut state = match self.state.lock() {
                Ok(g) => g,
                // Poisoned — another caller panicked while holding the
                // lock. Treat as a no-op rather than propagating the
                // panic across the transfer path.
                Err(poisoned) => poisoned.into_inner(),
            };

            let now = Instant::now();
            let elapsed = now.saturating_duration_since(state.last_refill);
            let refill =
                (self.bytes_per_sec as u128).saturating_mul(elapsed.as_nanos()) / 1_000_000_000;
            // Cap burst at one second of budget.
            let cap = self.bytes_per_sec as i64;
            state.budget = (state
                .budget
                .saturating_add(refill.min(i64::MAX as u128) as i64))
            .min(cap);
            state.last_refill = now;

            state.budget = state.budget.saturating_sub(bytes as i64);

            if state.budget >= 0 {
                Duration::ZERO
            } else {
                let debt_ns = (-state.budget as u128).saturating_mul(1_000_000_000)
                    / (self.bytes_per_sec as u128);
                Duration::from_nanos(debt_ns.min(u64::MAX as u128) as u64)
            }
        };

        if sleep_for > Duration::ZERO {
            sleep(sleep_for).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn unlimited_throttle_is_a_noop() {
        let t = TransferThrottle::unlimited();
        let start = Instant::now();
        t.acquire(1_000_000_000).await;
        let elapsed = Instant::now().saturating_duration_since(start);
        assert!(
            elapsed < Duration::from_millis(1),
            "unlimited throttle must not sleep, got {:?}",
            elapsed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn small_acquire_within_burst_budget_does_not_sleep() {
        // 1 MiB/s limit, 1 s burst → first 1 MiB acquire should be
        // instant (budget starts at bytes_per_sec).
        let t = TransferThrottle::new(1_000_000);
        let start = Instant::now();
        t.acquire(500_000).await; // well under the 1 MiB burst
        let elapsed = Instant::now().saturating_duration_since(start);
        assert!(
            elapsed < Duration::from_millis(1),
            "acquire inside burst budget must not sleep, got {:?}",
            elapsed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_beyond_budget_sleeps_for_the_debt() {
        // 1 MiB/s limit; drain the burst then request another 1 MiB —
        // must sleep exactly 1 s for the budget to refill.
        let t = TransferThrottle::new(1_000_000);
        t.acquire(1_000_000).await; // drain burst
        let start = Instant::now();
        t.acquire(1_000_000).await;
        let elapsed = Instant::now().saturating_duration_since(start);
        // start_paused=true + sleep advances the clock exactly; tolerate
        // a few nanos for arithmetic rounding.
        assert!(
            elapsed >= Duration::from_millis(999),
            "must sleep ~1 s for the refill, got {:?}",
            elapsed
        );
        assert!(
            elapsed <= Duration::from_millis(1_010),
            "must not over-sleep, got {:?}",
            elapsed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_rate_respects_limit() {
        // Drain the initial burst so the test measures steady-state
        // refill rather than the 1 s of burst headroom. Send 10 × 1 MiB
        // chunks at 1 MiB/s; steady state must take ~10 s.
        let t = Arc::new(TransferThrottle::new(1_000_000));
        t.acquire(1_000_000).await;
        let start = Instant::now();
        for _ in 0..10 {
            t.acquire(1_000_000).await;
        }
        let elapsed = Instant::now().saturating_duration_since(start);
        assert!(
            elapsed >= Duration::from_secs(10),
            "10 × 1 MiB at 1 MiB/s must take at least 10 s (post-burst), got {:?}",
            elapsed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn burst_cap_limits_idle_accumulation() {
        // Idle for 100 s at 1 MiB/s — budget must cap at 1 MiB (one
        // second of headroom), not 100 MiB. Draining 2 MiB back-to-back
        // after the idle period should still sleep ~1 s for the second
        // half.
        let t = TransferThrottle::new(1_000_000);
        tokio::time::advance(Duration::from_secs(100)).await;
        t.acquire(1_000_000).await; // uses the capped burst
        let start = Instant::now();
        t.acquire(1_000_000).await; // must wait ~1 s
        let elapsed = Instant::now().saturating_duration_since(start);
        assert!(
            elapsed >= Duration::from_millis(999),
            "burst must be capped at 1 s of budget, got {:?}",
            elapsed
        );
    }
}
