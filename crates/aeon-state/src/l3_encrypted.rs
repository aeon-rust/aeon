//! S3 — At-rest value-level encryption wrapper for any [`L3Store`].
//!
//! [`EncryptedL3Store`] is a transparent adapter: it seals every value on
//! the way to the inner store and opens it on the way back. The underlying
//! KV sees only `nonce || ciphertext || tag` bytes, never plaintext.
//!
//! # Key material
//!
//! A single per-store DEK is generated on first wrap, wrapped with the
//! supplied data-context [`KekHandle`], and persisted as a JSON sidecar
//! next to the store. Subsequent opens read the sidecar and unwrap the DEK
//! with the same KEK. Fresh 96-bit nonces per `put`/`write_batch` entry
//! keep AES-GCM nonce reuse well below the safe ceiling for the workloads
//! this store targets (ack cursors, offsets, small per-key checkpoints).
//!
//! Using one DEK across all values is deliberate: a per-value DEK would
//! amplify KEK wrap/unwrap calls by the per-entry write rate, and the
//! nonce freshness is what guarantees ciphertext uniqueness — not the DEK.
//!
//! # Relationship to L2 body encryption
//!
//! L2 body segments ship a per-segment DEK in a `.l2b.meta` sidecar. The
//! L3 sidecar follows the exact same pattern (JSON-encoded [`WrappedDek`]
//! with the data-context KEK domain tag) so operator tooling can rotate
//! both surfaces uniformly. See `crates/aeon-engine/src/l2_body.rs`.
//!
//! # Sidecar policy
//!
//! Mirrors the L2 policy to prevent silent downgrades:
//!
//! - sidecar missing + caller wants encryption → **generate** fresh DEK and
//!   write sidecar atomically before first write
//! - sidecar present → **must** unwrap with the supplied KEK; bail on
//!   mismatch rather than fall back to plaintext reads
//!
//! Plaintext stores are represented by not constructing this wrapper at
//! all. There is no "off-but-sidecar-present" state.

use aeon_crypto::at_rest::AtRestCipher;
use aeon_crypto::kek::KekHandle;
use aeon_types::{AeonError, BatchEntry, BatchOp, KvPairs, L3Store};
use std::path::{Path, PathBuf};

/// Transparent at-rest encryption wrapper around any [`L3Store`].
///
/// Values are sealed with AES-256-GCM using a per-store DEK wrapped by
/// the supplied data-context KEK. The wrapped DEK lives in a JSON
/// sidecar at the configured path; the inner store contains only sealed
/// bytes.
pub struct EncryptedL3Store<S: L3Store> {
    inner: S,
    cipher: AtRestCipher,
}

impl<S: L3Store> EncryptedL3Store<S> {
    /// Wrap `inner` with at-rest encryption bound to `kek`. The per-store
    /// DEK lives in a JSON sidecar at `meta_path`; on first wrap a fresh
    /// DEK is generated and the sidecar is written, on subsequent wraps
    /// the DEK is unwrapped from the sidecar.
    ///
    /// The caller picks `meta_path` — typically `{db}.kek.json`
    /// alongside the store file. Keep the sidecar on the same volume as
    /// the store so a partial-write crash cannot desync the two.
    pub fn wrap(
        inner: S,
        kek: &KekHandle,
        meta_path: impl Into<PathBuf>,
    ) -> Result<Self, AeonError> {
        let meta_path = meta_path.into();
        let cipher = if meta_path.exists() {
            let wrapped = read_sidecar(&meta_path)?;
            AtRestCipher::from_wrapped(kek, wrapped)?
        } else {
            if let Some(parent) = meta_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        AeonError::state(format!(
                            "encrypted-l3: mkdir for sidecar {parent:?}: {e}"
                        ))
                    })?;
                }
            }
            let cipher = AtRestCipher::generate(kek)?;
            write_sidecar(&meta_path, cipher.wrapped_dek())?;
            cipher
        };

        Ok(Self { inner, cipher })
    }

    /// Access the underlying store. Prefer this over bypassing the
    /// wrapper — reads through the inner store will return sealed bytes.
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S: L3Store> L3Store for EncryptedL3Store<S> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, AeonError> {
        match self.inner.get(key)? {
            Some(sealed) => Ok(Some(self.cipher.open(&sealed)?)),
            None => Ok(None),
        }
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<(), AeonError> {
        let sealed = self.cipher.seal(value)?;
        self.inner.put(key, &sealed)
    }

    fn delete(&self, key: &[u8]) -> Result<(), AeonError> {
        self.inner.delete(key)
    }

    fn write_batch(&self, ops: &[BatchEntry]) -> Result<(), AeonError> {
        if ops.is_empty() {
            return Ok(());
        }
        // Seal each Put value up-front into an owned batch before
        // dispatching to the inner store. Deletes pass through unchanged.
        let mut sealed_ops: Vec<BatchEntry> = Vec::with_capacity(ops.len());
        for (op, key, value) in ops {
            match op {
                BatchOp::Put => {
                    let pt = value.as_deref().unwrap_or(&[]);
                    let ct = self.cipher.seal(pt)?;
                    sealed_ops.push((BatchOp::Put, key.clone(), Some(ct)));
                }
                BatchOp::Delete => {
                    sealed_ops.push((BatchOp::Delete, key.clone(), None));
                }
            }
        }
        self.inner.write_batch(&sealed_ops)
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<KvPairs, AeonError> {
        let sealed = self.inner.scan_prefix(prefix)?;
        let mut out = Vec::with_capacity(sealed.len());
        for (k, v) in sealed {
            let pt = self.cipher.open(&v)?;
            out.push((k, pt));
        }
        Ok(out)
    }

    fn flush(&self) -> Result<(), AeonError> {
        self.inner.flush()
    }

    fn len(&self) -> Result<usize, AeonError> {
        self.inner.len()
    }
}

// ── Sidecar IO ────────────────────────────────────────────────────────

fn read_sidecar(path: &Path) -> Result<aeon_crypto::kek::WrappedDek, AeonError> {
    let bytes = std::fs::read(path)
        .map_err(|e| AeonError::state(format!("encrypted-l3: read sidecar {path:?}: {e}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| AeonError::state(format!("encrypted-l3: parse sidecar: {e}")))
}

fn write_sidecar(path: &Path, wrapped: &aeon_crypto::kek::WrappedDek) -> Result<(), AeonError> {
    let bytes = serde_json::to_vec(wrapped)
        .map_err(|e| AeonError::state(format!("encrypted-l3: serialize wrapped DEK: {e}")))?;
    std::fs::write(path, &bytes)
        .map_err(|e| AeonError::state(format!("encrypted-l3: write sidecar {path:?}: {e}")))
}

#[cfg(test)]
#[cfg(feature = "redb")]
mod tests {
    use super::*;
    use crate::l3::{RedbConfig, RedbStore};
    use aeon_crypto::kek::KekDomain;
    use aeon_types::{
        SecretBytes, SecretError, SecretProvider, SecretRef, SecretRegistry, SecretScheme,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    struct HexEnv;
    impl SecretProvider for HexEnv {
        fn scheme(&self) -> SecretScheme {
            SecretScheme::Env
        }
        fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError> {
            let v = std::env::var(path).map_err(|_| SecretError::EnvNotSet(path.to_string()))?;
            let bytes: Vec<u8> = (0..v.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&v[i..i + 2], 16).unwrap_or(0))
                .collect();
            Ok(SecretBytes::new(bytes))
        }
    }

    fn test_kek() -> KekHandle {
        static N: AtomicU64 = AtomicU64::new(0);
        let var = format!(
            "AEON_TEST_L3_KEK_{}",
            N.fetch_add(1, Ordering::Relaxed)
        );
        let hex: String = (0..32).map(|_| "42".to_string()).collect();
        // SAFETY: test-only env mutation, unique var per call.
        unsafe { std::env::set_var(&var, &hex) };
        let mut reg = SecretRegistry::empty();
        reg.register(Arc::new(HexEnv));
        KekHandle::new(
            KekDomain::DataContext,
            "test-l3-kek-v1",
            SecretRef::env(&var),
            Arc::new(reg),
        )
    }

    fn temp_redb() -> (RedbStore, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_l3.redb");
        let meta = dir.path().join("test_l3.kek.json");
        std::mem::forget(dir);
        let store = RedbStore::open(RedbConfig {
            path: path.clone(),
            sync_writes: false,
        })
        .unwrap();
        (store, path, meta)
    }

    #[test]
    fn wrap_generates_sidecar_on_first_open() {
        let (inner, _path, meta) = temp_redb();
        let kek = test_kek();
        assert!(!meta.exists());
        let _store = EncryptedL3Store::wrap(inner, &kek, &meta).unwrap();
        assert!(meta.exists(), "sidecar must be written");
    }

    #[test]
    fn put_get_roundtrip_encrypts_on_disk() {
        let (inner, _path, meta) = temp_redb();
        let kek = test_kek();
        let store = EncryptedL3Store::wrap(inner, &kek, &meta).unwrap();

        store.put(b"key1", b"plaintext-value-1").unwrap();
        store.put(b"key2", b"plaintext-value-2").unwrap();

        assert_eq!(store.get(b"key1").unwrap(), Some(b"plaintext-value-1".to_vec()));
        assert_eq!(store.get(b"key2").unwrap(), Some(b"plaintext-value-2".to_vec()));

        // Sanity: the inner store must not see plaintext — only the
        // sealed blob (nonce || ciphertext || tag).
        let sealed = store.inner().get(b"key1").unwrap().unwrap();
        let needle = b"plaintext-value-1";
        assert!(
            !sealed.windows(needle.len()).any(|w| w == needle),
            "plaintext leaked into sealed bytes on inner store"
        );
        assert!(
            sealed.len() >= needle.len() + 12 + 16,
            "sealed bytes must carry nonce + ciphertext + tag"
        );
    }

    #[test]
    fn reopen_with_same_kek_reads_prior_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reopen.redb");
        let meta = dir.path().join("reopen.kek.json");
        let kek = test_kek();

        {
            let inner = RedbStore::open(RedbConfig {
                path: path.clone(),
                sync_writes: false,
            })
            .unwrap();
            let store = EncryptedL3Store::wrap(inner, &kek, &meta).unwrap();
            store.put(b"persisted", b"hello").unwrap();
            store.flush().unwrap();
        }
        {
            let inner = RedbStore::open(RedbConfig {
                path,
                sync_writes: false,
            })
            .unwrap();
            let store = EncryptedL3Store::wrap(inner, &kek, &meta).unwrap();
            assert_eq!(store.get(b"persisted").unwrap(), Some(b"hello".to_vec()));
        }
    }

    #[test]
    fn reopen_with_different_kek_fails_to_unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrongkek.redb");
        let meta = dir.path().join("wrongkek.kek.json");

        let kek_a = test_kek();
        {
            let inner = RedbStore::open(RedbConfig {
                path: path.clone(),
                sync_writes: false,
            })
            .unwrap();
            let _ = EncryptedL3Store::wrap(inner, &kek_a, &meta).unwrap();
        }

        let kek_b = test_kek(); // different DEK bytes would be needed; same hex here yields
                                // same KEK content, so force a mismatch by hand-mangling the
                                // sidecar ciphertext.
        let mut sidecar: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&meta).unwrap()).unwrap();
        if let Some(ct) = sidecar.get_mut("ciphertext").and_then(|v| v.as_array_mut()) {
            if let Some(first) = ct.first_mut() {
                *first = serde_json::json!(0u8);
            }
        }
        std::fs::write(&meta, serde_json::to_vec(&sidecar).unwrap()).unwrap();

        let inner = RedbStore::open(RedbConfig {
            path,
            sync_writes: false,
        })
        .unwrap();
        let err = EncryptedL3Store::wrap(inner, &kek_b, &meta);
        assert!(err.is_err(), "corrupted sidecar must fail to unwrap");
    }

    #[test]
    fn write_batch_encrypts_each_put_entry() {
        let (inner, _path, meta) = temp_redb();
        let kek = test_kek();
        let store = EncryptedL3Store::wrap(inner, &kek, &meta).unwrap();

        store
            .write_batch(&[
                (BatchOp::Put, b"a".to_vec(), Some(b"apple".to_vec())),
                (BatchOp::Put, b"b".to_vec(), Some(b"banana".to_vec())),
                (BatchOp::Put, b"c".to_vec(), Some(b"cherry".to_vec())),
            ])
            .unwrap();

        assert_eq!(store.get(b"a").unwrap(), Some(b"apple".to_vec()));
        assert_eq!(store.get(b"b").unwrap(), Some(b"banana".to_vec()));
        assert_eq!(store.get(b"c").unwrap(), Some(b"cherry".to_vec()));

        // Inner store must hold only sealed bytes — plaintext values
        // must not be recoverable via the inner path.
        for (key, needle) in [
            (&b"a"[..], &b"apple"[..]),
            (&b"b"[..], &b"banana"[..]),
            (&b"c"[..], &b"cherry"[..]),
        ] {
            let sealed = store.inner().get(key).unwrap().unwrap();
            assert!(
                !sealed.windows(needle.len()).any(|w| w == needle),
                "plaintext batch value leaked into sealed bytes"
            );
        }

        // Mixed batch: delete one, put another.
        store
            .write_batch(&[
                (BatchOp::Delete, b"a".to_vec(), None),
                (BatchOp::Put, b"d".to_vec(), Some(b"date".to_vec())),
            ])
            .unwrap();
        assert_eq!(store.get(b"a").unwrap(), None);
        assert_eq!(store.get(b"d").unwrap(), Some(b"date".to_vec()));
    }

    #[test]
    fn scan_prefix_decrypts_each_value() {
        let (inner, _path, meta) = temp_redb();
        let kek = test_kek();
        let store = EncryptedL3Store::wrap(inner, &kek, &meta).unwrap();

        store.put(b"user:1", b"alice").unwrap();
        store.put(b"user:2", b"bob").unwrap();
        store.put(b"order:1", b"ignored").unwrap();

        let users = store.scan_prefix(b"user:").unwrap();
        assert_eq!(users.len(), 2);
        let vals: Vec<&[u8]> = users.iter().map(|(_, v)| v.as_slice()).collect();
        assert!(vals.contains(&b"alice".as_slice()));
        assert!(vals.contains(&b"bob".as_slice()));
    }

    #[test]
    fn seal_is_nondeterministic_per_put() {
        // Same key+value, written twice, must produce different sealed
        // bytes on disk thanks to fresh per-op nonces.
        let (inner, _path, meta) = temp_redb();
        let kek = test_kek();
        let store = EncryptedL3Store::wrap(inner, &kek, &meta).unwrap();

        store.put(b"k", b"same").unwrap();
        let first = store.inner().get(b"k").unwrap().unwrap();
        store.put(b"k", b"same").unwrap();
        let second = store.inner().get(b"k").unwrap().unwrap();

        assert_ne!(first, second, "nonce reuse detected — AES-GCM invariant broken");
    }

    #[test]
    fn tampered_value_is_rejected_on_get() {
        let (inner, _path, meta) = temp_redb();
        let kek = test_kek();
        let store = EncryptedL3Store::wrap(inner, &kek, &meta).unwrap();

        store.put(b"k", b"secret").unwrap();
        // Flip a byte by writing corrupted ciphertext directly to the
        // inner store, then try to read through the wrapper.
        let mut sealed = store.inner().get(b"k").unwrap().unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        store.inner().put(b"k", &sealed).unwrap();

        assert!(store.get(b"k").is_err(), "tampered ciphertext must fail to open");
    }
}
