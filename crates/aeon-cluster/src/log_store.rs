//! Persistent Raft log storage backed by an `L3Store` (FT-1).
//!
//! Replaces the in-memory [`crate::store::MemLogStore`] for production clusters
//! where the Raft log must survive node restarts. In-memory storage is retained
//! for single-node dev/test mode.
//!
//! # Key layout
//!
//! All keys live under the `raft/` prefix so multiple L3 consumers (Raft log,
//! checkpoint offsets, future partition state) can share a single backing store
//! without collision:
//!
//! | Key                          | Value                            |
//! |------------------------------|----------------------------------|
//! | `raft/vote`                  | bincode(`Vote<u64>`)             |
//! | `raft/committed`             | bincode(`LogId<u64>`)            |
//! | `raft/last_purged`           | bincode(`LogId<u64>`)            |
//! | `raft/log/{be_u64:index}`    | bincode(`Entry<AeonRaftConfig>`) |
//!
//! Log entry keys use **big-endian** encoding of the u64 index so `scan_prefix`
//! returns entries in ascending order — required for efficient range reads and
//! truncate/purge operations.
//!
//! # Durability
//!
//! Writes pass through to the L3 backend on each mutating call. The openraft
//! `LogFlushed` callback is invoked *after* the L3 write returns, so openraft
//! only considers entries committed once they are durably on disk. For redb
//! this means every append carries one fsync when `sync_writes: true`;
//! configurable via L3 backend config.

#![cfg(feature = "cluster")]

use std::fmt::Debug;
use std::ops::{Bound, RangeBounds};
use std::sync::Arc;

use aeon_types::{BatchOp, L3Store};
use openraft::storage::{LogFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::{Entry, ErrorSubject, ErrorVerb, LogId, StorageError, Vote};
use tokio::sync::RwLock;

use crate::raft_config::AeonRaftConfig;

// ── Key builders ───────────────────────────────────────────────────────────

const VOTE_KEY: &[u8] = b"raft/vote";
const COMMITTED_KEY: &[u8] = b"raft/committed";
const LAST_PURGED_KEY: &[u8] = b"raft/last_purged";
const LOG_PREFIX: &[u8] = b"raft/log/";

fn log_key(index: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(LOG_PREFIX.len() + 8);
    k.extend_from_slice(LOG_PREFIX);
    k.extend_from_slice(&index.to_be_bytes());
    k
}

fn log_index_from_key(key: &[u8]) -> Option<u64> {
    if key.len() != LOG_PREFIX.len() + 8 || !key.starts_with(LOG_PREFIX) {
        return None;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&key[LOG_PREFIX.len()..]);
    Some(u64::from_be_bytes(buf))
}

// ── Error helpers ──────────────────────────────────────────────────────────

fn storage_err_read<E: std::fmt::Display>(e: E, subject: ErrorSubject<u64>) -> StorageError<u64> {
    StorageError::from_io_error(
        subject,
        ErrorVerb::Read,
        std::io::Error::other(e.to_string()),
    )
}

fn storage_err_write<E: std::fmt::Display>(e: E, subject: ErrorSubject<u64>) -> StorageError<u64> {
    StorageError::from_io_error(
        subject,
        ErrorVerb::Write,
        std::io::Error::other(e.to_string()),
    )
}

// ── Cached metadata ────────────────────────────────────────────────────────

/// Hot metadata cached in-memory so `get_log_state`, `read_vote`, and
/// `read_committed` don't hit L3 on every call. All writes update both the
/// L3 backend and this cache atomically under the appropriate write lock.
#[derive(Debug, Default)]
struct Cache {
    vote: RwLock<Option<Vote<u64>>>,
    committed: RwLock<Option<LogId<u64>>>,
    last_purged: RwLock<Option<LogId<u64>>>,
    /// Highest log index present in L3 (None if log is empty). Maintained
    /// alongside append/truncate/purge so `get_log_state` is O(1).
    last_log_id: RwLock<Option<LogId<u64>>>,
}

// ── Persistent Raft log store ──────────────────────────────────────────────

/// L3-backed Raft log storage.
///
/// Cheap to clone — the underlying L3 handle is `Arc<dyn L3Store>` and the
/// metadata cache is `Arc<RwLock<_>>` internally. openraft's
/// `get_log_reader()` returns a clone; both the store and the reader share the
/// same backing state.
#[derive(Clone)]
pub struct L3RaftLogStore {
    l3: Arc<dyn L3Store>,
    cache: Arc<Cache>,
}

impl Debug for L3RaftLogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L3RaftLogStore").finish_non_exhaustive()
    }
}

impl L3RaftLogStore {
    /// Test-only helper: append entries and update caches without needing a
    /// real `LogFlushed` callback (whose constructor is crate-private in
    /// openraft). The production path goes through `RaftLogStorage::append`.
    #[cfg(test)]
    async fn append_for_test(
        &mut self,
        entries: Vec<Entry<AeonRaftConfig>>,
    ) -> Result<(), StorageError<u64>> {
        let mut ops: Vec<(BatchOp, Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        let mut new_last: Option<LogId<u64>> = *self.cache.last_log_id.read().await;
        for entry in entries {
            let key = log_key(entry.log_id.index);
            let value = bincode::serialize(&entry)
                .map_err(|e| storage_err_write(e, ErrorSubject::Logs))?;
            new_last = Some(match new_last {
                Some(prev) if prev.index >= entry.log_id.index => prev,
                _ => entry.log_id,
            });
            ops.push((BatchOp::Put, key, Some(value)));
        }
        if !ops.is_empty() {
            self.l3
                .write_batch(&ops)
                .map_err(|e| storage_err_write(e, ErrorSubject::Logs))?;
            *self.cache.last_log_id.write().await = new_last;
        }
        Ok(())
    }

    /// Open a persistent log store over an existing L3 backend. Loads any
    /// previously-persisted vote, committed, and last-purged markers into the
    /// in-memory cache, and scans the log prefix to recover the current
    /// `last_log_id`.
    pub async fn open(l3: Arc<dyn L3Store>) -> Result<Self, StorageError<u64>> {
        let cache = Cache::default();

        // Hydrate vote.
        if let Some(bytes) = l3
            .get(VOTE_KEY)
            .map_err(|e| storage_err_read(e, ErrorSubject::Vote))?
        {
            let vote: Vote<u64> = bincode::deserialize(&bytes)
                .map_err(|e| storage_err_read(e, ErrorSubject::Vote))?;
            *cache.vote.write().await = Some(vote);
        }

        // Hydrate committed marker.
        if let Some(bytes) = l3
            .get(COMMITTED_KEY)
            .map_err(|e| storage_err_read(e, ErrorSubject::Store))?
        {
            let committed: LogId<u64> = bincode::deserialize(&bytes)
                .map_err(|e| storage_err_read(e, ErrorSubject::Store))?;
            *cache.committed.write().await = Some(committed);
        }

        // Hydrate last_purged marker.
        if let Some(bytes) = l3
            .get(LAST_PURGED_KEY)
            .map_err(|e| storage_err_read(e, ErrorSubject::Store))?
        {
            let last_purged: LogId<u64> = bincode::deserialize(&bytes)
                .map_err(|e| storage_err_read(e, ErrorSubject::Store))?;
            *cache.last_purged.write().await = Some(last_purged);
        }

        // Recover last_log_id by scanning the log prefix. Entries are keyed by
        // big-endian index so the *last* entry in scan order has the highest
        // index — no need to deserialize every entry; we only need the tail.
        let entries = l3
            .scan_prefix(LOG_PREFIX)
            .map_err(|e| storage_err_read(e, ErrorSubject::Logs))?;
        if let Some((_, bytes)) = entries.last() {
            let entry: Entry<AeonRaftConfig> = bincode::deserialize(bytes)
                .map_err(|e| storage_err_read(e, ErrorSubject::Logs))?;
            *cache.last_log_id.write().await = Some(entry.log_id);
        }

        Ok(Self {
            l3,
            cache: Arc::new(cache),
        })
    }
}

impl RaftLogReader<AeonRaftConfig> for L3RaftLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<AeonRaftConfig>>, StorageError<u64>> {
        // scan_prefix returns entries in key order (big-endian index), which
        // matches ascending log-index order.
        let raw = self
            .l3
            .scan_prefix(LOG_PREFIX)
            .map_err(|e| storage_err_read(e, ErrorSubject::Logs))?;

        let mut out = Vec::new();
        for (k, v) in raw {
            let Some(idx) = log_index_from_key(&k) else {
                continue;
            };
            // Inclusive/exclusive bound handling.
            let lower_ok = match range.start_bound() {
                Bound::Included(s) => idx >= *s,
                Bound::Excluded(s) => idx > *s,
                Bound::Unbounded => true,
            };
            let upper_ok = match range.end_bound() {
                Bound::Included(e) => idx <= *e,
                Bound::Excluded(e) => idx < *e,
                Bound::Unbounded => true,
            };
            if !(lower_ok && upper_ok) {
                continue;
            }
            let entry: Entry<AeonRaftConfig> = bincode::deserialize(&v)
                .map_err(|e| storage_err_read(e, ErrorSubject::Logs))?;
            out.push(entry);
        }
        Ok(out)
    }
}

impl RaftLogStorage<AeonRaftConfig> for L3RaftLogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<AeonRaftConfig>, StorageError<u64>> {
        let last_log_id = *self.cache.last_log_id.read().await;
        let last_purged_log_id = *self.cache.last_purged.read().await;
        Ok(LogState {
            last_purged_log_id,
            // openraft expects last_log_id to fall back to last_purged when the
            // log is empty but has been purged — matches MemLogStore behaviour.
            last_log_id: last_log_id.or(last_purged_log_id),
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let bytes =
            bincode::serialize(vote).map_err(|e| storage_err_write(e, ErrorSubject::Vote))?;
        self.l3
            .put(VOTE_KEY, &bytes)
            .map_err(|e| storage_err_write(e, ErrorSubject::Vote))?;
        *self.cache.vote.write().await = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        Ok(*self.cache.vote.read().await)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        match committed {
            Some(c) => {
                let bytes = bincode::serialize(&c)
                    .map_err(|e| storage_err_write(e, ErrorSubject::Store))?;
                self.l3
                    .put(COMMITTED_KEY, &bytes)
                    .map_err(|e| storage_err_write(e, ErrorSubject::Store))?;
            }
            None => {
                self.l3
                    .delete(COMMITTED_KEY)
                    .map_err(|e| storage_err_write(e, ErrorSubject::Store))?;
            }
        }
        *self.cache.committed.write().await = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        Ok(*self.cache.committed.read().await)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<AeonRaftConfig>,
    ) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<AeonRaftConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut ops: Vec<(BatchOp, Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        let mut new_last: Option<LogId<u64>> = *self.cache.last_log_id.read().await;
        for entry in entries {
            let key = log_key(entry.log_id.index);
            let value = bincode::serialize(&entry)
                .map_err(|e| storage_err_write(e, ErrorSubject::Logs))?;
            new_last = Some(match new_last {
                Some(prev) if prev.index >= entry.log_id.index => prev,
                _ => entry.log_id,
            });
            ops.push((BatchOp::Put, key, Some(value)));
        }

        if !ops.is_empty() {
            self.l3
                .write_batch(&ops)
                .map_err(|e| storage_err_write(e, ErrorSubject::Logs))?;
            *self.cache.last_log_id.write().await = new_last;
        }

        // Signal openraft that the batch is durably written. For write-through
        // L3 backends this is safe to call synchronously — the data is
        // already on disk by the time write_batch returns.
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        // Delete all entries with index >= log_id.index.
        let raw = self
            .l3
            .scan_prefix(LOG_PREFIX)
            .map_err(|e| storage_err_write(e, ErrorSubject::Logs))?;
        let mut ops: Vec<(BatchOp, Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        for (k, _) in raw {
            if let Some(idx) = log_index_from_key(&k) {
                if idx >= log_id.index {
                    ops.push((BatchOp::Delete, k, None));
                }
            }
        }
        if !ops.is_empty() {
            self.l3
                .write_batch(&ops)
                .map_err(|e| storage_err_write(e, ErrorSubject::Logs))?;
        }

        // Recompute last_log_id from what remains.
        let remaining = self
            .l3
            .scan_prefix(LOG_PREFIX)
            .map_err(|e| storage_err_write(e, ErrorSubject::Logs))?;
        let new_last = match remaining.last() {
            Some((_, bytes)) => {
                let entry: Entry<AeonRaftConfig> = bincode::deserialize(bytes)
                    .map_err(|e| storage_err_write(e, ErrorSubject::Logs))?;
                Some(entry.log_id)
            }
            None => None,
        };
        *self.cache.last_log_id.write().await = new_last;
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        // Delete all entries with index <= log_id.index and record the marker.
        let raw = self
            .l3
            .scan_prefix(LOG_PREFIX)
            .map_err(|e| storage_err_write(e, ErrorSubject::Logs))?;
        let mut ops: Vec<(BatchOp, Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        for (k, _) in raw {
            if let Some(idx) = log_index_from_key(&k) {
                if idx <= log_id.index {
                    ops.push((BatchOp::Delete, k, None));
                }
            }
        }
        // Persist the last_purged marker in the same batch for atomicity.
        let marker =
            bincode::serialize(&log_id).map_err(|e| storage_err_write(e, ErrorSubject::Store))?;
        ops.push((BatchOp::Put, LAST_PURGED_KEY.to_vec(), Some(marker)));
        self.l3
            .write_batch(&ops)
            .map_err(|e| storage_err_write(e, ErrorSubject::Logs))?;

        *self.cache.last_purged.write().await = Some(log_id);
        // If purging emptied the log below last_log_id, leave last_log_id as-is
        // so get_log_state can still return (last_purged, last_log_id=last_purged)
        // when the log is entirely gone (matches MemLogStore behaviour).
        if let Some(current) = *self.cache.last_log_id.read().await {
            if current.index <= log_id.index {
                *self.cache.last_log_id.write().await = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::{CommittedLeaderId, EntryPayload};

    /// In-memory L3 stub so tests don't need redb. Implements only what the
    /// log store exercises; ops are atomic-per-call (sufficient here — the
    /// store never relies on write_batch atomicity across a crash in tests).
    #[derive(Default)]
    struct MemL3 {
        data: std::sync::Mutex<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>,
    }

    impl aeon_types::L3Store for MemL3 {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, aeon_types::AeonError> {
            Ok(self.data.lock().unwrap().get(key).cloned())
        }
        fn put(&self, key: &[u8], value: &[u8]) -> Result<(), aeon_types::AeonError> {
            self.data
                .lock()
                .unwrap()
                .insert(key.to_vec(), value.to_vec());
            Ok(())
        }
        fn delete(&self, key: &[u8]) -> Result<(), aeon_types::AeonError> {
            self.data.lock().unwrap().remove(key);
            Ok(())
        }
        fn write_batch(
            &self,
            ops: &[aeon_types::BatchEntry],
        ) -> Result<(), aeon_types::AeonError> {
            let mut d = self.data.lock().unwrap();
            for (op, k, v) in ops {
                match op {
                    BatchOp::Put => {
                        d.insert(k.clone(), v.clone().unwrap_or_default());
                    }
                    BatchOp::Delete => {
                        d.remove(k);
                    }
                }
            }
            Ok(())
        }
        fn scan_prefix(
            &self,
            prefix: &[u8],
        ) -> Result<aeon_types::KvPairs, aeon_types::AeonError> {
            let d = self.data.lock().unwrap();
            Ok(d.range(prefix.to_vec()..)
                .take_while(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect())
        }
        fn flush(&self) -> Result<(), aeon_types::AeonError> {
            Ok(())
        }
        fn len(&self) -> Result<usize, aeon_types::AeonError> {
            Ok(self.data.lock().unwrap().len())
        }
    }

    fn mk_entry(index: u64, term: u64) -> Entry<AeonRaftConfig> {
        Entry {
            log_id: LogId::new(CommittedLeaderId::new(term, 0), index),
            payload: EntryPayload::Blank,
        }
    }

    fn mk_log_id(index: u64, term: u64) -> LogId<u64> {
        LogId::new(CommittedLeaderId::new(term, 0), index)
    }

    async fn mk_store() -> (Arc<MemL3>, L3RaftLogStore) {
        let l3: Arc<MemL3> = Arc::new(MemL3::default());
        let store = L3RaftLogStore::open(l3.clone() as Arc<dyn L3Store>)
            .await
            .unwrap();
        (l3, store)
    }

    #[tokio::test]
    async fn empty_store_has_no_state() {
        let (_l3, mut store) = mk_store().await;
        let state = store.get_log_state().await.unwrap();
        assert!(state.last_log_id.is_none());
        assert!(state.last_purged_log_id.is_none());
        assert!(store.read_vote().await.unwrap().is_none());
        assert!(store.read_committed().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn vote_roundtrip_persists() {
        let (l3, mut store) = mk_store().await;
        let vote = Vote::new(7, 42);
        store.save_vote(&vote).await.unwrap();
        assert_eq!(store.read_vote().await.unwrap(), Some(vote));

        // Reopen against the same L3 — vote must survive.
        let mut reopened = L3RaftLogStore::open(l3 as Arc<dyn L3Store>)
            .await
            .unwrap();
        assert_eq!(reopened.read_vote().await.unwrap(), Some(vote));
    }

    #[tokio::test]
    async fn append_and_read_range() {
        let (_l3, mut store) = mk_store().await;
        let entries = vec![mk_entry(1, 1), mk_entry(2, 1), mk_entry(3, 1)];
        store.append_for_test(entries).await.unwrap();

        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id.unwrap().index, 3);

        let read = store.try_get_log_entries(1..=2).await.unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].log_id.index, 1);
        assert_eq!(read[1].log_id.index, 2);
    }

    #[tokio::test]
    async fn append_survives_reopen() {
        let (l3, mut store) = mk_store().await;
        store
            .append_for_test(vec![mk_entry(1, 1), mk_entry(2, 1)])
            .await
            .unwrap();

        let mut reopened = L3RaftLogStore::open(l3 as Arc<dyn L3Store>)
            .await
            .unwrap();
        let state = reopened.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id.unwrap().index, 2);

        let all = reopened.try_get_log_entries(..).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn truncate_removes_tail() {
        let (_l3, mut store) = mk_store().await;
        store
            .append_for_test(vec![mk_entry(1, 1), mk_entry(2, 1), mk_entry(3, 1)])
            .await
            .unwrap();

        store.truncate(mk_log_id(2, 1)).await.unwrap();

        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id.unwrap().index, 1);

        let all = store.try_get_log_entries(..).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].log_id.index, 1);
    }

    #[tokio::test]
    async fn purge_removes_head_and_records_marker() {
        let (l3, mut store) = mk_store().await;
        store
            .append_for_test(vec![mk_entry(1, 1), mk_entry(2, 1), mk_entry(3, 1)])
            .await
            .unwrap();

        store.purge(mk_log_id(2, 1)).await.unwrap();

        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id.unwrap().index, 2);
        assert_eq!(state.last_log_id.unwrap().index, 3);

        let all = store.try_get_log_entries(..).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].log_id.index, 3);

        // Marker must survive reopen.
        let mut reopened = L3RaftLogStore::open(l3 as Arc<dyn L3Store>)
            .await
            .unwrap();
        let state = reopened.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id.unwrap().index, 2);
    }

    #[tokio::test]
    async fn save_committed_clears_when_none() {
        let (_l3, mut store) = mk_store().await;
        store
            .save_committed(Some(mk_log_id(5, 1)))
            .await
            .unwrap();
        assert_eq!(store.read_committed().await.unwrap().unwrap().index, 5);

        store.save_committed(None).await.unwrap();
        assert!(store.read_committed().await.unwrap().is_none());
    }

    #[test]
    fn log_key_roundtrip() {
        let k = log_key(42);
        assert_eq!(log_index_from_key(&k), Some(42));
        assert_eq!(log_index_from_key(b"raft/vote"), None);
        assert_eq!(log_index_from_key(b"raft/log/short"), None);
    }

    #[test]
    fn log_key_is_sorted_big_endian() {
        // Verify that string ordering of keys matches numeric ordering of
        // indices — required for scan_prefix to return entries in log order.
        let k1 = log_key(1);
        let k2 = log_key(2);
        let k255 = log_key(255);
        let k256 = log_key(256);
        assert!(k1 < k2);
        assert!(k255 < k256);
    }
}
