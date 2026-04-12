//! L3 persistent state store trait and associated types.
//!
//! This module defines the [`L3Store`] trait — the adapter interface for Aeon's
//! durable key-value layer. Implementations live in `aeon-state` (redb, RocksDB in
//! future), but the trait itself is defined here so that any crate can depend on the
//! abstraction without pulling in `aeon-state` (notably `aeon-cluster` uses this for
//! persistent Raft log storage — see `FAULT-TOLERANCE-ANALYSIS.md` FT-1).
//!
//! **Adapter pattern**: The trait is deliberately object-safe (`Box<dyn L3Store>` /
//! `Arc<dyn L3Store>` allowed). It has no generic methods and no `Self`-returning
//! methods.
//!
//! **Design rationale**: L3 is write-heavy in steady state (write-behind from L1,
//! checkpoint offsets) but read-infrequent (crash recovery, L1+L2 miss fallthrough).
//! Backends selected via [`L3Backend`] config.

use crate::AeonError;

/// Result type for key-value scan operations.
pub type KvPairs = Vec<(Vec<u8>, Vec<u8>)>;

/// Batch operation type for [`L3Store::write_batch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOp {
    Put,
    Delete,
}

/// A single batch operation: (op, key, optional value).
pub type BatchEntry = (BatchOp, Vec<u8>, Option<Vec<u8>>);

/// Configuration for the L3 backend selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum L3Backend {
    /// redb — pure Rust B-tree database (default).
    #[default]
    Redb,
    /// RocksDB — LSM-tree (future, feature-gated).
    RocksDb,
}

/// L3 persistent state store trait.
///
/// Implementations must be thread-safe. The interface is synchronous — the async
/// boundary lives at the `TieredStore` level (in `aeon-state`), which calls L3 ops
/// on a background task.
///
/// Object-safe: usable as `Box<dyn L3Store>` or `Arc<dyn L3Store>`.
pub trait L3Store: Send + Sync {
    /// Get a value by key. Returns `None` if not found.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, AeonError>;

    /// Put a key-value pair. Overwrites if key exists.
    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), AeonError>;

    /// Delete a key. No-op if key doesn't exist.
    fn delete(&self, key: &[u8]) -> Result<(), AeonError>;

    /// Execute a batch of operations atomically.
    ///
    /// For `BatchOp::Put`, the value slice must be `Some`. For `BatchOp::Delete`, it's `None`.
    fn write_batch(&self, ops: &[BatchEntry]) -> Result<(), AeonError>;

    /// Scan all entries whose keys start with `prefix`.
    fn scan_prefix(&self, prefix: &[u8]) -> Result<KvPairs, AeonError>;

    /// Flush any buffered writes to durable storage.
    fn flush(&self) -> Result<(), AeonError>;

    /// Approximate number of entries.
    fn len(&self) -> Result<usize, AeonError>;

    /// Whether the store is empty.
    fn is_empty(&self) -> Result<bool, AeonError> {
        Ok(self.len()? == 0)
    }
}
