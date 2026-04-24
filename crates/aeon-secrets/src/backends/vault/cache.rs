//! In-memory TTL cache for KV-v2 resolves.
//!
//! KV-v2 reads happen at pipeline start (resolving auth tokens, KEK
//! refs, TLS keys). For long-running processes, re-resolving the same
//! ref — e.g. during L2/L3 re-open, config reload, or rotation of a
//! downstream kek — turns into a stream of hot requests back to Vault.
//! A small TTL cache absorbs those without relaxing the always-fresh
//! contract on the first read.
//!
//! ### Zeroization
//!
//! [`aeon_types::SecretBytes`] deliberately omits `Clone` to force a
//! grep-able call site before any copy. The cache holds a `SecretBytes`
//! directly (so eviction zeroizes), and returns a fresh
//! `SecretBytes::new(...)` to the caller by re-copying from
//! `expose_bytes()`. The extra allocation is deliberate — KV reads
//! are never on the hot path.
//!
//! ### TTL semantics
//!
//! - `ttl > 0`  → entries expire after `ttl` wall-clock seconds.
//! - `ttl == 0` → cache is disabled (`get` always misses, `put` is no-op).
//!
//! Expired entries are evicted lazily on read; we don't run a cleaner.

use aeon_types::SecretBytes;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) struct SecretCache {
    ttl: Duration,
    entries: Mutex<HashMap<String, CachedEntry>>,
}

struct CachedEntry {
    bytes: SecretBytes,
    inserted_at: Instant,
}

impl SecretCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    pub fn get(&self, key: &str) -> Option<SecretBytes> {
        if !self.is_enabled() {
            return None;
        }
        let mut guard = self.entries.lock().ok()?;
        let fresh = match guard.get(key) {
            Some(entry) if entry.inserted_at.elapsed() <= self.ttl => true,
            Some(_) => {
                guard.remove(key);
                return None;
            }
            None => return None,
        };
        if fresh {
            // Re-copy from the stored SecretBytes rather than cloning,
            // because SecretBytes is deliberately !Clone.
            let entry = guard.get(key)?;
            return Some(SecretBytes::new(entry.bytes.expose_bytes().to_vec()));
        }
        None
    }

    pub fn put(&self, key: String, bytes: SecretBytes) {
        if !self.is_enabled() {
            return;
        }
        if let Ok(mut guard) = self.entries.lock() {
            guard.insert(
                key,
                CachedEntry {
                    bytes,
                    inserted_at: Instant::now(),
                },
            );
        }
    }

    /// Drop a single key. Used when a caller knows the upstream value
    /// has rotated (e.g. after a 403 forces re-auth and the cached
    /// token is now stale).
    #[allow(dead_code)]
    pub fn invalidate(&self, key: &str) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.remove(key);
        }
    }

    /// Drop every entry. Used on manual registry refresh.
    #[allow(dead_code)]
    pub fn invalidate_all(&self) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
    }
}

impl std::fmt::Debug for SecretCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.entries.lock().map(|g| g.len()).unwrap_or(0);
        f.debug_struct("SecretCache")
            .field("ttl", &self.ttl)
            .field("entries", &len)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_cache_never_hits() {
        let c = SecretCache::new(Duration::ZERO);
        assert!(!c.is_enabled());
        c.put("k".to_string(), SecretBytes::new(vec![1, 2, 3]));
        assert!(c.get("k").is_none());
    }

    #[test]
    fn put_then_get_returns_same_bytes() {
        let c = SecretCache::new(Duration::from_secs(60));
        c.put("k".to_string(), SecretBytes::new(vec![9, 8, 7]));
        let got = c.get("k").unwrap();
        assert_eq!(got.expose_bytes(), &[9, 8, 7]);
    }

    #[test]
    fn get_after_ttl_expiry_returns_none() {
        let c = SecretCache::new(Duration::from_millis(20));
        c.put("k".to_string(), SecretBytes::new(vec![1]));
        std::thread::sleep(Duration::from_millis(40));
        assert!(c.get("k").is_none());
    }

    #[test]
    fn invalidate_removes_single_entry() {
        let c = SecretCache::new(Duration::from_secs(60));
        c.put("a".to_string(), SecretBytes::new(vec![1]));
        c.put("b".to_string(), SecretBytes::new(vec![2]));
        c.invalidate("a");
        assert!(c.get("a").is_none());
        assert!(c.get("b").is_some());
    }

    #[test]
    fn invalidate_all_clears_cache() {
        let c = SecretCache::new(Duration::from_secs(60));
        c.put("a".to_string(), SecretBytes::new(vec![1]));
        c.put("b".to_string(), SecretBytes::new(vec![2]));
        c.invalidate_all();
        assert!(c.get("a").is_none());
        assert!(c.get("b").is_none());
    }

    #[test]
    fn debug_redacts_values() {
        let c = SecretCache::new(Duration::from_secs(60));
        c.put("k".to_string(), SecretBytes::new(b"very-secret-value".to_vec()));
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("very-secret-value"), "leaked: {dbg}");
    }

    #[test]
    fn repeated_get_yields_independent_buffers() {
        // Because SecretBytes is !Clone, get() reallocates on each
        // call. Each return must be independent — callers may drop one
        // without affecting the other.
        let c = SecretCache::new(Duration::from_secs(60));
        c.put("k".to_string(), SecretBytes::new(vec![1, 2, 3]));
        let a = c.get("k").unwrap();
        let b = c.get("k").unwrap();
        assert_eq!(a.expose_bytes(), b.expose_bytes());
        assert_ne!(
            a.expose_bytes().as_ptr(),
            b.expose_bytes().as_ptr(),
            "expected distinct allocations"
        );
    }
}
