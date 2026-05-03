//! S3.5 — Pipeline-start at-rest encryption probe.
//!
//! Translates the declared [`EncryptionBlock`] on a `PipelineManifest`
//! into a concrete plan consumed by L2 body segments (see `l2_body.rs`)
//! and L3 value-level encryption (see `aeon_state::l3_encrypted`).
//!
//! The probe runs on the pipeline-start cold path. It does not touch
//! disk, does not fetch KEK bytes, and does not generate DEKs — those
//! operations happen later inside the L2/L3 opens. The probe only makes
//! the policy decision:
//!
//! | `at_rest`   | `kek` provided | Outcome                             |
//! |-------------|----------------|-------------------------------------|
//! | `Off`       | any            | `plan.kek = None`                   |
//! | `Optional`  | `Some(k)`      | `plan.kek = Some(k)`                |
//! | `Optional`  | `None`         | `plan.kek = None` (warn-only)       |
//! | `Required`  | `Some(k)`      | `plan.kek = Some(k)`                |
//! | `Required`  | `None`         | **hard error** — pipeline refuses to start |
//!
//! S4.2's compliance validator will later consume the same `kek` handle
//! to confirm the regime's `requires_encryption()` precondition is met.

use aeon_crypto::kek::{KekDomain, KekHandle};
use aeon_types::{AeonError, AtRestEncryption, EncryptionBlock};
use std::sync::Arc;

/// Concrete at-rest plan produced by [`resolve_encryption_plan`].
#[derive(Clone, Default)]
pub struct EncryptionPlan {
    /// Data-context KEK to install on new L2 segments and the L3 store
    /// wrapper. `None` means plaintext writes.
    pub kek: Option<Arc<KekHandle>>,
    /// The declared mode that produced this plan. Preserved so callers
    /// (telemetry, /metrics, compliance validator) can distinguish
    /// "ran as plaintext because config said `off`" from "ran as
    /// plaintext because `optional` was declared but no KEK was
    /// provided."
    pub declared: AtRestEncryption,
}

impl std::fmt::Debug for EncryptionPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptionPlan")
            .field("declared", &self.declared)
            .field("kek", &self.kek.as_ref().map(|_| "<kek>"))
            .finish()
    }
}

impl EncryptionPlan {
    /// `true` when writes through this plan will seal payloads before
    /// landing on disk.
    pub fn is_active(&self) -> bool {
        self.kek.is_some()
    }
}

/// Resolve a pipeline's declared encryption configuration into a plan.
///
/// `kek` should be the data-context KEK handle produced by the secret
/// provider during pipeline bootstrap. Passing a log-context handle
/// here is a bug — the probe rejects it so the mistake fails fast
/// instead of silently wrapping event payloads under the wrong domain.
pub fn resolve_encryption_plan(
    block: &EncryptionBlock,
    kek: Option<Arc<KekHandle>>,
) -> Result<EncryptionPlan, AeonError> {
    if let Some(k) = kek.as_ref() {
        if k.domain() != KekDomain::DataContext {
            return Err(AeonError::state(format!(
                "encryption probe: KEK must be in DataContext domain (got {:?}) — \
                 payload sealing under log-context would break S1 domain separation",
                k.domain()
            )));
        }
    }

    match block.at_rest {
        AtRestEncryption::Off => Ok(EncryptionPlan {
            kek: None,
            declared: AtRestEncryption::Off,
        }),
        AtRestEncryption::Optional => Ok(EncryptionPlan {
            kek,
            declared: AtRestEncryption::Optional,
        }),
        AtRestEncryption::Required => {
            let k = kek.ok_or_else(|| {
                AeonError::state(
                    "encryption probe: at_rest=required but no data-context KEK was \
                     supplied — pipeline refuses to start without the KEK needed to \
                     seal L2 segments and L3 values",
                )
            })?;
            Ok(EncryptionPlan {
                kek: Some(k),
                declared: AtRestEncryption::Required,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{
        SecretBytes, SecretError, SecretProvider, SecretRef, SecretRegistry, SecretScheme,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

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

    fn kek_for(domain: KekDomain) -> Arc<KekHandle> {
        static N: AtomicU64 = AtomicU64::new(0);
        let var = format!("AEON_TEST_PROBE_KEK_{}", N.fetch_add(1, Ordering::Relaxed));
        let hex: String = (0..32).map(|_| "42".to_string()).collect();
        // SAFETY: test-only env mutation, unique var per call.
        unsafe { std::env::set_var(&var, &hex) };
        let mut reg = SecretRegistry::empty();
        reg.register(Arc::new(HexEnv));
        Arc::new(KekHandle::new(
            domain,
            "test-probe-kek",
            SecretRef::env(&var),
            Arc::new(reg),
        ))
    }

    #[test]
    fn off_always_plaintext_regardless_of_kek() {
        let plan = resolve_encryption_plan(
            &EncryptionBlock {
                at_rest: AtRestEncryption::Off,
            },
            None,
        )
        .unwrap();
        assert!(!plan.is_active());
        assert_eq!(plan.declared, AtRestEncryption::Off);

        let plan = resolve_encryption_plan(
            &EncryptionBlock {
                at_rest: AtRestEncryption::Off,
            },
            Some(kek_for(KekDomain::DataContext)),
        )
        .unwrap();
        assert!(!plan.is_active(), "off must ignore any supplied KEK");
    }

    #[test]
    fn optional_uses_kek_when_supplied() {
        let plan = resolve_encryption_plan(
            &EncryptionBlock {
                at_rest: AtRestEncryption::Optional,
            },
            Some(kek_for(KekDomain::DataContext)),
        )
        .unwrap();
        assert!(plan.is_active());
    }

    #[test]
    fn optional_is_plaintext_when_no_kek() {
        let plan = resolve_encryption_plan(
            &EncryptionBlock {
                at_rest: AtRestEncryption::Optional,
            },
            None,
        )
        .unwrap();
        assert!(!plan.is_active());
        assert_eq!(plan.declared, AtRestEncryption::Optional);
    }

    #[test]
    fn required_hard_fails_without_kek() {
        let err = resolve_encryption_plan(
            &EncryptionBlock {
                at_rest: AtRestEncryption::Required,
            },
            None,
        );
        assert!(err.is_err(), "required + no KEK must refuse to start");
    }

    #[test]
    fn required_succeeds_with_data_context_kek() {
        let plan = resolve_encryption_plan(
            &EncryptionBlock {
                at_rest: AtRestEncryption::Required,
            },
            Some(kek_for(KekDomain::DataContext)),
        )
        .unwrap();
        assert!(plan.is_active());
        assert_eq!(plan.declared, AtRestEncryption::Required);
    }

    #[test]
    fn log_context_kek_is_rejected() {
        let err = resolve_encryption_plan(
            &EncryptionBlock {
                at_rest: AtRestEncryption::Required,
            },
            Some(kek_for(KekDomain::LogContext)),
        );
        assert!(
            err.is_err(),
            "log-context KEK must not be accepted for at-rest"
        );
    }

    #[test]
    fn default_block_yields_plaintext() {
        let plan = resolve_encryption_plan(&EncryptionBlock::default(), None).unwrap();
        assert!(!plan.is_active());
        assert_eq!(plan.declared, AtRestEncryption::Off);
    }
}
