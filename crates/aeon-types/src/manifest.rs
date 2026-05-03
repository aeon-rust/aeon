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

use crate::compliance::ComplianceBlock;
use crate::durability::DurabilityMode;
use crate::encryption::EncryptionBlock;
use crate::error::AeonError;
use crate::event_time::EventTime;
use crate::poh::PohBlock;
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

    /// S4 compliance declaration. Defaults to an inert block
    /// (`regime=None`, `enforcement=Off`) so pipelines opt in explicitly.
    #[serde(default, skip_serializing_if = "compliance_is_default")]
    pub compliance: ComplianceBlock,

    /// S3 at-rest encryption declaration. Defaults to an inert block
    /// (`at_rest=Off`) so dev pipelines pay zero cost.
    #[serde(default, skip_serializing_if = "encryption_is_default")]
    pub encryption: EncryptionBlock,

    /// V5 — Proof-of-History declaration. Defaults to an inert block
    /// (`enabled=false`) so pipelines opt in explicitly. When enabled,
    /// the engine wires a `PipelineConfig.poh` and the PoH chain head
    /// becomes accessible at
    /// `/api/v1/pipelines/{name}/partitions/{partition}/poh-head`.
    #[serde(default, skip_serializing_if = "PohBlock::is_default")]
    pub poh: PohBlock,

    pub sources: Vec<SourceManifest>,

    pub processor: ProcessorManifest,

    pub sinks: Vec<SinkManifest>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_strategy: Option<String>,
}

fn compliance_is_default(b: &ComplianceBlock) -> bool {
    b == &ComplianceBlock::default()
}

fn encryption_is_default(b: &EncryptionBlock) -> bool {
    b == &EncryptionBlock::default()
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

    /// S5: optional per-tier retention overrides. Defaults to inert
    /// (ack-driven GC only, keep-all checkpoint history) so existing
    /// manifests behave identically.
    #[serde(
        default,
        skip_serializing_if = "crate::retention::RetentionBlock::is_default"
    )]
    pub retention: crate::retention::RetentionBlock,
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

    /// S6.7 — optional GDPR subject-id extraction rule. When present,
    /// the source populates `Event.metadata["aeon.subject_id"]`
    /// before the event enters the pipeline. See
    /// [`SubjectExtractConfig`]. `None` means the source emits
    /// events without a subject id (acceptable for pipelines that do
    /// not declare `compliance.regime = gdpr|mixed`, or for sources
    /// where the upstream producer sets the metadata itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_extract: Option<SubjectExtractConfig>,

    /// Connector-specific free-form config. Validated by the connector
    /// during `open()`, not here.
    #[serde(default, flatten)]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// S6.7 — how a source connector derives the `aeon.subject_id`
/// metadata entry for each event. Three extraction kinds, matching
/// the design decision at `docs/aeon-dev-notes.txt` §6.1.d
/// (source-side extraction is the default; processor-side is also
/// supported by simply not configuring a source-side rule).
///
/// The extraction itself runs inside the connector; this enum is the
/// **schema only** — validation lives here, the extraction logic
/// lives in `aeon-connectors` and is wired in a separate sub-atom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubjectExtractConfig {
    /// Read a field from the payload using a JSON pointer / path
    /// expression (e.g. `$.user.id`). The extracted string is
    /// combined with `namespace` to form the canonical
    /// `namespace/id` subject id.
    JsonPath {
        /// Namespace component of the emitted subject id. Must match
        /// the same character rules as [`crate::SubjectId`]
        /// namespaces.
        namespace: String,
        /// JSON path expression. Dialect supported by the connector
        /// (JSONPath-plus or RFC 6901 pointer, connector-specific).
        path: String,
    },
    /// Read the subject id from a wire-protocol header / metadata
    /// key set by the upstream producer (e.g. an HTTP request
    /// header, a Kafka record header). The value is treated as the
    /// id component — `namespace` is provided by config.
    Header {
        /// Namespace component of the emitted subject id.
        namespace: String,
        /// Name of the header / metadata key to read.
        name: String,
    },
    /// The upstream producer already attaches a well-formed
    /// `aeon.subject_id` metadata entry with canonical
    /// `namespace/id` form. The source copies it through unchanged.
    /// No connector-side parsing is required; this variant is a
    /// declaration that the pipeline trusts its upstream to emit
    /// subject-ids correctly.
    MetadataLiteral,
}

impl SubjectExtractConfig {
    /// Validate config components. Namespace validation reuses the
    /// subject-id rules so `$extract` → `SubjectId` cannot fail at
    /// runtime if the config passed here; similarly, `path` and
    /// `name` must be non-empty.
    pub fn validate(&self) -> Result<(), crate::AeonError> {
        use crate::subject_id::validate_namespace_for_wildcard;
        match self {
            Self::JsonPath { namespace, path } => {
                validate_namespace_for_wildcard(namespace)?;
                if path.trim().is_empty() {
                    return Err(crate::AeonError::config(
                        "subject_extract.json_path: path must not be empty".to_string(),
                    ));
                }
                Ok(())
            }
            Self::Header { namespace, name } => {
                validate_namespace_for_wildcard(namespace)?;
                if name.trim().is_empty() {
                    return Err(crate::AeonError::config(
                        "subject_extract.header: name must not be empty".to_string(),
                    ));
                }
                Ok(())
            }
            Self::MetadataLiteral => Ok(()),
        }
    }
}

/// Identity derivation mode — see EO-2 §4.
///
/// `Native`   — pull connector uses broker-provided offset (Kafka, CDC).
/// `Random`   — push connector uses random UUIDv7 + sub-ms counter.
/// `Compound` — poll connector uses cursor + content-hash window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum IdentityConfig {
    #[default]
    Native,
    Random,
    Compound {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<ContentHashConfig>,
    },
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

    // S4.3 — client-side compliance block sanity check.
    m.compliance
        .validate_shape()
        .map_err(|e| AeonError::state(format!("pipeline '{}': {e}", m.name)))?;

    Ok(())
}

// ── Manifest → PipelineDefinition bridge ────────────────────────────────

impl PipelineManifest {
    /// Convert an EO-2 manifest into a legacy `PipelineDefinition` suitable
    /// for the REST API and pipeline registry. Uses the first source and
    /// first sink for the single-source/single-sink fields.
    ///
    /// This is a lossy bridge — multi-source/multi-sink information beyond
    /// the first entry is stored in the `sources_extra` / `sinks_extra`
    /// config overrides until the registry schema is upgraded in a future
    /// phase. The important thing is that the typed manifest has already
    /// been validated before this conversion runs.
    pub fn to_pipeline_definition(&self) -> crate::registry::PipelineDefinition {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let sources: Vec<crate::registry::SourceConfig> = self
            .sources
            .iter()
            .map(|s| {
                let mut config = std::collections::BTreeMap::new();
                for (k, v) in &s.config {
                    config.insert(k.clone(), v.to_string().trim_matches('"').to_string());
                }
                config.insert("source_kind".into(), format!("{:?}", s.kind));
                config.insert("identity".into(), format!("{:?}", s.identity));
                config.insert("event_time".into(), format!("{:?}", s.event_time));
                crate::registry::SourceConfig {
                    source_type: s.connector_type.clone(),
                    topic: config.remove("topic"),
                    partitions: vec![],
                    config,
                }
            })
            .collect();

        let sinks: Vec<crate::registry::SinkConfig> = self
            .sinks
            .iter()
            .map(|s| {
                let mut config = std::collections::BTreeMap::new();
                for (k, v) in &s.config {
                    config.insert(k.clone(), v.to_string().trim_matches('"').to_string());
                }
                config.insert("eos_tier".into(), format!("{:?}", s.eos_tier));
                crate::registry::SinkConfig {
                    sink_type: s.connector_type.clone(),
                    topic: config.remove("topic"),
                    config,
                }
            })
            .collect();

        let mut processor = crate::registry::ProcessorRef::new(
            &self.processor.name,
            self.processor.version.as_deref().unwrap_or("latest"),
        );
        if let Some(tier) = self.processor.tier.as_ref() {
            processor = processor.with_tier(tier.clone());
        }

        crate::registry::PipelineDefinition {
            name: self.name.clone(),
            sources,
            processor,
            sinks,
            upgrade_strategy: crate::registry::UpgradeStrategy::default(),
            state: crate::registry::PipelineState::Created,
            created_at: now,
            updated_at: now,
            assigned_node: None,
            transport_codec: crate::transport_codec::TransportCodec::default(),
            upgrade_state: None,
            durability: self.durability.clone(),
            encryption: self.encryption.clone(),
            compliance: self.compliance.clone(),
            poh: self.poh.clone(),
        }
    }
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
            subject_extract: None,
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
            compliance: ComplianceBlock::default(),
            encryption: EncryptionBlock::default(),
            poh: PohBlock::default(),
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
        assert!(
            validate_sink_tier(
                "s",
                SinkTierDecl::T2TransactionalStream,
                SinkEosTier::TransactionalStream
            )
            .is_ok()
        );
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
        assert!(
            validate_source_shape(
                "kafka",
                SourceKind::Pull,
                &IdentityConfig::Native,
                &EventTime::Broker,
                true
            )
            .is_ok()
        );
    }

    #[test]
    fn header_time_never_needs_broker_support() {
        assert!(
            validate_source_shape(
                "webhook",
                SourceKind::Push,
                &IdentityConfig::Random,
                &EventTime::Header {
                    name: "X-Ts".into()
                },
                false
            )
            .is_ok()
        );
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
        assert!(
            validate_source_shape(
                "weather",
                SourceKind::Poll,
                &IdentityConfig::Compound {
                    cursor_path: Some("$.cursor".into()),
                    content_hash: None,
                },
                &EventTime::AeonIngest,
                false,
            )
            .is_ok()
        );
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

    #[test]
    fn pipeline_shape_rejects_malformed_compliance_block() {
        use crate::compliance::{
            ComplianceRegime, DataClass, EnforcementLevel, ErasureConfig, PayloadFormat,
            PiiSelector,
        };
        let mut m = sample_manifest();
        m.compliance = ComplianceBlock {
            regime: ComplianceRegime::Pci,
            enforcement: EnforcementLevel::Warn,
            selectors: vec![PiiSelector {
                path: "".into(), // empty — validate_shape should reject
                format: PayloadFormat::Json,
                class: DataClass::Pii,
            }],
            erasure: ErasureConfig::default(),
        };
        let err = validate_pipeline_shape(&m).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("selectors[0].path"),
            "expected selector error: {msg}"
        );
        assert!(
            msg.contains("'orders'"),
            "expected pipeline name prefix: {msg}"
        );
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

    /// Empirical proof that `#[serde(flatten)]` + `BTreeMap<String, Value>`
    /// on `SourceManifest::config` requires connector keys at the **source
    /// top level**, NOT nested under a literal `config:` block. Nesting
    /// collapses the whole sub-object as a single entry `("config", …)`
    /// and downstream factory lookups miss every field.
    ///
    /// Found the hard way during the 2026-04-24 Gate 2 Pre-Session-B
    /// T0 run — the `count: "0"` unbounded escape didn't reach the
    /// Memory source because the fixture used the aspirational
    /// nested-block shape from `docs/EO-2-DURABILITY-DESIGN.md`.
    /// This test locks the actual wiring in place so anyone writing
    /// fresh fixtures against the design doc sees the regression
    /// here first.
    #[test]
    fn source_config_keys_must_be_flat_not_nested() {
        // Use JSON (aeon-types ships serde_json, not serde_yaml) — the
        // flatten behaviour is format-agnostic, YAML parses through the
        // same code path.
        let nested = r#"{
            "name": "test",
            "type": "memory",
            "kind": "push",
            "config": {
                "count": "0",
                "payload_size": "256"
            }
        }"#;
        let flat = r#"{
            "name": "test",
            "type": "memory",
            "kind": "push",
            "count": "0",
            "payload_size": "256"
        }"#;
        let n: SourceManifest = serde_json::from_str(nested).unwrap();
        let f: SourceManifest = serde_json::from_str(flat).unwrap();

        // Nested: one entry keyed `"config"` holding the sub-object.
        assert_eq!(n.config.len(), 1);
        assert!(n.config.contains_key("config"));
        assert!(!n.config.contains_key("count"));

        // Flat: two entries, both reachable by their intended keys.
        assert_eq!(f.config.len(), 2);
        assert_eq!(f.config.get("count").and_then(|v| v.as_str()), Some("0"));
        assert_eq!(
            f.config.get("payload_size").and_then(|v| v.as_str()),
            Some("256")
        );
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
    fn compliance_defaults_to_inert_and_is_omitted_from_json() {
        let m = sample_manifest();
        assert_eq!(m.compliance, ComplianceBlock::default());
        let j = serde_json::to_string(&m).unwrap();
        assert!(
            !j.contains("compliance"),
            "inert compliance block should be skipped, got: {j}"
        );
    }

    #[test]
    fn encryption_defaults_to_inert_and_is_omitted_from_json() {
        let m = sample_manifest();
        assert_eq!(m.encryption, EncryptionBlock::default());
        let j = serde_json::to_string(&m).unwrap();
        assert!(
            !j.contains("encryption"),
            "inert encryption block should be skipped, got: {j}"
        );
    }

    #[test]
    fn encryption_roundtrip_when_required() {
        use crate::encryption::AtRestEncryption;
        let mut m = sample_manifest();
        m.encryption.at_rest = AtRestEncryption::Required;
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains(r#""encryption""#));
        assert!(j.contains(r#""required""#));
        let back: PipelineManifest = serde_json::from_str(&j).unwrap();
        assert_eq!(back.encryption.at_rest, AtRestEncryption::Required);
    }

    #[test]
    fn compliance_roundtrip_when_set() {
        use crate::compliance::{ComplianceRegime, EnforcementLevel};
        let mut m = sample_manifest();
        m.compliance.regime = ComplianceRegime::Pci;
        m.compliance.enforcement = EnforcementLevel::Strict;
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains(r#""compliance""#));
        assert!(j.contains(r#""pci""#));
        assert!(j.contains(r#""strict""#));
        let back: PipelineManifest = serde_json::from_str(&j).unwrap();
        assert_eq!(back.compliance.regime, ComplianceRegime::Pci);
        assert_eq!(back.compliance.enforcement, EnforcementLevel::Strict);
    }

    #[test]
    fn durability_block_defaults_to_none_mode() {
        let d = DurabilityBlock::default();
        assert_eq!(d.mode, DurabilityMode::None);
        assert_eq!(d.checkpoint.backend, CheckpointBackendDecl::StateStore);
    }

    #[test]
    fn to_pipeline_definition_preserves_name_and_types() {
        let m = sample_manifest();
        let def = m.to_pipeline_definition();
        assert_eq!(def.name, "orders");
        assert_eq!(def.sources.len(), 1);
        assert_eq!(def.sources[0].source_type, "kafka");
        assert_eq!(def.sinks.len(), 1);
        assert_eq!(def.sinks[0].sink_type, "kafka");
        assert_eq!(def.processor.name, "p");
        assert_eq!(def.processor.version, "latest");
    }

    #[test]
    fn to_pipeline_definition_stores_source_kind_in_config() {
        let m = sample_manifest();
        let def = m.to_pipeline_definition();
        assert_eq!(def.sources[0].config.get("source_kind").unwrap(), "Pull");
    }

    #[test]
    fn to_pipeline_definition_stores_eos_tier_in_config() {
        let m = sample_manifest();
        let def = m.to_pipeline_definition();
        assert!(def.sinks[0].config.get("eos_tier").unwrap().contains("T2"));
    }

    // ── V5.1 PohBlock carry-through ────────────────────────────────────

    #[test]
    fn to_pipeline_definition_default_poh_block_round_trips_inert() {
        let m = sample_manifest();
        let def = m.to_pipeline_definition();
        assert!(def.poh.is_default());
        assert!(!def.poh.enabled);
    }

    #[test]
    fn to_pipeline_definition_carries_enabled_poh_through() {
        let mut m = sample_manifest();
        m.poh = crate::poh::PohBlock {
            enabled: true,
            max_recent_entries: 8192,
            signing_key_ref: Some("data-context/v1".into()),
        };
        let def = m.to_pipeline_definition();
        assert!(def.poh.enabled);
        assert_eq!(def.poh.max_recent_entries, 8192);
        assert_eq!(def.poh.signing_key_ref.as_deref(), Some("data-context/v1"));
    }

    // ── S6.7 SubjectExtractConfig ──────────────────────────────────────

    #[test]
    fn subject_extract_json_path_roundtrips() {
        let cfg = SubjectExtractConfig::JsonPath {
            namespace: "tenant-acme".into(),
            path: "$.user.id".into(),
        };
        let j = serde_json::to_string(&cfg).unwrap();
        // Verify the externally-tagged form with `kind: json_path`.
        assert!(j.contains("\"kind\":\"json_path\""));
        assert!(j.contains("\"namespace\":\"tenant-acme\""));
        assert!(j.contains("\"path\":\"$.user.id\""));
        let back: SubjectExtractConfig = serde_json::from_str(&j).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn subject_extract_header_roundtrips() {
        let cfg = SubjectExtractConfig::Header {
            namespace: "tenant-acme".into(),
            name: "X-User-Id".into(),
        };
        let j = serde_json::to_string(&cfg).unwrap();
        assert!(j.contains("\"kind\":\"header\""));
        let back: SubjectExtractConfig = serde_json::from_str(&j).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn subject_extract_metadata_literal_roundtrips() {
        let cfg = SubjectExtractConfig::MetadataLiteral;
        let j = serde_json::to_string(&cfg).unwrap();
        assert!(j.contains("\"kind\":\"metadata_literal\""));
        let back: SubjectExtractConfig = serde_json::from_str(&j).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn subject_extract_validate_accepts_good_config() {
        SubjectExtractConfig::JsonPath {
            namespace: "tenant-acme".into(),
            path: "$.user.id".into(),
        }
        .validate()
        .unwrap();
        SubjectExtractConfig::Header {
            namespace: "tenant-acme".into(),
            name: "X-User-Id".into(),
        }
        .validate()
        .unwrap();
        SubjectExtractConfig::MetadataLiteral.validate().unwrap();
    }

    #[test]
    fn subject_extract_validate_rejects_bad_namespace() {
        let cfg = SubjectExtractConfig::JsonPath {
            namespace: "has whitespace".into(),
            path: "$.x".into(),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn subject_extract_validate_rejects_empty_path() {
        let cfg = SubjectExtractConfig::JsonPath {
            namespace: "tenant-a".into(),
            path: "  ".into(),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn subject_extract_validate_rejects_empty_header_name() {
        let cfg = SubjectExtractConfig::Header {
            namespace: "tenant-a".into(),
            name: "".into(),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn source_manifest_defaults_subject_extract_to_none() {
        // Back-compat: a manifest without `subject_extract` must still
        // parse, leaving the field as None (schema-level Default).
        let j = r#"{"name":"orders-kafka","type":"kafka","kind":"pull"}"#;
        let m: SourceManifest = serde_json::from_str(j).unwrap();
        assert!(m.subject_extract.is_none());
    }

    #[test]
    fn source_manifest_parses_subject_extract() {
        let j = r#"{
            "name": "orders-kafka",
            "type": "kafka",
            "kind": "pull",
            "subject_extract": {
                "kind": "header",
                "namespace": "tenant-acme",
                "name": "X-User-Id"
            }
        }"#;
        let m: SourceManifest = serde_json::from_str(j).unwrap();
        match m.subject_extract.unwrap() {
            SubjectExtractConfig::Header { namespace, name } => {
                assert_eq!(namespace, "tenant-acme");
                assert_eq!(name, "X-User-Id");
            }
            other => panic!("expected Header, got {other:?}"),
        }
    }
}
