//! Tiered state store — L1 (DashMap) → L2 (mmap) → L3 (persistent).
//!
//! Read path:  L1 hit → return | L1 miss → L2 hit → promote to L1 → return | L2 miss → L3
//! Write path: Write to L1 (hot). Background promotion/demotion moves data between tiers.
//!
//! Currently only L1 is implemented. L2 (mmap-backed) and L3 (RocksDB) are placeholders
//! for future phases. The tiered store provides the abstraction now so that upstream code
//! (typed wrappers, engine) doesn't need to change when lower tiers are added.

use aeon_types::{AeonError, StateOps};

use crate::l1::L1Store;

/// Configuration for tier promotion/demotion thresholds.
#[derive(Debug, Clone)]
pub struct TieredConfig {
    /// Maximum approximate bytes in L1 before demotion to L2 is considered.
    pub l1_max_bytes: u64,
    /// Whether L2 (mmap) tier is enabled.
    pub l2_enabled: bool,
    /// Whether L3 (persistent) tier is enabled.
    pub l3_enabled: bool,
}

impl Default for TieredConfig {
    fn default() -> Self {
        Self {
            l1_max_bytes: 256 * 1024 * 1024, // 256 MiB
            l2_enabled: false,
            l3_enabled: false,
        }
    }
}

/// Multi-tier state store with read-through and write-through semantics.
///
/// - **L1** (DashMap): fastest, in-memory, volatile. All reads/writes go here first.
/// - **L2** (mmap): medium speed, memory-mapped files. Future implementation.
/// - **L3** (RocksDB): persistent, survives restarts. Future implementation.
///
/// Read path tries L1 first, then falls through to lower tiers, promoting on hit.
/// Write path always writes to L1. Demotion runs asynchronously when L1 exceeds thresholds.
pub struct TieredStore {
    l1: L1Store,
    config: TieredConfig,
    // L2 and L3 fields will be added when those tiers are implemented.
    // l2: Option<L2Store>,
    // l3: Option<L3Store>,
}

impl TieredStore {
    /// Create a new tiered store with default configuration (L1 only).
    pub fn new() -> Self {
        Self {
            l1: L1Store::new(),
            config: TieredConfig::default(),
        }
    }

    /// Create a tiered store with custom configuration.
    pub fn with_config(config: TieredConfig) -> Self {
        Self {
            l1: L1Store::new(),
            config,
        }
    }

    /// Get a reference to the L1 store.
    pub fn l1(&self) -> &L1Store {
        &self.l1
    }

    /// Current configuration.
    pub fn config(&self) -> &TieredConfig {
        &self.config
    }

    /// Approximate memory used by L1.
    pub fn l1_memory(&self) -> u64 {
        self.l1.approx_memory()
    }

    /// Number of entries in L1.
    pub fn l1_entries(&self) -> usize {
        self.l1.len()
    }

    /// Check if L1 is above its memory threshold.
    pub fn l1_over_threshold(&self) -> bool {
        self.l1.approx_memory() > self.config.l1_max_bytes
    }

    /// Scan L1 for keys with a given prefix.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.l1.scan_prefix(prefix)
    }

    /// Clear all tiers.
    pub fn clear(&self) {
        self.l1.clear();
        // Future: clear L2/L3 as well
    }
}

impl Default for TieredStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StateOps for TieredStore {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, AeonError> {
        // L1 lookup
        if let Some(value) = self.l1.get(key).await? {
            return Ok(Some(value));
        }

        // Future: L2 lookup → promote to L1 on hit
        // Future: L3 lookup → promote to L1 on hit

        Ok(None)
    }

    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), AeonError> {
        // Always write to L1 (hot tier)
        self.l1.put(key, value).await?;

        // Future: async write-behind to L3 for durability
        // Future: if L1 over threshold, schedule demotion

        Ok(())
    }

    async fn delete(&self, key: &[u8]) -> Result<(), AeonError> {
        self.l1.delete(key).await?;

        // Future: tombstone in L2/L3

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tiered_basic_operations() {
        let store = TieredStore::new();

        // Put and get
        store.put(b"key1", b"value1").await.unwrap();
        assert_eq!(store.get(b"key1").await.unwrap(), Some(b"value1".to_vec()));

        // Missing key
        assert_eq!(store.get(b"missing").await.unwrap(), None);

        // Delete
        store.delete(b"key1").await.unwrap();
        assert_eq!(store.get(b"key1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn tiered_memory_tracking() {
        let store = TieredStore::new();
        assert_eq!(store.l1_memory(), 0);
        assert_eq!(store.l1_entries(), 0);

        store.put(b"key", b"value").await.unwrap();
        assert!(store.l1_memory() > 0);
        assert_eq!(store.l1_entries(), 1);
    }

    #[tokio::test]
    async fn tiered_threshold_check() {
        let config = TieredConfig {
            l1_max_bytes: 10, // tiny threshold for testing
            l2_enabled: false,
            l3_enabled: false,
        };
        let store = TieredStore::with_config(config);

        assert!(!store.l1_over_threshold());

        // Put enough data to exceed 10 bytes
        store.put(b"key123", b"value12345").await.unwrap();
        assert!(store.l1_over_threshold());
    }

    #[tokio::test]
    async fn tiered_scan_prefix() {
        let store = TieredStore::new();

        store.put(b"user:1:name", b"alice").await.unwrap();
        store.put(b"user:1:age", b"30").await.unwrap();
        store.put(b"order:1", b"data").await.unwrap();

        let results = store.scan_prefix(b"user:1:");
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn tiered_clear() {
        let store = TieredStore::new();
        store.put(b"k1", b"v1").await.unwrap();
        store.put(b"k2", b"v2").await.unwrap();

        store.clear();
        assert_eq!(store.l1_entries(), 0);
        assert_eq!(store.l1_memory(), 0);
        assert_eq!(store.get(b"k1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn tiered_works_with_typed_wrappers() {
        use crate::typed::{CounterState, ValueState};

        let store = TieredStore::new();

        // ValueState over TieredStore
        let vs = ValueState::new(&store, "total");
        vs.set(&42i64).await.unwrap();
        assert_eq!(vs.get::<i64>().await.unwrap(), Some(42));

        // CounterState over TieredStore
        let cs = CounterState::new(&store, "clicks");
        cs.increment(10).await.unwrap();
        assert_eq!(cs.get().await.unwrap(), 10);
    }

    #[test]
    fn default_config() {
        let config = TieredConfig::default();
        assert_eq!(config.l1_max_bytes, 256 * 1024 * 1024);
        assert!(!config.l2_enabled);
        assert!(!config.l3_enabled);
    }
}
