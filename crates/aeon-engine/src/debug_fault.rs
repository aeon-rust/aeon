//! W3 / F1 — debug-only L3 fault injection wrapper.
//!
//! Wraps an inner `Arc<dyn L3Store>` with an atomic "fail next N writes"
//! counter. When the counter is non-zero, every `put` / `delete` /
//! `write_batch` call decrements it and returns
//! `Err(AeonError::state(...))`. `get` / `scan_prefix` / `len` / `flush`
//! pass through unconditionally — the goal is to exercise the
//! `FallbackCheckpointStore::engage_fallback` path, which only triggers
//! on write errors.
//!
//! Wired in `cmd_serve` so the supervisor's installed L3 store is
//! always wrapped (the wrapper is a no-op when the counter is 0). The
//! REST endpoint `POST /api/v1/test/inject-l3-fault?count=N` flips the
//! counter; the matching CLI subcommand `aeon test inject-l3-fault`
//! drives it from outside.
//!
//! This is the only path proven to drive `engage_fallback` live on
//! k3s + redb without breaking the open `O_RDWR` mmap'd fd. See
//! `docs/examples/chaos-l3-fault-eaccess.yaml` for the chaos
//! mechanisms tried before this one.

use aeon_types::{AeonError, BatchEntry, KvPairs, L3Store};
#[cfg(test)]
use aeon_types::BatchOp;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Wraps an inner L3 store and fails the next N write operations.
///
/// `Default` produces a no-op wrapper (counter at 0). Cloning shares
/// the same atomic counter — the supervisor holds one Arc, the REST
/// handler holds another, both see the same state.
#[derive(Clone)]
pub struct FaultyL3Wrapper {
    inner: Arc<dyn L3Store>,
    /// Number of write operations to fail before returning to passthrough.
    /// Decremented on every put / delete / write_batch when non-zero.
    fail_remaining: Arc<AtomicU64>,
    /// Total faults injected — useful for tests and `/poh-head`-style
    /// observability. Never decremented.
    fail_count_total: Arc<AtomicU64>,
}

impl FaultyL3Wrapper {
    pub fn new(inner: Arc<dyn L3Store>) -> Self {
        Self {
            inner,
            fail_remaining: Arc::new(AtomicU64::new(0)),
            fail_count_total: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Schedule the next `count` write operations to fail. Resets the
    /// counter; subsequent writes decrement it until it hits 0, after
    /// which writes pass through to the inner store.
    pub fn inject_faults(&self, count: u64) {
        self.fail_remaining.store(count, Ordering::SeqCst);
    }

    /// Currently-armed fault count.
    pub fn fault_remaining(&self) -> u64 {
        self.fail_remaining.load(Ordering::Relaxed)
    }

    /// Cumulative fault count since process start.
    pub fn fault_total(&self) -> u64 {
        self.fail_count_total.load(Ordering::Relaxed)
    }

    /// Check-and-decrement the fault counter. Returns Some(error)
    /// when a fault should be injected.
    fn maybe_inject(&self, op: &'static str) -> Option<AeonError> {
        loop {
            let cur = self.fail_remaining.load(Ordering::Relaxed);
            if cur == 0 {
                return None;
            }
            if self
                .fail_remaining
                .compare_exchange_weak(cur, cur - 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                self.fail_count_total.fetch_add(1, Ordering::Relaxed);
                return Some(AeonError::state(format!(
                    "FaultyL3Wrapper: injected fault on {op} ({} writes remaining)",
                    cur - 1
                )));
            }
        }
    }
}

impl L3Store for FaultyL3Wrapper {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, AeonError> {
        self.inner.get(key)
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), AeonError> {
        if let Some(e) = self.maybe_inject("put") {
            return Err(e);
        }
        self.inner.put(key, value)
    }

    fn delete(&self, key: &[u8]) -> Result<(), AeonError> {
        if let Some(e) = self.maybe_inject("delete") {
            return Err(e);
        }
        self.inner.delete(key)
    }

    fn write_batch(&self, ops: &[BatchEntry]) -> Result<(), AeonError> {
        if let Some(e) = self.maybe_inject("write_batch") {
            return Err(e);
        }
        self.inner.write_batch(ops)
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<KvPairs, AeonError> {
        self.inner.scan_prefix(prefix)
    }

    fn flush(&self) -> Result<(), AeonError> {
        self.inner.flush()
    }

    fn len(&self) -> Result<usize, AeonError> {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Trivial in-memory L3Store for tests.
    #[derive(Default)]
    struct MemL3 {
        data: Mutex<std::collections::HashMap<Vec<u8>, Vec<u8>>>,
    }
    impl L3Store for MemL3 {
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
            for (op, key, value) in ops {
                match op {
                    BatchOp::Put => {
                        if let Some(v) = value {
                            d.insert(key.clone(), v.clone());
                        }
                    }
                    BatchOp::Delete => {
                        d.remove(key);
                    }
                }
            }
            Ok(())
        }
        fn scan_prefix(&self, _prefix: &[u8]) -> Result<KvPairs, AeonError> {
            Ok(KvPairs::default())
        }
        fn flush(&self) -> Result<(), AeonError> {
            Ok(())
        }
        fn len(&self) -> Result<usize, AeonError> {
            Ok(self.data.lock().unwrap().len())
        }
    }

    #[test]
    fn passthrough_when_counter_is_zero() {
        let inner: Arc<dyn L3Store> = Arc::new(MemL3::default());
        let w = FaultyL3Wrapper::new(inner);
        w.put(b"k", b"v").unwrap();
        assert_eq!(w.get(b"k").unwrap(), Some(b"v".to_vec()));
        assert_eq!(w.fault_total(), 0);
    }

    #[test]
    fn injects_exactly_n_faults_then_resumes() {
        let inner: Arc<dyn L3Store> = Arc::new(MemL3::default());
        let w = FaultyL3Wrapper::new(inner);
        w.inject_faults(3);
        assert_eq!(w.fault_remaining(), 3);

        // First three writes fail.
        for i in 0..3 {
            let e = w.put(format!("k{i}").as_bytes(), b"v").unwrap_err();
            assert!(format!("{e}").contains("injected fault on put"));
        }
        assert_eq!(w.fault_remaining(), 0);
        assert_eq!(w.fault_total(), 3);

        // Fourth write succeeds.
        w.put(b"k3", b"v").unwrap();
        assert_eq!(w.get(b"k3").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn reads_pass_through_during_injection() {
        let inner: Arc<dyn L3Store> = Arc::new(MemL3::default());
        let w = FaultyL3Wrapper::new(Arc::clone(&inner) as Arc<dyn L3Store>);
        // Seed via the inner store directly (bypasses fault injection).
        inner.put(b"seed", b"v").unwrap();
        w.inject_faults(5);
        // get / scan / len pass through even when faults are armed.
        assert_eq!(w.get(b"seed").unwrap(), Some(b"v".to_vec()));
        assert_eq!(w.len().unwrap(), 1);
        // No put/delete attempted, so the counter stays at 5.
        assert_eq!(w.fault_remaining(), 5);
    }

    #[test]
    fn write_batch_counts_as_one_fault() {
        let inner: Arc<dyn L3Store> = Arc::new(MemL3::default());
        let w = FaultyL3Wrapper::new(inner);
        w.inject_faults(1);
        let ops: Vec<BatchEntry> = vec![
            (BatchOp::Put, b"k1".to_vec(), Some(b"v1".to_vec())),
            (BatchOp::Put, b"k2".to_vec(), Some(b"v2".to_vec())),
        ];
        let e = w.write_batch(&ops).unwrap_err();
        assert!(format!("{e}").contains("write_batch"));
        // Counter consumed by the single batch op, not per-entry.
        assert_eq!(w.fault_remaining(), 0);
        // Next batch passes through.
        w.write_batch(&ops).unwrap();
        assert_eq!(w.len().unwrap(), 2);
    }

    #[test]
    fn shared_arc_sees_same_counter() {
        let inner: Arc<dyn L3Store> = Arc::new(MemL3::default());
        let w1 = FaultyL3Wrapper::new(inner);
        let w2 = w1.clone();
        // w1 arms faults; w2 sees them too because the counter is Arc-shared.
        w1.inject_faults(2);
        assert_eq!(w2.fault_remaining(), 2);
        let _ = w2.put(b"k", b"v");
        assert_eq!(w1.fault_remaining(), 1);
    }
}
