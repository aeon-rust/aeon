//! Processor Registry and Pipeline Management types.
//!
//! These types are Raft-replicated — every node in the cluster holds the
//! same registry and pipeline state. Serialized via serde for Raft log entries.

use crate::partition::PartitionId;
use crate::processor_identity::ProcessorIdentity;
use crate::processor_transport::{ProcessorBinding, ProcessorConnectionConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// G9.d — Per-partition Raft-committed sequence boundaries for
/// processor lifecycle transitions.
///
/// Carries `(partition → PoH sequence)` pairs that name *exactly* the
/// in-stream point at which a per-partition transition must apply.
/// Set by the leader before propose (read from
/// `LivePohChainRegistry::sequence()` per partition); honoured on every
/// node by `PipelineControl::drain_partitions_at_seq` after Raft commit.
///
/// Empty (the default) preserves G9.c behaviour: the variant transitions
/// immediately on apply with no per-partition draining. v0.1 log entries
/// without this field deserialise to an empty map via `#[serde(default)]`,
/// so log replay across the v0.1 → v0.2 boundary stays correct.
///
/// `BTreeMap` (not `HashMap`) so the on-wire ordering is deterministic —
/// two engines applying the same Raft entry must produce byte-identical
/// state-machine snapshots.
pub type PartitionBoundaries = BTreeMap<PartitionId, u64>;

/// G9.d — Custom serde for [`PartitionBoundaries`] that survives the
/// JSON map-key constraint.
///
/// JSON object keys must be strings, but `PartitionId` is a
/// `#[derive(Serialize, Deserialize)]` newtype around `u16` — its
/// `Deserialize` impl rejects strings. Without this helper, a JSON
/// round-trip of any non-empty boundary map fails at `from_str` time
/// with `invalid type: string "0", expected u16`.
///
/// The helper stringifies the `u16` on the way out and parses it back on
/// the way in, leaving bincode (the wire format used by Raft log
/// entries in production) untouched on the value type.
mod boundaries_serde {
    use super::PartitionBoundaries;
    use crate::partition::PartitionId;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S>(b: &PartitionBoundaries, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let stringified: BTreeMap<String, u64> = b
            .iter()
            .map(|(p, seq)| (p.as_u16().to_string(), *seq))
            .collect();
        stringified.serialize(s)
    }

    pub fn deserialize<'de, D>(d: D) -> Result<PartitionBoundaries, D::Error>
    where
        D: Deserializer<'de>,
    {
        let stringified: BTreeMap<String, u64> = BTreeMap::deserialize(d)?;
        let mut out = PartitionBoundaries::new();
        for (k, v) in stringified {
            let id: u16 = k.parse().map_err(serde::de::Error::custom)?;
            out.insert(PartitionId::new(id), v);
        }
        Ok(out)
    }
}

// ── Processor Registry ─────────────────────────────────────────────────

/// Type of processor artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessorType {
    /// WebAssembly module (`.wasm`).
    Wasm,
    /// Native shared library (`.so` / `.dll` / `.dylib`).
    NativeSo,
    /// WebTransport (HTTP/3 + QUIC) out-of-process processor.
    WebTransport,
    /// WebSocket (HTTP/2 + HTTP/1.1) out-of-process processor.
    WebSocket,
}

impl std::fmt::Display for ProcessorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wasm => write!(f, "wasm"),
            Self::NativeSo => write!(f, "native-so"),
            Self::WebTransport => write!(f, "web-transport"),
            Self::WebSocket => write!(f, "web-socket"),
        }
    }
}

/// Version status in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VersionStatus {
    /// Available for deployment but not currently active in any pipeline.
    Available,
    /// Currently active in one or more pipelines.
    Active,
    /// Archived — kept for rollback but not shown in default listings.
    Archived,
}

/// A specific version of a processor in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorVersion {
    /// Semantic version string (e.g., "1.2.3").
    pub version: String,
    /// SHA-512 hash of the artifact bytes.
    pub sha512: String,
    /// Artifact size in bytes.
    pub size_bytes: u64,
    /// Processor type.
    pub processor_type: ProcessorType,
    /// Target platform (e.g., "wasm32", "linux-x86_64", "windows-x86_64").
    pub platform: String,
    /// Current status.
    pub status: VersionStatus,
    /// Unix epoch millis when registered.
    pub registered_at: i64,
    /// Who registered this version (operator ID or "cli").
    pub registered_by: String,
    /// Endpoint URL for T3/T4 processors.
    /// T3 WebTransport: `https://host:4472`; T4 WebSocket: `ws://host:4471/api/v1/processors/connect`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Maximum batch size this processor version supports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_batch_size: Option<u32>,
}

/// A processor record in the registry — contains all versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorRecord {
    /// Processor name (unique identifier).
    pub name: String,
    /// Description (optional, human-readable).
    pub description: String,
    /// All versions, keyed by version string. BTreeMap for sorted iteration.
    pub versions: BTreeMap<String, ProcessorVersion>,
    /// Unix epoch millis when first registered.
    pub created_at: i64,
    /// Unix epoch millis of last modification.
    pub updated_at: i64,
}

impl ProcessorRecord {
    /// Create a new processor record with no versions.
    pub fn new(name: impl Into<String>, description: impl Into<String>, now: i64) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            versions: BTreeMap::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Get the latest available version (highest semver string, status != Archived).
    pub fn latest_version(&self) -> Option<&ProcessorVersion> {
        self.versions
            .values()
            .rev()
            .find(|v| v.status != VersionStatus::Archived)
    }

    /// Get a specific version.
    pub fn get_version(&self, version: &str) -> Option<&ProcessorVersion> {
        self.versions.get(version)
    }

    /// Count of non-archived versions.
    pub fn available_version_count(&self) -> usize {
        self.versions
            .values()
            .filter(|v| v.status != VersionStatus::Archived)
            .count()
    }
}

// ── Pipeline Management ────────────────────────────────────────────────

/// Pipeline execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PipelineState {
    /// Created but not started.
    Created,
    /// Running and processing events.
    Running,
    /// Stopped (manually or due to error).
    Stopped,
    /// Upgrading processor (drain-swap in progress).
    Upgrading,
    /// Failed — requires manual intervention.
    Failed,
}

impl std::fmt::Display for PipelineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Running => write!(f, "running"),
            Self::Stopped => write!(f, "stopped"),
            Self::Upgrading => write!(f, "upgrading"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// Upgrade strategy for a pipeline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeStrategy {
    /// Drain in-flight events, swap processor, resume. <100ms pause.
    #[default]
    DrainSwap,
    /// Run old + new in parallel, instant cutover after validation.
    BlueGreen,
    /// Gradual traffic shift with auto-rollback on threshold breach.
    Canary,
}

/// Source configuration for a pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    /// Source type (e.g., "kafka", "memory", "blackhole").
    #[serde(rename = "type")]
    pub source_type: String,
    /// Topic name (for Kafka/Redpanda sources).
    pub topic: Option<String>,
    /// Partition assignments.
    #[serde(default)]
    pub partitions: Vec<u16>,
    /// Additional configuration.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

/// Sink configuration for a pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkConfig {
    /// Sink type (e.g., "kafka", "blackhole", "stdout").
    #[serde(rename = "type")]
    pub sink_type: String,
    /// Topic name (for Kafka/Redpanda sinks).
    pub topic: Option<String>,
    /// Additional configuration.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

/// Processor reference within a pipeline definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorRef {
    /// Processor name in the registry.
    pub name: String,
    /// Version string (e.g., "1.2.3" or "latest").
    pub version: String,
    /// Binding mode — dedicated (default) or shared across pipelines.
    #[serde(default)]
    pub binding: ProcessorBinding,
    /// Per-pipeline connection overrides (T3/T4 only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ProcessorConnectionConfig>,
    /// V3 — execution tier (`"native"`, `"wasm"`, etc.). When set, the
    /// supervisor's `build_processor` switches on this to load the right
    /// artifact from the runtime's processors directory. Carried over
    /// from `ProcessorManifest.tier` by the manifest bridge. `None`
    /// resolves to the legacy passthrough path so pre-V3-native
    /// pipelines and tests stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

impl ProcessorRef {
    /// Create a new ProcessorRef with default binding and no connection overrides.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            binding: ProcessorBinding::default(),
            connection: None,
            tier: None,
        }
    }

    /// Set the execution tier (`"native"`, `"wasm"`, etc.).
    pub fn with_tier(mut self, tier: impl Into<String>) -> Self {
        self.tier = Some(tier.into());
        self
    }
}

impl std::fmt::Display for ProcessorRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.name, self.version)
    }
}

/// A pipeline definition — the unit of deployment in Aeon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineDefinition {
    /// Pipeline name (unique identifier).
    pub name: String,
    /// Source configurations. At least one required.
    pub sources: Vec<SourceConfig>,
    /// Processor reference.
    pub processor: ProcessorRef,
    /// Sink configurations. At least one required.
    pub sinks: Vec<SinkConfig>,
    /// Upgrade strategy.
    #[serde(default)]
    pub upgrade_strategy: UpgradeStrategy,
    /// Current state.
    pub state: PipelineState,
    /// Unix epoch millis when created.
    pub created_at: i64,
    /// Unix epoch millis of last state change.
    pub updated_at: i64,
    /// Node ID this pipeline is assigned to (for cluster mode).
    pub assigned_node: Option<u64>,
    /// Transport codec for T3/T4 AWPP data stream serialization.
    /// Defaults to MsgPack. Only applies when processor uses T3/T4 transport.
    #[serde(default)]
    pub transport_codec: crate::transport_codec::TransportCodec,
    /// In-progress upgrade state (blue-green or canary). None when stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_state: Option<UpgradeInfo>,
    /// EO-2 durability block — drives engine-side DeliveryConfig (durability
    /// mode, checkpoint backend, flush cadence, L2 byte cap) when the
    /// pipeline runtime spawns. Defaults to `None` mode for backward
    /// compatibility with pre-EO-2 serialized forms.
    #[serde(default)]
    pub durability: crate::manifest::DurabilityBlock,
    /// S3 at-rest encryption posture. Defaults to an inert block
    /// (`at_rest=off`) so pipelines pay zero cost unless they opt in.
    /// The supervisor runs `resolve_encryption_plan` against this block
    /// at start time; `Required` without a resolvable data-context KEK
    /// is a hard refusal to start.
    #[serde(default)]
    pub encryption: crate::encryption::EncryptionBlock,
    /// S4/S6 compliance declaration. Defaults to an inert block
    /// (`regime=None`, `enforcement=Off`) so pre-S6 serialized forms
    /// deserialize cleanly and pipelines pay zero cost unless they
    /// opt in. `PipelineSupervisor::start` runs `resolve_erasure_plan`
    /// against this block so a GDPR/Mixed pipeline declared under
    /// strict enforcement refuses to start when the structural
    /// erasure preconditions (at-rest encryption) are missing.
    #[serde(default)]
    pub compliance: crate::compliance::ComplianceBlock,
    /// V5 — Proof-of-History posture. Defaults to an inert block
    /// (`enabled=false`) so pre-V5 serialized forms deserialize
    /// cleanly. When `enabled`, `pipeline_config_for` translates
    /// this block onto `PipelineConfig.poh`, the processor task
    /// records a hash chain of every batch, and the chain head
    /// becomes accessible via the `/poh-head` REST endpoint.
    #[serde(default)]
    pub poh: crate::poh::PohBlock,
}

impl PipelineDefinition {
    /// Create a new pipeline definition in Created state.
    pub fn new(
        name: impl Into<String>,
        source: SourceConfig,
        processor: ProcessorRef,
        sink: SinkConfig,
        now: i64,
    ) -> Self {
        Self {
            name: name.into(),
            sources: vec![source],
            processor,
            sinks: vec![sink],
            upgrade_strategy: UpgradeStrategy::default(),
            state: PipelineState::Created,
            created_at: now,
            updated_at: now,
            assigned_node: None,
            transport_codec: crate::transport_codec::TransportCodec::default(),
            upgrade_state: None,
            durability: crate::manifest::DurabilityBlock::default(),
            encryption: crate::encryption::EncryptionBlock::default(),
            compliance: crate::compliance::ComplianceBlock::default(),
            poh: crate::poh::PohBlock::default(),
        }
    }
}

/// A history entry for pipeline lifecycle events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineHistoryEntry {
    /// Unix epoch millis.
    pub timestamp: i64,
    /// What happened.
    pub action: PipelineAction,
    /// Who initiated it.
    pub actor: String,
    /// Previous state.
    pub from_state: PipelineState,
    /// New state.
    pub to_state: PipelineState,
    /// Optional details (e.g., processor version for upgrades).
    pub details: Option<String>,
}

/// Pipeline lifecycle actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PipelineAction {
    Created,
    Started,
    Stopped,
    UpgradeStarted,
    UpgradeCompleted,
    UpgradeRolledBack,
    Failed,
    /// Blue-green: shadow processor deployed and warming up.
    BlueGreenStarted,
    /// Blue-green: traffic cut over to green processor.
    BlueGreenCutover,
    /// Canary: deployment started at initial traffic percentage.
    CanaryStarted,
    /// Canary: traffic percentage promoted to next step.
    CanaryPromoted,
    /// Canary: fully promoted to 100%, canary completed.
    CanaryCompleted,
    /// Source or sink reconfigured (same connector type, new config).
    Reconfigured,
}

// ── Upgrade State Tracking ─────────────────────────────────────────────

/// In-progress upgrade information attached to a pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "kebab-case")]
pub enum UpgradeInfo {
    /// Blue-green: old + new running simultaneously, instant cutover.
    BlueGreen(BlueGreenState),
    /// Canary: gradual traffic shift with metrics-based auto-promote/rollback.
    Canary(CanaryState),
}

/// Blue-green upgrade state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlueGreenState {
    /// The "blue" (old, currently active) processor.
    pub blue_processor: ProcessorRef,
    /// The "green" (new, warming up or ready) processor.
    pub green_processor: ProcessorRef,
    /// Which version is currently receiving live traffic.
    pub active: BlueGreenActive,
    /// When the upgrade started (Unix epoch millis).
    pub started_at: i64,
}

/// Which processor version is active in a blue-green deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlueGreenActive {
    /// Old (original) processor is handling traffic.
    Blue,
    /// New processor has been cut over to handle traffic.
    Green,
}

/// Canary upgrade state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryState {
    /// The baseline (old) processor.
    pub baseline_processor: ProcessorRef,
    /// The canary (new) processor.
    pub canary_processor: ProcessorRef,
    /// Current traffic percentage routed to canary (0–100).
    pub traffic_pct: u8,
    /// Traffic shift steps (e.g., [10, 50, 100]).
    pub steps: Vec<u8>,
    /// Current step index into `steps`.
    pub current_step: usize,
    /// Thresholds for auto-promote/rollback decisions.
    pub thresholds: CanaryThresholds,
    /// When the canary started (Unix epoch millis).
    pub started_at: i64,
}

/// Thresholds that control canary auto-promote and auto-rollback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryThresholds {
    /// Max error rate (fraction, 0.0–1.0) before auto-rollback.
    #[serde(default = "default_max_error_rate")]
    pub max_error_rate: f64,
    /// Max P99 latency (ms) before auto-rollback.
    #[serde(default = "default_max_p99_latency_ms")]
    pub max_p99_latency_ms: u64,
    /// Min throughput ratio (canary / baseline) before auto-rollback.
    #[serde(default = "default_min_throughput_ratio")]
    pub min_throughput_ratio: f64,
}

fn default_max_error_rate() -> f64 {
    0.01
}
fn default_max_p99_latency_ms() -> u64 {
    100
}
fn default_min_throughput_ratio() -> f64 {
    0.8
}

impl Default for CanaryThresholds {
    fn default() -> Self {
        Self {
            max_error_rate: default_max_error_rate(),
            max_p99_latency_ms: default_max_p99_latency_ms(),
            min_throughput_ratio: default_min_throughput_ratio(),
        }
    }
}

// ── Raft State Machine Commands ────────────────────────────────────────

/// Commands that modify the registry/pipeline state via Raft.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RegistryCommand {
    /// Register a new processor or add a version.
    RegisterProcessor {
        name: String,
        description: String,
        version: ProcessorVersion,
    },
    /// Delete a specific processor version.
    DeleteVersion { name: String, version: String },
    /// Update version status (e.g., Available → Active).
    SetVersionStatus {
        name: String,
        version: String,
        status: VersionStatus,
    },
    /// Create a new pipeline.
    CreatePipeline { definition: Box<PipelineDefinition> },
    /// Update pipeline state.
    SetPipelineState { name: String, state: PipelineState },
    /// Upgrade pipeline processor.
    UpgradePipeline {
        name: String,
        new_processor: ProcessorRef,
    },
    /// Delete a pipeline.
    DeletePipeline { name: String },
    /// G9.c — Start a blue-green upgrade. Pipeline transitions to
    /// `Upgrading` state with `BlueGreen` upgrade metadata holding both
    /// the old (blue) and new (green) processor refs. Cluster-wide so
    /// every node converges on the same upgrade-in-progress declaration.
    /// `actor` records who initiated the change for the per-pipeline
    /// history log.
    ///
    /// G9.d — `boundaries` names the per-partition PoH sequence at
    /// which the green processor takes over reading. Empty map
    /// preserves G9.c immediate-on-apply semantics; non-empty triggers
    /// `PipelineControl::drain_partitions_at_seq` on every node.
    BlueGreenStart {
        name: String,
        new_processor: ProcessorRef,
        actor: String,
        #[serde(
            default,
            skip_serializing_if = "BTreeMap::is_empty",
            with = "boundaries_serde"
        )]
        boundaries: PartitionBoundaries,
    },
    /// G9.c — Cut over from blue to green: green becomes the active
    /// processor, upgrade metadata is cleared, pipeline returns to
    /// `Running`. Replicated so every node's PipelineManager flips at
    /// the same Raft commit; supervisor side-effect on each node drives
    /// the local PipelineControl through `cutover_blue_green()`.
    ///
    /// G9.d — `boundaries` names the per-partition PoH sequence at
    /// which the cut-over actually applies. Empty preserves G9.c
    /// immediate-on-apply.
    BlueGreenCutover {
        name: String,
        actor: String,
        #[serde(
            default,
            skip_serializing_if = "BTreeMap::is_empty",
            with = "boundaries_serde"
        )]
        boundaries: PartitionBoundaries,
    },
    /// G9.c — Roll back an in-flight upgrade: drop the green processor,
    /// keep blue active, clear upgrade metadata, return to `Running`.
    /// Symmetric counterpart of `BlueGreenCutover`.
    ///
    /// G9.d — `boundaries` names the per-partition PoH sequence at
    /// which the rollback applies on each node. Empty preserves G9.c
    /// immediate-on-apply.
    RollbackUpgrade {
        name: String,
        actor: String,
        #[serde(
            default,
            skip_serializing_if = "BTreeMap::is_empty",
            with = "boundaries_serde"
        )]
        boundaries: PartitionBoundaries,
    },
    /// G9.c — Start a canary upgrade. Pipeline transitions to
    /// `Upgrading` with `Canary` upgrade metadata holding the new
    /// processor + traffic-step schedule + auto-promotion thresholds.
    /// Each step advances when `promote_canary` is invoked (manually or
    /// by an automated promoter watching the thresholds).
    ///
    /// G9.d — `boundaries` names the per-partition PoH sequence at
    /// which the canary processor begins receiving its traffic split.
    /// Empty preserves G9.c immediate-on-apply.
    CanaryStart {
        name: String,
        new_processor: ProcessorRef,
        steps: Vec<u8>,
        thresholds: CanaryThresholds,
        actor: String,
        #[serde(
            default,
            skip_serializing_if = "BTreeMap::is_empty",
            with = "boundaries_serde"
        )]
        boundaries: PartitionBoundaries,
    },
    /// G9.c — Advance a canary upgrade by one step. When all steps are
    /// exhausted, the canary auto-promotes to full cutover.
    ///
    /// G9.d — `boundaries` names the per-partition PoH sequence at
    /// which the new traffic split applies. Empty preserves G9.c
    /// immediate-on-apply.
    CanaryPromote {
        name: String,
        actor: String,
        #[serde(
            default,
            skip_serializing_if = "BTreeMap::is_empty",
            with = "boundaries_serde"
        )]
        boundaries: PartitionBoundaries,
    },
    /// G9.c — Hot-reconfigure a source connector while the pipeline
    /// keeps running. The new SourceConfig must declare the same
    /// connector type as the existing one (cross-type rejected at apply
    /// time). Each node's supervisor side-effect drives
    /// `PipelineControl::drain_and_swap_source()` against the local
    /// pipeline task.
    ///
    /// G9.d — `boundaries` names the per-partition PoH sequence at
    /// which the new source takes over fetching. Empty preserves G9.c
    /// immediate-on-apply.
    ReconfigureSource {
        name: String,
        new_source: SourceConfig,
        actor: String,
        #[serde(
            default,
            skip_serializing_if = "BTreeMap::is_empty",
            with = "boundaries_serde"
        )]
        boundaries: PartitionBoundaries,
    },
    /// G9.c — Hot-reconfigure a sink connector. Same contract as
    /// `ReconfigureSource` but for sinks.
    ///
    /// G9.d — `boundaries` names the per-partition PoH sequence at
    /// which the new sink takes over writing. Empty preserves G9.c
    /// immediate-on-apply.
    ReconfigureSink {
        name: String,
        new_sink: SinkConfig,
        actor: String,
        #[serde(
            default,
            skip_serializing_if = "BTreeMap::is_empty",
            with = "boundaries_serde"
        )]
        boundaries: PartitionBoundaries,
    },
    /// Register a processor identity (ED25519 public key).
    RegisterIdentity {
        processor_name: String,
        identity: ProcessorIdentity,
    },
    /// Revoke a processor identity by fingerprint.
    RevokeIdentity {
        processor_name: String,
        fingerprint: String,
        revoked_at: i64,
    },
}

/// Response from applying a RegistryCommand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RegistryResponse {
    Ok,
    ProcessorRegistered { name: String, version: String },
    PipelineCreated { name: String },
    IdentityRegistered { fingerprint: String },
    IdentityRevoked { fingerprint: String },
    Error { message: String },
}

/// Pluggable applier for Raft-replicated RegistryCommand entries.
///
/// The cluster state machine replicates registry commands across all nodes but
/// does not itself own PipelineManager / ProcessorRegistry (avoiding a cyclic
/// `aeon-cluster → aeon-engine` dependency). Instead, `aeon-engine` registers
/// an implementation of this trait on the cluster node; the state machine then
/// dispatches `ClusterRequest::Registry(cmd)` entries through it after the log
/// entry is committed, so every node converges on the same local state.
pub trait RegistryApplier: Send + Sync {
    /// Apply a replicated command to this node's local registry/pipeline state.
    ///
    /// Called on every node after Raft commits the entry. Must be deterministic
    /// w.r.t. the input command + prior applied state.
    fn apply(
        &self,
        cmd: RegistryCommand,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RegistryResponse> + Send + '_>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processor_record_new() {
        let rec = ProcessorRecord::new("my-proc", "A test processor", 1000);
        assert_eq!(rec.name, "my-proc");
        assert!(rec.versions.is_empty());
        assert!(rec.latest_version().is_none());
        assert_eq!(rec.available_version_count(), 0);
    }

    #[test]
    fn processor_record_with_versions() {
        let mut rec = ProcessorRecord::new("proc", "", 1000);
        rec.versions.insert(
            "0.1.0".into(),
            ProcessorVersion {
                version: "0.1.0".into(),
                sha512: "abc123".into(),
                size_bytes: 1024,
                processor_type: ProcessorType::Wasm,
                platform: "wasm32".into(),
                status: VersionStatus::Archived,
                registered_at: 1000,
                registered_by: "cli".into(),
                endpoint: None,
                max_batch_size: None,
            },
        );
        rec.versions.insert(
            "0.2.0".into(),
            ProcessorVersion {
                version: "0.2.0".into(),
                sha512: "def456".into(),
                size_bytes: 2048,
                processor_type: ProcessorType::Wasm,
                platform: "wasm32".into(),
                status: VersionStatus::Available,
                registered_at: 2000,
                registered_by: "cli".into(),
                endpoint: None,
                max_batch_size: None,
            },
        );

        assert_eq!(rec.available_version_count(), 1);
        let latest = rec.latest_version().unwrap();
        assert_eq!(latest.version, "0.2.0");
        assert!(rec.get_version("0.1.0").is_some());
        assert!(rec.get_version("0.3.0").is_none());
    }

    #[test]
    fn pipeline_definition_new() {
        let src = SourceConfig {
            source_type: "kafka".into(),
            topic: Some("input".into()),
            partitions: vec![0, 1, 2, 3],
            config: BTreeMap::new(),
        };
        let sink = SinkConfig {
            sink_type: "kafka".into(),
            topic: Some("output".into()),
            config: BTreeMap::new(),
        };
        let proc_ref = ProcessorRef::new("enricher", "1.0.0");

        let pipeline = PipelineDefinition::new("my-pipeline", src, proc_ref, sink, 5000);
        assert_eq!(pipeline.name, "my-pipeline");
        assert_eq!(pipeline.state, PipelineState::Created);
        assert_eq!(pipeline.upgrade_strategy, UpgradeStrategy::DrainSwap);
        assert_eq!(pipeline.processor.to_string(), "enricher:1.0.0");
    }

    #[test]
    fn pipeline_state_display() {
        assert_eq!(PipelineState::Running.to_string(), "running");
        assert_eq!(PipelineState::Upgrading.to_string(), "upgrading");
    }

    #[test]
    fn processor_type_display() {
        assert_eq!(ProcessorType::Wasm.to_string(), "wasm");
        assert_eq!(ProcessorType::NativeSo.to_string(), "native-so");
        assert_eq!(ProcessorType::WebTransport.to_string(), "web-transport");
        assert_eq!(ProcessorType::WebSocket.to_string(), "web-socket");
    }

    #[test]
    fn processor_type_serde_roundtrip() {
        let json = serde_json::to_string(&ProcessorType::WebTransport).unwrap();
        assert_eq!(json, "\"web-transport\"");
        let back: ProcessorType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ProcessorType::WebTransport);

        let json = serde_json::to_string(&ProcessorType::WebSocket).unwrap();
        assert_eq!(json, "\"web-socket\"");
        let back: ProcessorType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ProcessorType::WebSocket);
    }

    #[test]
    fn processor_ref_new_defaults() {
        let r = ProcessorRef::new("my-proc", "1.0.0");
        assert_eq!(r.name, "my-proc");
        assert_eq!(r.version, "1.0.0");
        assert!(matches!(r.binding, ProcessorBinding::Dedicated));
        assert!(r.connection.is_none());
    }

    #[test]
    fn processor_ref_serde_with_binding() {
        let mut r = ProcessorRef::new("proc", "2.0.0");
        r.binding = ProcessorBinding::Shared {
            group: "analytics".into(),
        };
        r.connection = Some(ProcessorConnectionConfig {
            endpoint: Some("https://host:4472".into()),
            batch_size: Some(512),
            timeout_ms: None,
            min_instances: None,
        });

        let json = serde_json::to_string(&r).unwrap();
        let back: ProcessorRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "proc");
        match back.binding {
            ProcessorBinding::Shared { group } => assert_eq!(group, "analytics"),
            _ => panic!("expected Shared"),
        }
        assert_eq!(back.connection.unwrap().batch_size, Some(512));
    }

    #[test]
    fn processor_version_with_endpoint() {
        let pv = ProcessorVersion {
            version: "1.0.0".into(),
            sha512: "hash".into(),
            size_bytes: 1024,
            processor_type: ProcessorType::WebTransport,
            platform: "linux-x86_64".into(),
            status: VersionStatus::Available,
            registered_at: 1000,
            registered_by: "cli".into(),
            endpoint: Some("https://proc.example.com:4472".into()),
            max_batch_size: Some(2048),
        };
        let json = serde_json::to_string(&pv).unwrap();
        let back: ProcessorVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.endpoint.as_deref(),
            Some("https://proc.example.com:4472")
        );
        assert_eq!(back.max_batch_size, Some(2048));
    }

    #[test]
    fn registry_command_serde_roundtrip() {
        let cmd = RegistryCommand::RegisterProcessor {
            name: "test".into(),
            description: "desc".into(),
            version: ProcessorVersion {
                version: "1.0.0".into(),
                sha512: "hash".into(),
                size_bytes: 512,
                processor_type: ProcessorType::Wasm,
                platform: "wasm32".into(),
                status: VersionStatus::Available,
                registered_at: 9999,
                registered_by: "test".into(),
                endpoint: None,
                max_batch_size: None,
            },
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let deserialized: RegistryCommand = serde_json::from_str(&json).unwrap();

        match deserialized {
            RegistryCommand::RegisterProcessor { name, version, .. } => {
                assert_eq!(name, "test");
                assert_eq!(version.version, "1.0.0");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pipeline_definition_serde_roundtrip() {
        let def = PipelineDefinition::new(
            "pipe-1",
            SourceConfig {
                source_type: "kafka".into(),
                topic: Some("in".into()),
                partitions: vec![0],
                config: BTreeMap::new(),
            },
            ProcessorRef::new("proc", "1.0.0"),
            SinkConfig {
                sink_type: "kafka".into(),
                topic: Some("out".into()),
                config: BTreeMap::new(),
            },
            1000,
        );

        let json = serde_json::to_string(&def).unwrap();
        let back: PipelineDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "pipe-1");
        assert_eq!(back.state, PipelineState::Created);
    }

    #[test]
    fn upgrade_strategy_default_is_drain_swap() {
        assert_eq!(UpgradeStrategy::default(), UpgradeStrategy::DrainSwap);
    }

    // ── G9.c — Lifecycle RegistryCommand variants serde round-trip ──
    // Pins the wire format so a Raft log entry written by one version
    // of the engine can be applied by a later version. New variants
    // here MUST extend `#[serde(tag = "type", rename_all = "snake_case")]`
    // — adding a struct-style variant flips the JSON schema and
    // breaks log replay across version skew.

    fn proc_ref_for_test() -> ProcessorRef {
        ProcessorRef::new("my-proc", "1.0.0")
    }

    #[test]
    fn blue_green_start_serde_roundtrip() {
        let cmd = RegistryCommand::BlueGreenStart {
            name: "p".into(),
            new_processor: proc_ref_for_test(),
            actor: "api".into(),
            boundaries: PartitionBoundaries::new(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"blue_green_start\""));
        // Empty boundaries must NOT appear on the wire — preserves
        // byte-identical output with v0.1 log entries.
        assert!(!json.contains("boundaries"));
        let back: RegistryCommand = serde_json::from_str(&json).unwrap();
        match back {
            RegistryCommand::BlueGreenStart {
                name,
                actor,
                boundaries,
                ..
            } => {
                assert_eq!(name, "p");
                assert_eq!(actor, "api");
                assert!(boundaries.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn blue_green_cutover_serde_roundtrip() {
        let cmd = RegistryCommand::BlueGreenCutover {
            name: "p".into(),
            actor: "op".into(),
            boundaries: PartitionBoundaries::new(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"blue_green_cutover\""));
        assert!(!json.contains("boundaries"));
        let _back: RegistryCommand = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn rollback_upgrade_serde_roundtrip() {
        let cmd = RegistryCommand::RollbackUpgrade {
            name: "p".into(),
            actor: "op".into(),
            boundaries: PartitionBoundaries::new(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"rollback_upgrade\""));
        let _back: RegistryCommand = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn canary_start_serde_roundtrip() {
        let cmd = RegistryCommand::CanaryStart {
            name: "p".into(),
            new_processor: proc_ref_for_test(),
            steps: vec![10, 50, 100],
            thresholds: CanaryThresholds::default(),
            actor: "api".into(),
            boundaries: PartitionBoundaries::new(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"canary_start\""));
        let back: RegistryCommand = serde_json::from_str(&json).unwrap();
        match back {
            RegistryCommand::CanaryStart { steps, .. } => {
                assert_eq!(steps, vec![10, 50, 100]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn canary_promote_serde_roundtrip() {
        let cmd = RegistryCommand::CanaryPromote {
            name: "p".into(),
            actor: "op".into(),
            boundaries: PartitionBoundaries::new(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"canary_promote\""));
        let _back: RegistryCommand = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn reconfigure_source_serde_roundtrip() {
        let cmd = RegistryCommand::ReconfigureSource {
            name: "p".into(),
            new_source: SourceConfig {
                source_type: "kafka".into(),
                topic: Some("in2".into()),
                partitions: vec![0, 1],
                config: BTreeMap::new(),
            },
            actor: "api".into(),
            boundaries: PartitionBoundaries::new(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"reconfigure_source\""));
        let back: RegistryCommand = serde_json::from_str(&json).unwrap();
        match back {
            RegistryCommand::ReconfigureSource { new_source, .. } => {
                assert_eq!(new_source.source_type, "kafka");
                assert_eq!(new_source.topic.as_deref(), Some("in2"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn reconfigure_sink_serde_roundtrip() {
        let cmd = RegistryCommand::ReconfigureSink {
            name: "p".into(),
            new_sink: SinkConfig {
                sink_type: "blackhole".into(),
                topic: None,
                config: BTreeMap::new(),
            },
            actor: "api".into(),
            boundaries: PartitionBoundaries::new(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"reconfigure_sink\""));
        let back: RegistryCommand = serde_json::from_str(&json).unwrap();
        match back {
            RegistryCommand::ReconfigureSink { new_sink, .. } => {
                assert_eq!(new_sink.sink_type, "blackhole");
            }
            _ => panic!("wrong variant"),
        }
    }

    // ── G9.d — Boundary-map backward + forward compatibility ──
    // A v0.1 Raft log entry (G9.c shape, no `boundaries` field) MUST
    // deserialize cleanly into the v0.2 shape with an empty map. A v0.2
    // entry with a populated map MUST round-trip its keys and values
    // intact. Each test pins the wire shape for one of the seven
    // lifecycle variants — drift here breaks log replay across the
    // version skew that an in-place rolling upgrade introduces.

    /// G9.c-shape JSON (no `boundaries` key) replays into the v0.2
    /// variant with an empty `PartitionBoundaries`.
    #[test]
    fn g9c_blue_green_start_replays_into_v2_with_empty_boundaries() {
        let v01_json = r#"{
            "type": "blue_green_start",
            "name": "p",
            "new_processor": { "name": "my-proc", "version": "1.0.0" },
            "actor": "api"
        }"#;
        let cmd: RegistryCommand = serde_json::from_str(v01_json).unwrap();
        match cmd {
            RegistryCommand::BlueGreenStart { boundaries, .. } => {
                assert!(boundaries.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    /// Same backward-compat assertion across the other six variants.
    #[test]
    fn g9c_lifecycle_variants_all_replay_with_empty_boundaries() {
        let cases = [
            (
                r#"{"type": "blue_green_cutover", "name": "p", "actor": "api"}"#,
                "blue_green_cutover",
            ),
            (
                r#"{"type": "rollback_upgrade", "name": "p", "actor": "api"}"#,
                "rollback_upgrade",
            ),
            (
                r#"{
                    "type": "canary_start",
                    "name": "p",
                    "new_processor": { "name": "my-proc", "version": "1.0.0" },
                    "steps": [10, 50, 100],
                    "thresholds": {},
                    "actor": "api"
                }"#,
                "canary_start",
            ),
            (
                r#"{"type": "canary_promote", "name": "p", "actor": "api"}"#,
                "canary_promote",
            ),
            (
                r#"{
                    "type": "reconfigure_source",
                    "name": "p",
                    "new_source": {
                        "type": "kafka",
                        "topic": "in",
                        "partitions": [0, 1],
                        "config": {}
                    },
                    "actor": "api"
                }"#,
                "reconfigure_source",
            ),
            (
                r#"{
                    "type": "reconfigure_sink",
                    "name": "p",
                    "new_sink": { "type": "blackhole", "config": {} },
                    "actor": "api"
                }"#,
                "reconfigure_sink",
            ),
        ];
        for (json, label) in cases {
            let cmd: RegistryCommand = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("{label} v0.1 replay failed: {e}"));
            let boundaries = match cmd {
                RegistryCommand::BlueGreenCutover { boundaries, .. }
                | RegistryCommand::RollbackUpgrade { boundaries, .. }
                | RegistryCommand::CanaryStart { boundaries, .. }
                | RegistryCommand::CanaryPromote { boundaries, .. }
                | RegistryCommand::ReconfigureSource { boundaries, .. }
                | RegistryCommand::ReconfigureSink { boundaries, .. } => boundaries,
                _ => panic!("{label}: unexpected variant"),
            };
            assert!(
                boundaries.is_empty(),
                "{label}: empty boundaries on v0.1 replay"
            );
        }
    }

    /// A v0.2 entry with a populated `boundaries` map round-trips its
    /// (partition → sequence) pairs intact. BTreeMap iteration order is
    /// deterministic so the on-wire bytes are stable across runs.
    #[test]
    fn g9d_populated_boundaries_round_trip() {
        let mut boundaries = PartitionBoundaries::new();
        boundaries.insert(PartitionId::new(0), 1234);
        boundaries.insert(PartitionId::new(7), u64::MAX);
        let cmd = RegistryCommand::BlueGreenCutover {
            name: "p".into(),
            actor: "api".into(),
            boundaries: boundaries.clone(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        // Non-empty map MUST appear on the wire (skip-if-empty toggles off).
        assert!(json.contains("boundaries"));
        let back: RegistryCommand = serde_json::from_str(&json).unwrap();
        match back {
            RegistryCommand::BlueGreenCutover {
                boundaries: round_tripped,
                ..
            } => {
                assert_eq!(round_tripped, boundaries);
                assert_eq!(round_tripped.len(), 2);
                assert_eq!(round_tripped.get(&PartitionId::new(0)), Some(&1234));
                assert_eq!(round_tripped.get(&PartitionId::new(7)), Some(&u64::MAX));
            }
            _ => panic!("wrong variant"),
        }
    }
}
