//! L3 state store — persistent key-value backend.
//!
//! The L3 tier provides durable state that survives process restarts. It sits
//! at the bottom of the tiered hierarchy (L1 DashMap → L2 mmap → L3 persistent).
//!
//! **Adapter pattern**: The [`L3Store`] trait is defined in `aeon-types` (so that
//! crates like `aeon-cluster` can depend on the abstraction without pulling in
//! `aeon-state`). Implementations are selected via the [`L3Backend`] config enum:
//!
//! - `redb` (default): Pure Rust B-tree database, ACID, no C dependencies.
//!   Feature-gated behind `redb`.
//! - `rocksdb` (future): LSM-tree, battle-tested at scale. Feature-gated behind
//!   `rocksdb`. Same `L3Store` trait, different implementation.
//!
//! **Design rationale** (see memory: `project_l3_backend_design.md`):
//! L3 is write-heavy in steady state (write-behind from L1, checkpoint offsets)
//! but read-infrequent (crash recovery, L1+L2 miss fallthrough). redb's
//! copy-on-write B-tree avoids compaction stalls, providing predictable latency
//! for a real-time pipeline engine.

// Re-export the trait and associated types from aeon-types so downstream code
// using `crate::l3::L3Store` etc. continues to work unchanged.
pub use aeon_types::{BatchEntry, BatchOp, KvPairs, L3Backend, L3Store};

// --- redb implementation ---

#[cfg(feature = "redb")]
mod redb_store {
    use aeon_types::{AeonError, BatchEntry, BatchOp, KvPairs, L3Store};
    use redb::{ReadableTable, ReadableTableMetadata};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Table definition for the key-value store.
    const TABLE: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("state");

    /// L3 configuration for the redb backend.
    #[derive(Debug, Clone)]
    pub struct RedbConfig {
        /// Path to the database file.
        pub path: PathBuf,
        /// Whether to fsync on every write transaction (durability vs speed).
        /// Default: false (OS decides when to flush).
        pub sync_writes: bool,
    }

    impl Default for RedbConfig {
        fn default() -> Self {
            Self {
                path: PathBuf::from("data/state/l3.redb"),
                sync_writes: false,
            }
        }
    }

    /// L3 persistent state store backed by redb.
    ///
    /// redb is a pure Rust, ACID-compliant embedded database using copy-on-write
    /// B-trees. No compaction, no C dependencies.
    pub struct RedbStore {
        db: redb::Database,
        path: PathBuf,
        entry_count: AtomicU64,
    }

    impl RedbStore {
        /// Open or create a redb database at the configured path.
        pub fn open(config: RedbConfig) -> Result<Self, AeonError> {
            // Ensure parent directory exists.
            if let Some(parent) = config.path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AeonError::state(format!("L3 redb: create dir: {e}")))?;
            }

            let mut builder = redb::Builder::new();
            if !config.sync_writes {
                builder.set_cache_size(64 * 1024 * 1024); // 64 MiB cache
            }

            let db = builder
                .create(&config.path)
                .map_err(|e| AeonError::state(format!("L3 redb: open: {e}")))?;

            // Ensure table exists by opening a write transaction.
            {
                let txn = db
                    .begin_write()
                    .map_err(|e| AeonError::state(format!("L3 redb: begin_write: {e}")))?;
                {
                    let _table = txn
                        .open_table(TABLE)
                        .map_err(|e| AeonError::state(format!("L3 redb: open_table: {e}")))?;
                }
                txn.commit()
                    .map_err(|e| AeonError::state(format!("L3 redb: commit: {e}")))?;
            }

            // Count entries for len() tracking.
            let count = {
                let txn = db
                    .begin_read()
                    .map_err(|e| AeonError::state(format!("L3 redb: begin_read: {e}")))?;
                let table = txn
                    .open_table(TABLE)
                    .map_err(|e| AeonError::state(format!("L3 redb: open_table: {e}")))?;
                table
                    .len()
                    .map_err(|e| AeonError::state(format!("L3 redb: len: {e}")))?
            };

            Ok(Self {
                db,
                path: config.path,
                entry_count: AtomicU64::new(count),
            })
        }

        /// Path to the database file.
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl L3Store for RedbStore {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, AeonError> {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| AeonError::state(format!("L3 redb get: begin_read: {e}")))?;
            let table = txn
                .open_table(TABLE)
                .map_err(|e| AeonError::state(format!("L3 redb get: open_table: {e}")))?;

            match table.get(key) {
                Ok(Some(value)) => Ok(Some(value.value().to_vec())),
                Ok(None) => Ok(None),
                Err(e) => Err(AeonError::state(format!("L3 redb get: {e}"))),
            }
        }

        fn put(&self, key: &[u8], value: &[u8]) -> Result<(), AeonError> {
            let txn = self
                .db
                .begin_write()
                .map_err(|e| AeonError::state(format!("L3 redb put: begin_write: {e}")))?;
            {
                let mut table = txn
                    .open_table(TABLE)
                    .map_err(|e| AeonError::state(format!("L3 redb put: open_table: {e}")))?;

                let was_new = table
                    .get(key)
                    .map_err(|e| AeonError::state(format!("L3 redb put: get: {e}")))?
                    .is_none();

                table
                    .insert(key, value)
                    .map_err(|e| AeonError::state(format!("L3 redb put: insert: {e}")))?;

                if was_new {
                    self.entry_count.fetch_add(1, Ordering::Relaxed);
                }
            }
            txn.commit()
                .map_err(|e| AeonError::state(format!("L3 redb put: commit: {e}")))?;

            Ok(())
        }

        fn delete(&self, key: &[u8]) -> Result<(), AeonError> {
            let txn = self
                .db
                .begin_write()
                .map_err(|e| AeonError::state(format!("L3 redb delete: begin_write: {e}")))?;
            {
                let mut table = txn
                    .open_table(TABLE)
                    .map_err(|e| AeonError::state(format!("L3 redb delete: open_table: {e}")))?;

                if table
                    .remove(key)
                    .map_err(|e| AeonError::state(format!("L3 redb delete: remove: {e}")))?
                    .is_some()
                {
                    self.entry_count.fetch_sub(1, Ordering::Relaxed);
                }
            }
            txn.commit()
                .map_err(|e| AeonError::state(format!("L3 redb delete: commit: {e}")))?;

            Ok(())
        }

        fn write_batch(&self, ops: &[BatchEntry]) -> Result<(), AeonError> {
            if ops.is_empty() {
                return Ok(());
            }

            let txn = self
                .db
                .begin_write()
                .map_err(|e| AeonError::state(format!("L3 redb batch: begin_write: {e}")))?;
            {
                let mut table = txn
                    .open_table(TABLE)
                    .map_err(|e| AeonError::state(format!("L3 redb batch: open_table: {e}")))?;

                for (op, key, value) in ops {
                    match op {
                        BatchOp::Put => {
                            let was_new = table
                                .get(key.as_slice())
                                .map_err(|e| AeonError::state(format!("L3 redb batch: get: {e}")))?
                                .is_none();

                            let val = value.as_deref().unwrap_or(&[]);
                            table.insert(key.as_slice(), val).map_err(|e| {
                                AeonError::state(format!("L3 redb batch: insert: {e}"))
                            })?;

                            if was_new {
                                self.entry_count.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        BatchOp::Delete => {
                            if table
                                .remove(key.as_slice())
                                .map_err(|e| {
                                    AeonError::state(format!("L3 redb batch: remove: {e}"))
                                })?
                                .is_some()
                            {
                                self.entry_count.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
            txn.commit()
                .map_err(|e| AeonError::state(format!("L3 redb batch: commit: {e}")))?;

            Ok(())
        }

        fn scan_prefix(&self, prefix: &[u8]) -> Result<KvPairs, AeonError> {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| AeonError::state(format!("L3 redb scan: begin_read: {e}")))?;
            let table = txn
                .open_table(TABLE)
                .map_err(|e| AeonError::state(format!("L3 redb scan: open_table: {e}")))?;

            let mut results = Vec::new();

            // Build the upper bound for prefix scan: increment the last byte.
            // This gives us a range [prefix, prefix_end) that covers all keys with
            // the given prefix.
            let range = if let Some(upper) = prefix_upper_bound(prefix) {
                // Range: [prefix..upper)
                let iter = table
                    .range(prefix..upper.as_slice())
                    .map_err(|e| AeonError::state(format!("L3 redb scan: range: {e}")))?;
                for entry in iter {
                    let entry =
                        entry.map_err(|e| AeonError::state(format!("L3 redb scan: iter: {e}")))?;
                    results.push((entry.0.value().to_vec(), entry.1.value().to_vec()));
                }
                return Ok(results);
            } else {
                // Prefix is all 0xFF bytes or empty — scan from prefix to end.
                table
                    .range(prefix..)
                    .map_err(|e| AeonError::state(format!("L3 redb scan: range: {e}")))?
            };

            for entry in range {
                let entry =
                    entry.map_err(|e| AeonError::state(format!("L3 redb scan: iter: {e}")))?;
                let key = entry.0.value();
                if !key.starts_with(prefix) {
                    break;
                }
                results.push((key.to_vec(), entry.1.value().to_vec()));
            }

            Ok(results)
        }

        fn flush(&self) -> Result<(), AeonError> {
            // redb commits are durable after commit(). Nothing extra needed.
            Ok(())
        }

        fn len(&self) -> Result<usize, AeonError> {
            Ok(self.entry_count.load(Ordering::Relaxed) as usize)
        }
    }

    /// Compute the upper bound for a prefix scan.
    ///
    /// Increments the last non-0xFF byte. Returns `None` if all bytes are 0xFF
    /// (meaning the prefix covers everything from prefix to end of keyspace).
    pub(crate) fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
        let mut upper = prefix.to_vec();
        // Walk backwards, incrementing the last byte that isn't 0xFF.
        while let Some(last) = upper.last_mut() {
            if *last < 0xFF {
                *last += 1;
                return Some(upper);
            }
            upper.pop();
        }
        None // All bytes were 0xFF
    }
}

#[cfg(feature = "redb")]
pub use redb_store::{RedbConfig, RedbStore};

#[cfg(test)]
#[cfg(feature = "redb")]
mod tests {
    use super::*;

    fn temp_store() -> RedbStore {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_l3.redb");
        std::mem::forget(dir);
        RedbStore::open(RedbConfig {
            path,
            sync_writes: false,
        })
        .unwrap()
    }

    #[test]
    fn l3_create_new() {
        let store = temp_store();
        assert_eq!(store.len().unwrap(), 0);
        assert!(store.is_empty().unwrap());
    }

    #[test]
    fn l3_put_and_get() {
        let store = temp_store();
        store.put(b"key1", b"value1").unwrap();
        assert_eq!(store.get(b"key1").unwrap(), Some(b"value1".to_vec()));
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn l3_get_missing() {
        let store = temp_store();
        assert_eq!(store.get(b"missing").unwrap(), None);
    }

    #[test]
    fn l3_put_overwrite() {
        let store = temp_store();
        store.put(b"key", b"old").unwrap();
        store.put(b"key", b"new").unwrap();
        assert_eq!(store.get(b"key").unwrap(), Some(b"new".to_vec()));
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn l3_delete() {
        let store = temp_store();
        store.put(b"key", b"value").unwrap();
        store.delete(b"key").unwrap();
        assert_eq!(store.get(b"key").unwrap(), None);
        assert_eq!(store.len().unwrap(), 0);
    }

    #[test]
    fn l3_delete_nonexistent() {
        let store = temp_store();
        store.delete(b"missing").unwrap(); // no-op
    }

    #[test]
    fn l3_write_batch() {
        let store = temp_store();

        store
            .write_batch(&[
                (BatchOp::Put, b"k1".to_vec(), Some(b"v1".to_vec())),
                (BatchOp::Put, b"k2".to_vec(), Some(b"v2".to_vec())),
                (BatchOp::Put, b"k3".to_vec(), Some(b"v3".to_vec())),
            ])
            .unwrap();

        assert_eq!(store.len().unwrap(), 3);
        assert_eq!(store.get(b"k1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(store.get(b"k2").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(store.get(b"k3").unwrap(), Some(b"v3".to_vec()));

        // Batch with mixed ops.
        store
            .write_batch(&[
                (BatchOp::Delete, b"k1".to_vec(), None),
                (BatchOp::Put, b"k4".to_vec(), Some(b"v4".to_vec())),
            ])
            .unwrap();

        assert_eq!(store.len().unwrap(), 3);
        assert_eq!(store.get(b"k1").unwrap(), None);
        assert_eq!(store.get(b"k4").unwrap(), Some(b"v4".to_vec()));
    }

    #[test]
    fn l3_write_batch_empty() {
        let store = temp_store();
        store.write_batch(&[]).unwrap(); // no-op
    }

    #[test]
    fn l3_scan_prefix() {
        let store = temp_store();

        store.put(b"user:1:name", b"alice").unwrap();
        store.put(b"user:1:age", b"30").unwrap();
        store.put(b"user:2:name", b"bob").unwrap();
        store.put(b"order:1", b"data").unwrap();

        let user1 = store.scan_prefix(b"user:1:").unwrap();
        assert_eq!(user1.len(), 2);

        let users = store.scan_prefix(b"user:").unwrap();
        assert_eq!(users.len(), 3);

        let orders = store.scan_prefix(b"order:").unwrap();
        assert_eq!(orders.len(), 1);

        let empty = store.scan_prefix(b"nonexistent:").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn l3_scan_prefix_sorted() {
        let store = temp_store();

        store.put(b"c", b"3").unwrap();
        store.put(b"a", b"1").unwrap();
        store.put(b"b", b"2").unwrap();

        let all = store.scan_prefix(b"").unwrap();
        assert_eq!(all.len(), 3);
        // redb B-tree stores keys in sorted order.
        assert_eq!(all[0].0, b"a");
        assert_eq!(all[1].0, b"b");
        assert_eq!(all[2].0, b"c");
    }

    #[test]
    fn l3_persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist_test.redb");

        // Write data.
        {
            let store = RedbStore::open(RedbConfig {
                path: path.clone(),
                sync_writes: false,
            })
            .unwrap();
            store.put(b"key1", b"value1").unwrap();
            store.put(b"key2", b"value2").unwrap();
            store.delete(b"key1").unwrap();
        }

        // Reopen and verify.
        {
            let store = RedbStore::open(RedbConfig {
                path,
                sync_writes: false,
            })
            .unwrap();
            assert_eq!(store.get(b"key1").unwrap(), None);
            assert_eq!(store.get(b"key2").unwrap(), Some(b"value2".to_vec()));
            assert_eq!(store.len().unwrap(), 1);
        }
    }

    #[test]
    fn l3_flush_is_noop() {
        let store = temp_store();
        store.put(b"key", b"value").unwrap();
        store.flush().unwrap(); // Should succeed (redb commits are already durable).
    }

    #[test]
    fn l3_backend_default() {
        assert_eq!(L3Backend::default(), L3Backend::Redb);
    }

    #[test]
    fn prefix_upper_bound_basic() {
        use super::redb_store::prefix_upper_bound;
        assert_eq!(prefix_upper_bound(b"abc"), Some(b"abd".to_vec()));
        assert_eq!(prefix_upper_bound(b"a\xff"), Some(b"b".to_vec()));
        assert_eq!(prefix_upper_bound(b"\xff\xff"), None);
        assert_eq!(prefix_upper_bound(b""), None);
    }
}
