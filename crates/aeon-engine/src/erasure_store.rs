//! S6.3 — Erasure tombstone storage on L3.
//!
//! One [`ErasureTombstone`] per pending subject. Written once by the
//! REST erasure handler, read by the compactor on every sweep, and
//! transitioned to `Completed` / `Failed` once the physical delete +
//! PoH null-receipt has been appended (S6.4).
//!
//! # On-disk layout
//!
//! ```text
//! erasure/tombstones/{uuid_bytes}   →  bincode(ErasureTombstone)
//! ```
//!
//! UUIDv7 keys sort naturally by accept time (timestamp-first bit
//! layout), so `scan_prefix` returns oldest tombstones first — the
//! compactor walks them in GDPR-SLA order.
//!
//! # Trait separation
//!
//! `ErasureStore` is the minimal surface the REST handler and
//! compactor consume. `L3ErasureStore` is the L3-backed impl.
//! Future backends (e.g. a cluster-replicated store) implement the
//! trait without changing call sites.

use aeon_types::{
    AeonError, BatchEntry, BatchOp, ErasureTombstone, L3Store, TombstoneState,
};
use std::sync::Arc;
use uuid::Uuid;

/// Minimal surface for persisting erasure tombstones. The trait is
/// synchronous — this path is not on the event hot path, and L3 I/O
/// in aeon-engine is blocking (same as checkpoint persist).
pub trait ErasureStore: Send + Sync {
    /// Persist a new tombstone. Returns an error if an entry with the
    /// same id already exists — callers must generate a fresh UUIDv7.
    fn append(&self, t: &ErasureTombstone) -> Result<(), AeonError>;

    /// Load every tombstone (all states). Used by operator endpoints
    /// and by startup recovery.
    fn list_all(&self) -> Result<Vec<ErasureTombstone>, AeonError>;

    /// Load only tombstones in `Pending` state. Used by the compactor
    /// on every sweep to find work.
    fn list_pending(&self) -> Result<Vec<ErasureTombstone>, AeonError>;

    /// Look up a single tombstone by id. Returns `None` if absent.
    fn get(&self, id: &Uuid) -> Result<Option<ErasureTombstone>, AeonError>;

    /// Update the `state` of an existing tombstone. Used by the
    /// compactor once a tombstone has been fully honored (or has
    /// permanently failed). Errors if the tombstone is absent.
    fn mark_state(&self, id: &Uuid, state: TombstoneState) -> Result<(), AeonError>;

    /// Count of tombstones in `Pending` state — for the
    /// `aeon_erasure_tombstones_pending` metric.
    fn pending_count(&self) -> Result<usize, AeonError>;
}

/// Prefix for all erasure tombstones in the L3 keyspace. Kept stable
/// across versions so an in-place upgrade does not lose pending work.
pub const L3_ERASURE_PREFIX: &[u8] = b"erasure/tombstones/";

/// L3-backed [`ErasureStore`]. Thread-safe — the trait methods take
/// `&self` because the underlying `L3Store` is already internally
/// synchronized.
pub struct L3ErasureStore {
    l3: Arc<dyn L3Store>,
}

impl L3ErasureStore {
    /// Open an erasure store on top of the shared L3 backend.
    pub fn open(l3: Arc<dyn L3Store>) -> Self {
        Self { l3 }
    }

    fn record_key(id: &Uuid) -> Vec<u8> {
        let mut k = Vec::with_capacity(L3_ERASURE_PREFIX.len() + 16);
        k.extend_from_slice(L3_ERASURE_PREFIX);
        k.extend_from_slice(id.as_bytes());
        k
    }

    fn encode(t: &ErasureTombstone) -> Result<Vec<u8>, AeonError> {
        bincode::serialize(t)
            .map_err(|e| AeonError::state(format!("erasure tombstone serialize: {e}")))
    }

    fn decode(bytes: &[u8]) -> Result<ErasureTombstone, AeonError> {
        bincode::deserialize(bytes)
            .map_err(|e| AeonError::state(format!("erasure tombstone deserialize: {e}")))
    }
}

impl ErasureStore for L3ErasureStore {
    fn append(&self, t: &ErasureTombstone) -> Result<(), AeonError> {
        let key = Self::record_key(&t.id);
        // Reject duplicate-id writes — the REST handler is the only
        // writer and must allocate a fresh UUIDv7 per request; a
        // collision here is a bug and must not silently overwrite.
        if self.l3.get(&key)?.is_some() {
            return Err(AeonError::state(format!(
                "erasure tombstone {} already exists",
                t.id
            )));
        }
        let value = Self::encode(t)?;
        let ops: Vec<BatchEntry> = vec![(BatchOp::Put, key, Some(value))];
        self.l3.write_batch(&ops)?;
        self.l3.flush()?;
        Ok(())
    }

    fn list_all(&self) -> Result<Vec<ErasureTombstone>, AeonError> {
        let entries = self.l3.scan_prefix(L3_ERASURE_PREFIX)?;
        let mut out = Vec::with_capacity(entries.len());
        for (_, bytes) in entries {
            out.push(Self::decode(&bytes)?);
        }
        Ok(out)
    }

    fn list_pending(&self) -> Result<Vec<ErasureTombstone>, AeonError> {
        let all = self.list_all()?;
        Ok(all.into_iter().filter(|t| t.state == TombstoneState::Pending).collect())
    }

    fn get(&self, id: &Uuid) -> Result<Option<ErasureTombstone>, AeonError> {
        let key = Self::record_key(id);
        match self.l3.get(&key)? {
            Some(bytes) => Ok(Some(Self::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    fn mark_state(&self, id: &Uuid, state: TombstoneState) -> Result<(), AeonError> {
        let key = Self::record_key(id);
        let bytes = self
            .l3
            .get(&key)?
            .ok_or_else(|| AeonError::state(format!("erasure tombstone {id} not found")))?;
        let mut t = Self::decode(&bytes)?;
        t.state = state;
        let value = Self::encode(&t)?;
        let ops: Vec<BatchEntry> = vec![(BatchOp::Put, key, Some(value))];
        self.l3.write_batch(&ops)?;
        self.l3.flush()?;
        Ok(())
    }

    fn pending_count(&self) -> Result<usize, AeonError> {
        Ok(self.list_pending()?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{ErasureRequest, ErasureSelector, KvPairs};
    use std::time::Duration;

    /// In-memory `L3Store` stub — mirrors the one used in
    /// `checkpoint::tests::MemL3`. Kept local so this module's tests
    /// can run without leaking a memory backend into the public API.
    #[derive(Default)]
    struct MemL3 {
        data: std::sync::Mutex<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>,
    }

    impl aeon_types::L3Store for MemL3 {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, AeonError> {
            Ok(self.data.lock().unwrap().get(key).cloned())
        }
        fn put(&self, key: &[u8], value: &[u8]) -> Result<(), AeonError> {
            self.data.lock().unwrap().insert(key.to_vec(), value.to_vec());
            Ok(())
        }
        fn delete(&self, key: &[u8]) -> Result<(), AeonError> {
            self.data.lock().unwrap().remove(key);
            Ok(())
        }
        fn write_batch(&self, ops: &[BatchEntry]) -> Result<(), AeonError> {
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
        fn scan_prefix(&self, prefix: &[u8]) -> Result<KvPairs, AeonError> {
            let d = self.data.lock().unwrap();
            Ok(d.range(prefix.to_vec()..)
                .take_while(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect())
        }
        fn flush(&self) -> Result<(), AeonError> {
            Ok(())
        }
        fn len(&self) -> Result<usize, AeonError> {
            Ok(self.data.lock().unwrap().len())
        }
    }

    fn store() -> L3ErasureStore {
        L3ErasureStore::open(Arc::new(MemL3::default()))
    }

    fn make_tombstone(pipeline: &str, subject: &str, accepted_at: i64) -> ErasureTombstone {
        let req = ErasureRequest {
            pipeline: Arc::from(pipeline),
            selector: ErasureSelector::parse(subject).unwrap(),
            reason: None,
            soft_delete: None,
        };
        req.into_tombstone(accepted_at, Uuid::now_v7())
    }

    #[test]
    fn append_and_get_roundtrip() {
        let s = store();
        let t = make_tombstone("p1", "tenant-a/user-1", 1_000);
        s.append(&t).unwrap();
        let back = s.get(&t.id).unwrap().expect("present");
        assert_eq!(back, t);
    }

    #[test]
    fn append_rejects_duplicate_id() {
        let s = store();
        let t = make_tombstone("p1", "tenant-a/user-1", 1_000);
        s.append(&t).unwrap();
        let err = s.append(&t).unwrap_err();
        assert!(format!("{err}").contains("already exists"));
    }

    #[test]
    fn get_absent_returns_none() {
        let s = store();
        let id = Uuid::now_v7();
        assert!(s.get(&id).unwrap().is_none());
    }

    #[test]
    fn list_all_empty() {
        let s = store();
        assert!(s.list_all().unwrap().is_empty());
    }

    #[test]
    fn list_all_returns_everything() {
        let s = store();
        let a = make_tombstone("p1", "tenant-a/user-1", 1);
        let b = make_tombstone("p1", "tenant-a/user-2", 2);
        s.append(&a).unwrap();
        s.append(&b).unwrap();
        let all = s.list_all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn list_pending_filters_completed() {
        let s = store();
        let a = make_tombstone("p1", "tenant-a/user-1", 1);
        let b = make_tombstone("p1", "tenant-a/user-2", 2);
        s.append(&a).unwrap();
        s.append(&b).unwrap();
        s.mark_state(&a.id, TombstoneState::Completed).unwrap();
        let pending = s.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, b.id);
    }

    #[test]
    fn mark_state_updates_existing() {
        let s = store();
        let t = make_tombstone("p1", "tenant-a/user-1", 1);
        s.append(&t).unwrap();
        s.mark_state(&t.id, TombstoneState::Completed).unwrap();
        let back = s.get(&t.id).unwrap().expect("present");
        assert_eq!(back.state, TombstoneState::Completed);
    }

    #[test]
    fn mark_state_errors_on_absent() {
        let s = store();
        let id = Uuid::now_v7();
        let err = s.mark_state(&id, TombstoneState::Completed).unwrap_err();
        assert!(format!("{err}").contains("not found"));
    }

    #[test]
    fn mark_state_failed_is_not_pending() {
        let s = store();
        let t = make_tombstone("p1", "tenant-a/user-1", 1);
        s.append(&t).unwrap();
        s.mark_state(&t.id, TombstoneState::Failed).unwrap();
        assert!(s.list_pending().unwrap().is_empty());
    }

    #[test]
    fn pending_count_matches_list_pending() {
        let s = store();
        for i in 0..5 {
            let t = make_tombstone("p1", &format!("tenant-a/user-{i}"), i as i64);
            s.append(&t).unwrap();
        }
        assert_eq!(s.pending_count().unwrap(), 5);
        let first = s.list_all().unwrap()[0].id;
        s.mark_state(&first, TombstoneState::Completed).unwrap();
        assert_eq!(s.pending_count().unwrap(), 4);
    }

    #[test]
    fn wildcard_tombstone_roundtrips() {
        let s = store();
        let req = ErasureRequest {
            pipeline: Arc::from("p1"),
            selector: ErasureSelector::parse("tenant-a/*").unwrap(),
            reason: Some("audit-77".to_string()),
            soft_delete: Some(Duration::from_secs(60)),
        };
        let t = req.into_tombstone(42, Uuid::now_v7());
        s.append(&t).unwrap();
        let back = s.get(&t.id).unwrap().expect("present");
        assert_eq!(back, t);
        assert!(back.selector.is_wildcard());
        assert_eq!(back.hard_delete_after_nanos, Some(42 + 60_000_000_000));
    }

    #[test]
    fn list_pending_is_uuidv7_sorted() {
        // UUIDv7 timestamp-first layout ⇒ scan_prefix returns
        // oldest-first; the compactor relies on this to honor the
        // 30-day SLA in arrival order.
        let s = store();
        let t1 = make_tombstone("p1", "tenant-a/user-1", 1);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t2 = make_tombstone("p1", "tenant-a/user-2", 2);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t3 = make_tombstone("p1", "tenant-a/user-3", 3);
        // Insert out of order — storage sort must still be by id.
        s.append(&t2).unwrap();
        s.append(&t3).unwrap();
        s.append(&t1).unwrap();
        let pending = s.list_pending().unwrap();
        assert_eq!(pending[0].id, t1.id);
        assert_eq!(pending[1].id, t2.id);
        assert_eq!(pending[2].id, t3.id);
    }
}
