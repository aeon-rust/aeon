//! S4.2 — Cross-cutting compliance precondition validator.
//!
//! Runs on the pipeline-start cold path, *after* S3 encryption,
//! S5 retention, and S6.9 erasure probes have each resolved their
//! plans. Where the individual probes enforce their own local
//! invariants (is this block parseable? is `at_rest = required`
//! backed by a KEK?), this validator enforces the **regime-driven**
//! preconditions that cross-cut all three subsystems:
//!
//! | regime       | encryption | retention | erasure |
//! |--------------|-----------:|----------:|--------:|
//! | `none`       |     —      |     —     |    —    |
//! | `pci`        |     ✓      |     ✓     |    —    |
//! | `hipaa`      |     ✓      |     ✓     |    —    |
//! | `gdpr`       |     ✓      |     ✓     |    ✓    |
//! | `mixed`      |     ✓      |     ✓     |    ✓    |
//!
//! - Under `Strict`, any unmet precondition is a hard refusal to start.
//! - Under `Warn`, each finding is logged at `warn` level but the
//!   pipeline boots. Operators get a single authoritative message per
//!   concern per pipeline start rather than a flood.
//! - Under `Off`, this function is a no-op — so are the individual
//!   regime checks below.
//!
//! **Overlap with S6.9**: `resolve_erasure_plan` already refuses to
//! return an `ErasurePlan` for `gdpr/mixed + strict + plaintext`, so
//! the `requires_encryption` check here is redundant for that specific
//! combination. The redundancy is intentional — S6.9 guards any
//! caller that runs the erasure probe directly (tests, future
//! subsystems), while S4.2 is the single entry point that closes the
//! encryption gap for `pci/hipaa`, which S6.9 deliberately leaves
//! alone. Neither probe can be safely removed.
//!
//! S4.2 does **not** evaluate `ComplianceBlock.selectors`. The selectors
//! are descriptors for downstream consumers (at-rest encryption of
//! named fields, erasure extraction, retention classification); their
//! *presence* is not a precondition, and their semantics are
//! format-specific (JSON / MessagePack / binary). A future S4.3 may
//! add a selector-shape validator.

use aeon_types::{AeonError, ComplianceBlock, ComplianceRegime, EnforcementLevel};

use crate::encryption_probe::EncryptionPlan;
use crate::erasure_probe::ErasurePlan;
use crate::retention_probe::RetentionPlan;

/// A single precondition failure identified by the validator. Kept as a
/// public type so REST / audit surfaces can classify findings without
/// regex-matching the human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComplianceFinding {
    /// Stable short tag — `"encryption"`, `"retention"`, or `"erasure"`.
    /// Safe to emit into metrics labels.
    pub concern: &'static str,
    /// Human-readable description of what was missing.
    pub message: String,
}

/// Validate that a pipeline's resolved plans satisfy its declared
/// compliance regime.
///
/// Returns `Ok(Vec<ComplianceFinding>)` on success — the vector is
/// non-empty only under `Warn` enforcement, where it carries the
/// findings the caller already saw logged. Under `Strict` with
/// findings, returns `Err(AeonError::config(…))`. Under `Off` or
/// `regime = None`, returns `Ok(vec![])` after a cheap short-circuit.
pub fn validate_compliance(
    compliance: &ComplianceBlock,
    encryption: &EncryptionPlan,
    retention: &RetentionPlan,
    erasure: &ErasurePlan,
) -> Result<Vec<ComplianceFinding>, AeonError> {
    // Fast path: operator has not opted into enforcement, or regime is
    // `None`. Both `is_active()` (regime != None AND enforcement != Off)
    // would skip this too, but spelling out `regime == None` here keeps
    // the intent obvious.
    if !compliance.enforcement.is_active()
        || matches!(compliance.regime, ComplianceRegime::None)
    {
        return Ok(Vec::new());
    }

    let mut findings: Vec<ComplianceFinding> = Vec::new();

    if compliance.regime.requires_encryption() && !encryption.is_active() {
        findings.push(ComplianceFinding {
            concern: "encryption",
            message: format!(
                "regime {:?} requires at-rest encryption but resolved \
                 encryption plan is inactive (set `encryption.at_rest` \
                 to `required` with a data-context KEK, or `optional` \
                 with a KEK supplied)",
                compliance.regime
            ),
        });
    }

    if compliance.regime.requires_retention() && retention.is_inert() {
        findings.push(ComplianceFinding {
            concern: "retention",
            message: format!(
                "regime {:?} requires a retention policy but resolved \
                 retention plan is inert (set `durability.retention.l2_body.hold_after_ack` \
                 or `durability.retention.l3_ack.max_records`)",
                compliance.regime
            ),
        });
    }

    if compliance.regime.requires_erasure() && !erasure.required {
        findings.push(ComplianceFinding {
            concern: "erasure",
            message: format!(
                "regime {:?} requires GDPR erasure surface but resolved \
                 erasure plan is inactive (enable `compliance.enforcement` \
                 of at least `warn` so the erasure probe marks the \
                 pipeline's surface as required)",
                compliance.regime
            ),
        });
    }

    if findings.is_empty() {
        return Ok(Vec::new());
    }

    match compliance.enforcement {
        EnforcementLevel::Strict => {
            let details = findings
                .iter()
                .map(|f| format!("[{}] {}", f.concern, f.message))
                .collect::<Vec<_>>()
                .join("; ");
            Err(AeonError::config(format!(
                "compliance validator: regime {:?} under strict \
                 enforcement has {} unmet precondition{}: {}",
                compliance.regime,
                findings.len(),
                if findings.len() == 1 { "" } else { "s" },
                details
            )))
        }
        EnforcementLevel::Warn => {
            for f in &findings {
                tracing::warn!(
                    concern = f.concern,
                    regime = ?compliance.regime,
                    "compliance precondition unmet (warn-only): {}",
                    f.message
                );
            }
            Ok(findings)
        }
        EnforcementLevel::Off => {
            debug_assert!(false, "Off enforcement must short-circuit earlier");
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::compliance::{DataClass, PayloadFormat, PiiSelector};
    use aeon_types::{AtRestEncryption, ErasureConfig};
    use std::time::Duration;

    fn enc_off() -> EncryptionPlan {
        EncryptionPlan {
            kek: None,
            declared: AtRestEncryption::Off,
        }
    }

    fn enc_active() -> EncryptionPlan {
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
            "AEON_TEST_VALIDATOR_KEK_{}",
            N.fetch_add(1, Ordering::Relaxed)
        );
        let hex: String = (0..32).map(|_| "42".to_string()).collect();
        // SAFETY: test-only env mutation, unique var per call.
        unsafe { std::env::set_var(&var, &hex) };
        let mut reg = SecretRegistry::empty();
        reg.register(Arc::new(HexEnv));
        let kek = Arc::new(KekHandle::new(
            KekDomain::DataContext,
            "test-validator-kek",
            SecretRef::env(&var),
            Arc::new(reg),
        ));
        EncryptionPlan {
            kek: Some(kek),
            declared: AtRestEncryption::Required,
        }
    }

    fn ret_inert() -> RetentionPlan {
        RetentionPlan::default()
    }

    fn ret_active() -> RetentionPlan {
        RetentionPlan {
            l2_hold_after_ack: Duration::from_secs(300),
            l3_max_records: Some(10_000),
        }
    }

    fn era_noop() -> ErasurePlan {
        ErasurePlan {
            required: false,
            max_delay_hours: 24,
        }
    }

    fn era_required() -> ErasurePlan {
        ErasurePlan {
            required: true,
            max_delay_hours: 24,
        }
    }

    fn block(regime: ComplianceRegime, enforcement: EnforcementLevel) -> ComplianceBlock {
        ComplianceBlock {
            regime,
            enforcement,
            selectors: vec![PiiSelector {
                path: "$.x".into(),
                format: PayloadFormat::Json,
                class: DataClass::Pii,
            }],
            erasure: ErasureConfig::default(),
        }
    }

    // ── Off / None short-circuits ────────────────────────────────────

    #[test]
    fn off_enforcement_is_noop_even_with_bad_state() {
        let b = block(ComplianceRegime::Gdpr, EnforcementLevel::Off);
        let out = validate_compliance(&b, &enc_off(), &ret_inert(), &era_noop()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn none_regime_is_noop_even_under_strict() {
        let b = block(ComplianceRegime::None, EnforcementLevel::Strict);
        let out = validate_compliance(&b, &enc_off(), &ret_inert(), &era_noop()).unwrap();
        assert!(out.is_empty());
    }

    // ── PCI ──────────────────────────────────────────────────────────

    #[test]
    fn pci_strict_passes_when_encryption_and_retention_active() {
        let b = block(ComplianceRegime::Pci, EnforcementLevel::Strict);
        let out = validate_compliance(&b, &enc_active(), &ret_active(), &era_noop()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn pci_strict_fails_without_encryption() {
        let b = block(ComplianceRegime::Pci, EnforcementLevel::Strict);
        let err =
            validate_compliance(&b, &enc_off(), &ret_active(), &era_noop()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("encryption"), "error must cite encryption: {msg}");
        assert!(!msg.contains("erasure"), "PCI must not surface an erasure finding: {msg}");
    }

    #[test]
    fn pci_strict_fails_without_retention() {
        let b = block(ComplianceRegime::Pci, EnforcementLevel::Strict);
        let err =
            validate_compliance(&b, &enc_active(), &ret_inert(), &era_noop()).unwrap_err();
        assert!(format!("{err}").contains("retention"));
    }

    #[test]
    fn pci_strict_reports_multiple_findings() {
        let b = block(ComplianceRegime::Pci, EnforcementLevel::Strict);
        let err =
            validate_compliance(&b, &enc_off(), &ret_inert(), &era_noop()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("2 unmet preconditions"), "expected '2 unmet preconditions' in: {msg}");
        assert!(msg.contains("encryption"));
        assert!(msg.contains("retention"));
    }

    // ── HIPAA (parallel to PCI) ──────────────────────────────────────

    #[test]
    fn hipaa_strict_requires_encryption_and_retention() {
        let b = block(ComplianceRegime::Hipaa, EnforcementLevel::Strict);
        assert!(
            validate_compliance(&b, &enc_off(), &ret_active(), &era_noop()).is_err()
        );
        assert!(
            validate_compliance(&b, &enc_active(), &ret_inert(), &era_noop()).is_err()
        );
        assert!(
            validate_compliance(&b, &enc_active(), &ret_active(), &era_noop())
                .unwrap()
                .is_empty()
        );
    }

    // ── GDPR ─────────────────────────────────────────────────────────

    #[test]
    fn gdpr_strict_full_green() {
        let b = block(ComplianceRegime::Gdpr, EnforcementLevel::Strict);
        let out =
            validate_compliance(&b, &enc_active(), &ret_active(), &era_required()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn gdpr_strict_fails_without_erasure_plan() {
        // Simulated scenario: ErasurePlan.required is false (operator
        // passed Off to resolve_erasure_plan somehow). S4.2 must catch
        // the gap independently.
        let b = block(ComplianceRegime::Gdpr, EnforcementLevel::Strict);
        let err =
            validate_compliance(&b, &enc_active(), &ret_active(), &era_noop()).unwrap_err();
        assert!(format!("{err}").contains("erasure"));
    }

    #[test]
    fn gdpr_strict_lists_all_three_concerns_when_all_missing() {
        let b = block(ComplianceRegime::Gdpr, EnforcementLevel::Strict);
        let err =
            validate_compliance(&b, &enc_off(), &ret_inert(), &era_noop()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("3 unmet preconditions"));
        assert!(msg.contains("encryption"));
        assert!(msg.contains("retention"));
        assert!(msg.contains("erasure"));
    }

    // ── Mixed ────────────────────────────────────────────────────────

    #[test]
    fn mixed_strict_requires_all_three() {
        let b = block(ComplianceRegime::Mixed, EnforcementLevel::Strict);
        assert!(
            validate_compliance(&b, &enc_active(), &ret_active(), &era_required())
                .unwrap()
                .is_empty()
        );
        assert!(
            validate_compliance(&b, &enc_off(), &ret_active(), &era_required()).is_err()
        );
    }

    // ── Warn enforcement ────────────────────────────────────────────

    #[test]
    fn warn_enforcement_returns_findings_instead_of_err() {
        let b = block(ComplianceRegime::Gdpr, EnforcementLevel::Warn);
        let findings =
            validate_compliance(&b, &enc_off(), &ret_inert(), &era_noop()).unwrap();
        assert_eq!(findings.len(), 3);
        let concerns: Vec<_> = findings.iter().map(|f| f.concern).collect();
        assert!(concerns.contains(&"encryption"));
        assert!(concerns.contains(&"retention"));
        assert!(concerns.contains(&"erasure"));
    }

    #[test]
    fn warn_with_clean_state_returns_empty() {
        let b = block(ComplianceRegime::Pci, EnforcementLevel::Warn);
        let findings =
            validate_compliance(&b, &enc_active(), &ret_active(), &era_noop()).unwrap();
        assert!(findings.is_empty());
    }

    // ── Stability contract: concern tags are stable strings ─────────

    #[test]
    fn concern_tags_are_stable_identifiers() {
        let b = block(ComplianceRegime::Gdpr, EnforcementLevel::Warn);
        let findings =
            validate_compliance(&b, &enc_off(), &ret_inert(), &era_noop()).unwrap();
        for f in &findings {
            assert!(
                matches!(f.concern, "encryption" | "retention" | "erasure"),
                "unexpected concern tag: {}",
                f.concern
            );
        }
    }
}
