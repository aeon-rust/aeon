//! S1.4 — Hardware Security Module (HSM) provider trait stub.
//!
//! Defines the interface a future HSM / PKCS#11 driver must implement so
//! it can slot under [`crate::kek::KekHandle`] without reshaping the
//! existing wrap/unwrap call sites. The trait is **stub-only** for this
//! atom — no driver is wired up today because operators land on Vault or
//! AWS KMS first; HSM is an extreme-deployment fallback.
//!
//! # Why now, driver later
//!
//! Shipping the trait now pins the shape against the S1.2 KEK semantics
//! (dual-domain, rotation-aware, 32-byte AES-256-GCM DEKs) while the
//! design is fresh. A driver authored six months from now can implement
//! this trait without rediscovering constraints the codebase has already
//! internalised. Deferring the driver keeps `aeon-crypto` free of
//! `libsofthsm2` / `cryptoki` dependencies until a concrete deployment
//! asks for one.
//!
//! # Relationship with `KekHandle`
//!
//! The existing [`KekHandle`](crate::kek::KekHandle) fetches raw KEK
//! bytes from a `SecretRegistry` and performs AES-256-GCM in-process.
//! An HSM-backed handle would instead hold `Arc<dyn HsmProvider>` and
//! forward `wrap_dek` / `unwrap_dek` to the provider — the KEK bytes
//! never leave the HSM boundary. Neither call site in the engine
//! (`l2_body`, `l3_encrypted`) needs to change; only `KekHandle` grows
//! a variant that dispatches on `HsmProvider` vs `SecretRegistry`.
//!
//! The shape below keeps the door open without closing it: no concrete
//! enum variant yet, so the trait can evolve before we commit to a
//! stable internal dispatch.

use std::fmt::Debug;

use aeon_types::AeonError;

use crate::kek::{DekBytes, KekDomain, WrappedDek};

/// An HSM-backed KEK provider. Implementors delegate DEK wrap / unwrap
/// to key material that never leaves the HSM boundary.
///
/// **Threading:** implementations must be `Send + Sync`. PKCS#11 v3
/// sessions are not thread-safe — a driver will typically hold an
/// internal pool of sessions behind a mutex.
///
/// **Error discipline:** malformed `kek_id`, unknown slots, session
/// failures, and mechanism-not-supported all surface as
/// `AeonError::Crypto` so the engine's no-panic policy (FT-10) stays
/// intact across the FFI boundary.
pub trait HsmProvider: Send + Sync + Debug {
    /// Short human-readable identifier for the provider — surfaces in
    /// error messages and metrics labels. Examples: `"pkcs11/softhsm2"`,
    /// `"pkcs11/yubikey-5"`.
    fn provider_name(&self) -> &str;

    /// Wrap a fresh 32-byte DEK under the KEK identified by `kek_id`
    /// within the given domain. Returns the nonce + ciphertext that
    /// callers store alongside the encrypted payload.
    ///
    /// Implementors should:
    /// - produce a fresh 96-bit nonce per wrap (AES-GCM semantics);
    /// - tag the result with `domain` + `kek_id` so unwrap can
    ///   dispatch correctly;
    /// - zeroize any transient buffers before returning.
    fn wrap_dek(
        &self,
        domain: KekDomain,
        kek_id: &str,
        dek: &DekBytes,
    ) -> Result<WrappedDek, AeonError>;

    /// Unwrap the DEK previously sealed by [`HsmProvider::wrap_dek`]. The
    /// `domain` and `kek_id` fields on `wrapped` must be enforced by the
    /// implementation — returning a DEK sealed under a different
    /// domain's KEK is a hard S1 violation.
    fn unwrap_dek(&self, wrapped: &WrappedDek) -> Result<DekBytes, AeonError>;

    /// Optional: generate a fresh KEK inside the HSM and return a
    /// handle-friendly id. `None` from the default impl means the
    /// provider expects KEKs to be provisioned out-of-band (the Vault /
    /// KMS path) — today this is the expected shape for PKCS#11 drivers
    /// where KEKs are set up via a token-initialisation ritual.
    fn provision_kek(&self, _domain: KekDomain) -> Result<Option<String>, AeonError> {
        Ok(None)
    }
}

// ─── Stub PKCS#11 provider ────────────────────────────────────────────

/// Placeholder PKCS#11 provider. All operations return
/// `AeonError::Crypto("PKCS#11 driver not yet implemented")`. Exists so
/// future code can reference `Pkcs11Provider` at the type level — a
/// real driver will replace the stub body, not the public surface.
///
/// Parameters intentionally mirror a typical PKCS#11 session: module
/// path, slot id, optional PIN reference. PIN is a `SecretRef` so it
/// flows through the S1 secret provider (Vault / env / dotenv) and
/// never appears in logs or process arguments.
#[derive(Debug, Clone)]
pub struct Pkcs11Provider {
    /// Filesystem path to the PKCS#11 shared library (e.g.
    /// `/usr/lib/softhsm/libsofthsm2.so`).
    pub module_path: String,
    /// Slot id within the HSM to attach to.
    pub slot_id: u64,
    /// Optional label used to identify the token for friendlier errors.
    pub token_label: Option<String>,
}

impl Pkcs11Provider {
    /// Construct a stub. Does not load the shared library — calling a
    /// trait method immediately returns the driver-not-implemented
    /// error. Intended for type-level references and test fixtures
    /// until a real driver lands.
    pub fn new(module_path: impl Into<String>, slot_id: u64) -> Self {
        Self {
            module_path: module_path.into(),
            slot_id,
            token_label: None,
        }
    }

    pub fn with_token_label(mut self, label: impl Into<String>) -> Self {
        self.token_label = Some(label.into());
        self
    }

    fn unimplemented(&self, op: &str) -> AeonError {
        AeonError::Crypto {
            message: format!(
                "PKCS#11 driver not yet implemented: '{op}' against module='{}' slot={} \
                 (S1.4 shipped the trait only; driver is a future atom — \
                 use Vault or AWS KMS backends in the meantime)",
                self.module_path, self.slot_id
            ),
            source: None,
        }
    }
}

impl HsmProvider for Pkcs11Provider {
    fn provider_name(&self) -> &str {
        "pkcs11/stub"
    }

    fn wrap_dek(
        &self,
        _domain: KekDomain,
        _kek_id: &str,
        _dek: &DekBytes,
    ) -> Result<WrappedDek, AeonError> {
        Err(self.unimplemented("wrap_dek"))
    }

    fn unwrap_dek(&self, _wrapped: &WrappedDek) -> Result<DekBytes, AeonError> {
        Err(self.unimplemented("unwrap_dek"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_wrapped() -> WrappedDek {
        WrappedDek {
            kek_domain: KekDomain::DataContext,
            kek_id: "kek-v1".into(),
            nonce: vec![0u8; 12],
            ciphertext: vec![0u8; 48],
        }
    }

    #[test]
    fn stub_name_is_stable() {
        let p = Pkcs11Provider::new("/usr/lib/softhsm2.so", 0);
        assert_eq!(p.provider_name(), "pkcs11/stub");
    }

    #[test]
    fn stub_wrap_returns_not_implemented() {
        let p = Pkcs11Provider::new("/usr/lib/softhsm2.so", 0);
        let dek = DekBytes::generate();
        let err = p.wrap_dek(KekDomain::DataContext, "kek-v1", &dek).unwrap_err();
        assert!(
            format!("{err}").contains("PKCS#11 driver not yet implemented"),
            "error should name the missing driver: {err}"
        );
    }

    #[test]
    fn stub_unwrap_returns_not_implemented() {
        let p = Pkcs11Provider::new("/usr/lib/softhsm2.so", 0);
        let err = p.unwrap_dek(&sample_wrapped()).unwrap_err();
        assert!(
            format!("{err}").contains("PKCS#11 driver not yet implemented"),
            "error should name the missing driver: {err}"
        );
    }

    #[test]
    fn provision_kek_default_is_none() {
        let p = Pkcs11Provider::new("/usr/lib/softhsm2.so", 0);
        let out = p.provision_kek(KekDomain::DataContext).unwrap();
        assert!(out.is_none(), "default provision returns None until a driver opts in");
    }

    #[test]
    fn builder_sets_optional_label() {
        let p = Pkcs11Provider::new("/mod.so", 3).with_token_label("aeon-primary");
        assert_eq!(p.slot_id, 3);
        assert_eq!(p.token_label.as_deref(), Some("aeon-primary"));
    }

    #[test]
    fn trait_is_object_safe() {
        // This must compile: HsmProvider behind `dyn` is required for
        // the future `KekHandle::Hsm(Arc<dyn HsmProvider>)` dispatch.
        let p: Box<dyn HsmProvider> = Box::new(Pkcs11Provider::new("/m.so", 0));
        assert_eq!(p.provider_name(), "pkcs11/stub");
    }

    #[test]
    fn error_mentions_module_and_slot() {
        let p = Pkcs11Provider::new("/usr/lib/opensc-pkcs11.so", 7);
        let err = p.unwrap_dek(&sample_wrapped()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("opensc-pkcs11"));
        assert!(msg.contains("slot=7"));
    }
}
