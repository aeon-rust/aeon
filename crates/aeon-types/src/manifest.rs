//! EO-2 Pipeline manifest schema — typed shape for YAML pipeline definitions.
//!
//! Covers §11 and §12 of `docs/EO-2-DURABILITY-DESIGN.md`:
//! multi-source + multi-sink lists, per-source `kind` / `identity` /
//! `event_time`, per-sink `eos_tier` declaration, pipeline-level
//! `durability` block.
//!
//! This module owns only the **schema** and **fail-fast validators**. The
//! aeon-cli YAML loader and aeon-engine runtime topology wiring consume
//! these types; neither lives here. Keeping the schema in `aeon-types`
//! lets connectors introspect their own declared shape without taking an
//! engine dependency.
//!
//! Validators:
//! - [`validate_sink_tier`] — declared `eos_tier` must match the connector's
//!   actual `TransactionalSink::eos_tier()` at pipeline start.
//! - [`validate_event_time`] — `event_time: broker` requires the connector
//!   to advertise `Source::supports_broker_event_time() == true`; otherwise
//!   the pipeline fails to start (no silent `AeonIngest` fallback).

use crate::durability::DurabilityMode;
use crate::error::AeonError;
use crate::event_time::EventTime;
use crate::traits::{SinkEosTier, SourceKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Pipeline root ────────────────────────────────────────────────────────

/// Top-level pipeline manifest. One entry per pipeline in the YAML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineManifest {
    pub name: String,

    /// Immutable after creation; drives per-partition L2 segment count.
    pub partitions: u16,

    #[serde(default)]
    pub durability: DurabilityBlock,

    pub sources: Vec<SourceManifest>,

    pub processor: ProcessorManifest,

    pub sinks: Vec<SinkManifest>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_strategy: Option<String>,
}

// ── Durability block ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DurabilityBlock {
    #[serde(default)]
    pub mode: DurabilityMode,

    /// Per-pipeline L2 cap; when unset inherits the node-global cap.
    /// Human-readable string at the YAML layer ("8Gi"); parsed to u64 by
    /// the CLI loader before it lands here. String form kept for now so
    /// round-trips stay lossless until the loader is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l2_max_bytes: Option<String>,

    #[serde(default)]
    pub checkpoint: CheckpointBlock,

    #[serde(default)]
    pub flush: FlushBlock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointBlock {
    #[serde(default = "default_checkpoint_backend")]
    pub backend: CheckpointBackendDecl,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<String>,
}

impl Default for CheckpointBlock {
    fn default() -> Self {
        Self {
            backend: default_checkpoint_backend(),
            interval: None,
            retention: None,
        }
    }
}

fn default_checkpoint_backend() -> CheckpointBackendDecl {
    CheckpointBackendDecl::StateStore
}

/// Declared checkpoint backend. `StateStore` is the L3-primary path; it
/// auto-falls-back to `Wal` on L3 error (see EO-2 §6.2 / eo2_recovery).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointBackendDecl {
    StateStore,
    Wal,
    None,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlushBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pending: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive: Option<bool>,
}

// ── Source manifest ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceManifest {
    pub name: String,

    /// Connector type key, e.g. `"kafka"`, `"webhook"`, `"http_polling"`.
    #[serde(rename = "type")]
    pub connector_type: String,

    pub kind: SourceKind,

    #[serde(default)]
    pub identity: IdentityConfig,

    #[serde(default)]
    pub event_time: EventTime,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backpressure: Option<BackpressureConfig>,

    /// Connector-specific free-form config. Validated by the connector
    /// during `open()`, not here.
    #[serde(default, flatten)]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// Identity derivation mode — see EO-2 §4.
///
/// `Native`   — pull connector uses broker-provided offset (Kafka, CDC).
/// `Random`   — push connector uses random UUIDv7 + sub-ms counter.
/// `Compound` — poll connector uses cursor + content-hash window.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum IdentityConfig {
    Native,
    Random,
    Compound {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<ContentHashConfig>,
    },
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self::Native
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentHashConfig {
    pub algorithm: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackpressureConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase1_buffer: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase3_threshold: Option<f32>,
}

// ── Processor manifest ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorManifest {
    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<serde_json::Value>,
}

// ── Sink manifest ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkManifest {
    pub name: String,

    #[serde(rename = "type")]
    pub connector_type: String,

    /// Declared EOS tier. Validated against the connector's actual
    /// `TransactionalSink::eos_tier()` at pipeline start by
    /// [`validate_sink_tier`].
    pub eos_tier: SinkTierDecl,

    #[serde(default, flatten)]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// YAML-facing tier declaration. Serialises using the stable
/// `t<N>_<slug>` form from the design doc (§12) so the manifest stays
/// readable by ops without consulting the trait docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkTierDecl {
    T1TransactionalDb,
    T2TransactionalStream,
    T3AtomicRenameFile,
    T4DedupKeyed,
    T5IdempotencyKey,
    T6FireAndForget,
}

impl SinkTierDecl {
    /// The actual trait-level tier this declaration corresponds to.
    pub const fn to_trait(self) -> SinkEosTier {
        match self {
            Self::T1TransactionalDb => SinkEosTier::TransactionalDb,
            Self::T2TransactionalStream => SinkEosTier::TransactionalStream,
            Self::T3AtomicRenameFile => SinkEosTier::AtomicRenameFile,
            Self::T4DedupKeyed => SinkEosTier::DedupKeyed,
            Self::T5IdempotencyKey => SinkEosTier::IdempotencyKey,
            Self::T6FireAndForget => SinkEosTier::FireAndForget,
        }
    }

    /// Inverse of [`to_trait`], for error construction and round-trips.
    pub const fn from_trait(tier: SinkEosTier) -> Self {
        match tier {
            SinkEosTier::TransactionalDb => Self::T1TransactionalDb,
            SinkEosTier::TransactionalStream => Self::T2TransactionalStream,
            SinkEosTier::AtomicRenameFile => Self::T3AtomicRenameFile,
            SinkEosTier::DedupKeyed => Self::T4DedupKeyed,
            SinkEosTier::IdempotencyKey => Self::T5IdempotencyKey,
            SinkEosTier::FireAndForget => Self::T6FireAndForget,
        }
    }
}

// ── Validators ───────────────────────────────────────────────────────────

/// Fail-fast: declared sink tier must match the connector's runtime tier.
///
/// Called once per sink at pipeline start, right after the connector is
/// constructed. Mismatch is a manifest bug — the operator wrote
/// `eos_tier: t2_transactional_stream` for a connector that actually
/// exposes `T4DedupKeyed`. The pipeline refuses to start so the mistake
/// surfaces loudly instead of silently downgrading EOS guarantees.
pub fn validate_sink_tier(
    sink_name: &str,
    declared: SinkTierDecl,
    actual: SinkEosTier,
) -> Result<(), AeonError> {
    if declared.to_trait() == actual {
        Ok(())
    } else {
        Err(AeonError::state(format!(
            "sink '{}' declared eos_tier={:?} but connector reports {:?}",
            sink_name,
            declared,
            SinkTierDecl::from_trait(actual),
        )))
    }
}

/// Fail-fast: `event_time: broker` requires the source connector to
/// advertise `Source::supports_broker_event_time() == true`. No silent
/// fallback to `AeonIngest` — the user must either switch the connector
/// or change `event_time` to `aeon_ingest` / `header:<name>` explicitly.
///
/// Also enforces the source-kind / identity pairing invariants from
/// §4 of the design doc:
/// - `Pull` + `Random` is nonsense (drops cross-replay determinism).
/// - `Push` + `Native` is nonsense (there is no broker offset).
/// - `Poll` + `Native` is nonsense (poll has no upstream replay position).
pub fn validate_source_shape(
    source_name: &str,
    kind: SourceKind,
    identity: &IdentityConfig,
    event_time: &EventTime,
    supports_broker: bool,
) -> Result<(), AeonError> {
    if matches!(event_time, EventTime::Broker) && !supports_broker {
        return Err(AeonError::state(format!(
            "source '{source_name}' has event_time=broker but connector does not supply broker timestamps; set event_time to aeon_ingest or header:<name>"
        )));
    }

    match (kind, identity) {
        (SourceKind::Pull, IdentityConfig::Native) => Ok(()),
        (SourceKind::Push, IdentityConfig::Random) => Ok(()),
        (SourceKind::Poll, IdentityConfig::Compound { .. }) => Ok(()),
        (SourceKind::Poll, IdentityConfig::Random) => Ok(()),
        (k, id) => Err(AeonError::state(format!(
            "source '{source_name}' kind={k:?} is incompatible with identity={id:?}"
        ))),
    }
}

/// Pipeline-level structural validation. Call after the manifest has been
/// parsed and before any connector is constructed.
pub fn validate_pipeline_shape(m: &PipelineManifest) -> Result<(), AeonError> {
    if m.name.trim().is_empty() {
        return Err(AeonError::state("pipeline name must be non-empty"));
    }
    if m.partitions == 0 {
        return Err(AeonError::state(format!(
            "pipeline '{}' partitions must be > 0",
            m.name
        )));
    }
    if m.sources.is_empty() {
        return Err(AeonError::state(format!(
            "pipeline '{}' must declare at least one source",
            m.name
        )));
    }
    if m.sinks.is_empty() {
        return Err(AeonError::state(format!(
            "pipeline '{}' must declare at least one sink",
            m.name
        )));
    }

    // Unique names within each list.
    let mut seen = std::collections::HashSet::new();
    for s in &m.sources {
        if !seen.insert(s.name.as_str()) {
            return Err(AeonError::state(format!(
                "pipeline '{}' has duplicate source name '{}'",
                m.name, s.name
            )));
        }
    }
    seen.clear();
    for s in &m.sinks {
        if !seen.insert(s.name.as_str()) {
            return Err(AeonError::state(format!(
                "pipeline '{}' has duplicate sink name '{}'",
                m.name, s.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_source() -> SourceManifest {
        SourceManifest {
            name: "orders-kafka".into(),
            connector_type: "kafka".into(),
            kind: SourceKind::Pull,
            identity: IdentityConfig::Native,
            event_time: EventTime::Broker,
            backpressure: None,
            config: BTreeMap::new(),
        }
    }

    fn sample_sink(tier: SinkTierDecl) -> SinkManifest {
        SinkManifest {
            name: "enriched-kafka".into(),
            connector_type: "kafka".into(),
            eos_tier: tier,
            config: BTreeMap::new(),
        }
    }

    fn sample_manifest() -> PipelineManifest {
        PipelineManifest {
            name: "orders".into(),
            partitions: 8,
            durability: DurabilityBlock::default(),
            sources: vec![sample_source()],
            processor: ProcessorManifest {
                name: "p".into(),
                version: None,
                tier: None,
                runtime: None,
            },
            sinks: vec![sample_sink(SinkTierDecl::T2TransactionalStream)],
            upgrade_strategy: None,
        }
    }

    #[test]
    fn tier_roundtrip_through_trait() {
        for d in [
            SinkTierDecl::T1TransactionalDb,
            SinkTierDecl::T2TransactionalStream,
            SinkTierDecl::T3AtomicRenameFile,
            SinkTierDecl::T4DedupKeyed,
            SinkTierDecl::T5IdempotencyKey,
            SinkTierDecl::T6FireAndForget,
        ] {
            assert_eq!(SinkTierDecl::from_trait(d.to_trait()), d);
        }
    }

    #[test]
    fn tier_match_passes() {
        assert!(validate_sink_tier(
            "s",
            SinkTierDecl::T2TransactionalStream,
            SinkEosTier::TransactionalStream
        )
        .is_ok());
    }

    #[test]
    fn tier_mismatch_fails_with_both_sides_in_message() {
        let err = validate_sink_tier(
            "enriched-kafka",
            SinkTierDecl::T2TransactionalStream,
            SinkEosTier::DedupKeyed,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("enriched-kafka"));
        assert!(msg.contains("T2TransactionalStream"));
        assert!(msg.contains("T4DedupKeyed"));
    }

    #[test]
    fn broker_time_without_support_fails() {
        let err = validate_source_shape(
            "webhook",
            SourceKind::Push,
            &IdentityConfig::Random,
            &EventTime::Broker,
            false,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("event_time=broker"));
    }

    #[test]
    fn broker_time_with_support_passes() {
        assert!(validate_source_shape(
            "kafka",
            SourceKind::Pull,
            &IdentityConfig::Native,
            &EventTime::Broker,
            true
        )
        .is_ok());
    }

    #[test]
    fn header_time_never_needs_broker_support() {
        assert!(validate_source_shape(
            "webhook",
            SourceKind::Push,
            &IdentityConfig::Random,
            &EventTime::Header {
                name: "X-Ts".into()
            },
            false
        )
        .is_ok());
    }

    #[test]
    fn push_with_native_identity_rejected() {
        let err = validate_source_shape(
            "webhook",
            SourceKind::Push,
            &IdentityConfig::Native,
            &EventTime::AeonIngest,
            false,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("incompatible"));
    }

    #[test]
    fn pull_with_random_identity_rejected() {
        let err = validate_source_shape(
            "kafka",
            SourceKind::Pull,
            &IdentityConfig::Random,
            &EventTime::Broker,
            true,
        )
        .unwrap_err();
        assert!(format!("{err}").contains("incompatible"));
    }

    #[test]
    fn poll_with_compound_identity_ok() {
        assert!(validate_source_shape(
            "weather",
            SourceKind::Poll,
            &IdentityConfig::Compound {
                cursor_path: Some("$.cursor".into()),
                content_hash: None,
            },
            &EventTime::AeonIngest,
            false,
        )
        .is_ok());
    }

    #[test]
    fn pipeline_shape_requires_sources_and_sinks() {
        let mut m = sample_manifest();
        m.sources.clear();
        assert!(validate_pipeline_shape(&m).is_err());

        let mut m = sample_manifest();
        m.sinks.clear();
        assert!(validate_pipeline_shape(&m).is_err());
    }

    #[test]
    fn pipeline_shape_rejects_zero_partitions() {
        let mut m = sample_manifest();
        m.partitions = 0;
        assert!(validate_pipeline_shape(&m).is_err());
    }

    #[test]
    fn pipeline_shape_rejects_duplicate_source_names() {
        let mut m = sample_manifest();
        m.sources.push(sample_source());
        let err = validate_pipeline_shape(&m).unwrap_err();
        assert!(format!("{err}").contains("duplicate source"));
    }

    #[test]
    fn pipeline_shape_rejects_duplicate_sink_names() {
        let mut m = sample_manifest();
        m.sinks
            .push(sample_sink(SinkTierDecl::T2TransactionalStream));
        let err = validate_pipeline_shape(&m).unwrap_err();
        assert!(format!("{err}").contains("duplicate sink"));
    }

    #[test]
    fn pipeline_shape_happy_path() {
        assert!(validate_pipeline_shape(&sample_manifest()).is_ok());
    }

    // ── YAML-adjacent round-trips via serde_json ────────────────────────

    #[test]
    fn source_kind_serialises_snake_case() {
        assert_eq!(
            serde_json::to_string(&SourceKind::Pull).unwrap(),
            r#""pull""#
        );
        assert_eq!(
            serde_json::to_string(&SourceKind::Push).unwrap(),
            r#""push""#
        );
        assert_eq!(
            serde_json::to_string(&SourceKind::Poll).unwrap(),
            r#""poll""#
        );
    }

    #[test]
    fn tier_decl_serialises_with_t_prefix() {
        let j = serde_json::to_string(&SinkTierDecl::T2TransactionalStream).unwrap();
        assert_eq!(j, r#""t2_transactional_stream""#);
    }

    #[test]
    fn identity_mode_serde_shape() {
        let j = serde_json::to_string(&IdentityConfig::Native).unwrap();
        assert_eq!(j, r#"{"mode":"native"}"#);
        let j = serde_json::to_string(&IdentityConfig::Compound {
            cursor_path: Some("$.x".into()),
            content_hash: None,
        })
        .unwrap();
        let back: IdentityConfig = serde_json::from_str(&j).unwrap();
        matches!(back, IdentityConfig::Compound { .. });
    }

    #[test]
    fn manifest_json_roundtrip() {
        let m = sample_manifest();
        let j = serde_json::to_string(&m).unwrap();
        let back: PipelineManifest = serde_json::from_str(&j).unwrap();
        assert_eq!(back.name, m.name);
        assert_eq!(back.sources.len(), 1);
        assert_eq!(back.sinks.len(), 1);
        assert_eq!(back.sinks[0].eos_tier, SinkTierDecl::T2TransactionalStream);
    }

    #[test]
    fn durability_block_defaults_to_none_mode() {
        let d = DurabilityBlock::default();
        assert_eq!(d.mode, DurabilityMode::None);
        assert_eq!(d.checkpoint.backend, CheckpointBackendDecl::StateStore);
    }
}
