//! Tiered state store — L1 (DashMap) → L2 (mmap) → L3 (persistent).
//!
//! Read path:  L1 hit → return | L1 miss → L2 hit → promote to L1 → return | L2 miss → L3
//! Write path: Write to L1 (hot). Write-through to L3 when enabled for durability.
//!             Background demotion moves cold entries from L1 to L2 when L1 exceeds threshold.
//!
//! The tiered store provides the abstraction so that upstream code (typed wrappers,
//! engine) doesn't need to change regardless of which tiers are enabled.

use std::sync::Arc;

use aeon_types::{AeonError, StateOps};

use crate::l1::L1Store;
use crate::l3::L3Store;
#[cfg(feature = "redb")]
use crate::l3::RedbStore;

/// Configuration for tier promotion/demotion thresholds.
#[derive(Debug, Clone)]
pub struct TieredConfig {
    /// Maximum approximate bytes in L1 before demotion to L2 is considered.
    pub l1_max_bytes: u64,
    /// Whether L2 (mmap) tier is enabled.
    pub l2_enabled: bool,
    /// Whether L3 (persistent) tier is enabled.
    pub l3_enabled: bool,
    /// L2 backing file path (required if l2_enabled).
    #[cfg(feature = "mmap")]
    pub l2_path: Option<std::path::PathBuf>,
    /// L3 backend selector (FT-7). Defaults to `Redb`. When the RocksDB
    /// adapter lands, set to `L3Backend::RocksDb` at config-parse time.
    pub l3_backend: aeon_types::L3Backend,
    /// L3 configuration (required if l3_enabled and l3_backend=Redb).
    #[cfg(feature = "redb")]
    pub l3_config: Option<crate::l3::RedbConfig>,
}

impl Default for TieredConfig {
    fn default() -> Self {
        Self {
            l1_max_bytes: 256 * 1024 * 1024, // 256 MiB
            l2_enabled: false,
            l3_enabled: false,
            #[cfg(feature = "mmap")]
            l2_path: None,
            l3_backend: aeon_types::L3Backend::default(),
            #[cfg(feature = "redb")]
            l3_config: None,
        }
    }
}

/// Open the L3 backend selected by `config.l3_backend`.
///
/// `Redb` requires the `redb` feature and a populated `config.l3_config`.
/// `RocksDb` is not yet implemented — returns an error until the adapter lands.
fn open_l3_backend(config: &TieredConfig) -> Result<Arc<dyn L3Store>, AeonError> {
    match config.l3_backend {
        aeon_types::L3Backend::Redb => {
            #[cfg(feature = "redb")]
            {
                let cfg = config
                    .l3_config
                    .clone()
                    .ok_or_else(|| AeonError::state("L3 Redb enabled but l3_config not set"))?;
                Ok(Arc::new(RedbStore::open(cfg)?) as Arc<dyn L3Store>)
            }
            #[cfg(not(feature = "redb"))]
            {
                let _ = config; // suppress unused
                Err(AeonError::state(
                    "L3 backend=Redb but crate built without `redb` feature",
                ))
            }
        }
        #[cfg(feature = "rocksdb")]
        aeon_types::L3Backend::RocksDb => Err(AeonError::state(
            "L3 backend=RocksDb: adapter not yet implemented (FT-7 placeholder)",
        )),
    }
}

/// Multi-tier state store with read-through and write-through semantics.
///
/// - **L1** (DashMap): fastest, in-memory, volatile. All reads/writes go here first.
/// - **L2** (mmap): medium speed, memory-mapped files. Feature-gated behind `mmap`.
/// - **L3** (redb/RocksDB): persistent, survives restarts. Feature-gated behind `redb`.
///
/// **Read path**: L1 → L2 → L3. On hit at a lower tier, the value is promoted to L1.
/// **Write path**: Always write to L1. Write-through to L3 when enabled.
/// **Demotion**: When L1 exceeds `l1_max_bytes`, cold entries can be demoted to L2.
pub struct TieredStore {
    l1: L1Store,
    #[cfg(feature = "mmap")]
    l2: Option<crate::l2::L2Store>,
    /// L3 backend, held behind `Arc<dyn L3Store>` so the runtime backend
    /// (redb today, RocksDB or others in future) is selectable via config
    /// rather than hardcoded (FT-7).
    l3: Option<Arc<dyn L3Store>>,
    config: TieredConfig,
}

impl TieredStore {
    /// Create a new tiered store with default configuration (L1 only).
    pub fn new() -> Self {
        Self {
            l1: L1Store::new(),
            #[cfg(feature = "mmap")]
            l2: None,
            l3: None,
            config: TieredConfig::default(),
        }
    }

    /// Create a tiered store with custom configuration.
    ///
    /// Opens L2/L3 backends if their respective features are enabled and configured.
    /// The L3 backend is selected by [`TieredConfig::l3_backend`] — currently `Redb`
    /// is the only shipping implementation; `RocksDb` returns a "not implemented"
    /// error until the adapter lands.
    pub fn with_config(config: TieredConfig) -> Result<Self, AeonError> {
        #[cfg(feature = "mmap")]
        let l2 = if config.l2_enabled {
            let path = config
                .l2_path
                .clone()
                .ok_or_else(|| AeonError::state("L2 enabled but l2_path not configured"))?;
            Some(crate::l2::L2Store::open(path)?)
        } else {
            None
        };

        let l3: Option<Arc<dyn L3Store>> = if config.l3_enabled {
            Some(open_l3_backend(&config)?)
        } else {
            None
        };

        Ok(Self {
            l1: L1Store::new(),
            #[cfg(feature = "mmap")]
            l2,
            l3,
            config,
        })
    }

    /// Create a tiered store with a pre-constructed L3 backend injected.
    ///
    /// Enables callers (notably `aeon-cluster` persistent Raft log storage, FT-1)
    /// to share a single L3 backend instance across multiple consumers without
    /// re-opening the file.
    pub fn with_l3_store(
        mut config: TieredConfig,
        l3: Arc<dyn L3Store>,
    ) -> Result<Self, AeonError> {
        config.l3_enabled = true;
        #[cfg(feature = "mmap")]
        let l2 = if config.l2_enabled {
            let path = config
                .l2_path
                .clone()
                .ok_or_else(|| AeonError::state("L2 enabled but l2_path not configured"))?;
            Some(crate::l2::L2Store::open(path)?)
        } else {
            None
        };

        Ok(Self {
            l1: L1Store::new(),
            #[cfg(feature = "mmap")]
            l2,
            l3: Some(l3),
            config,
        })
    }

    /// Borrow the L3 backend, if enabled. Lets callers with a shared handle
    /// (e.g., `aeon-cluster` RaftLogStore) reuse the same backing store.
    pub fn l3(&self) -> Option<&Arc<dyn L3Store>> {
        self.l3.as_ref()
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

    /// Scan for keys with a given prefix across all enabled tiers.
    ///
    /// Results are deduplicated: L1 entries take precedence over L2/L3.
    pub fn scan_prefix(&self, prefix: &[u8]) -> crate::l3::KvPairs {
        let mut results: std::collections::HashMap<Vec<u8>, Vec<u8>> =
            std::collections::HashMap::new();

        // L3 first (lowest priority).
        if let Some(ref l3) = self.l3 {
            if let Ok(entries) = l3.scan_prefix(prefix) {
                for (k, v) in entries {
                    results.insert(k, v);
                }
            }
        }

        // L2 overrides L3.
        #[cfg(feature = "mmap")]
        if let Some(ref l2) = self.l2 {
            for (k, v) in l2.scan_prefix(prefix) {
                results.insert(k, v);
            }
        }

        // L1 overrides everything.
        for (k, v) in self.l1.scan_prefix(prefix) {
            results.insert(k, v);
        }

        results.into_iter().collect()
    }

    /// Clear all tiers.
    pub fn clear(&self) {
        self.l1.clear();

        #[cfg(feature = "mmap")]
        if let Some(ref l2) = self.l2 {
            let _ = l2.clear();
        }

        // L3 is persistent — clearing it is intentional.
        // Use a write_batch with deletes or reopen with a fresh file.
    }

    /// Demote cold entries from L1 to L2.
    ///
    /// Moves entries from L1 to L2 until L1 is below its threshold or `max_entries`
    /// have been moved. Returns the number of entries demoted.
    ///
    /// **Note**: This is a simple eviction — it doesn't track access frequency.
    /// A future LRU/LFU policy can be added by wrapping L1 keys with timestamps.
    #[cfg(feature = "mmap")]
    pub async fn demote_to_l2(&self, max_entries: usize) -> Result<usize, AeonError> {
        let l2 = match &self.l2 {
            Some(l2) => l2,
            None => return Ok(0),
        };

        if !self.l1_over_threshold() {
            return Ok(0);
        }

        // Collect keys to demote (grab a snapshot of some entries).
        let keys_to_demote: Vec<Vec<u8>> = self
            .l1
            .scan_prefix(&[])
            .into_iter()
            .take(max_entries)
            .map(|(k, _)| k)
            .collect();

        let mut demoted = 0;
        for key in &keys_to_demote {
            if let Some(value) = self.l1.get(key).await? {
                // Write to L2.
                l2.put(key, &value).await?;
                // Remove from L1.
                self.l1.delete(key).await?;
                demoted += 1;

                if !self.l1_over_threshold() {
                    break;
                }
            }
        }

        Ok(demoted)
    }

    /// Export all state from L3 for partition transfer.
    ///
    /// Returns all entries with keys starting with `prefix` from the persistent tier.
    /// Used during partition rebalance (source node exports, target node imports).
    pub fn export_partition(&self, prefix: &[u8]) -> Result<crate::l3::KvPairs, AeonError> {
        match &self.l3 {
            Some(l3) => l3.scan_prefix(prefix),
            None => Ok(Vec::new()),
        }
    }

    /// Import state into L3 for partition transfer.
    ///
    /// Bulk-writes entries into the persistent tier. Used on the target node during
    /// partition rebalance.
    pub fn import_partition(&self, entries: Vec<(Vec<u8>, Vec<u8>)>) -> Result<(), AeonError> {
        match &self.l3 {
            Some(l3) => {
                let ops: Vec<_> = entries
                    .into_iter()
                    .map(|(k, v)| (crate::l3::BatchOp::Put, k, Some(v)))
                    .collect();
                l3.write_batch(&ops)
            }
            None => Ok(()),
        }
    }
}

impl Default for TieredStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StateOps for TieredStore {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, AeonError> {
        // L1 lookup (hot).
        if let Some(value) = self.l1.get(key).await? {
            return Ok(Some(value));
        }

        // L2 lookup → promote to L1 on hit.
        #[cfg(feature = "mmap")]
        if let Some(ref l2) = self.l2 {
            if let Some(value) = l2.get(key).await? {
                // Promote to L1 for faster subsequent access.
                self.l1.put(key, &value).await?;
                return Ok(Some(value));
            }
        }

        // L3 lookup → promote to L1 on hit.
        if let Some(ref l3) = self.l3 {
            if let Ok(Some(value)) = l3.get(key) {
                // Promote to L1 for faster subsequent access.
                self.l1.put(key, &value).await?;
                return Ok(Some(value));
            }
        }

        Ok(None)
    }

    async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), AeonError> {
        // Always write to L1 (hot tier).
        self.l1.put(key, value).await?;

        // Write-through to L3 for durability (if enabled).
        if let Some(ref l3) = self.l3 {
            l3.put(key, value)
                .map_err(|e| AeonError::state(format!("L3 write-through: {e}")))?;
        }

        Ok(())
    }

    async fn delete(&self, key: &[u8]) -> Result<(), AeonError> {
        self.l1.delete(key).await?;

        // Propagate delete to L2.
        #[cfg(feature = "mmap")]
        if let Some(ref l2) = self.l2 {
            l2.delete(key).await?;
        }

        // Propagate delete to L3.
        if let Some(ref l3) = self.l3 {
            l3.delete(key)
                .map_err(|e| AeonError::state(format!("L3 delete propagation: {e}")))?;
        }

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
            #[cfg(feature = "mmap")]
            l2_path: None,
            l3_backend: aeon_types::L3Backend::default(),
            #[cfg(feature = "redb")]
            l3_config: None,
        };
        let store = TieredStore::with_config(config).unwrap();

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

    // --- L2 integration tests ---

    #[cfg(feature = "mmap")]
    #[tokio::test]
    async fn tiered_l2_read_through() {
        let dir = tempfile::tempdir().unwrap();
        let l2_path = dir.path().join("tiered_l2_test.dat");

        // Pre-populate L2 directly.
        {
            let l2 = crate::l2::L2Store::open(&l2_path).unwrap();
            l2.put(b"warm_key", b"warm_value").await.unwrap();
        }

        // Create tiered store with L2 enabled.
        let config = TieredConfig {
            l1_max_bytes: 256 * 1024 * 1024,
            l2_enabled: true,
            l3_enabled: false,
            l2_path: Some(l2_path),
            l3_backend: aeon_types::L3Backend::default(),
            #[cfg(feature = "redb")]
            l3_config: None,
        };
        let store = TieredStore::with_config(config).unwrap();

        // L1 is empty, but L2 has the key.
        assert_eq!(store.l1_entries(), 0);

        // Get should find it in L2 and promote to L1.
        let result = store.get(b"warm_key").await.unwrap();
        assert_eq!(result, Some(b"warm_value".to_vec()));

        // Now it should be in L1 (promoted).
        assert_eq!(store.l1_entries(), 1);

        // Second get should hit L1 directly.
        let result2 = store.get(b"warm_key").await.unwrap();
        assert_eq!(result2, Some(b"warm_value".to_vec()));
    }

    #[cfg(feature = "mmap")]
    #[tokio::test]
    async fn tiered_l2_demotion() {
        let dir = tempfile::tempdir().unwrap();
        let l2_path = dir.path().join("tiered_demote_test.dat");

        let config = TieredConfig {
            l1_max_bytes: 20, // tiny threshold
            l2_enabled: true,
            l3_enabled: false,
            l2_path: Some(l2_path),
            l3_backend: aeon_types::L3Backend::default(),
            #[cfg(feature = "redb")]
            l3_config: None,
        };
        let store = TieredStore::with_config(config).unwrap();

        // Fill L1 past threshold.
        store.put(b"key1", b"value1_long").await.unwrap();
        store.put(b"key2", b"value2_long").await.unwrap();
        assert!(store.l1_over_threshold());

        // Demote to L2.
        let demoted = store.demote_to_l2(10).await.unwrap();
        assert!(demoted > 0);

        // Values should still be accessible (via L2 read-through).
        // At least one key was demoted to L2.
        let v1 = store.get(b"key1").await.unwrap();
        let v2 = store.get(b"key2").await.unwrap();
        assert!(v1.is_some() || v2.is_some());
    }

    #[cfg(feature = "mmap")]
    #[tokio::test]
    async fn tiered_delete_propagates_to_l2() {
        let dir = tempfile::tempdir().unwrap();
        let l2_path = dir.path().join("tiered_delete_l2.dat");

        let config = TieredConfig {
            l1_max_bytes: 256 * 1024 * 1024,
            l2_enabled: true,
            l3_enabled: false,
            l2_path: Some(l2_path),
            l3_backend: aeon_types::L3Backend::default(),
            #[cfg(feature = "redb")]
            l3_config: None,
        };
        let store = TieredStore::with_config(config).unwrap();

        store.put(b"key", b"value").await.unwrap();
        store.delete(b"key").await.unwrap();

        // Should be gone from both L1 and L2.
        assert_eq!(store.get(b"key").await.unwrap(), None);
    }

    // --- L3 integration tests ---

    #[cfg(feature = "redb")]
    #[tokio::test]
    async fn tiered_l3_write_through() {
        let dir = tempfile::tempdir().unwrap();
        let l3_path = dir.path().join("tiered_l3_wt.redb");

        let config = TieredConfig {
            l1_max_bytes: 256 * 1024 * 1024,
            l2_enabled: false,
            l3_enabled: true,
            #[cfg(feature = "mmap")]
            l2_path: None,
            l3_backend: aeon_types::L3Backend::default(),
            l3_config: Some(crate::l3::RedbConfig {
                path: l3_path.clone(),
                sync_writes: false,
            }),
        };
        let store = TieredStore::with_config(config).unwrap();

        // Put a value — should be in both L1 and L3.
        store.put(b"durable_key", b"durable_value").await.unwrap();

        // Verify L3 has it directly.
        let l3_value = store.l3.as_ref().unwrap().get(b"durable_key").unwrap();
        assert_eq!(l3_value, Some(b"durable_value".to_vec()));
    }

    #[cfg(feature = "redb")]
    #[tokio::test]
    async fn tiered_l3_read_through() {
        let dir = tempfile::tempdir().unwrap();
        let l3_path = dir.path().join("tiered_l3_rt.redb");

        // Pre-populate L3 directly.
        {
            let l3 = RedbStore::open(crate::l3::RedbConfig {
                path: l3_path.clone(),
                sync_writes: false,
            })
            .unwrap();
            l3.put(b"cold_key", b"cold_value").unwrap();
        }

        // Create tiered store with L3.
        let config = TieredConfig {
            l1_max_bytes: 256 * 1024 * 1024,
            l2_enabled: false,
            l3_enabled: true,
            #[cfg(feature = "mmap")]
            l2_path: None,
            l3_backend: aeon_types::L3Backend::default(),
            l3_config: Some(crate::l3::RedbConfig {
                path: l3_path,
                sync_writes: false,
            }),
        };
        let store = TieredStore::with_config(config).unwrap();

        // L1 is empty.
        assert_eq!(store.l1_entries(), 0);

        // Get should find in L3 and promote to L1.
        let result = store.get(b"cold_key").await.unwrap();
        assert_eq!(result, Some(b"cold_value".to_vec()));

        // Now promoted to L1.
        assert_eq!(store.l1_entries(), 1);
    }

    #[cfg(feature = "redb")]
    #[tokio::test]
    async fn tiered_l3_delete_propagation() {
        let dir = tempfile::tempdir().unwrap();
        let l3_path = dir.path().join("tiered_l3_del.redb");

        let config = TieredConfig {
            l1_max_bytes: 256 * 1024 * 1024,
            l2_enabled: false,
            l3_enabled: true,
            #[cfg(feature = "mmap")]
            l2_path: None,
            l3_backend: aeon_types::L3Backend::default(),
            l3_config: Some(crate::l3::RedbConfig {
                path: l3_path,
                sync_writes: false,
            }),
        };
        let store = TieredStore::with_config(config).unwrap();

        store.put(b"key", b"value").await.unwrap();

        // Verify L3 has it.
        assert!(store.l3.as_ref().unwrap().get(b"key").unwrap().is_some());

        // Delete — should propagate to L3.
        store.delete(b"key").await.unwrap();

        // Gone from both L1 and L3.
        assert_eq!(store.get(b"key").await.unwrap(), None);
        assert_eq!(store.l3.as_ref().unwrap().get(b"key").unwrap(), None);
    }

    #[cfg(feature = "redb")]
    #[tokio::test]
    async fn tiered_l3_persistence_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let l3_path = dir.path().join("tiered_l3_persist.redb");

        // Write data.
        {
            let config = TieredConfig {
                l1_max_bytes: 256 * 1024 * 1024,
                l2_enabled: false,
                l3_enabled: true,
                #[cfg(feature = "mmap")]
                l2_path: None,
                l3_backend: aeon_types::L3Backend::default(),
                l3_config: Some(crate::l3::RedbConfig {
                    path: l3_path.clone(),
                    sync_writes: false,
                }),
            };
            let store = TieredStore::with_config(config).unwrap();

            store.put(b"persist1", b"data1").await.unwrap();
            store.put(b"persist2", b"data2").await.unwrap();
            store.delete(b"persist1").await.unwrap();
            // store dropped here — L1 gone, but L3 persisted.
        }

        // Reopen — L1 is fresh, but L3 has the data.
        {
            let config = TieredConfig {
                l1_max_bytes: 256 * 1024 * 1024,
                l2_enabled: false,
                l3_enabled: true,
                #[cfg(feature = "mmap")]
                l2_path: None,
                l3_backend: aeon_types::L3Backend::default(),
                l3_config: Some(crate::l3::RedbConfig {
                    path: l3_path,
                    sync_writes: false,
                }),
            };
            let store = TieredStore::with_config(config).unwrap();

            // L1 is empty.
            assert_eq!(store.l1_entries(), 0);

            // Read-through from L3.
            assert_eq!(store.get(b"persist1").await.unwrap(), None); // deleted
            assert_eq!(
                store.get(b"persist2").await.unwrap(),
                Some(b"data2".to_vec())
            );
        }
    }

    // --- Full stack (L1 + L2 + L3) ---

    #[cfg(all(feature = "mmap", feature = "redb"))]
    #[tokio::test]
    async fn tiered_full_stack_read_through() {
        let dir = tempfile::tempdir().unwrap();
        let l2_path = dir.path().join("full_l2.dat");
        let l3_path = dir.path().join("full_l3.redb");

        // Pre-populate L3 with a cold key.
        {
            let l3 = RedbStore::open(crate::l3::RedbConfig {
                path: l3_path.clone(),
                sync_writes: false,
            })
            .unwrap();
            l3.put(b"cold", b"from_l3").unwrap();
        }

        // Pre-populate L2 with a warm key.
        {
            let l2 = crate::l2::L2Store::open(&l2_path).unwrap();
            l2.put(b"warm", b"from_l2").await.unwrap();
        }

        let config = TieredConfig {
            l1_max_bytes: 256 * 1024 * 1024,
            l2_enabled: true,
            l3_enabled: true,
            l2_path: Some(l2_path),
            l3_backend: aeon_types::L3Backend::default(),
            l3_config: Some(crate::l3::RedbConfig {
                path: l3_path,
                sync_writes: false,
            }),
        };
        let store = TieredStore::with_config(config).unwrap();

        // L1 is empty.
        assert_eq!(store.l1_entries(), 0);

        // Read warm key — found in L2, promoted to L1.
        assert_eq!(store.get(b"warm").await.unwrap(), Some(b"from_l2".to_vec()));
        assert_eq!(store.l1_entries(), 1);

        // Read cold key — L1 miss, L2 miss, found in L3, promoted to L1.
        assert_eq!(store.get(b"cold").await.unwrap(), Some(b"from_l3".to_vec()));
        assert_eq!(store.l1_entries(), 2);

        // Both now in L1.
        assert!(store.l1().contains_key(b"warm"));
        assert!(store.l1().contains_key(b"cold"));
    }

    #[cfg(all(feature = "mmap", feature = "redb"))]
    #[tokio::test]
    async fn tiered_full_stack_scan_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let l2_path = dir.path().join("scan_l2.dat");
        let l3_path = dir.path().join("scan_l3.redb");

        let config = TieredConfig {
            l1_max_bytes: 256 * 1024 * 1024,
            l2_enabled: true,
            l3_enabled: true,
            l2_path: Some(l2_path),
            l3_backend: aeon_types::L3Backend::default(),
            l3_config: Some(crate::l3::RedbConfig {
                path: l3_path,
                sync_writes: false,
            }),
        };
        let store = TieredStore::with_config(config).unwrap();

        // Put data — goes to L1 and L3 (write-through).
        store.put(b"ns:a", b"val_a").await.unwrap();
        store.put(b"ns:b", b"val_b").await.unwrap();

        // Scan should return 2 entries, not 4 (deduplicated).
        let results = store.scan_prefix(b"ns:");
        assert_eq!(results.len(), 2);
    }

    /// FT-7 / S8.3: rocksdb backend selected but not implemented — surfaces a
    /// clear error rather than silently falling back. Only compiled when the
    /// `rocksdb` feature is enabled (the variant is otherwise gated out at the
    /// type level, so YAML configs selecting `rocksdb` fail at parse time).
    #[cfg(feature = "rocksdb")]
    #[test]
    fn tiered_l3_backend_rocksdb_not_implemented() {
        let config = TieredConfig {
            l3_enabled: true,
            l3_backend: aeon_types::L3Backend::RocksDb,
            ..Default::default()
        };
        let err = match TieredStore::with_config(config) {
            Ok(_) => panic!("expected RocksDb backend to return not-implemented"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(msg.contains("RocksDb"), "unexpected error: {msg}");
    }

    /// FT-7: callers can inject a pre-constructed `Arc<dyn L3Store>` so that a
    /// single backing store is shared between `TieredStore` and other consumers
    /// (e.g., RaftLogStore in FT-1).
    #[cfg(feature = "redb")]
    #[tokio::test]
    async fn tiered_with_injected_l3_store() {
        let dir = tempfile::tempdir().unwrap();
        let l3_path = dir.path().join("inject.redb");
        let shared: Arc<dyn L3Store> = Arc::new(
            RedbStore::open(crate::l3::RedbConfig {
                path: l3_path,
                sync_writes: false,
            })
            .unwrap(),
        );

        // Pre-populate through the shared handle.
        shared.put(b"shared_key", b"shared_val").unwrap();

        // Inject the same backend into a TieredStore.
        let store =
            TieredStore::with_l3_store(TieredConfig::default(), Arc::clone(&shared)).unwrap();

        // Read-through finds the pre-written value.
        assert_eq!(
            store.get(b"shared_key").await.unwrap(),
            Some(b"shared_val".to_vec())
        );

        // Write through TieredStore also visible via the shared handle.
        store.put(b"new_key", b"new_val").await.unwrap();
        assert_eq!(shared.get(b"new_key").unwrap(), Some(b"new_val".to_vec()));
    }

    #[cfg(feature = "redb")]
    #[tokio::test]
    async fn tiered_export_import_partition() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("export_src.redb");
        let dst_path = dir.path().join("export_dst.redb");

        // Source store with partition data.
        let src_config = TieredConfig {
            l1_max_bytes: 256 * 1024 * 1024,
            l2_enabled: false,
            l3_enabled: true,
            #[cfg(feature = "mmap")]
            l2_path: None,
            l3_backend: aeon_types::L3Backend::default(),
            l3_config: Some(crate::l3::RedbConfig {
                path: src_path,
                sync_writes: false,
            }),
        };
        let src = TieredStore::with_config(src_config).unwrap();

        src.put(b"p0:key1", b"val1").await.unwrap();
        src.put(b"p0:key2", b"val2").await.unwrap();
        src.put(b"p1:key1", b"other").await.unwrap();

        // Export partition 0.
        let exported = src.export_partition(b"p0:").unwrap();
        assert_eq!(exported.len(), 2);

        // Import into destination store.
        let dst_config = TieredConfig {
            l1_max_bytes: 256 * 1024 * 1024,
            l2_enabled: false,
            l3_enabled: true,
            #[cfg(feature = "mmap")]
            l2_path: None,
            l3_backend: aeon_types::L3Backend::default(),
            l3_config: Some(crate::l3::RedbConfig {
                path: dst_path,
                sync_writes: false,
            }),
        };
        let dst = TieredStore::with_config(dst_config).unwrap();

        dst.import_partition(exported).unwrap();

        // Verify data arrived.
        assert_eq!(dst.get(b"p0:key1").await.unwrap(), Some(b"val1".to_vec()));
        assert_eq!(dst.get(b"p0:key2").await.unwrap(), Some(b"val2".to_vec()));
        // p1 was not exported.
        assert_eq!(dst.get(b"p1:key1").await.unwrap(), None);
    }
}
