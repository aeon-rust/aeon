//! Processor identity store — CRUD for ED25519 processor identities.
//!
//! Manages the registration, lookup, and revocation of processor ED25519
//! public keys. In production, this state is Raft-replicated; this module
//! provides the in-memory data structure used by the local node.

use aeon_types::processor_identity::ProcessorIdentity;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, Ordering};

/// In-memory store for processor identities.
///
/// Keyed by (processor_name, fingerprint). Thread-safe via DashMap.
/// Tracks active connection counts per identity for max_instances enforcement.
pub struct ProcessorIdentityStore {
    /// All identities, keyed by fingerprint.
    identities: DashMap<String, ProcessorIdentity>,
    /// Active connection count per fingerprint.
    connections: DashMap<String, AtomicU32>,
}

impl ProcessorIdentityStore {
    pub fn new() -> Self {
        Self {
            identities: DashMap::new(),
            connections: DashMap::new(),
        }
    }

    /// Register a new processor identity.
    ///
    /// Returns the fingerprint on success, or an error if an active identity
    /// with the same fingerprint already exists.
    pub fn register(&self, identity: ProcessorIdentity) -> Result<String, aeon_types::AeonError> {
        let fingerprint = identity.fingerprint.clone();

        if let Some(existing) = self.identities.get(&fingerprint) {
            if existing.is_active() {
                return Err(aeon_types::AeonError::state(format!(
                    "identity with fingerprint {} already exists and is active",
                    fingerprint
                )));
            }
        }

        self.identities.insert(fingerprint.clone(), identity);
        self.connections
            .entry(fingerprint.clone())
            .or_insert_with(|| AtomicU32::new(0));
        Ok(fingerprint)
    }

    /// Look up an identity by fingerprint.
    pub fn get(&self, fingerprint: &str) -> Option<ProcessorIdentity> {
        self.identities.get(fingerprint).map(|r| r.clone())
    }

    /// Look up an active identity by public key string (e.g., "ed25519:MCow...").
    pub fn get_by_public_key(&self, public_key: &str) -> Option<ProcessorIdentity> {
        self.identities
            .iter()
            .find(|r| r.public_key == public_key && r.is_active())
            .map(|r| r.clone())
    }

    /// List all identities for a given processor name.
    pub fn list_for_processor(&self, processor_name: &str) -> Vec<ProcessorIdentity> {
        self.identities
            .iter()
            .filter(|r| r.processor_name == processor_name)
            .map(|r| r.clone())
            .collect()
    }

    /// Revoke an identity by fingerprint. Returns the revoked identity, or None
    /// if not found or already revoked.
    pub fn revoke(&self, fingerprint: &str, revoked_at: i64) -> Option<ProcessorIdentity> {
        let mut entry = self.identities.get_mut(fingerprint)?;
        if entry.revoked_at.is_some() {
            return None; // already revoked
        }
        entry.revoked_at = Some(revoked_at);
        Some(entry.clone())
    }

    /// Increment the active connection count for an identity.
    /// Returns the new count, or None if the identity doesn't exist.
    pub fn connect(&self, fingerprint: &str) -> Option<u32> {
        self.connections
            .get(fingerprint)
            .map(|counter| counter.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Decrement the active connection count for an identity.
    pub fn disconnect(&self, fingerprint: &str) {
        if let Some(counter) = self.connections.get(fingerprint) {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Get the current active connection count for an identity.
    pub fn active_connections(&self, fingerprint: &str) -> u32 {
        self.connections
            .get(fingerprint)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Check whether a new connection is allowed (within max_instances).
    pub fn can_connect(&self, fingerprint: &str) -> bool {
        if let Some(identity) = self.identities.get(fingerprint) {
            if !identity.is_active() {
                return false;
            }
            let current = self.active_connections(fingerprint);
            current < identity.max_instances
        } else {
            false
        }
    }

    /// Total number of identities (active + revoked).
    pub fn count(&self) -> usize {
        self.identities.len()
    }

    /// Number of active (non-revoked) identities.
    pub fn active_count(&self) -> usize {
        self.identities.iter().filter(|r| r.is_active()).count()
    }

    /// Snapshot all identities (for Raft snapshot).
    pub fn snapshot(&self) -> Vec<ProcessorIdentity> {
        self.identities.iter().map(|r| r.clone()).collect()
    }

    /// Restore from a snapshot (for Raft restore).
    pub fn restore(&self, identities: Vec<ProcessorIdentity>) {
        self.identities.clear();
        self.connections.clear();
        for id in identities {
            let fp = id.fingerprint.clone();
            self.identities.insert(fp.clone(), id);
            self.connections.insert(fp, AtomicU32::new(0));
        }
    }
}

impl Default for ProcessorIdentityStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::processor_identity::PipelineScope;

    fn make_identity(name: &str, fingerprint: &str) -> ProcessorIdentity {
        ProcessorIdentity {
            public_key: format!("ed25519:key-{fingerprint}"),
            fingerprint: fingerprint.into(),
            processor_name: name.into(),
            allowed_pipelines: PipelineScope::AllMatchingPipelines,
            max_instances: 2,
            registered_at: 1000,
            registered_by: "test".into(),
            revoked_at: None,
        }
    }

    #[test]
    fn register_and_get() {
        let store = ProcessorIdentityStore::new();
        let id = make_identity("proc-a", "fp-1");
        store.register(id).unwrap();

        let got = store.get("fp-1").unwrap();
        assert_eq!(got.processor_name, "proc-a");
        assert!(got.is_active());
    }

    #[test]
    fn register_duplicate_active_fails() {
        let store = ProcessorIdentityStore::new();
        store.register(make_identity("proc-a", "fp-1")).unwrap();
        let result = store.register(make_identity("proc-a", "fp-1"));
        assert!(result.is_err());
    }

    #[test]
    fn register_after_revoke_succeeds() {
        let store = ProcessorIdentityStore::new();
        store.register(make_identity("proc-a", "fp-1")).unwrap();
        store.revoke("fp-1", 2000);
        // Re-register with same fingerprint after revocation
        let result = store.register(make_identity("proc-a", "fp-1"));
        assert!(result.is_ok());
    }

    #[test]
    fn get_by_public_key() {
        let store = ProcessorIdentityStore::new();
        store.register(make_identity("proc-a", "fp-1")).unwrap();

        let got = store.get_by_public_key("ed25519:key-fp-1").unwrap();
        assert_eq!(got.fingerprint, "fp-1");

        assert!(store.get_by_public_key("ed25519:nonexistent").is_none());
    }

    #[test]
    fn list_for_processor() {
        let store = ProcessorIdentityStore::new();
        store.register(make_identity("proc-a", "fp-1")).unwrap();
        store.register(make_identity("proc-a", "fp-2")).unwrap();
        store.register(make_identity("proc-b", "fp-3")).unwrap();

        let list = store.list_for_processor("proc-a");
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn revoke_identity() {
        let store = ProcessorIdentityStore::new();
        store.register(make_identity("proc-a", "fp-1")).unwrap();

        let revoked = store.revoke("fp-1", 5000).unwrap();
        assert_eq!(revoked.revoked_at, Some(5000));
        assert!(!store.get("fp-1").unwrap().is_active());

        // Double revoke returns None
        assert!(store.revoke("fp-1", 6000).is_none());
    }

    #[test]
    fn connection_tracking() {
        let store = ProcessorIdentityStore::new();
        store.register(make_identity("proc-a", "fp-1")).unwrap();

        assert!(store.can_connect("fp-1"));
        assert_eq!(store.connect("fp-1"), Some(1));
        assert!(store.can_connect("fp-1")); // max_instances = 2
        assert_eq!(store.connect("fp-1"), Some(2));
        assert!(!store.can_connect("fp-1")); // at max

        store.disconnect("fp-1");
        assert!(store.can_connect("fp-1"));
        assert_eq!(store.active_connections("fp-1"), 1);
    }

    #[test]
    fn revoked_identity_cannot_connect() {
        let store = ProcessorIdentityStore::new();
        store.register(make_identity("proc-a", "fp-1")).unwrap();
        store.revoke("fp-1", 2000);
        assert!(!store.can_connect("fp-1"));
    }

    #[test]
    fn snapshot_and_restore() {
        let store = ProcessorIdentityStore::new();
        store.register(make_identity("proc-a", "fp-1")).unwrap();
        store.register(make_identity("proc-b", "fp-2")).unwrap();
        store.revoke("fp-2", 3000);

        let snap = store.snapshot();
        assert_eq!(snap.len(), 2);

        let store2 = ProcessorIdentityStore::new();
        store2.restore(snap);
        assert_eq!(store2.count(), 2);
        assert!(store2.get("fp-1").unwrap().is_active());
        assert!(!store2.get("fp-2").unwrap().is_active());
    }

    #[test]
    fn counts() {
        let store = ProcessorIdentityStore::new();
        store.register(make_identity("p", "fp-1")).unwrap();
        store.register(make_identity("p", "fp-2")).unwrap();
        store.revoke("fp-2", 2000);

        assert_eq!(store.count(), 2);
        assert_eq!(store.active_count(), 1);
    }
}
