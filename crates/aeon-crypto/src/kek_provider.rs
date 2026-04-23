//! Async [`KekProvider`] trait — the adapter-pattern seam for envelope
//! encryption backends (local AES-GCM, AWS KMS, GCP KMS, Azure Key Vault,
//! Vault Transit, OpenBao Transit, PKCS#11).
//!
//! The trait is "remote-op shaped": implementations may perform key-wrap
//! and key-unwrap against a remote KMS without the raw KEK bytes ever
//! entering the Aeon process. The existing in-process AES-GCM path
//! ([`crate::kek::KekHandle`]) implements this trait too, so the
//! local-KEK case remains a first-class backend and not a special case.
//!
//! Concrete backends (Vault / AWS KMS / ...) live in the `aeon-secrets`
//! crate behind feature flags; they depend on this trait and on the
//! [`WrappedDek`] / [`DekBytes`] / [`KekDomain`] types from
//! [`crate::kek`].

use std::future::Future;
use std::pin::Pin;

use aeon_types::AeonError;

use crate::kek::{DekBytes, KekDomain, KekHandle, WrappedDek};

/// Pinned-box future alias for `KekProvider` methods. Keeps the trait
/// dyn-compatible (matches the `PohChainProvider` / `PartitionTransferProvider`
/// style used elsewhere in the workspace — we do not depend on the
/// `async_trait` macro crate).
pub type KekFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, AeonError>> + Send + 'a>>;

/// Source-of-truth for envelope encryption on an Aeon node.
///
/// A single provider instance owns one logical key family — one
/// [`KekDomain`], one active key id, and an optional previous key id
/// used during rotation. `unwrap_dek` is expected to dispatch on the
/// `kek_id` carried in the [`WrappedDek`] — callers never have to pick
/// the right key version themselves.
pub trait KekProvider: Send + Sync {
    /// Which Aeon key domain this provider serves. `LogContext` and
    /// `DataContext` are strictly non-fungible — the registry enforces
    /// that a `DataContext` `WrappedDek` is never handed to a
    /// `LogContext` provider.
    fn domain(&self) -> KekDomain;

    /// The active key id new wraps will be produced under.
    fn active_id(&self) -> &str;

    /// The previous key id, during a rotation window. `None` outside
    /// rotation.
    fn previous_id(&self) -> Option<&str>;

    /// Generate a fresh 256-bit DEK and return it alongside its
    /// wrapped form. The plaintext DEK must be used immediately by the
    /// caller and dropped; it is not cached.
    fn wrap_new_dek<'a>(&'a self) -> KekFuture<'a, (DekBytes, WrappedDek)>;

    /// Wrap an existing DEK. Used when the caller already holds a DEK
    /// and wants to persist it under the active key.
    fn wrap_dek<'a>(&'a self, dek: &'a DekBytes) -> KekFuture<'a, WrappedDek>;

    /// Unwrap a [`WrappedDek`]. Implementations dispatch on
    /// `wrapped.kek_id` between active and previous, and surface
    /// [`AeonError::Crypto`] if neither matches.
    fn unwrap_dek<'a>(&'a self, wrapped: &'a WrappedDek) -> KekFuture<'a, DekBytes>;
}

// ─── Local (in-process AES-GCM) impl ────────────────────────────────

/// The existing [`KekHandle`] path — reads KEK bytes from a
/// [`aeon_types::SecretRegistry`] and does AES-256-GCM envelope
/// encryption in-process — is the canonical `LocalKekProvider`.
/// Implementing the trait on `KekHandle` directly keeps zero
/// wrap/unwrap overhead and avoids a shim type.
impl KekProvider for KekHandle {
    fn domain(&self) -> KekDomain {
        // UFCS picks the inherent method (same name). Required because
        // the trait method also named `domain` would otherwise recurse.
        KekHandle::domain(self)
    }

    fn active_id(&self) -> &str {
        KekHandle::active_id(self)
    }

    fn previous_id(&self) -> Option<&str> {
        KekHandle::previous_id(self)
    }

    fn wrap_new_dek<'a>(&'a self) -> KekFuture<'a, (DekBytes, WrappedDek)> {
        Box::pin(async move { KekHandle::wrap_new_dek(self) })
    }

    fn wrap_dek<'a>(&'a self, dek: &'a DekBytes) -> KekFuture<'a, WrappedDek> {
        Box::pin(async move { KekHandle::wrap_dek(self, dek) })
    }

    fn unwrap_dek<'a>(&'a self, wrapped: &'a WrappedDek) -> KekFuture<'a, DekBytes> {
        Box::pin(async move { KekHandle::unwrap_dek(self, wrapped) })
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kek::{DekBytes, KekDomain, KekHandle};
    use aeon_types::{SecretBytes, SecretProvider, SecretRef, SecretRegistry, SecretScheme};
    use std::sync::Arc;

    // Reuse the same hex-decoding env provider style the kek.rs tests
    // use, to produce raw 32-byte KEKs from hex env vars.
    struct HexEnvProvider;
    impl SecretProvider for HexEnvProvider {
        fn scheme(&self) -> SecretScheme {
            SecretScheme::Env
        }
        fn resolve(&self, path: &str) -> Result<SecretBytes, aeon_types::SecretError> {
            let val = std::env::var(path)
                .map_err(|_| aeon_types::SecretError::EnvNotSet(path.to_string()))?;
            let bytes = (0..val.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&val[i..i + 2], 16).unwrap())
                .collect();
            Ok(SecretBytes::new(bytes))
        }
    }

    fn hex_registry() -> Arc<SecretRegistry> {
        let mut r = SecretRegistry::empty();
        r.register(Arc::new(HexEnvProvider));
        Arc::new(r)
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

    #[tokio::test]
    async fn kek_handle_impl_kek_provider_roundtrips() {
        const VAR: &str = "AEON_TEST_KEK_PROVIDER_TRAIT_ROUNDTRIP";
        unsafe { std::env::set_var(VAR, hex_encode(&[0x42u8; 32])) };

        let handle = KekHandle::new(
            KekDomain::DataContext,
            "v1",
            SecretRef::env(VAR),
            hex_registry(),
        );

        // Drive through the trait object — proves vtable dispatch works.
        let provider: &dyn KekProvider = &handle;
        assert_eq!(provider.domain(), KekDomain::DataContext);
        assert_eq!(provider.active_id(), "v1");
        assert!(provider.previous_id().is_none());

        let (dek, wrapped) = provider.wrap_new_dek().await.unwrap();
        assert_eq!(wrapped.kek_domain, KekDomain::DataContext);
        assert_eq!(wrapped.kek_id, "v1");

        let unwrapped = provider.unwrap_dek(&wrapped).await.unwrap();
        assert_eq!(dek.expose_bytes(), unwrapped.expose_bytes());

        unsafe { std::env::remove_var(VAR) };
    }

    #[tokio::test]
    async fn kek_provider_wrap_dek_with_existing_dek() {
        const VAR: &str = "AEON_TEST_KEK_PROVIDER_WRAP_EXISTING";
        unsafe { std::env::set_var(VAR, hex_encode(&[0x55u8; 32])) };

        let handle = KekHandle::new(
            KekDomain::LogContext,
            "logs-v1",
            SecretRef::env(VAR),
            hex_registry(),
        );
        let provider: &dyn KekProvider = &handle;

        let dek = DekBytes::from_bytes([0x7Fu8; 32]);
        let wrapped = provider.wrap_dek(&dek).await.unwrap();
        let unwrapped = provider.unwrap_dek(&wrapped).await.unwrap();
        assert_eq!(dek.expose_bytes(), unwrapped.expose_bytes());

        unsafe { std::env::remove_var(VAR) };
    }
}
