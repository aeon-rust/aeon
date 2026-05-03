//! V5.1 — PoH signing-key resolver.
//!
//! Translates the declared [`PohBlock`]'s `signing_key_ref` field into a
//! concrete `Arc<SigningKey>` consumed by the per-batch PoH appender to
//! stamp signed Merkle roots. The probe runs on the pipeline-start cold
//! path; it does not touch disk, does not generate keys, and does not
//! sign anything — it only resolves a secret reference into a usable key
//! handle and validates the byte length.
//!
//! Resolution rules:
//!
//! | `enabled` | `signing_key_ref` | `registry`     | Outcome                          |
//! |-----------|-------------------|----------------|----------------------------------|
//! | `false`   | any               | any            | `Ok(None)`                       |
//! | `true`    | `None`            | any            | `Ok(None)` (chain-only verify)   |
//! | `true`    | `Some(..)`        | `None`         | **hard error** — no secret store |
//! | `true`    | `Some(..)`        | unknown scheme | **hard error** — provider missing |
//! | `true`    | `Some(..)`        | wrong length   | **hard error** — bad key bytes   |
//! | `true`    | `Some(..)`        | resolves OK    | `Ok(Some(Arc<SigningKey>))`      |
//!
//! Encoding: the resolved bytes are accepted in two shapes —
//! - exactly 32 raw bytes (an `Ed25519` private scalar), or
//! - exactly 64 ASCII characters that hex-decode to 32 bytes.
//!
//! The hex-string fallback is what makes plain-`env:` secret references
//! usable: env vars cannot easily carry binary payloads, so the canonical
//! format ends up being a 64-char hex string. Raw bytes are accepted so
//! that downstream secret stores (Vault transit, KMS) which return
//! literal key material work unchanged.

#![cfg(feature = "processor-auth")]

use aeon_crypto::signing::SigningKey;
use aeon_types::{AeonError, PohBlock, SecretRegistry};
use std::sync::Arc;

/// Resolve a pipeline's PoH signing-key reference into an `Arc<SigningKey>`.
///
/// Returns `Ok(None)` for the disabled / no-ref paths so the caller can
/// stamp `PipelineConfig.poh.signing_key` unconditionally without first
/// pattern-matching on `block.enabled`.
pub fn resolve_poh_signing_key(
    block: &PohBlock,
    registry: Option<&SecretRegistry>,
) -> Result<Option<Arc<SigningKey>>, AeonError> {
    if !block.enabled {
        return Ok(None);
    }
    let Some(ref_str) = block.signing_key_ref.as_deref() else {
        return Ok(None);
    };

    let registry = registry.ok_or_else(|| {
        AeonError::state(
            "poh probe: signing_key_ref is set but no SecretRegistry is \
             installed on the supervisor — call \
             PipelineSupervisor::set_secret_registry from cmd_serve before \
             starting pipelines that reference signing keys",
        )
    })?;

    let bytes = registry.resolve_str(ref_str).map_err(|e| {
        AeonError::state(format!(
            "poh probe: failed to resolve signing_key_ref '{ref_str}': {e}"
        ))
    })?;

    let key_bytes = decode_signing_key_bytes(bytes.expose_bytes(), ref_str)?;
    Ok(Some(Arc::new(SigningKey::from_bytes(&key_bytes))))
}

fn decode_signing_key_bytes(raw: &[u8], ref_str: &str) -> Result<[u8; 32], AeonError> {
    if raw.len() == 32 {
        let mut out = [0u8; 32];
        out.copy_from_slice(raw);
        return Ok(out);
    }
    if raw.len() == 64 {
        // Hex-string fallback: the typical env-var shape.
        if let Ok(s) = std::str::from_utf8(raw) {
            if let Ok(decoded) = hex::decode(s) {
                if decoded.len() == 32 {
                    let mut out = [0u8; 32];
                    out.copy_from_slice(&decoded);
                    return Ok(out);
                }
            }
        }
    }
    Err(AeonError::state(format!(
        "poh probe: signing_key_ref '{ref_str}' resolved to {} bytes — \
         expected 32 raw bytes or 64 hex characters for an Ed25519 secret \
         scalar",
        raw.len()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{SecretBytes, SecretError, SecretProvider, SecretRef, SecretScheme};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Test-only env provider that returns the env var's UTF-8 bytes
    /// verbatim. Mirrors `aeon_types::EnvProvider` for the resolution
    /// path; the canonical 64-char hex case lives upstream of decoding.
    struct RawEnv;
    impl SecretProvider for RawEnv {
        fn scheme(&self) -> SecretScheme {
            SecretScheme::Env
        }
        fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError> {
            let v = std::env::var(path).map_err(|_| SecretError::EnvNotSet(path.to_string()))?;
            Ok(SecretBytes::new(v.into_bytes()))
        }
    }

    fn unique_env_var(prefix: &str) -> String {
        static N: AtomicU64 = AtomicU64::new(0);
        format!("{prefix}_{}", N.fetch_add(1, Ordering::Relaxed))
    }

    fn env_registry() -> SecretRegistry {
        let mut reg = SecretRegistry::empty();
        reg.register(Arc::new(RawEnv));
        reg
    }

    #[test]
    fn disabled_block_returns_none_regardless_of_ref() {
        let block = PohBlock {
            enabled: false,
            max_recent_entries: 1024,
            signing_key_ref: Some("${ENV:WHATEVER}".into()),
        };
        let key = resolve_poh_signing_key(&block, None).unwrap();
        assert!(key.is_none());

        let key = resolve_poh_signing_key(&block, Some(&env_registry())).unwrap();
        assert!(key.is_none());
    }

    #[test]
    fn enabled_without_ref_returns_none() {
        let block = PohBlock {
            enabled: true,
            max_recent_entries: 1024,
            signing_key_ref: None,
        };
        let key = resolve_poh_signing_key(&block, Some(&env_registry())).unwrap();
        assert!(key.is_none());
    }

    #[test]
    fn ref_set_without_registry_hard_fails() {
        let block = PohBlock {
            enabled: true,
            max_recent_entries: 1024,
            signing_key_ref: Some("${ENV:NOT_BOUND}".into()),
        };
        let err = match resolve_poh_signing_key(&block, None) {
            Err(e) => e,
            Ok(_) => panic!("expected hard refusal when no registry installed"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("SecretRegistry"), "got {msg}");
    }

    #[test]
    fn hex_encoded_env_var_resolves_to_signing_key() {
        let var = unique_env_var("AEON_TEST_POH_SIG_HEX");
        let hex_key: String = (0..32).map(|_| "11".to_string()).collect();
        // SAFETY: test-only env mutation, unique var per call.
        unsafe { std::env::set_var(&var, &hex_key) };

        let block = PohBlock {
            enabled: true,
            max_recent_entries: 1024,
            signing_key_ref: Some(format!("${{ENV:{var}}}")),
        };
        let key = resolve_poh_signing_key(&block, Some(&env_registry()))
            .unwrap()
            .expect("hex-encoded env var must resolve");
        // Round-trip verifies the bytes round through the SigningKey.
        let scalar = key.to_bytes();
        assert_eq!(scalar, [0x11u8; 32]);
    }

    #[test]
    fn raw_32_byte_secret_resolves_to_signing_key() {
        // A SecretProvider that returns raw 32 bytes (mimics Vault-transit
        // / KMS providers that hand back literal key material).
        struct Raw32(Vec<u8>);
        impl SecretProvider for Raw32 {
            fn scheme(&self) -> SecretScheme {
                SecretScheme::Literal
            }
            fn resolve(&self, _path: &str) -> Result<SecretBytes, SecretError> {
                Ok(SecretBytes::new(self.0.clone()))
            }
        }

        let mut reg = SecretRegistry::empty();
        reg.register(Arc::new(Raw32(vec![0x42u8; 32])));

        let block = PohBlock {
            enabled: true,
            max_recent_entries: 1024,
            signing_key_ref: Some("plain-literal-passthrough".into()),
        };
        let key = resolve_poh_signing_key(&block, Some(&reg))
            .unwrap()
            .expect("raw 32 bytes must resolve");
        assert_eq!(key.to_bytes(), [0x42u8; 32]);
    }

    #[test]
    fn wrong_length_secret_hard_fails() {
        let var = unique_env_var("AEON_TEST_POH_SIG_BAD");
        // 30 hex chars — neither 32 raw bytes nor 64 hex chars.
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var(&var, "112233445566778899aabbccddeeff") };
        let block = PohBlock {
            enabled: true,
            max_recent_entries: 1024,
            signing_key_ref: Some(format!("${{ENV:{var}}}")),
        };
        let err = match resolve_poh_signing_key(&block, Some(&env_registry())) {
            Err(e) => e,
            Ok(_) => panic!("expected hard refusal on wrong-length secret"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("expected 32 raw bytes"), "got {msg}");
    }

    #[test]
    fn malformed_hex_string_hard_fails() {
        let var = unique_env_var("AEON_TEST_POH_SIG_NOTHEX");
        // 64 chars but non-hex — falls through to length error.
        let bad: String = (0..64).map(|_| 'z').collect();
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var(&var, &bad) };
        let block = PohBlock {
            enabled: true,
            max_recent_entries: 1024,
            signing_key_ref: Some(format!("${{ENV:{var}}}")),
        };
        let err = match resolve_poh_signing_key(&block, Some(&env_registry())) {
            Err(e) => e,
            Ok(_) => panic!("expected hard refusal on malformed hex"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("expected 32 raw bytes"), "got {msg}");
    }

    #[test]
    fn unknown_scheme_hard_fails() {
        let block = PohBlock {
            enabled: true,
            max_recent_entries: 1024,
            // SecretScheme::Vault not registered in the empty registry.
            signing_key_ref: Some("${VAULT:kv/aeon/poh-signing-key}".into()),
        };
        let err = match resolve_poh_signing_key(&block, Some(&SecretRegistry::empty())) {
            Err(e) => e,
            Ok(_) => panic!("expected hard refusal on unknown scheme"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("failed to resolve signing_key_ref"),
            "got {msg}"
        );
    }

    #[test]
    fn malformed_ref_string_hard_fails() {
        let block = PohBlock {
            enabled: true,
            max_recent_entries: 1024,
            signing_key_ref: Some("not a ref".into()),
        };
        // Plain string with no `${...}` and no scheme → treated as literal,
        // but the env registry has no Literal provider, so resolution fails.
        // Either way, the contract is: bad refs do not panic — they
        // surface as a typed AeonError.
        match resolve_poh_signing_key(&block, Some(&env_registry())) {
            Err(_) => {}
            Ok(_) => panic!("expected hard refusal on malformed/unresolvable ref"),
        }
    }

    /// Suppress the unused-import warning when the SecretRef import is
    /// unused on a particular cfg path. Keeps the test module compile-
    /// clean alongside the other probe tests.
    #[allow(dead_code)]
    fn _suppress_unused_secret_ref(_r: SecretRef) {}
}
