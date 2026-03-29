//! L1 state store — in-memory DashMap.
//!
//! Fastest tier: concurrent read/write without global locks.
//! DashMap uses shard-based locking — each shard is independently locked,
//! so concurrent access to different keys rarely contends.
//!
//! All values are stored as raw bytes (`Vec<u8>`). Typed wrappers
//! (ValueState, MapState, etc.) handle serialization on top.

use aeon_types::{AeonError, StateOps};
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// L1 in-memory state store backed by DashMap.
///
/// - Lock-free reads for non-contended keys
/// - Shard-based write locking (default 64 shards)
/// - Tracks approximate memory usage for promotion/demotion decisions
pub struct L1Store {
    data: DashMap<Vec<u8>, Vec<u8>>,
    /// Approximate total bytes stored (keys + values).
    approx_bytes: AtomicU64,
    /// Number of entries.
    entry_count: AtomicU64,
}

impl L1Store {
    /// Create a new empty L1 store.
    pub fn new() -> Self {
        Self {
            data: DashMap::new(),
            approx_bytes: AtomicU64::new(0),
            entry_count: AtomicU64::new(0),
        }
    }

    /// Create a new L1 store with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            data: DashMap::with_capacity(capacity),
            approx_bytes: AtomicU64::new(0),
            entry_count: AtomicU64::new(0),
        }
    }

    /// Number of entries in the store.
    pub fn len(&self) -> usize {
        self.entry_count.load(Ordering::Relaxed) as usize
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Approximate total memory usage in bytes (keys + values).
    pub fn approx_memory(&self) -> u64 {
        self.approx_bytes.load(Ordering::Relaxed)
    }

    /// Scan all keys with a given prefix.
    /// Returns key-value pairs where the key starts with `prefix`.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.data
            .iter()
            .filter(|entry| entry.key().starts_with(prefix))
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    /// Remove all entries. Resets memory tracking.
    pub fn clear(&self) {
        self.data.clear();
        self.approx_bytes.store(0, Ordering::Relaxed);
        self.entry_count.store(0, Ordering::Relaxed);
    }

    /// Check if a key exists without reading the value.
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.data.contains_key(key)
    }
}

impl Default for L1Store {
    fn default() -> Self {
        Self::new()
    }
}

impl StateOps for L1Store {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, AeonError> {
        Ok(self.data.get(key).map(|v| v.value().clone()))
    }

    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), AeonError> {
        let key_len = key.len() as u64;
        let val_len = value.len() as u64;

        if let Some(mut existing) = self.data.get_mut(key) {
            // Update existing entry — adjust memory tracking
            let old_len = existing.len() as u64;
            *existing = value.to_vec();
            // Adjust: remove old value size, add new value size
            if val_len > old_len {
                self.approx_bytes
                    .fetch_add(val_len - old_len, Ordering::Relaxed);
            } else {
                self.approx_bytes
                    .fetch_sub(old_len - val_len, Ordering::Relaxed);
            }
        } else {
            // New entry
            self.data.insert(key.to_vec(), value.to_vec());
            self.approx_bytes
                .fetch_add(key_len + val_len, Ordering::Relaxed);
            self.entry_count.fetch_add(1, Ordering::Relaxed);
        }

        Ok(())
    }

    async fn delete(&self, key: &[u8]) -> Result<(), AeonError> {
        if let Some((k, v)) = self.data.remove(key) {
            let freed = (k.len() + v.len()) as u64;
            self.approx_bytes.fetch_sub(freed, Ordering::Relaxed);
            self.entry_count.fetch_sub(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_missing_key_returns_none() {
        let store = L1Store::new();
        assert_eq!(store.get(b"missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn put_and_get() {
        let store = L1Store::new();
        store.put(b"key1", b"value1").await.unwrap();
        assert_eq!(store.get(b"key1").await.unwrap(), Some(b"value1".to_vec()));
    }

    #[tokio::test]
    async fn put_overwrites_existing() {
        let store = L1Store::new();
        store.put(b"key", b"old").await.unwrap();
        store.put(b"key", b"new").await.unwrap();
        assert_eq!(store.get(b"key").await.unwrap(), Some(b"new".to_vec()));
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let store = L1Store::new();
        store.put(b"key", b"value").await.unwrap();
        store.delete(b"key").await.unwrap();
        assert_eq!(store.get(b"key").await.unwrap(), None);
        assert_eq!(store.len(), 0);
    }

    #[tokio::test]
    async fn delete_nonexistent_is_ok() {
        let store = L1Store::new();
        store.delete(b"missing").await.unwrap();
    }

    #[tokio::test]
    async fn memory_tracking() {
        let store = L1Store::new();
        store.put(b"k1", b"v1").await.unwrap(); // 2 + 2 = 4 bytes
        assert_eq!(store.approx_memory(), 4);

        store.put(b"k2", b"longer_value").await.unwrap(); // 2 + 12 = 14
        assert_eq!(store.approx_memory(), 18);

        store.delete(b"k1").await.unwrap(); // -4
        assert_eq!(store.approx_memory(), 14);
    }

    #[tokio::test]
    async fn memory_tracking_on_update() {
        let store = L1Store::new();
        store.put(b"key", b"short").await.unwrap(); // 3 + 5 = 8
        assert_eq!(store.approx_memory(), 8);

        store.put(b"key", b"much_longer_value").await.unwrap(); // value: 5→17, delta +12
        assert_eq!(store.approx_memory(), 20); // 3 + 17
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn scan_prefix() {
        let store = L1Store::new();
        store.put(b"user:1:name", b"alice").await.unwrap();
        store.put(b"user:1:age", b"30").await.unwrap();
        store.put(b"user:2:name", b"bob").await.unwrap();
        store.put(b"order:1", b"data").await.unwrap();

        let user1 = store.scan_prefix(b"user:1:");
        assert_eq!(user1.len(), 2);

        let users = store.scan_prefix(b"user:");
        assert_eq!(users.len(), 3);

        let orders = store.scan_prefix(b"order:");
        assert_eq!(orders.len(), 1);

        let empty = store.scan_prefix(b"nonexistent:");
        assert_eq!(empty.len(), 0);
    }

    #[tokio::test]
    async fn contains_key_check() {
        let store = L1Store::new();
        assert!(!store.contains_key(b"key"));
        store.put(b"key", b"val").await.unwrap();
        assert!(store.contains_key(b"key"));
    }

    #[tokio::test]
    async fn clear_resets_everything() {
        let store = L1Store::new();
        store.put(b"k1", b"v1").await.unwrap();
        store.put(b"k2", b"v2").await.unwrap();
        assert_eq!(store.len(), 2);

        store.clear();
        assert_eq!(store.len(), 0);
        assert_eq!(store.approx_memory(), 0);
        assert!(store.is_empty());
    }
}
