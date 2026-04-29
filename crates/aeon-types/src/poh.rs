//! S2 / V5 — Proof-of-History pipeline declaration.
//!
//! Per-pipeline opt-in for the SHA-512 hash chain + Merkle tree of payloads
//! the processor task appends to during batch processing. When `enabled`,
//! the engine constructs a `PohConfig` at pipeline start and
//! `create_poh_state` produces a per-partition `PohChain`. The
//! `/api/v1/pipelines/{name}/partitions/{partition}/poh-head` REST
//! endpoint exposes the chain head for verification (`Verify`,
//! `VerifyWithKey`, `TrustExtend` walks).
//!
//! Lives alongside [`crate::DurabilityBlock`], [`crate::EncryptionBlock`],
//! and [`crate::ComplianceBlock`] on [`crate::PipelineManifest`] as a peer
//! cross-cutting block. Defaults to inert (`enabled = false`) so existing
//! manifests are byte-equivalent.
//!
//! Signing-key resolution is deliberately a separate atom: this block
//! only declares whether PoH is enabled and how much in-memory history
//! to keep. `signing_key_ref` is preserved through the schema today but
//! not consumed by the engine; resolution lands when V5's signed-Merkle
//! verify walks pull a key from the secret-provider layer.

use serde::{Deserialize, Serialize};

/// Per-pipeline Proof-of-History declaration.
///
/// Defaults to `enabled = false` so pipelines opt in explicitly and pay
/// zero PoH cost otherwise. When `enabled` is `true`, the engine wires a
/// PoH chain into the processor task at pipeline start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PohBlock {
    /// Master switch. When `false` (default), the pipeline runs without a
    /// PoH chain and `PipelineConfig.poh` stays `None`.
    #[serde(default)]
    pub enabled: bool,

    /// Maximum number of recent PoH entries kept in memory for verification.
    /// Older entries remain tracked in the MMR (Merkle Mountain Range), so
    /// shrinking this value trades RAM for verifier round-trips.
    /// Matches the default in `aeon-engine::pipeline::PohConfig`.
    #[serde(default = "default_max_recent_entries")]
    pub max_recent_entries: u32,

    /// Symbolic reference to a signing key in the secret-provider layer.
    /// Reserved for the V5 signed-Merkle-root walk; preserved through
    /// the schema today but not consumed by the engine yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key_ref: Option<String>,
}

fn default_max_recent_entries() -> u32 {
    1024
}

impl Default for PohBlock {
    fn default() -> Self {
        Self {
            enabled: false,
            max_recent_entries: default_max_recent_entries(),
            signing_key_ref: None,
        }
    }
}

impl PohBlock {
    /// `true` when the block will cause runtime PoH work to happen.
    /// Engine startup uses this to skip chain construction entirely
    /// when PoH is off.
    pub const fn is_active(&self) -> bool {
        self.enabled
    }

    /// `true` when the block carries no non-default values; the
    /// `PipelineManifest`/`PipelineDefinition` `skip_serializing_if`
    /// hooks consult this so disabled pipelines round-trip without
    /// growing the YAML/JSON surface.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled_and_inert() {
        let b = PohBlock::default();
        assert!(!b.enabled);
        assert!(!b.is_active());
        assert!(b.is_default());
        assert_eq!(b.max_recent_entries, 1024);
        assert!(b.signing_key_ref.is_none());
    }

    #[test]
    fn enabled_is_active_and_not_default() {
        let b = PohBlock {
            enabled: true,
            ..PohBlock::default()
        };
        assert!(b.is_active());
        assert!(!b.is_default());
    }

    // The aeon-types crate ships serde_json but not serde_yaml; the YAML
    // pass happens in the aeon-cli loader. Round-trip via serde_json which
    // exercises the same serde derive paths.
    #[test]
    fn json_round_trip_disabled_preserves_default() {
        let b = PohBlock::default();
        let v = serde_json::to_value(&b).unwrap();
        let parsed: PohBlock = serde_json::from_value(v).unwrap();
        assert_eq!(parsed, b);
    }

    #[test]
    fn json_round_trip_enabled_preserves_fields() {
        let b = PohBlock {
            enabled: true,
            max_recent_entries: 4096,
            signing_key_ref: Some("data-context/v1".into()),
        };
        let s = serde_json::to_string(&b).unwrap();
        let parsed: PohBlock = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, b);
    }

    #[test]
    fn json_partial_block_fills_defaults() {
        // Only `enabled` set; max_recent_entries should fall to default.
        let parsed: PohBlock = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.max_recent_entries, 1024);
        assert!(parsed.signing_key_ref.is_none());
    }

    #[test]
    fn json_skip_serializing_signing_key_ref_when_none() {
        // `signing_key_ref` should be omitted from the serialized form
        // when None so disabled blocks don't grow the on-disk surface.
        let b = PohBlock::default();
        let s = serde_json::to_string(&b).unwrap();
        assert!(!s.contains("signing_key_ref"), "got {s}");
    }
}
