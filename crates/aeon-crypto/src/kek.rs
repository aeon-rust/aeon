//! Dual-domain Key Encryption Keys + envelope encryption (S1.2).
//!
//! Aeon holds two logically-separate key domains:
//!
//! - **Log-context KEK** — wraps DEKs used when redacted references leak
//!   into tracing fields (hashed subject-ids, segment coordinates). A
//!   compromise of this domain must not unlock event payloads.
//! - **Data-context KEK** — wraps DEKs used for L2 event bodies and L3
//!   ack-checkpoint storage.
//!
//! Each domain has an **active** KEK and, optionally, a **previous**
//! KEK held during a rotation window. Wrapped DEKs carry the KEK id
//! they were produced with, so the unwrap path can pick the right key
//! without operator intervention.
//!
//! KEK bytes are fetched from a [`SecretProvider`] (Vault / AWS KMS /
//! env / dotenv) at wrap/unwrap time and zeroized immediately after
//! use; they are never held in a process-global. DEKs are 32-byte
//! AES-256-GCM keys; envelope wrap is AES-256-GCM with a fresh 96-bit
//! nonce per operation.

use std::sync::Arc;

use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, KeyInit, Nonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use aeon_types::{AeonError, SecretBytes, SecretRef, SecretRegistry};

// ─── Domain ─────────────────────────────────────────────────────────

/// Logical domain a KEK belongs to. Domains are strictly non-fungible:
/// a wrapped DEK tagged `DataContext` must not be unwrappable by a
/// `LogContext` handle even if the underlying KEK bytes happened to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KekDomain {
    LogContext,
    DataContext,
}

impl KekDomain {
    pub fn as_str(&self) -> &'static str {
        match self {
            KekDomain::LogContext => "log-context",
            KekDomain::DataContext => "data-context",
        }
    }
}

// ─── Wrapped DEK ────────────────────────────────────────────────────

/// Ciphertext + metadata stored alongside an L2 segment / L3 checkpoint.
/// Encodes which KEK wrapped the payload, so the unwrap path can choose
/// active vs. previous during rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrappedDek {
    pub kek_domain: KekDomain,
    pub kek_id: String,
    /// 96-bit GCM nonce, unique per wrap operation.
    pub nonce: Vec<u8>,
    /// 32 bytes of plaintext DEK + 16-byte GCM tag.
    pub ciphertext: Vec<u8>,
}

// ─── DEK ────────────────────────────────────────────────────────────

/// 32-byte Data Encryption Key. Zeroizes on drop; omits `Debug`
/// content and `Clone` so accidental duplication / logging fails at
/// compile time.
pub struct DekBytes([u8; 32]);

impl DekBytes {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn expose_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn len(&self) -> usize {
        32
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

impl Drop for DekBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for DekBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DekBytes(<redacted, 32 bytes>)")
    }
}

// ─── KekHandle ──────────────────────────────────────────────────────

/// A domain-tagged KEK with an active id and optional previous id for
/// dual-key read during rotation. Holds a reference to the
/// [`SecretRegistry`] so it can fetch KEK bytes on demand; does not
/// cache them in memory.
pub struct KekHandle {
    domain: KekDomain,
    active_id: String,
    active_ref: SecretRef,
    previous_id: Option<String>,
    previous_ref: Option<SecretRef>,
    registry: Arc<SecretRegistry>,
}

impl KekHandle {
    pub fn new(
        domain: KekDomain,
        active_id: impl Into<String>,
        active_ref: SecretRef,
        registry: Arc<SecretRegistry>,
    ) -> Self {
        Self {
            domain,
            active_id: active_id.into(),
            active_ref,
            previous_id: None,
            previous_ref: None,
            registry,
        }
    }

    pub fn with_previous(
        mut self,
        previous_id: impl Into<String>,
        previous_ref: SecretRef,
    ) -> Self {
        self.previous_id = Some(previous_id.into());
        self.previous_ref = Some(previous_ref);
        self
    }

    pub fn domain(&self) -> KekDomain {
        self.domain
    }

    pub fn active_id(&self) -> &str {
        &self.active_id
    }

    pub fn previous_id(&self) -> Option<&str> {
        self.previous_id.as_deref()
    }

    /// Fetch the active KEK bytes — 32 bytes required for AES-256-GCM.
    pub fn active_bytes(&self) -> Result<SecretBytes, AeonError> {
        let b = self.registry.resolve(&self.active_ref)?;
        ensure_kek_len(&b, self.domain, &self.active_id)?;
        Ok(b)
    }

    pub fn previous_bytes(&self) -> Result<Option<SecretBytes>, AeonError> {
        let pref = match &self.previous_ref {
            Some(r) => r,
            None => return Ok(None),
        };
        let b = self.registry.resolve(pref)?;
        ensure_kek_len(
            &b,
            self.domain,
            self.previous_id.as_deref().unwrap_or("<unknown>"),
        )?;
        Ok(Some(b))
    }

    /// Generate a fresh random DEK and wrap it with the active KEK.
    pub fn wrap_new_dek(&self) -> Result<(DekBytes, WrappedDek), AeonError> {
        let dek = DekBytes::generate();
        let wrapped = self.wrap_dek(&dek)?;
        Ok((dek, wrapped))
    }

    /// Wrap the given DEK with the active KEK.
    pub fn wrap_dek(&self, dek: &DekBytes) -> Result<WrappedDek, AeonError> {
        let kek = self.active_bytes()?;
        let (nonce, ciphertext) = encrypt_dek(kek.expose_bytes(), dek.expose_bytes())?;
        Ok(WrappedDek {
            kek_domain: self.domain,
            kek_id: self.active_id.clone(),
            nonce,
            ciphertext,
        })
    }

    /// Unwrap a DEK, dispatching to active or previous KEK based on the
    /// `kek_id` carried in `wrapped`. Fails if neither matches (i.e. the
    /// wrapped DEK was produced by a KEK this handle no longer tracks).
    ///
    /// S2.5 — every failure path emits an audit event on the dedicated
    /// audit sink (domain mismatch, unknown kek id, decrypt failure).
    /// Success paths are intentionally quiet: successful unwrap is
    /// hot-path and every encrypted L2 segment or L3 checkpoint triggers
    /// one; auditing every success would flood the channel.
    pub fn unwrap_dek(&self, wrapped: &WrappedDek) -> Result<DekBytes, AeonError> {
        if wrapped.kek_domain != self.domain {
            emit_kek_audit(
                "kek.unwrap.denied",
                aeon_types::audit::AuditOutcome::Denied,
                self.domain.as_str(),
                &self.active_id,
                Some("domain_mismatch"),
            );
            return Err(AeonError::Crypto {
                message: format!(
                    "wrapped DEK belongs to {} domain, handle is {}",
                    wrapped.kek_domain.as_str(),
                    self.domain.as_str()
                ),
                source: None,
            });
        }
        if wrapped.kek_id == self.active_id {
            let kek = self.active_bytes()?;
            let r = decrypt_dek(kek.expose_bytes(), &wrapped.nonce, &wrapped.ciphertext);
            if r.is_err() {
                emit_kek_audit(
                    "kek.unwrap.failed",
                    aeon_types::audit::AuditOutcome::Failure,
                    self.domain.as_str(),
                    &self.active_id,
                    Some("aead_decrypt_failed"),
                );
            }
            return r;
        }
        if let Some(prev_id) = &self.previous_id {
            if prev_id == &wrapped.kek_id {
                if let Some(kek) = self.previous_bytes()? {
                    let r = decrypt_dek(kek.expose_bytes(), &wrapped.nonce, &wrapped.ciphertext);
                    if r.is_err() {
                        emit_kek_audit(
                            "kek.unwrap.failed",
                            aeon_types::audit::AuditOutcome::Failure,
                            self.domain.as_str(),
                            prev_id,
                            Some("aead_decrypt_failed_previous"),
                        );
                    }
                    return r;
                }
            }
        }
        emit_kek_audit(
            "kek.unwrap.denied",
            aeon_types::audit::AuditOutcome::Denied,
            self.domain.as_str(),
            &wrapped.kek_id,
            Some("unknown_kek_id"),
        );
        Err(AeonError::Crypto {
            message: format!(
                "no KEK available for id '{}' in {} domain (active='{}', previous={:?})",
                wrapped.kek_id,
                self.domain.as_str(),
                self.active_id,
                self.previous_id
            ),
            source: None,
        })
    }
}

/// S2.5 helper — emit an audit event on the Key channel. Kept local so
/// the call sites above stay terse and so rotation / unwrap / rotate
/// operations share one format.
fn emit_kek_audit(
    action: &'static str,
    outcome: aeon_types::audit::AuditOutcome,
    domain: &str,
    kek_id: &str,
    reason_tag: Option<&'static str>,
) {
    let mut event = aeon_types::audit::AuditEvent::new(
        aeon_observability::now_unix_nanos(),
        aeon_types::audit::AuditCategory::Key,
        action,
        outcome,
    )
    .with_resource(format!("kek/{domain}/{kek_id}"))
    .with_detail("domain", domain.to_string());
    if let Some(tag) = reason_tag {
        event = event.with_detail("reason_tag", tag.to_string());
    }
    aeon_observability::emit_audit(&event);
}

fn ensure_kek_len(b: &SecretBytes, domain: KekDomain, id: &str) -> Result<(), AeonError> {
    if b.len() == 32 {
        return Ok(());
    }
    Err(AeonError::Crypto {
        message: format!(
            "{} KEK '{}' is {} bytes; AES-256-GCM requires 32",
            domain.as_str(),
            id,
            b.len()
        ),
        source: None,
    })
}

// ─── Envelope primitives ───────────────────────────────────────────

const GCM_NONCE_LEN: usize = 12;
const DEK_LEN: usize = 32;

fn encrypt_dek(kek: &[u8], dek: &[u8]) -> Result<(Vec<u8>, Vec<u8>), AeonError> {
    if kek.len() != DEK_LEN {
        return Err(AeonError::Crypto {
            message: format!("KEK must be {DEK_LEN} bytes, got {}", kek.len()),
            source: None,
        });
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(kek));
    let nonce_bytes = Aes256Gcm::generate_nonce(&mut OsRng);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, dek).map_err(|e| AeonError::Crypto {
        message: format!("DEK wrap failed: {e}"),
        source: None,
    })?;
    Ok((nonce_bytes.to_vec(), ciphertext))
}

fn decrypt_dek(kek: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Result<DekBytes, AeonError> {
    if kek.len() != DEK_LEN {
        return Err(AeonError::Crypto {
            message: format!("KEK must be {DEK_LEN} bytes, got {}", kek.len()),
            source: None,
        });
    }
    if nonce.len() != GCM_NONCE_LEN {
        return Err(AeonError::Crypto {
            message: format!(
                "GCM nonce must be {GCM_NONCE_LEN} bytes, got {}",
                nonce.len()
            ),
            source: None,
        });
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(kek));
    let mut plaintext = cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|e| AeonError::Crypto {
            message: format!("DEK unwrap failed (wrong KEK? integrity failure?): {e}"),
            source: None,
        })?;
    if plaintext.len() != DEK_LEN {
        plaintext.zeroize();
        return Err(AeonError::Crypto {
            message: format!(
                "unwrapped DEK is {} bytes; expected {DEK_LEN}",
                plaintext.len()
            ),
            source: None,
        });
    }
    let mut arr = [0u8; DEK_LEN];
    arr.copy_from_slice(&plaintext);
    plaintext.zeroize();
    Ok(DekBytes::from_bytes(arr))
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a SecretRegistry where `${ENV:VAR}` refs resolve to 32-byte
    /// test KEKs via unique env-var names.
    fn registry_with_keks(pairs: &[(&str, [u8; 32])]) -> Arc<SecretRegistry> {
        for (name, bytes) in pairs {
            // SAFETY: test-only env mutation.
            unsafe {
                std::env::set_var(name, hex_encode(bytes));
            }
        }
        Arc::new(SecretRegistry::default_local())
    }

    fn cleanup(names: &[&str]) {
        for n in names {
            unsafe { std::env::remove_var(n) };
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

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // The env-backed secret provider returns the hex string as UTF-8
    // bytes. For 32-byte KEK tests we decode the hex at the caller, so
    // wire the registry to a SecretRef that *is* already 32 bytes when
    // interpreted as raw bytes — simplest is to store the KEK as a raw
    // 32-byte UTF-8 string. We use a custom provider that decodes hex.
    struct HexEnvProvider;
    impl aeon_types::SecretProvider for HexEnvProvider {
        fn scheme(&self) -> aeon_types::SecretScheme {
            aeon_types::SecretScheme::Env
        }
        fn resolve(&self, path: &str) -> Result<SecretBytes, aeon_types::SecretError> {
            let val = std::env::var(path)
                .map_err(|_| aeon_types::SecretError::EnvNotSet(path.to_string()))?;
            Ok(SecretBytes::new(hex_decode(&val)))
        }
    }

    fn hex_registry() -> Arc<SecretRegistry> {
        let mut r = SecretRegistry::empty();
        r.register(Arc::new(HexEnvProvider));
        Arc::new(r)
    }

    // ─── wrap/unwrap roundtrip ──────────────────────────────────────

    #[test]
    fn wrap_unwrap_roundtrip_active() {
        const VAR: &str = "AEON_TEST_KEK_WRAP_ROUNDTRIP_V1";
        let kek = [0x11u8; 32];
        registry_with_keks(&[(VAR, kek)]);
        let registry = hex_registry();

        let handle = KekHandle::new(KekDomain::DataContext, "v1", SecretRef::env(VAR), registry);
        let (dek, wrapped) = handle.wrap_new_dek().unwrap();
        assert_eq!(wrapped.kek_domain, KekDomain::DataContext);
        assert_eq!(wrapped.kek_id, "v1");
        assert_eq!(wrapped.nonce.len(), 12);

        let unwrapped = handle.unwrap_dek(&wrapped).unwrap();
        assert_eq!(dek.expose_bytes(), unwrapped.expose_bytes());
        cleanup(&[VAR]);
    }

    #[test]
    fn wrap_unwrap_roundtrip_previous_key() {
        const V_ACTIVE: &str = "AEON_TEST_KEK_ROTATE_V2";
        const V_PREV: &str = "AEON_TEST_KEK_ROTATE_V1";
        let kek_new = [0x22u8; 32];
        let kek_old = [0x11u8; 32];
        registry_with_keks(&[(V_ACTIVE, kek_new), (V_PREV, kek_old)]);
        let registry = hex_registry();

        // Producer: "v1" is active, wraps a DEK.
        let producer = KekHandle::new(
            KekDomain::DataContext,
            "v1",
            SecretRef::env(V_PREV),
            registry.clone(),
        );
        let (dek, wrapped) = producer.wrap_new_dek().unwrap();

        // After rotation: "v2" is active, "v1" is previous.
        let reader = KekHandle::new(
            KekDomain::DataContext,
            "v2",
            SecretRef::env(V_ACTIVE),
            registry,
        )
        .with_previous("v1", SecretRef::env(V_PREV));

        let unwrapped = reader.unwrap_dek(&wrapped).unwrap();
        assert_eq!(dek.expose_bytes(), unwrapped.expose_bytes());
        cleanup(&[V_ACTIVE, V_PREV]);
    }

    #[test]
    fn unwrap_rejects_domain_mismatch() {
        const VAR: &str = "AEON_TEST_KEK_DOMAIN_MISMATCH";
        registry_with_keks(&[(VAR, [0x33u8; 32])]);
        let registry = hex_registry();

        let data_handle = KekHandle::new(
            KekDomain::DataContext,
            "v1",
            SecretRef::env(VAR),
            registry.clone(),
        );
        let (_dek, data_wrapped) = data_handle.wrap_new_dek().unwrap();

        let log_handle = KekHandle::new(KekDomain::LogContext, "v1", SecretRef::env(VAR), registry);
        let err = log_handle.unwrap_dek(&data_wrapped).unwrap_err();
        assert!(matches!(err, AeonError::Crypto { .. }));
        assert!(format!("{err}").contains("domain"));
        cleanup(&[VAR]);
    }

    #[test]
    fn unwrap_rejects_unknown_kek_id() {
        const VAR: &str = "AEON_TEST_KEK_UNKNOWN_ID";
        registry_with_keks(&[(VAR, [0x44u8; 32])]);
        let registry = hex_registry();

        let handle = KekHandle::new(
            KekDomain::DataContext,
            "active",
            SecretRef::env(VAR),
            registry,
        );
        let (_dek, mut wrapped) = handle.wrap_new_dek().unwrap();
        wrapped.kek_id = "retired".into();

        let err = handle.unwrap_dek(&wrapped).unwrap_err();
        assert!(matches!(err, AeonError::Crypto { .. }));
        assert!(format!("{err}").contains("no KEK available"));
        cleanup(&[VAR]);
    }

    #[test]
    fn unwrap_fails_on_tampered_ciphertext() {
        const VAR: &str = "AEON_TEST_KEK_TAMPER";
        registry_with_keks(&[(VAR, [0x55u8; 32])]);
        let registry = hex_registry();

        let handle = KekHandle::new(KekDomain::DataContext, "v1", SecretRef::env(VAR), registry);
        let (_dek, mut wrapped) = handle.wrap_new_dek().unwrap();
        // Flip a bit in the ciphertext — GCM tag must reject.
        wrapped.ciphertext[0] ^= 0x01;
        let err = handle.unwrap_dek(&wrapped).unwrap_err();
        assert!(matches!(err, AeonError::Crypto { .. }));
        assert!(format!("{err}").contains("unwrap failed"));
        cleanup(&[VAR]);
    }

    #[test]
    fn wrong_length_kek_rejected() {
        const VAR: &str = "AEON_TEST_KEK_WRONG_LEN";
        // SAFETY: test-only.
        unsafe { std::env::set_var(VAR, hex_encode(&[0x66u8; 16])) };

        let mut r = SecretRegistry::empty();
        r.register(Arc::new(HexEnvProvider));
        let registry = Arc::new(r);

        let handle = KekHandle::new(KekDomain::DataContext, "v1", SecretRef::env(VAR), registry);
        let err = handle.wrap_new_dek().unwrap_err();
        assert!(matches!(err, AeonError::Crypto { .. }));
        assert!(format!("{err}").contains("32"));
        cleanup(&[VAR]);
    }

    // ─── DekBytes ──────────────────────────────────────────────────

    #[test]
    fn dek_bytes_debug_is_redacted() {
        let d = DekBytes::generate();
        let s = format!("{d:?}");
        assert!(s.contains("redacted"));
    }

    #[test]
    fn dek_generate_is_non_zero() {
        let d = DekBytes::generate();
        assert!(d.expose_bytes().iter().any(|&b| b != 0));
    }

    // ─── KekDomain ──────────────────────────────────────────────────

    #[test]
    fn kek_domain_strings_stable() {
        assert_eq!(KekDomain::LogContext.as_str(), "log-context");
        assert_eq!(KekDomain::DataContext.as_str(), "data-context");
    }

    #[test]
    fn wrapped_dek_serde_roundtrip() {
        const VAR: &str = "AEON_TEST_KEK_SERDE";
        registry_with_keks(&[(VAR, [0x77u8; 32])]);
        let registry = hex_registry();

        let handle = KekHandle::new(KekDomain::LogContext, "v1", SecretRef::env(VAR), registry);
        let (dek_before, wrapped) = handle.wrap_new_dek().unwrap();
        let json = serde_json::to_string(&wrapped).unwrap();
        let parsed: WrappedDek = serde_json::from_str(&json).unwrap();
        let dek_after = handle.unwrap_dek(&parsed).unwrap();
        assert_eq!(dek_before.expose_bytes(), dek_after.expose_bytes());
        assert!(json.contains("log_context"));
        cleanup(&[VAR]);
    }
}
