//! Per-domain dispatcher for [`aeon_crypto::kek_provider::KekProvider`]
//! instances. Mirrors [`aeon_types::SecretRegistry`] on the KEK side.
//!
//! A pipeline holds at most one provider per [`aeon_crypto::kek::KekDomain`]
//! at a time — log-context KEKs and data-context KEKs are strictly
//! non-fungible. The registry enforces this invariant on registration
//! and on unwrap dispatch.

use std::collections::HashMap;
use std::sync::Arc;

use aeon_crypto::kek::{DekBytes, KekDomain, WrappedDek};
use aeon_crypto::kek_provider::KekProvider;
use aeon_types::AeonError;

use crate::error::SecretsAdapterError;

/// Domain-keyed registry of [`KekProvider`] instances.
pub struct KekRegistry {
    providers: HashMap<KekDomain, Arc<dyn KekProvider>>,
}

impl std::fmt::Debug for KekRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let domains: Vec<_> = self.providers.keys().collect();
        f.debug_struct("KekRegistry")
            .field("domains", &domains)
            .finish()
    }
}

impl KekRegistry {
    pub fn empty() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// Register a provider for its self-declared domain. Duplicates for
    /// the same domain return [`SecretsAdapterError::DuplicateRegistration`]
    /// rather than silently overwriting.
    pub fn register(&mut self, provider: Arc<dyn KekProvider>) -> Result<(), SecretsAdapterError> {
        let domain = provider.domain();
        if self.providers.contains_key(&domain) {
            return Err(SecretsAdapterError::DuplicateRegistration {
                what: format!("KekProvider for {:?} domain", domain),
            });
        }
        self.providers.insert(domain, provider);
        Ok(())
    }

    pub fn has_domain(&self, domain: KekDomain) -> bool {
        self.providers.contains_key(&domain)
    }

    pub fn get(&self, domain: KekDomain) -> Option<&Arc<dyn KekProvider>> {
        self.providers.get(&domain)
    }

    /// Wrap a new DEK under the provider registered for `domain`.
    pub async fn wrap_new_dek(
        &self,
        domain: KekDomain,
    ) -> Result<(DekBytes, WrappedDek), AeonError> {
        let provider = self.expect_domain(domain)?;
        provider.wrap_new_dek().await
    }

    /// Unwrap a [`WrappedDek`], dispatching to the provider registered
    /// for its declared domain.
    pub async fn unwrap_dek(&self, wrapped: &WrappedDek) -> Result<DekBytes, AeonError> {
        let provider = self.expect_domain(wrapped.kek_domain)?;
        provider.unwrap_dek(wrapped).await
    }

    fn expect_domain(&self, domain: KekDomain) -> Result<&Arc<dyn KekProvider>, AeonError> {
        self.providers.get(&domain).ok_or_else(|| AeonError::Config {
            message: format!(
                "no KekProvider registered for {:?} domain — check providers.kek in your pipeline manifest",
                domain
            ),
        })
    }
}

impl Default for KekRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_crypto::kek::KekHandle;
    use aeon_types::{SecretBytes, SecretProvider, SecretRef, SecretRegistry, SecretScheme};

    struct HexEnvProvider;
    impl SecretProvider for HexEnvProvider {
        fn scheme(&self) -> SecretScheme {
            SecretScheme::Env
        }
        fn resolve(&self, path: &str) -> Result<SecretBytes, aeon_types::SecretError> {
            let val = std::env::var(path)
                .map_err(|_| aeon_types::SecretError::EnvNotSet(path.to_string()))?;
            let bytes: Vec<u8> = (0..val.len())
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

    fn make_local_handle(domain: KekDomain, var: &'static str, id: &str) -> Arc<dyn KekProvider> {
        unsafe { std::env::set_var(var, hex_encode(&[0x33u8; 32])) };
        Arc::new(KekHandle::new(
            domain,
            id.to_string(),
            SecretRef::env(var),
            hex_registry(),
        ))
    }

    #[tokio::test]
    async fn registry_wraps_and_unwraps_via_registered_provider() {
        const VAR: &str = "AEON_TEST_KEK_REGISTRY_WRAP";
        let mut reg = KekRegistry::empty();
        reg.register(make_local_handle(KekDomain::DataContext, VAR, "v1"))
            .unwrap();

        let (dek, wrapped) = reg.wrap_new_dek(KekDomain::DataContext).await.unwrap();
        let unwrapped = reg.unwrap_dek(&wrapped).await.unwrap();
        assert_eq!(dek.expose_bytes(), unwrapped.expose_bytes());

        unsafe { std::env::remove_var(VAR) };
    }

    #[tokio::test]
    async fn registry_rejects_duplicate_domain() {
        const V1: &str = "AEON_TEST_KEK_REGISTRY_DUP_1";
        const V2: &str = "AEON_TEST_KEK_REGISTRY_DUP_2";
        let mut reg = KekRegistry::empty();
        reg.register(make_local_handle(KekDomain::DataContext, V1, "v1"))
            .unwrap();
        let err = reg
            .register(make_local_handle(KekDomain::DataContext, V2, "v2"))
            .unwrap_err();
        assert!(matches!(
            err,
            SecretsAdapterError::DuplicateRegistration { .. }
        ));

        unsafe {
            std::env::remove_var(V1);
            std::env::remove_var(V2);
        }
    }

    #[tokio::test]
    async fn registry_unknown_domain_errors_on_wrap() {
        let reg = KekRegistry::empty();
        let err = reg.wrap_new_dek(KekDomain::LogContext).await.unwrap_err();
        assert!(matches!(err, AeonError::Config { .. }));
    }

    #[tokio::test]
    async fn registry_dispatches_unwrap_on_wrapped_domain() {
        const V_DATA: &str = "AEON_TEST_KEK_REGISTRY_DUAL_DATA";
        const V_LOG: &str = "AEON_TEST_KEK_REGISTRY_DUAL_LOG";
        let mut reg = KekRegistry::empty();
        reg.register(make_local_handle(KekDomain::DataContext, V_DATA, "d1"))
            .unwrap();
        reg.register(make_local_handle(KekDomain::LogContext, V_LOG, "l1"))
            .unwrap();

        let (_, data_wrapped) = reg.wrap_new_dek(KekDomain::DataContext).await.unwrap();
        let (_, log_wrapped) = reg.wrap_new_dek(KekDomain::LogContext).await.unwrap();

        // Each wrapped DEK gets routed to its own provider — no cross-talk.
        reg.unwrap_dek(&data_wrapped).await.unwrap();
        reg.unwrap_dek(&log_wrapped).await.unwrap();

        unsafe {
            std::env::remove_var(V_DATA);
            std::env::remove_var(V_LOG);
        }
    }
}
