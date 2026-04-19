//! EO-2 P10 — content-hash dedup for poll sources without durable cursors.
//!
//! Poll sources (HTTP polling, DNS, time-sampled) have no upstream offset, so
//! the engine can't tell an unchanged payload from a duplicate replay. The
//! EO-2 design (§4.3) prescribes:
//!
//! - Hash each payload with a fast non-cryptographic algorithm (xxhash3_64)
//! - Keep a TTL-windowed seen-set of recent hashes
//! - On each candidate, check-and-mark: if present, drop; else emit + record
//!
//! Target overhead per design doc: ~30 ns/event at steady state.
//!
//! This module is the standalone primitive. Wiring into poll connectors
//! (configured via `IdentityConfig::Compound { content_hash }`) is the
//! responsibility of each connector — this module provides the cheap,
//! correct core.
//!
//! L2-backed persistence of the seen-set across restarts is deliberately
//! deferred: the in-memory TTL seen-set is the hot-path cost; persistence
//! is an optimization for long windows that outlive a process.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aeon_types::error::AeonError;

/// Content-hash algorithm. Only `xxhash3_64` is supported today; the string
/// form lets the manifest accept additional algorithms without a breaking
/// change in the config surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentHashAlgorithm {
    Xxhash3_64,
}

impl ContentHashAlgorithm {
    pub fn parse(s: &str) -> Result<Self, AeonError> {
        match s {
            "xxhash3_64" | "xxh3_64" | "xxh3-64" => Ok(Self::Xxhash3_64),
            other => Err(AeonError::config(format!(
                "unsupported content_hash algorithm: {other} (supported: xxhash3_64)"
            ))),
        }
    }

    #[inline]
    fn hash(&self, bytes: &[u8]) -> u64 {
        match self {
            Self::Xxhash3_64 => xxhash_rust::xxh3::xxh3_64(bytes),
        }
    }
}

/// TTL-windowed content-hash dedup seen-set.
///
/// `check_and_mark(payload)` hashes the payload, evicts expired entries
/// opportunistically (not on every call — see below), then returns whether
/// the hash was already present within the window (if so, it's a duplicate).
///
/// **Sweep cadence**: `retain()` is O(n). Running it on every check turns
/// every dedup call into O(n) and destroys the ~30 ns/event target. Instead
/// we only sweep when `window / 8` has elapsed since the last sweep. Worst
/// case, an expired entry lives ~12.5 % longer than `window`, which is fine
/// because callers already chose `window` as a soft bound.
///
/// Not lock-free: uses a single `Mutex<HashMap>` so it can be safely shared
/// across a poll-source task's batch handling. Contention is bounded by
/// batch size — the happy path is a single lock per event.
pub struct ContentHashDedup {
    algorithm: ContentHashAlgorithm,
    window: Duration,
    inner: Mutex<DedupInner>,
}

struct DedupInner {
    map: HashMap<u64, Instant>,
    last_sweep: Instant,
}

impl ContentHashDedup {
    pub fn new(algorithm: ContentHashAlgorithm, window: Duration) -> Self {
        Self {
            algorithm,
            window,
            inner: Mutex::new(DedupInner {
                map: HashMap::new(),
                last_sweep: Instant::now(),
            }),
        }
    }

    /// Parse an algorithm string + build. Convenience for manifest wiring.
    pub fn from_config(algorithm: &str, window: Duration) -> Result<Self, AeonError> {
        Ok(Self::new(ContentHashAlgorithm::parse(algorithm)?, window))
    }

    /// Hash `payload` and check-and-mark.
    /// Returns `true` if the payload was already seen within the window.
    pub fn check_and_mark(&self, payload: &[u8]) -> bool {
        let h = self.algorithm.hash(payload);
        self.check_and_mark_hash(h, Instant::now())
    }

    /// Test-friendly core — lets tests inject a clock.
    fn check_and_mark_hash(&self, h: u64, now: Instant) -> bool {
        let Ok(mut g) = self.inner.lock() else {
            return false;
        };

        // Amortised sweep: only scan the map when more than `window / 8`
        // has elapsed since the last sweep. Keeps check_and_mark O(1) on
        // the hot path.
        let sweep_interval = self.window / 8;
        if now.duration_since(g.last_sweep) >= sweep_interval {
            let window = self.window;
            g.map
                .retain(|_, first_seen| now.duration_since(*first_seen) < window);
            g.last_sweep = now;
        }

        match g.map.get(&h).copied() {
            Some(prev) if now.duration_since(prev) < self.window => true,
            _ => {
                g.map.insert(h, now);
                false
            }
        }
    }

    /// Current number of tracked hashes — for metrics + tests.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.map.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Force eviction of expired entries. Not required on the hot path
    /// (check_and_mark sweeps opportunistically), but useful for tests and
    /// for long-lull operators wanting proactive memory reclaim.
    pub fn sweep_expired(&self) {
        let now = Instant::now();
        if let Ok(mut g) = self.inner.lock() {
            let window = self.window;
            g.map
                .retain(|_, first_seen| now.duration_since(*first_seen) < window);
            g.last_sweep = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithm_parse_accepts_known_aliases() {
        assert_eq!(
            ContentHashAlgorithm::parse("xxhash3_64").unwrap(),
            ContentHashAlgorithm::Xxhash3_64
        );
        assert_eq!(
            ContentHashAlgorithm::parse("xxh3_64").unwrap(),
            ContentHashAlgorithm::Xxhash3_64
        );
        assert_eq!(
            ContentHashAlgorithm::parse("xxh3-64").unwrap(),
            ContentHashAlgorithm::Xxhash3_64
        );
    }

    #[test]
    fn algorithm_parse_rejects_unknown() {
        assert!(ContentHashAlgorithm::parse("blake3").is_err());
        assert!(ContentHashAlgorithm::parse("").is_err());
    }

    #[test]
    fn first_seen_payload_returns_false() {
        let d = ContentHashDedup::new(
            ContentHashAlgorithm::Xxhash3_64,
            Duration::from_secs(60),
        );
        assert!(!d.check_and_mark(b"event-1"));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn repeat_payload_within_window_returns_true() {
        let d = ContentHashDedup::new(
            ContentHashAlgorithm::Xxhash3_64,
            Duration::from_secs(60),
        );
        assert!(!d.check_and_mark(b"duplicate"));
        assert!(d.check_and_mark(b"duplicate"));
        assert!(d.check_and_mark(b"duplicate"));
        assert_eq!(d.len(), 1, "same hash should not grow the map");
    }

    #[test]
    fn distinct_payloads_all_return_false_first_time() {
        let d = ContentHashDedup::new(
            ContentHashAlgorithm::Xxhash3_64,
            Duration::from_secs(60),
        );
        for i in 0..100 {
            let p = format!("distinct-{i}");
            assert!(!d.check_and_mark(p.as_bytes()));
        }
        assert_eq!(d.len(), 100);
    }

    #[test]
    fn expired_entry_is_swept_and_remarked_fresh() {
        // Inject a clock: use check_and_mark_hash directly so we can
        // fast-forward past the TTL window.
        let d = ContentHashDedup::new(
            ContentHashAlgorithm::Xxhash3_64,
            Duration::from_millis(10),
        );
        let t0 = Instant::now();
        assert!(!d.check_and_mark_hash(0xdeadbeef, t0));
        // Still within window.
        assert!(d.check_and_mark_hash(0xdeadbeef, t0 + Duration::from_millis(5)));
        // Past window — entry is swept, next call sees it fresh.
        assert!(!d.check_and_mark_hash(0xdeadbeef, t0 + Duration::from_millis(20)));
    }

    #[test]
    fn sweep_expired_shrinks_map() {
        let d = ContentHashDedup::new(
            ContentHashAlgorithm::Xxhash3_64,
            Duration::from_millis(1),
        );
        d.check_and_mark(b"a");
        d.check_and_mark(b"b");
        assert_eq!(d.len(), 2);
        std::thread::sleep(Duration::from_millis(5));
        d.sweep_expired();
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn from_config_builds_from_manifest_shape() {
        let d = ContentHashDedup::from_config("xxhash3_64", Duration::from_secs(300)).unwrap();
        assert!(!d.check_and_mark(b"x"));
        assert!(d.check_and_mark(b"x"));
    }
}
