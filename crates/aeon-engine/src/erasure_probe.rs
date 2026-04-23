//! S6.9 — Pipeline-start erasure precondition probe.
//!
//! Mirrors `encryption_probe` (S3) and `retention_probe` (S5): runs on the
//! cold start path, does no disk I/O, and hard-fails early when a pipeline
//! declares a regime whose structural preconditions are not met.
//!
//! The rule is narrow and load-bearing:
//!
//! A pipeline declaring `compliance.regime = gdpr|mixed` under
//! `compliance.enforcement = strict` must have at-rest encryption
//! **active** on the resolved [`EncryptionPlan`]. Without it, L2 segments
//! and L3 values land in plaintext, which means "erasure" by tombstone
//! only forgets the *index*; the underlying bytes survive on disk, on
//! backups, and in any replicated copies. GDPR Art. 17 cannot be
//! credibly claimed in that configuration — refusing to start is
//! strictly better than lying to an auditor.
//!
//! Matrix:
//!
//! | regime         | enforcement | encryption active | outcome             |
//! |----------------|-------------|-------------------|---------------------|
//! | none           | any         | any               | Ok (no-op)          |
//! | pci / hipaa    | any         | any               | Ok (not an erasure regime — S4.2 validates) |
//! | gdpr / mixed   | off         | any               | Ok (no-op)          |
//! | gdpr / mixed   | warn        | any               | Ok + warn log if missing |
//! | gdpr / mixed   | strict      | active            | Ok                  |
//! | gdpr / mixed   | strict      | inactive          | **hard error**      |
//!
//! Why not also check for a configured `subject_extract` or a mounted
//! `ErasureStore` here? Both can legitimately be runtime-resolved: the
//! source-side subject extractor is wired inside the connector factory,
//! and the erasure store is a supervisor singleton shared across
//! pipelines. Those stay the province of S4.2 (full compliance
//! validator). S6.9 owns only the piece that is trivially decidable
//! from the pipeline's own manifest state: did the operator enable
//! at-rest encryption?

use aeon_types::{AeonError, ComplianceBlock, ComplianceRegime, EnforcementLevel};

use crate::encryption_probe::EncryptionPlan;

/// Resolved erasure precondition outcome. Currently carries only the
/// max-delay policy copied from `ComplianceBlock::erasure` so the
/// `ErasurePolicy` evaluator (S6.8) can be built from it without
/// re-reading the manifest; future fields (regime-derived scan cadence,
/// per-selector hints) hang off this struct.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErasurePlan {
    /// `true` when the pipeline has opted into GDPR/Mixed under warn or
    /// strict enforcement. Probe callers check this before wiring an
    /// `ErasurePolicy` / compactor; pipelines that did not opt in skip
    /// that machinery entirely.
    pub required: bool,
    /// Copied from `ComplianceBlock::erasure.max_delay_hours`. Mirrors
    /// `RetentionPlan::l2_hold_after_ack`: the engine gets a
    /// strongly-typed value and never re-parses the manifest.
    pub max_delay_hours: u32,
}

/// Resolve the pipeline's declared compliance block into an
/// [`ErasurePlan`]. Called from `PipelineSupervisor::start` after the
/// encryption plan has been resolved, so this probe can inspect whether
/// at-rest encryption will actually be active when writes land.
///
/// Returns `Err` when the pipeline is in a configuration that cannot
/// credibly honour GDPR right-to-erasure (strict + gdpr/mixed +
/// plaintext writes). Returns `Ok` with `required = false` for any
/// configuration that does not need an erasure surface.
pub fn resolve_erasure_plan(
    compliance: &ComplianceBlock,
    encryption_plan: &EncryptionPlan,
) -> Result<ErasurePlan, AeonError> {
    let regime_needs_erasure = compliance.regime.requires_erasure();
    let enforcement_active = compliance.enforcement.is_active();

    if !regime_needs_erasure || !enforcement_active {
        // No-op: regime does not require erasure, or operator opted out
        // of enforcement. Keep `required=false` so the supervisor skips
        // the compactor machinery.
        return Ok(ErasurePlan {
            required: false,
            max_delay_hours: compliance.erasure.max_delay_hours,
        });
    }

    // From here: regime is gdpr/mixed AND enforcement is warn or strict.
    let encrypted = encryption_plan.is_active();

    if !encrypted {
        let msg = "erasure probe: compliance.regime requires GDPR erasure \
                   but at-rest encryption is not active — tombstones would \
                   only forget the index while plaintext segments remain \
                   readable on disk and in backups, so Art. 17 cannot be \
                   honoured";

        match compliance.enforcement {
            EnforcementLevel::Strict => {
                return Err(AeonError::config(format!(
                    "{msg}; set `encryption.at_rest = required` (or `optional` \
                     with a data-context KEK supplied) to start this pipeline"
                )));
            }
            EnforcementLevel::Warn => {
                tracing::warn!(
                    regime = ?compliance.regime,
                    "{msg} (warn-only enforcement; pipeline will start anyway)"
                );
            }
            // Off was already short-circuited above via `enforcement_active`.
            EnforcementLevel::Off => {
                debug_assert!(false, "Off enforcement must short-circuit earlier");
            }
        }
    }

    // Regime matches None path? No — we only reach here when
    // `regime.requires_erasure()` is true. Keep the branch explicit so
    // a future `ComplianceRegime` variant that adds a new erasure-class
    // regime is easy to spot here.
    debug_assert!(matches!(
        compliance.regime,
        ComplianceRegime::Gdpr | ComplianceRegime::Mixed
    ));

    Ok(ErasurePlan {
        required: true,
        max_delay_hours: compliance.erasure.max_delay_hours,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{AtRestEncryption, DEFAULT_ERASURE_MAX_DELAY_HOURS, ErasureConfig};
    use aeon_types::compliance::{DataClass, PayloadFormat, PiiSelector};

    fn plan_off() -> EncryptionPlan {
        EncryptionPlan {
            kek: None,
            declared: AtRestEncryption::Off,
        }
    }

    fn plan_active() -> EncryptionPlan {
        // `is_active()` only checks `kek.is_some()`, so a placeholder
        // Arc is enough to simulate active-at-rest here. No real bytes
        // are unwrapped — the probe never touches the KEK.
        use aeon_crypto::kek::{KekDomain, KekHandle};
        use aeon_types::{
            SecretBytes, SecretError, SecretProvider, SecretRef, SecretRegistry, SecretScheme,
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct HexEnv;
        impl SecretProvider for HexEnv {
            fn scheme(&self) -> SecretScheme {
                SecretScheme::Env
            }
            fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError> {
                let v = std::env::var(path).map_err(|_| SecretError::EnvNotSet(path.into()))?;
                let bytes: Vec<u8> = (0..v.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&v[i..i + 2], 16).unwrap_or(0))
                    .collect();
                Ok(SecretBytes::new(bytes))
            }
        }

        static N: AtomicU64 = AtomicU64::new(0);
        let var = format!(
            "AEON_TEST_ERASURE_PROBE_KEK_{}",
            N.fetch_add(1, Ordering::Relaxed)
        );
        let hex: String = (0..32).map(|_| "42".to_string()).collect();
        // SAFETY: test-only env mutation, unique var per call.
        unsafe { std::env::set_var(&var, &hex) };
        let mut reg = SecretRegistry::empty();
        reg.register(Arc::new(HexEnv));
        let kek = Arc::new(KekHandle::new(
            KekDomain::DataContext,
            "test-erasure-kek",
            SecretRef::env(&var),
            Arc::new(reg),
        ));
        EncryptionPlan {
            kek: Some(kek),
            declared: AtRestEncryption::Required,
        }
    }

    fn gdpr_strict() -> ComplianceBlock {
        ComplianceBlock {
            regime: ComplianceRegime::Gdpr,
            enforcement: EnforcementLevel::Strict,
            selectors: vec![PiiSelector {
                path: "$.user.id".into(),
                format: PayloadFormat::Json,
                class: DataClass::Pii,
            }],
            erasure: ErasureConfig::default(),
        }
    }

    #[test]
    fn default_block_is_noop() {
        let p = resolve_erasure_plan(&ComplianceBlock::default(), &plan_off()).unwrap();
        assert!(!p.required);
        assert_eq!(p.max_delay_hours, DEFAULT_ERASURE_MAX_DELAY_HOURS);
    }

    #[test]
    fn pci_strict_ignores_encryption_state() {
        // PCI is not an erasure regime — S4.2 will enforce its own
        // preconditions, but S6.9 must not refuse here.
        let b = ComplianceBlock {
            regime: ComplianceRegime::Pci,
            enforcement: EnforcementLevel::Strict,
            ..Default::default()
        };
        let p = resolve_erasure_plan(&b, &plan_off()).unwrap();
        assert!(!p.required);
    }

    #[test]
    fn hipaa_strict_ignores_encryption_state() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Hipaa,
            enforcement: EnforcementLevel::Strict,
            ..Default::default()
        };
        let p = resolve_erasure_plan(&b, &plan_off()).unwrap();
        assert!(!p.required);
    }

    #[test]
    fn gdpr_off_is_noop_regardless_of_encryption() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Gdpr,
            enforcement: EnforcementLevel::Off,
            ..Default::default()
        };
        let p = resolve_erasure_plan(&b, &plan_off()).unwrap();
        assert!(!p.required);
    }

    #[test]
    fn gdpr_strict_without_encryption_is_hard_error() {
        let err = resolve_erasure_plan(&gdpr_strict(), &plan_off()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("erasure") && msg.contains("encryption"),
            "error must name both concerns: {msg}"
        );
    }

    #[test]
    fn mixed_strict_without_encryption_is_hard_error() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Mixed,
            enforcement: EnforcementLevel::Strict,
            ..Default::default()
        };
        assert!(resolve_erasure_plan(&b, &plan_off()).is_err());
    }

    #[test]
    fn gdpr_strict_with_active_encryption_succeeds() {
        let p = resolve_erasure_plan(&gdpr_strict(), &plan_active()).unwrap();
        assert!(p.required);
        assert_eq!(p.max_delay_hours, DEFAULT_ERASURE_MAX_DELAY_HOURS);
    }

    #[test]
    fn gdpr_warn_without_encryption_starts_anyway() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Gdpr,
            enforcement: EnforcementLevel::Warn,
            ..Default::default()
        };
        // Warn must not return Err — it only logs.
        let p = resolve_erasure_plan(&b, &plan_off()).unwrap();
        assert!(p.required, "warn still marks the surface as needed");
    }

    #[test]
    fn gdpr_warn_with_encryption_succeeds() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Gdpr,
            enforcement: EnforcementLevel::Warn,
            ..Default::default()
        };
        let p = resolve_erasure_plan(&b, &plan_active()).unwrap();
        assert!(p.required);
    }

    #[test]
    fn plan_copies_custom_max_delay_hours() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Gdpr,
            enforcement: EnforcementLevel::Strict,
            erasure: ErasureConfig { max_delay_hours: 6 },
            ..Default::default()
        };
        let p = resolve_erasure_plan(&b, &plan_active()).unwrap();
        assert_eq!(p.max_delay_hours, 6);
    }

    #[test]
    fn none_regime_skips_probe_even_if_enforcement_strict() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::None,
            enforcement: EnforcementLevel::Strict,
            ..Default::default()
        };
        let p = resolve_erasure_plan(&b, &plan_off()).unwrap();
        assert!(!p.required);
    }
}
