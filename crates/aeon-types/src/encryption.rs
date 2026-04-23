//! S3 — At-rest encryption manifest primitives.
//!
//! Declares a pipeline's preference for L2 segment + L3 value encryption.
//! Drives the selection of the data-context KEK at pipeline start; actual
//! AES-256-GCM wrapping lives in `aeon-crypto::at_rest` and is invoked
//! from the L2 body store (`aeon-engine`) and an L3 store wrapper
//! (`aeon-state`).
//!
//! Three modes:
//!
//! - `Off` — no encryption. Dev default; must be overridden for any
//!   pipeline under a compliance regime (S4.2 validator hard-fails when
//!   `regime != None && at_rest == Off`).
//! - `Optional` — encrypt when a data-context KEK is available; warn and
//!   continue plaintext when it is not. Useful during a staged rollout
//!   where some nodes have Vault wired and others don't yet.
//! - `Required` — pipeline refuses to start if no data-context KEK is
//!   resolvable. Production default for anything carrying real data.

use serde::{Deserialize, Serialize};

/// At-rest encryption posture for L2 segment bodies and L3 stored values.
///
/// Semantic model mirrors `DurabilityMode` — a plain three-valued enum
/// at the YAML surface, with the policy gradient baked into the variant
/// names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtRestEncryption {
    #[default]
    Off,
    Optional,
    Required,
}

impl AtRestEncryption {
    /// `true` when the pipeline MUST encrypt or refuse to start. Used by
    /// the pipeline-start validator to decide whether a missing KEK is a
    /// hard gate or a warn-and-continue.
    pub const fn is_required(self) -> bool {
        matches!(self, Self::Required)
    }

    /// `true` when the pipeline will attempt to encrypt if a KEK is
    /// available. `Off` returns `false`; `Optional` / `Required` both
    /// return `true`.
    pub const fn is_attempted(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Per-pipeline encryption declaration. Lives alongside `DurabilityBlock`
/// and `ComplianceBlock` on `PipelineManifest`. Defaults to an inert
/// block (`at_rest=Off`) so development pipelines pay zero cost and the
/// YAML surface stays minimal.
///
/// # KEK domain
///
/// At-rest encryption always binds to the S1 **data-context KEK**
/// (distinct from the log-context KEK used by the redaction layer). The
/// selection happens at pipeline start based on the secret-provider
/// configuration — this block only declares posture, not key material.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionBlock {
    #[serde(default)]
    pub at_rest: AtRestEncryption,
}

impl EncryptionBlock {
    /// `true` when the block will cause any runtime encryption work to
    /// happen. Used by pipeline startup to skip KEK resolution entirely
    /// when encryption is off.
    pub fn is_active(&self) -> bool {
        self.at_rest.is_attempted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_off_and_inert() {
        let b = EncryptionBlock::default();
        assert_eq!(b.at_rest, AtRestEncryption::Off);
        assert!(!b.is_active());
        assert!(!b.at_rest.is_required());
        assert!(!b.at_rest.is_attempted());
    }

    #[test]
    fn optional_attempts_but_not_required() {
        let m = AtRestEncryption::Optional;
        assert!(m.is_attempted());
        assert!(!m.is_required());
    }

    #[test]
    fn required_is_attempted_and_required() {
        let m = AtRestEncryption::Required;
        assert!(m.is_attempted());
        assert!(m.is_required());
    }

    #[test]
    fn at_rest_serialises_snake_case() {
        assert_eq!(
            serde_json::to_string(&AtRestEncryption::Off).unwrap(),
            r#""off""#
        );
        assert_eq!(
            serde_json::to_string(&AtRestEncryption::Optional).unwrap(),
            r#""optional""#
        );
        assert_eq!(
            serde_json::to_string(&AtRestEncryption::Required).unwrap(),
            r#""required""#
        );
    }

    #[test]
    fn encryption_block_roundtrip_default_is_compact() {
        let b = EncryptionBlock::default();
        let j = serde_json::to_string(&b).unwrap();
        // `at_rest` is serialised even at default because the enum has no
        // skip rule — this is fine; it stays inside the block and the
        // manifest-level `skip_serializing_if` keeps the block itself out
        // of the top-level JSON when it is default.
        let back: EncryptionBlock = serde_json::from_str(&j).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn encryption_block_roundtrip_required() {
        let b = EncryptionBlock {
            at_rest: AtRestEncryption::Required,
        };
        let j = serde_json::to_string(&b).unwrap();
        assert!(j.contains("required"));
        let back: EncryptionBlock = serde_json::from_str(&j).unwrap();
        assert_eq!(back, b);
    }
}
