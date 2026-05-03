//! S4 — Compliance mode manifest primitives.
//!
//! A pipeline can declare a compliance regime (PCI / HIPAA / GDPR / mixed)
//! together with an enforcement level (`strict` / `warn` / `off`). Under
//! `strict`, the pipeline hard-fails to start if the environment does not
//! satisfy the regime's preconditions (at-rest encryption, Vault-backed
//! secrets, retention schedule). `warn` logs the same findings but starts
//! the pipeline. `off` is the development default.
//!
//! This module owns the **schema** only. The precondition validator that
//! decides "strict is satisfied here" lands as S4.2 in a follow-up atom;
//! it consumes these types.
//!
//! # PII / PHI selectors
//!
//! The manifest can carry a list of field selectors describing where PII /
//! PHI lives in the event payload. Supported payload formats are JSON,
//! MessagePack, and a length-prefixed binary convention. Protobuf is
//! deliberately out of scope — schema-registry integration is a separate
//! future initiative and would drag the compliance surface into protoc
//! tooling we do not want here.
//!
//! These selectors are consumed by the at-rest encryption path (S3),
//! retention (S5), and erasure (S6). They are not evaluated in this crate.

use serde::{Deserialize, Serialize};

// ── Regime ───────────────────────────────────────────────────────────────

/// Compliance regime the pipeline operates under.
///
/// `Mixed` is for pipelines that process records falling under more than
/// one regime (e.g. a payment receipt that also carries a patient name) —
/// strict enforcement treats it as the union of all preconditions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplianceRegime {
    #[default]
    None,
    Pci,
    Hipaa,
    Gdpr,
    Mixed,
}

impl ComplianceRegime {
    /// Does this regime require the at-rest encryption preconditions from
    /// S3 to be in place? (All regimes other than `None` do.)
    pub const fn requires_encryption(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Does this regime require GDPR-style subject erasure (S6) to be
    /// wired up?
    pub const fn requires_erasure(self) -> bool {
        matches!(self, Self::Gdpr | Self::Mixed)
    }

    /// Does this regime require a retention schedule (S5)?
    pub const fn requires_retention(self) -> bool {
        !matches!(self, Self::None)
    }
}

// ── Enforcement level ────────────────────────────────────────────────────

/// How strictly to enforce the declared regime at pipeline start.
///
/// - `Strict` — preconditions are hard gates; pipeline refuses to start.
/// - `Warn`   — preconditions are logged as warnings; pipeline starts.
/// - `Off`    — no checks. Development default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementLevel {
    #[default]
    Off,
    Warn,
    Strict,
}

impl EnforcementLevel {
    /// `true` when findings should abort pipeline start.
    pub const fn is_blocking(self) -> bool {
        matches!(self, Self::Strict)
    }

    /// `true` when findings should be surfaced at all (strict or warn).
    pub const fn is_active(self) -> bool {
        !matches!(self, Self::Off)
    }
}

// ── PII / PHI selector ───────────────────────────────────────────────────

/// Classification of a selected field. Drives which regime preconditions
/// the field contributes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClass {
    Pii,
    Phi,
}

/// Payload format a selector addresses. Protobuf is intentionally absent;
/// schema-registry-backed formats are out of scope for S4.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadFormat {
    #[default]
    Json,
    MessagePack,
    /// Length-prefixed binary. The selector uses a byte-offset path rather
    /// than a field-name path; `path` semantics are format-specific.
    BinaryLengthPrefix,
}

/// A single field selector. `path` is interpreted according to `format`:
/// - `Json` / `MessagePack` — JSONPath-style ("$.user.ssn").
/// - `BinaryLengthPrefix`  — byte-offset expression ("0:16" for the first
///   16 bytes) resolved by the scanner at scan time.
///
/// This type is a descriptor only; evaluation lives in the S3 / S5 / S6
/// consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiiSelector {
    pub path: String,
    #[serde(default)]
    pub format: PayloadFormat,
    pub class: DataClass,
}

// ── Erasure policy (S6.8) ────────────────────────────────────────────────

/// S6.8 — per-pipeline erasure policy. Applies when [`ComplianceRegime`] is
/// `Gdpr` or `Mixed`; ignored otherwise.
///
/// The GDPR right-to-erasure SLA is 30 days (Art. 17). A naïve compaction
/// strategy that only runs when a segment fills up can easily miss that
/// window on an idle partition. [`ErasureConfig::max_delay_hours`] bounds the
/// wall-clock time any pending tombstone is allowed to sit unprocessed —
/// once exceeded, the engine forces a compaction sweep regardless of segment
/// size. Default is 24h, which leaves 29 days of slack before the SLA is in
/// jeopardy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErasureConfig {
    /// Maximum age a `Pending` tombstone may reach before forcing a
    /// compaction sweep. Default 24h.
    pub max_delay_hours: u32,
}

impl Default for ErasureConfig {
    fn default() -> Self {
        Self {
            max_delay_hours: DEFAULT_ERASURE_MAX_DELAY_HOURS,
        }
    }
}

impl ErasureConfig {
    fn is_default(&self) -> bool {
        self.max_delay_hours == DEFAULT_ERASURE_MAX_DELAY_HOURS
    }
}

/// Default wall-clock cap on pending-tombstone age. See [`ErasureConfig`].
pub const DEFAULT_ERASURE_MAX_DELAY_HOURS: u32 = 24;

// ── Pipeline-level block ─────────────────────────────────────────────────

/// Per-pipeline compliance configuration. Slots into `PipelineManifest`
/// alongside `DurabilityBlock`. Defaults are safe for dev (`regime=None`,
/// `enforcement=Off`, no selectors) and therefore add no runtime cost when
/// not explicitly set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComplianceBlock {
    #[serde(default)]
    pub regime: ComplianceRegime,

    #[serde(default)]
    pub enforcement: EnforcementLevel,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selectors: Vec<PiiSelector>,

    /// S6.8 — erasure compaction policy. Applied only when `regime`
    /// requires erasure (GDPR / Mixed).
    #[serde(default, skip_serializing_if = "ErasureConfig::is_default")]
    pub erasure: ErasureConfig,
}

impl ComplianceBlock {
    /// `true` when the block will contribute any pipeline-start checks.
    /// Used by the engine to skip the validator cheaply when compliance is
    /// not configured.
    pub fn is_active(&self) -> bool {
        self.enforcement.is_active() && !matches!(self.regime, ComplianceRegime::None)
    }

    /// S4.3 — cheap client-side shape validation. Runs before a manifest
    /// leaves the CLI so operators see structural mistakes immediately
    /// rather than at pipeline start on the server. Distinct from S4.2
    /// `validate_compliance`, which cross-references resolved plans from
    /// S3 / S5 / S6 and therefore must run inside the engine.
    ///
    /// Checks:
    /// - selector `path` is non-empty (the regex/jsonpath evaluator will
    ///   reject it anyway, but the error there is less actionable).
    /// - `erasure.max_delay_hours > 0` — zero would mean "sweep every
    ///   check", swamping the engine; we cap the floor at 1h to force a
    ///   deliberate choice.
    /// - `regime = Gdpr | Mixed` + `enforcement = Strict` is logically
    ///   consistent with `erasure.max_delay_hours <= 24 * 30` (GDPR's
    ///   30-day SLA). A larger cap is refused because strict enforcement
    ///   then cannot meet the SLA even under ideal conditions.
    pub fn validate_shape(&self) -> Result<(), crate::AeonError> {
        for (idx, s) in self.selectors.iter().enumerate() {
            if s.path.trim().is_empty() {
                return Err(crate::AeonError::state(format!(
                    "compliance.selectors[{idx}].path must not be empty"
                )));
            }
        }

        if self.erasure.max_delay_hours == 0 {
            return Err(crate::AeonError::state(
                "compliance.erasure.max_delay_hours must be > 0; pick a value >= 1".to_string(),
            ));
        }

        if self.regime.requires_erasure()
            && matches!(self.enforcement, EnforcementLevel::Strict)
            && self.erasure.max_delay_hours > 24 * 30
        {
            return Err(crate::AeonError::state(format!(
                "compliance.erasure.max_delay_hours={} cannot satisfy the GDPR \
                 30-day SLA under strict enforcement (cap: {} hours)",
                self.erasure.max_delay_hours,
                24 * 30
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_inert() {
        let b = ComplianceBlock::default();
        assert_eq!(b.regime, ComplianceRegime::None);
        assert_eq!(b.enforcement, EnforcementLevel::Off);
        assert!(b.selectors.is_empty());
        assert!(!b.is_active());
    }

    #[test]
    fn is_active_requires_regime_and_enforcement() {
        let mut b = ComplianceBlock {
            regime: ComplianceRegime::Pci,
            ..Default::default()
        };
        assert!(!b.is_active(), "Off enforcement stays inert");

        b.enforcement = EnforcementLevel::Warn;
        assert!(b.is_active());

        b.regime = ComplianceRegime::None;
        assert!(!b.is_active(), "regime=None stays inert");
    }

    #[test]
    fn regime_precondition_shape() {
        assert!(!ComplianceRegime::None.requires_encryption());
        assert!(!ComplianceRegime::None.requires_retention());
        assert!(!ComplianceRegime::None.requires_erasure());

        assert!(ComplianceRegime::Pci.requires_encryption());
        assert!(ComplianceRegime::Pci.requires_retention());
        assert!(!ComplianceRegime::Pci.requires_erasure());

        assert!(ComplianceRegime::Hipaa.requires_encryption());
        assert!(ComplianceRegime::Hipaa.requires_retention());
        assert!(!ComplianceRegime::Hipaa.requires_erasure());

        assert!(ComplianceRegime::Gdpr.requires_erasure());
        assert!(ComplianceRegime::Mixed.requires_erasure());
    }

    #[test]
    fn enforcement_level_flags() {
        assert!(!EnforcementLevel::Off.is_active());
        assert!(!EnforcementLevel::Off.is_blocking());

        assert!(EnforcementLevel::Warn.is_active());
        assert!(!EnforcementLevel::Warn.is_blocking());

        assert!(EnforcementLevel::Strict.is_active());
        assert!(EnforcementLevel::Strict.is_blocking());
    }

    #[test]
    fn regime_serialises_snake_case() {
        assert_eq!(
            serde_json::to_string(&ComplianceRegime::Pci).unwrap(),
            r#""pci""#
        );
        assert_eq!(
            serde_json::to_string(&ComplianceRegime::Hipaa).unwrap(),
            r#""hipaa""#
        );
        assert_eq!(
            serde_json::to_string(&ComplianceRegime::Gdpr).unwrap(),
            r#""gdpr""#
        );
        assert_eq!(
            serde_json::to_string(&ComplianceRegime::Mixed).unwrap(),
            r#""mixed""#
        );
        assert_eq!(
            serde_json::to_string(&ComplianceRegime::None).unwrap(),
            r#""none""#
        );
    }

    #[test]
    fn enforcement_serialises_snake_case() {
        assert_eq!(
            serde_json::to_string(&EnforcementLevel::Strict).unwrap(),
            r#""strict""#
        );
        assert_eq!(
            serde_json::to_string(&EnforcementLevel::Warn).unwrap(),
            r#""warn""#
        );
        assert_eq!(
            serde_json::to_string(&EnforcementLevel::Off).unwrap(),
            r#""off""#
        );
    }

    #[test]
    fn selector_roundtrip_json() {
        let s = PiiSelector {
            path: "$.user.ssn".into(),
            format: PayloadFormat::Json,
            class: DataClass::Pii,
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: PiiSelector = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn selector_roundtrip_binary_length_prefix() {
        let s = PiiSelector {
            path: "0:16".into(),
            format: PayloadFormat::BinaryLengthPrefix,
            class: DataClass::Phi,
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: PiiSelector = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn selector_format_defaults_to_json() {
        let j = r#"{"path":"$.x","class":"pii"}"#;
        let s: PiiSelector = serde_json::from_str(j).unwrap();
        assert_eq!(s.format, PayloadFormat::Json);
    }

    #[test]
    fn block_roundtrip_full() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Hipaa,
            enforcement: EnforcementLevel::Strict,
            selectors: vec![PiiSelector {
                path: "$.patient.dob".into(),
                format: PayloadFormat::MessagePack,
                class: DataClass::Phi,
            }],
            erasure: ErasureConfig::default(),
        };
        let j = serde_json::to_string(&b).unwrap();
        let back: ComplianceBlock = serde_json::from_str(&j).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn block_roundtrip_default_omits_empty_selectors() {
        let b = ComplianceBlock::default();
        let j = serde_json::to_string(&b).unwrap();
        assert!(!j.contains("selectors"));
    }

    // ── S6.8 ErasureConfig ──────────────────────────────────────────

    #[test]
    fn erasure_config_default_is_24h() {
        let c = ErasureConfig::default();
        assert_eq!(c.max_delay_hours, DEFAULT_ERASURE_MAX_DELAY_HOURS);
        assert_eq!(c.max_delay_hours, 24);
    }

    #[test]
    fn erasure_config_roundtrip() {
        let c = ErasureConfig {
            max_delay_hours: 72,
        };
        let j = serde_json::to_string(&c).unwrap();
        let back: ErasureConfig = serde_json::from_str(&j).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn block_default_omits_default_erasure() {
        let b = ComplianceBlock::default();
        let j = serde_json::to_string(&b).unwrap();
        assert!(
            !j.contains("erasure"),
            "default block should not serialize erasure key: {j}"
        );
    }

    #[test]
    fn block_emits_erasure_when_non_default() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Gdpr,
            enforcement: EnforcementLevel::Strict,
            selectors: vec![],
            erasure: ErasureConfig { max_delay_hours: 6 },
        };
        let j = serde_json::to_string(&b).unwrap();
        assert!(j.contains("\"erasure\""));
        assert!(j.contains("\"max_delay_hours\":6"));
        let back: ComplianceBlock = serde_json::from_str(&j).unwrap();
        assert_eq!(back.erasure.max_delay_hours, 6);
    }

    #[test]
    fn block_parses_without_erasure_key() {
        // Forward-compat with manifests that predate S6.8.
        let j = r#"{"regime":"gdpr","enforcement":"strict"}"#;
        let b: ComplianceBlock = serde_json::from_str(j).unwrap();
        assert_eq!(b.erasure.max_delay_hours, 24);
    }

    // ── S4.3 validate_shape ────────────────────────────────────────────

    #[test]
    fn validate_shape_accepts_default_block() {
        assert!(ComplianceBlock::default().validate_shape().is_ok());
    }

    #[test]
    fn validate_shape_accepts_well_formed_gdpr_strict() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Gdpr,
            enforcement: EnforcementLevel::Strict,
            selectors: vec![PiiSelector {
                path: "$.user.email".into(),
                format: PayloadFormat::Json,
                class: DataClass::Pii,
            }],
            erasure: ErasureConfig {
                max_delay_hours: 12,
            },
        };
        assert!(b.validate_shape().is_ok());
    }

    #[test]
    fn validate_shape_rejects_empty_selector_path() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Pci,
            enforcement: EnforcementLevel::Warn,
            selectors: vec![PiiSelector {
                path: "   ".into(),
                format: PayloadFormat::Json,
                class: DataClass::Pii,
            }],
            erasure: ErasureConfig::default(),
        };
        let err = b.validate_shape().unwrap_err();
        assert!(format!("{err}").contains("selectors[0].path"));
    }

    #[test]
    fn validate_shape_rejects_zero_erasure_delay() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Gdpr,
            enforcement: EnforcementLevel::Warn,
            selectors: vec![],
            erasure: ErasureConfig { max_delay_hours: 0 },
        };
        let err = b.validate_shape().unwrap_err();
        assert!(format!("{err}").contains("max_delay_hours"));
    }

    #[test]
    fn validate_shape_rejects_gdpr_strict_beyond_30d_sla() {
        let b = ComplianceBlock {
            regime: ComplianceRegime::Gdpr,
            enforcement: EnforcementLevel::Strict,
            selectors: vec![],
            erasure: ErasureConfig {
                max_delay_hours: 24 * 31,
            },
        };
        let err = b.validate_shape().unwrap_err();
        assert!(format!("{err}").contains("30-day SLA"));
    }

    #[test]
    fn validate_shape_permits_long_delay_under_warn() {
        // Warn is advisory — operator can choose a longer window without
        // the strict gate tripping.
        let b = ComplianceBlock {
            regime: ComplianceRegime::Gdpr,
            enforcement: EnforcementLevel::Warn,
            selectors: vec![],
            erasure: ErasureConfig {
                max_delay_hours: 24 * 31,
            },
        };
        assert!(b.validate_shape().is_ok());
    }

    #[test]
    fn validate_shape_permits_long_delay_for_non_erasure_regime() {
        // PCI / HIPAA don't require GDPR erasure; the SLA cap doesn't apply.
        let b = ComplianceBlock {
            regime: ComplianceRegime::Pci,
            enforcement: EnforcementLevel::Strict,
            selectors: vec![],
            erasure: ErasureConfig {
                max_delay_hours: 24 * 90,
            },
        };
        assert!(b.validate_shape().is_ok());
    }
}
