//! Reconnect/retry backoff policy with exponential growth and jitter (TR-3).
//!
//! Shared across every connector that has a reconnect or retry loop
//! (MQTT, Kafka, Postgres CDC, HTTP polling, cluster QUIC, WebTransport, ...).
//!
//! A fixed-sleep retry loop (e.g., `sleep(1s)` after each failure) turns a
//! transient upstream outage into a thundering-herd reconnect storm: every
//! node reconnects in lockstep, amplifying load on the already-sick upstream
//! and delaying recovery. Exponential backoff with random jitter spreads
//! reconnect attempts in time, producing a gentler recovery curve.
//!
//! Usage:
//! ```ignore
//! let policy = BackoffPolicy::default();
//! let mut backoff = policy.iter();
//! loop {
//!     match try_connect().await {
//!         Ok(conn) => break conn,
//!         Err(_) => tokio::time::sleep(backoff.next_delay()).await,
//!     }
//! }
//! ```

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Backoff policy configuration. Cheap to clone.
///
/// Defaults: 100 ms → 30 s cap, multiplier 2.0, ±20 % jitter.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BackoffPolicy {
    /// First delay in milliseconds, before any growth.
    pub initial_ms: u64,
    /// Upper bound in milliseconds — delay never exceeds this.
    pub max_ms: u64,
    /// Per-attempt growth factor. Must be ≥ 1.0.
    pub multiplier: f64,
    /// Jitter as a fraction of the current delay, in [0.0, 1.0].
    /// `0.2` means the delay is scaled by a random factor in `[0.8, 1.2]`.
    pub jitter_pct: f64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            initial_ms: 100,
            max_ms: 30_000,
            multiplier: 2.0,
            jitter_pct: 0.2,
        }
    }
}

impl BackoffPolicy {
    /// Construct a new stateful iterator over delays.
    pub fn iter(&self) -> Backoff {
        Backoff::new(*self)
    }

    /// Validate the policy. Returns `Err` with a description if any field is unusable.
    pub fn validate(&self) -> Result<(), String> {
        if self.initial_ms == 0 {
            return Err("backoff.initial_ms must be > 0".to_string());
        }
        if self.max_ms < self.initial_ms {
            return Err(format!(
                "backoff.max_ms ({}) must be >= initial_ms ({})",
                self.max_ms, self.initial_ms
            ));
        }
        if !self.multiplier.is_finite() || self.multiplier < 1.0 {
            return Err(format!(
                "backoff.multiplier ({}) must be finite and >= 1.0",
                self.multiplier
            ));
        }
        if !self.jitter_pct.is_finite() || !(0.0..=1.0).contains(&self.jitter_pct) {
            return Err(format!(
                "backoff.jitter_pct ({}) must be in [0.0, 1.0]",
                self.jitter_pct
            ));
        }
        Ok(())
    }
}

/// Stateful iterator that produces the next delay on each call.
///
/// Growth is geometric up to `max_ms`; each returned value is then scaled by
/// a uniform random factor in `[1 - jitter_pct, 1 + jitter_pct]`.
///
/// Jitter uses a simple xorshift RNG seeded from process time — cheap and
/// good enough for reconnect scheduling (no cryptographic needs).
#[derive(Debug, Clone)]
pub struct Backoff {
    policy: BackoffPolicy,
    /// Next delay to return, pre-jitter, in milliseconds.
    current_ms: f64,
    rng_state: u64,
}

impl Backoff {
    pub fn new(policy: BackoffPolicy) -> Self {
        let seed = seed_from_clock();
        Self {
            current_ms: policy.initial_ms as f64,
            policy,
            rng_state: seed.max(1), // xorshift requires non-zero state
        }
    }

    /// Reset the backoff back to the initial delay — call after a successful attempt.
    pub fn reset(&mut self) {
        self.current_ms = self.policy.initial_ms as f64;
    }

    /// Return the next delay and advance internal state.
    pub fn next_delay(&mut self) -> Duration {
        let base_ms = self.current_ms.min(self.policy.max_ms as f64);

        // Grow for the *next* call (saturates at max_ms).
        self.current_ms = (self.current_ms * self.policy.multiplier).min(self.policy.max_ms as f64);

        // Apply symmetric jitter: factor in [1 - pct, 1 + pct]
        let jitter = self.policy.jitter_pct;
        let factor = if jitter > 0.0 {
            let r = self.next_f64_unit(); // in [0, 1)
            1.0 - jitter + 2.0 * jitter * r
        } else {
            1.0
        };

        let delay_ms = (base_ms * factor).max(0.0);
        Duration::from_millis(delay_ms as u64)
    }

    /// Advance the xorshift RNG and return a value in [0, 1).
    fn next_f64_unit(&mut self) -> f64 {
        // xorshift64*
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        // Use top 53 bits for f64 precision.
        ((x >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

fn seed_from_clock() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_validates() {
        BackoffPolicy::default().validate().unwrap();
    }

    #[test]
    fn rejects_zero_initial() {
        let p = BackoffPolicy {
            initial_ms: 0,
            ..Default::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn rejects_max_lt_initial() {
        let p = BackoffPolicy {
            initial_ms: 1000,
            max_ms: 500,
            ..Default::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn rejects_multiplier_below_one() {
        let p = BackoffPolicy {
            multiplier: 0.5,
            ..Default::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn rejects_jitter_out_of_range() {
        let p = BackoffPolicy {
            jitter_pct: 1.5,
            ..Default::default()
        };
        assert!(p.validate().is_err());
        let p = BackoffPolicy {
            jitter_pct: -0.1,
            ..Default::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn no_jitter_gives_deterministic_exponential_sequence() {
        let policy = BackoffPolicy {
            initial_ms: 100,
            max_ms: 10_000,
            multiplier: 2.0,
            jitter_pct: 0.0,
        };
        let mut b = policy.iter();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
        assert_eq!(b.next_delay(), Duration::from_millis(200));
        assert_eq!(b.next_delay(), Duration::from_millis(400));
        assert_eq!(b.next_delay(), Duration::from_millis(800));
    }

    #[test]
    fn delay_saturates_at_max() {
        let policy = BackoffPolicy {
            initial_ms: 100,
            max_ms: 500,
            multiplier: 10.0,
            jitter_pct: 0.0,
        };
        let mut b = policy.iter();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
        assert_eq!(b.next_delay(), Duration::from_millis(500));
        assert_eq!(b.next_delay(), Duration::from_millis(500));
        assert_eq!(b.next_delay(), Duration::from_millis(500));
    }

    #[test]
    fn reset_returns_to_initial() {
        let policy = BackoffPolicy {
            initial_ms: 100,
            max_ms: 10_000,
            multiplier: 2.0,
            jitter_pct: 0.0,
        };
        let mut b = policy.iter();
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
    }

    #[test]
    fn jitter_stays_within_band() {
        let policy = BackoffPolicy {
            initial_ms: 1000,
            max_ms: 1000, // fixed base
            multiplier: 1.0,
            jitter_pct: 0.2,
        };
        let mut b = policy.iter();
        // With base=1000ms and ±20% jitter, every sample must be in [800, 1200].
        for _ in 0..200 {
            let d = b.next_delay().as_millis() as u64;
            assert!(d >= 800, "delay too low: {d}");
            assert!(d <= 1200, "delay too high: {d}");
        }
    }

    #[test]
    fn jitter_produces_variation() {
        let policy = BackoffPolicy {
            initial_ms: 1000,
            max_ms: 1000,
            multiplier: 1.0,
            jitter_pct: 0.3,
        };
        let mut b = policy.iter();
        let samples: Vec<u64> = (0..50).map(|_| b.next_delay().as_millis() as u64).collect();
        let unique: std::collections::BTreeSet<_> = samples.iter().collect();
        assert!(unique.len() > 5, "expected variation, got {unique:?}");
    }

    #[test]
    fn policy_serde_roundtrip() {
        let p = BackoffPolicy::default();
        let json = serde_json::to_string(&p).unwrap();
        let decoded: BackoffPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(p, decoded);
    }
}
