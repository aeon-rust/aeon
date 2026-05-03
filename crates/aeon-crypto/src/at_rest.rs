//! S3 — At-rest AES-256-GCM payload sealing.
//!
//! `AtRestCipher` holds one unwrapped 32-byte DEK and seals individual
//! payloads with a fresh 96-bit random nonce per operation. Sealed
//! output layout:
//!
//! ```text
//! [nonce:12][ciphertext:n][gcm_tag:16]
//! ```
//!
//! Overhead: **28 bytes per sealed payload** (12-byte nonce + 16-byte
//! authentication tag). Callers persist the wrapped DEK once per
//! segment / per KV store alongside the sealed data; `open()` is
//! called with the same sealed layout.
//!
//! # Nonce collision bound
//!
//! 96-bit random nonces give ~2^48 seals before birthday collisions
//! become concerning (10^-9 probability at ~2^46 seals). One DEK per
//! L2 segment bounds seal count to a segment's record count — well
//! under the safe ceiling at any realistic segment size.
//!
//! # Usage
//!
//! - **L2 segment** — call `AtRestCipher::generate(kek)` on segment
//!   create, persist `wrapped_dek()` to the `.meta` sidecar, call
//!   `seal()` for each record write and `open()` on recovery.
//! - **L3 value store** — one cipher per store instance (or per table).
//!   Seal every value before `put()`, open every value after `get()`.

use aeon_types::AeonError;
use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, KeyInit, Nonce};

use crate::kek::{DekBytes, KekHandle, WrappedDek};

/// Length of the AES-256-GCM nonce prefix in sealed output.
pub const AT_REST_NONCE_LEN: usize = 12;
/// Length of the AES-256-GCM authentication tag suffix in sealed output.
pub const AT_REST_TAG_LEN: usize = 16;
/// Fixed byte overhead added to every sealed payload.
pub const AT_REST_OVERHEAD: usize = AT_REST_NONCE_LEN + AT_REST_TAG_LEN;

/// A loaded at-rest cipher — one DEK unwrapped and kept in memory for
/// the life of an L2 segment or L3 store instance. Drops zeroize the DEK.
pub struct AtRestCipher {
    dek: DekBytes,
    wrapped: WrappedDek,
}

impl AtRestCipher {
    /// Generate a fresh DEK, wrap it with the given KEK, and return a
    /// cipher ready to seal payloads. Persist `wrapped_dek()` alongside
    /// the data so it can be re-opened later.
    pub fn generate(kek: &KekHandle) -> Result<Self, AeonError> {
        let (dek, wrapped) = kek.wrap_new_dek()?;
        Ok(Self { dek, wrapped })
    }

    /// Unwrap an existing wrapped DEK using the given KEK. Used when an
    /// L2 segment or L3 store is re-opened after restart. The caller
    /// must have loaded the wrapped DEK from its `.meta` sidecar (L2)
    /// or store-level metadata (L3).
    pub fn from_wrapped(kek: &KekHandle, wrapped: WrappedDek) -> Result<Self, AeonError> {
        let dek = kek.unwrap_dek(&wrapped)?;
        Ok(Self { dek, wrapped })
    }

    /// The wrapped DEK for this cipher — persist alongside the sealed
    /// data so it can be re-opened later.
    pub fn wrapped_dek(&self) -> &WrappedDek {
        &self.wrapped
    }

    /// Seal a plaintext payload. Output is `nonce(12) || ciphertext ||
    /// gcm_tag(16)`. The nonce is sampled from `OsRng` per call.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, AeonError> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(self.dek.expose_bytes()));
        let nonce_bytes = Aes256Gcm::generate_nonce(&mut OsRng);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| AeonError::Crypto {
                message: format!("at-rest seal failed: {e}"),
                source: None,
            })?;
        let mut out = Vec::with_capacity(AT_REST_NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Open a previously-sealed payload. Input layout must match
    /// `seal()`'s output. Authentication failure (tampered ciphertext,
    /// wrong DEK, truncated blob) surfaces as [`AeonError::Crypto`].
    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, AeonError> {
        if sealed.len() < AT_REST_OVERHEAD {
            return Err(AeonError::Crypto {
                message: format!(
                    "at-rest sealed blob is {} bytes; need at least {AT_REST_OVERHEAD}",
                    sealed.len()
                ),
                source: None,
            });
        }
        let (nonce_bytes, ciphertext) = sealed.split_at(AT_REST_NONCE_LEN);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(self.dek.expose_bytes()));
        cipher
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|e| AeonError::Crypto {
                message: format!("at-rest open failed (wrong DEK? tampering?): {e}"),
                source: None,
            })
    }
}

impl std::fmt::Debug for AtRestCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtRestCipher")
            .field("kek_id", &self.wrapped.kek_id)
            .field("kek_domain", &self.wrapped.kek_domain)
            .field("dek", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kek::KekDomain;
    use aeon_types::{
        SecretBytes, SecretError, SecretProvider, SecretRef, SecretRegistry, SecretScheme,
    };
    use std::sync::Arc;

    // Reuse the hex-env-provider pattern from kek.rs tests so we can
    // feed 32-byte KEKs through a real SecretRegistry without embedding
    // raw bytes in env-var strings.
    struct HexEnvProvider;
    impl SecretProvider for HexEnvProvider {
        fn scheme(&self) -> SecretScheme {
            SecretScheme::Env
        }
        fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError> {
            let val = std::env::var(path).map_err(|_| SecretError::EnvNotSet(path.to_string()))?;
            let decoded: Vec<u8> = (0..val.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&val[i..i + 2], 16).unwrap_or(0))
                .collect();
            Ok(SecretBytes::new(decoded))
        }
    }

    fn hex_encode(bytes: &[u8]) -> String {
        const HEX: &[u8] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }

    fn kek_handle(var: &str, kek_bytes: [u8; 32], id: &str) -> KekHandle {
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var(var, hex_encode(&kek_bytes)) };
        let mut r = SecretRegistry::empty();
        r.register(Arc::new(HexEnvProvider));
        KekHandle::new(KekDomain::DataContext, id, SecretRef::env(var), Arc::new(r))
    }

    fn cleanup(var: &str) {
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn seal_open_roundtrip_small_payload() {
        const VAR: &str = "AEON_TEST_ATREST_SMALL";
        let kek = kek_handle(VAR, [0x11u8; 32], "v1");
        let cipher = AtRestCipher::generate(&kek).unwrap();

        let plaintext = b"order-42".to_vec();
        let sealed = cipher.seal(&plaintext).unwrap();
        assert_eq!(sealed.len(), plaintext.len() + AT_REST_OVERHEAD);

        let opened = cipher.open(&sealed).unwrap();
        assert_eq!(opened, plaintext);
        cleanup(VAR);
    }

    #[test]
    fn seal_open_roundtrip_large_payload() {
        const VAR: &str = "AEON_TEST_ATREST_LARGE";
        let kek = kek_handle(VAR, [0x22u8; 32], "v1");
        let cipher = AtRestCipher::generate(&kek).unwrap();

        // 64 KiB payload to exercise AES-GCM across many blocks.
        let plaintext = vec![0xA5u8; 64 * 1024];
        let sealed = cipher.seal(&plaintext).unwrap();
        let opened = cipher.open(&sealed).unwrap();
        assert_eq!(opened, plaintext);
        cleanup(VAR);
    }

    #[test]
    fn seal_is_non_deterministic_across_calls() {
        const VAR: &str = "AEON_TEST_ATREST_NONCE";
        let kek = kek_handle(VAR, [0x33u8; 32], "v1");
        let cipher = AtRestCipher::generate(&kek).unwrap();

        let pt = b"same-plaintext";
        let a = cipher.seal(pt).unwrap();
        let b = cipher.seal(pt).unwrap();
        assert_ne!(
            a, b,
            "nonce must differ so identical plaintexts seal differently"
        );
        // Nonce prefix differs; ciphertext differs; both open to same pt.
        assert_eq!(cipher.open(&a).unwrap(), pt);
        assert_eq!(cipher.open(&b).unwrap(), pt);
        cleanup(VAR);
    }

    #[test]
    fn open_rejects_truncated_blob() {
        const VAR: &str = "AEON_TEST_ATREST_TRUNC";
        let kek = kek_handle(VAR, [0x44u8; 32], "v1");
        let cipher = AtRestCipher::generate(&kek).unwrap();

        // Any blob shorter than nonce+tag is malformed.
        let err = cipher.open(&[0u8; 10]).unwrap_err();
        assert!(format!("{err}").contains("at-rest sealed blob"));
        cleanup(VAR);
    }

    #[test]
    fn open_rejects_tampered_ciphertext() {
        const VAR: &str = "AEON_TEST_ATREST_TAMPER";
        let kek = kek_handle(VAR, [0x55u8; 32], "v1");
        let cipher = AtRestCipher::generate(&kek).unwrap();

        let mut sealed = cipher.seal(b"payload").unwrap();
        // Flip a bit deep in the ciphertext region (past the nonce).
        sealed[AT_REST_NONCE_LEN + 1] ^= 0x01;
        let err = cipher.open(&sealed).unwrap_err();
        assert!(format!("{err}").contains("at-rest open failed"));
        cleanup(VAR);
    }

    #[test]
    fn open_rejects_tampered_tag() {
        const VAR: &str = "AEON_TEST_ATREST_TAG";
        let kek = kek_handle(VAR, [0x66u8; 32], "v1");
        let cipher = AtRestCipher::generate(&kek).unwrap();

        let mut sealed = cipher.seal(b"hello").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xFF;
        let err = cipher.open(&sealed).unwrap_err();
        assert!(format!("{err}").contains("at-rest open failed"));
        cleanup(VAR);
    }

    #[test]
    fn reopen_from_wrapped_dek_roundtrips() {
        const VAR: &str = "AEON_TEST_ATREST_REOPEN";
        let kek = kek_handle(VAR, [0x77u8; 32], "v1");

        let producer = AtRestCipher::generate(&kek).unwrap();
        let wrapped = producer.wrapped_dek().clone();
        let sealed = producer.seal(b"roundtrip").unwrap();
        drop(producer);

        // Simulate process restart — load sealed data + wrapped DEK from
        // sidecar, reopen the cipher via the KEK.
        let reader = AtRestCipher::from_wrapped(&kek, wrapped).unwrap();
        assert_eq!(reader.open(&sealed).unwrap(), b"roundtrip");
        cleanup(VAR);
    }

    #[test]
    fn wrapped_dek_accessor_matches_wrap_on_generate() {
        const VAR: &str = "AEON_TEST_ATREST_WRAP_ACCESS";
        let kek = kek_handle(VAR, [0x88u8; 32], "v2");
        let cipher = AtRestCipher::generate(&kek).unwrap();
        let w = cipher.wrapped_dek();
        assert_eq!(w.kek_id, "v2");
        assert_eq!(w.kek_domain, KekDomain::DataContext);
        assert_eq!(w.nonce.len(), 12);
        cleanup(VAR);
    }

    #[test]
    fn empty_plaintext_roundtrips() {
        const VAR: &str = "AEON_TEST_ATREST_EMPTY";
        let kek = kek_handle(VAR, [0x99u8; 32], "v1");
        let cipher = AtRestCipher::generate(&kek).unwrap();

        let sealed = cipher.seal(&[]).unwrap();
        assert_eq!(sealed.len(), AT_REST_OVERHEAD);
        assert_eq!(cipher.open(&sealed).unwrap(), Vec::<u8>::new());
        cleanup(VAR);
    }

    #[test]
    fn debug_redacts_dek() {
        const VAR: &str = "AEON_TEST_ATREST_DEBUG";
        let kek = kek_handle(VAR, [0xAAu8; 32], "v1");
        let cipher = AtRestCipher::generate(&kek).unwrap();
        let s = format!("{cipher:?}");
        assert!(s.contains("redacted"));
        assert!(s.contains("v1"));
        cleanup(VAR);
    }
}
