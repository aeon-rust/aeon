//! PipelineSupervisor — manifest → running tokio task bridge.
//!
//! `PipelineManager` owns the *declarative* state (Created/Running/Stopped) and
//! is what the REST API and Raft applier mutate. This supervisor is the thing
//! that **actually runs** a pipeline: it builds source / sink connectors from
//! the `ConnectorRegistry`, picks (for T0) a `PassthroughProcessor`, and spawns
//! `run_buffered_managed` in a tokio task. It tracks one `RunningPipeline` per
//! active pipeline so it can flip the shutdown flag and await the join handle
//! on stop.
//!
//! T0 scope (Gate 2 isolation matrix):
//! - Source: any registered `SourceFactory` (memory, kafka, …).
//! - Processor: hard-wired `PassthroughProcessor` keyed by name `"__identity"`
//!   or any name we don't recognise. Native/Wasm processor instantiation is
//!   wired in later phases through the existing `ProcessorRegistry` path used
//!   by the REST upgrade handler.
//! - Sink: any registered `SinkFactory`.
//! - Durability: `DurabilityBlock` is mapped onto `DeliveryConfig` (mode +
//!   strategy). EO-2 L2 body / L3 checkpoint wiring is plumbed through
//!   `PipelineConfig` in a follow-up — for `DurabilityMode::None` (the T0
//!   matrix's first row) the pipeline runs the legacy at-least-once path
//!   unchanged, which is what the matrix is meant to measure.
//!
//! The supervisor does **not** mutate `PipelineManager`'s declared state —
//! callers are expected to update the manager (which is what the REST
//! handler / Raft applier already do). The split keeps the running-process
//! lifecycle (this file) decoupled from the replicated-state lifecycle
//! (`PipelineManager`).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use aeon_crypto::kek::KekHandle;
use aeon_types::durability::DurabilityMode;
use aeon_types::error::AeonError;
use aeon_types::registry::PipelineDefinition;
use aeon_types::traits::Sink;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::connector_registry::{ConnectorRegistry, PartitionOwnershipResolver};
use crate::compliance_validator::validate_compliance;
use crate::encryption_probe::resolve_encryption_plan;
use crate::erasure_probe::resolve_erasure_plan;
use crate::retention_probe::resolve_retention_plan;
use crate::pipeline::{
    PipelineConfig, PipelineControl, PipelineMetrics, run_buffered_managed,
};
use crate::processor::PassthroughProcessor;
use crate::write_gate::WriteGateRegistry;
use aeon_types::partition::PartitionId;

/// Reserved processor name that resolves to `PassthroughProcessor`. Any
/// pipeline whose `processor.name` matches this gets the identity processor —
/// the T0 isolation matrix uses it to measure source→sink throughput without
/// any processing cost.
pub const IDENTITY_PROCESSOR: &str = "__identity";

/// Per-pipeline runtime state held while the pipeline is running.
struct RunningPipeline {
    /// JoinHandle for the spawned `run_buffered_managed` task. `Mutex<Option>`
    /// so `stop()` can `take()` it for a `.await` without holding the
    /// supervisor lock across the join.
    handle: Mutex<Option<JoinHandle<Result<(), AeonError>>>>,
    shutdown: Arc<AtomicBool>,
    control: Arc<PipelineControl>,
    metrics: Arc<PipelineMetrics>,
}

/// Supervises the set of pipelines currently running on this node. One
/// instance per process; held inside `AppState` as `Arc<PipelineSupervisor>`.
pub struct PipelineSupervisor {
    connectors: Arc<ConnectorRegistry>,
    /// Active pipelines keyed by name. `RwLock` rather than `DashMap` because
    /// start/stop already serialise on per-pipeline tokio operations and the
    /// map mutations are rare; readability wins over micro-throughput.
    running: Mutex<HashMap<String, Arc<RunningPipeline>>>,
    /// G2/CL-6: per-(pipeline, partition) write-freeze gates. The supervisor
    /// installs a gate for each running partition at `start()` time and
    /// stamps it into the pipeline's `PipelineConfig.write_gate`. The cluster
    /// `CutoverCoordinator` impl looks up the gate here during partition
    /// handover to freeze the source and read a stable watermark. `Arc`
    /// makes the registry cheaply shareable with the cluster crate.
    gate_registry: Arc<WriteGateRegistry>,
    /// G1 fix: cluster ownership resolver. Installed post-`bootstrap_multi`
    /// in `cmd_serve` (the supervisor is constructed before the cluster
    /// node exists, so this is write-once via `OnceLock`). When a source
    /// manifest leaves `partitions` empty, `start()` consults this to fill
    /// in this node's owned slice rather than silently defaulting to `[0]`.
    /// Absent resolver → sources keep their own fallbacks (single-node
    /// tests / benches). See `PartitionOwnershipResolver` for the contract.
    ownership: OnceLock<Arc<dyn PartitionOwnershipResolver>>,
    /// G11.c: shared `InstalledPohChainRegistry`. The cluster-side
    /// `PohChainInstallerImpl` (spawned in `aeon-cli` at bootstrap)
    /// writes per-`(pipeline, partition)` `PohChain` snapshots here on
    /// every completed CL-6 handover. `start()` stamps this Arc onto
    /// each pipeline's `PipelineConfig.poh_installed_chains` so
    /// `create_poh_state` can `take()` the snapshot and resume the
    /// chain when the post-transfer pipeline task starts. Absent
    /// registry → pipelines always genesis a fresh chain (the T0 /
    /// single-node / test path).
    #[cfg(all(feature = "processor-auth", feature = "cluster"))]
    poh_installed_chains:
        OnceLock<Arc<crate::partition_install::InstalledPohChainRegistry>>,
    /// CL-6c.4: shared `LivePohChainRegistry`. Constructed once at
    /// supervisor creation so `start()` can stamp the same Arc onto
    /// every pipeline's `PipelineConfig.poh_live_chains` and the
    /// source-side `PohChainExportProvider` reads from the same
    /// instance. Unlike `poh_installed_chains` this is always-on (not
    /// `OnceLock`): no external bootstrap step is required, and
    /// pipelines with `poh = None` simply never register anything.
    /// Available even without the cluster feature so the API is
    /// uniform — the cluster crate is the only consumer that reads it.
    #[cfg(all(feature = "processor-auth", feature = "cluster"))]
    poh_live_chains: Arc<crate::partition_install::LivePohChainRegistry>,
    /// S3: node-wide data-context KEK handle. Installed once by
    /// `cmd_serve` after the secret provider and KEK bootstrap finish;
    /// consumed by `start()` via `resolve_encryption_plan` to decide
    /// whether to accept a pipeline whose manifest declares
    /// `encryption.at_rest = required`. Absent handle + `required`
    /// manifest = hard refusal to start. Tests / benches / single-node
    /// callers that don't care about at-rest leave this unset.
    data_context_kek: OnceLock<Arc<KekHandle>>,
    /// EO-2: L2 body store root directory. Installed once by
    /// `cmd_serve` (same path the cluster crate's
    /// `L2SegmentTransferProvider` reads). Consumed by
    /// `pipeline_config_for` to build a `PipelineL2Registry` for
    /// pipelines whose `durability.mode.requires_l2_body_store()`
    /// returns true — without this, those pipelines would silently
    /// pass through `MaybeL2Wrapped::Direct` and skip L2 persistence.
    /// Discovered 2026-04-25 during V3 OrderedBatch validation: the
    /// pre-existing wiring left `l2_registry` at `None` for every
    /// supervisor-built pipeline and `/app/artifacts/l2body/` stayed
    /// empty even with `durability.mode = ordered_batch`.
    l2_root: OnceLock<std::path::PathBuf>,
    /// V5.1: node-wide `SecretRegistry` used to resolve PoH
    /// `signing_key_ref` references at pipeline start. Installed once
    /// by `cmd_serve` after the bootstrap layer composes the env /
    /// dotenv / vault provider chain. `start()` passes a borrow into
    /// `resolve_poh_signing_key`, which fails fast if a pipeline's
    /// manifest sets `poh.signing_key_ref` while no registry is
    /// installed. Without this OnceLock, supervisor callers that don't
    /// reference signing keys (most tests, single-node + chain-only
    /// verification) keep working without the bootstrap step.
    #[cfg(feature = "processor-auth")]
    secret_registry: OnceLock<Arc<aeon_types::SecretRegistry>>,
    /// M2 / FT-3: node-wide L3 store used as the checkpoint backend
    /// when a manifest declares `durability.checkpoint.backend:
    /// state_store`. Installed once by `cmd_serve` after the artifact
    /// directory is known (typically a redb file at
    /// `${artifact_dir}/l3-checkpoints.redb`). `start()` clones it
    /// onto `PipelineConfig.l3_checkpoint_store` so
    /// `build_sink_task_ctx` produces a real `L3CheckpointStore`
    /// instead of warning and proceeding without a writer. Tests /
    /// benches that don't exercise checkpointing leave this unset;
    /// `CheckpointBackend::Wal` and `None` paths are unaffected.
    l3_checkpoint_store: OnceLock<Arc<dyn aeon_types::L3Store>>,
    /// W3 / F1: side-channel clone of the `FaultyL3Wrapper` that wraps
    /// the installed L3 store. The trait-object `Arc<dyn L3Store>`
    /// can't be downcast directly (no Any supertrait), so cmd_serve
    /// installs the wrapper twice: once as the L3 store proper, once
    /// here for the REST fault-injection endpoint to reach.
    fault_injector_handle: OnceLock<crate::debug_fault::FaultyL3Wrapper>,
    /// O1 / EO-2 P8: node-wide `Eo2Metrics` registry. Always-on (no
    /// install step) — same pattern as `poh_live_chains`. `start()`
    /// stamps a clone onto every pipeline's
    /// `PipelineConfig.eo2_metrics` so the L2 body store, sink ack
    /// tracker, fallback checkpoint store, and capacity gate all
    /// publish into the same registry. The REST `/metrics` handler
    /// appends `render_prometheus()` so operators see
    /// `aeon_l2_bytes`, `aeon_l2_segments`, `aeon_l2_gc_lag_seq`,
    /// `aeon_l2_pressure`, `aeon_sink_ack_seq`, and
    /// `aeon_checkpoint_fallback_wal_total` alongside the per-pipeline
    /// counters. Pre-O1, these metrics ticked in-process but were
    /// invisible to Prometheus scrapers — engage_fallback's counter
    /// fires inside the FallbackCheckpointStore but the only path to
    /// observe it was unit tests.
    eo2_metrics: Arc<crate::eo2_metrics::Eo2Metrics>,
}

impl PipelineSupervisor {
    pub fn new(connectors: Arc<ConnectorRegistry>) -> Self {
        Self {
            connectors,
            running: Mutex::new(HashMap::new()),
            gate_registry: Arc::new(WriteGateRegistry::new()),
            ownership: OnceLock::new(),
            #[cfg(all(feature = "processor-auth", feature = "cluster"))]
            poh_installed_chains: OnceLock::new(),
            #[cfg(all(feature = "processor-auth", feature = "cluster"))]
            poh_live_chains: Arc::new(
                crate::partition_install::LivePohChainRegistry::new(),
            ),
            data_context_kek: OnceLock::new(),
            l2_root: OnceLock::new(),
            #[cfg(feature = "processor-auth")]
            secret_registry: OnceLock::new(),
            l3_checkpoint_store: OnceLock::new(),
            fault_injector_handle: OnceLock::new(),
            eo2_metrics: Arc::new(crate::eo2_metrics::Eo2Metrics::new()),
        }
    }

    /// O1 / EO-2 P8: shared Eo2Metrics registry. Cloned onto every
    /// pipeline's `PipelineConfig.eo2_metrics` at start so all
    /// pipelines publish into the same place. The REST `/metrics`
    /// handler reads from this for its `render_prometheus()` splice.
    pub fn eo2_metrics(&self) -> Arc<crate::eo2_metrics::Eo2Metrics> {
        Arc::clone(&self.eo2_metrics)
    }

    /// M2 / FT-3: install the node-wide L3 store used as the checkpoint
    /// backend. Intended to be called once by `cmd_serve` after the
    /// artifact directory is known. Returns `Err` if already installed —
    /// startup ordering bug, not a runtime condition. Pipelines whose
    /// manifest declares `durability.checkpoint.backend: state_store`
    /// without this installed log a warning and run without a checkpoint
    /// writer (current behaviour), which is why
    /// `aeon_pipeline_checkpoints_written_total` stayed at 0 across all
    /// runs prior to this fix.
    pub fn set_l3_checkpoint_store(
        &self,
        store: Arc<dyn aeon_types::L3Store>,
    ) -> Result<(), AeonError> {
        self.l3_checkpoint_store.set(store).map_err(|_| {
            AeonError::state(
                "PipelineSupervisor: L3 checkpoint store already installed",
            )
        })
    }

    /// W3 / F1: if the installed L3 checkpoint store is a
    /// `FaultyL3Wrapper`, return a clone (shared atomic counter). The
    /// REST handler `inject_l3_fault` calls this to arm faults from
    /// outside the engine. Returns `None` when the wrapper isn't
    /// installed — production builds + tests / benches that don't
    /// configure fault injection get a clean "not enabled" surface.
    pub fn fault_injector(&self) -> Option<crate::debug_fault::FaultyL3Wrapper> {
        // The wrapper is the outermost L3Store but `Arc<dyn L3Store>`
        // can't downcast directly (no Any supertrait), so cmd_serve
        // installs a side-channel handle via `set_fault_injector`.
        self.fault_injector_handle.get().cloned()
    }

    /// W3 / F1: install the side-channel handle to the
    /// `FaultyL3Wrapper` that wraps the installed L3 store. Called by
    /// `cmd_serve` right after `set_l3_checkpoint_store(wrapper)`.
    /// Once-only.
    pub fn set_fault_injector(
        &self,
        injector: crate::debug_fault::FaultyL3Wrapper,
    ) -> Result<(), AeonError> {
        self.fault_injector_handle.set(injector).map_err(|_| {
            AeonError::state(
                "PipelineSupervisor: fault injector already installed",
            )
        })
    }

    /// V5.1: install the node-wide `SecretRegistry`. Intended to be
    /// called once by `cmd_serve` after the secret-provider chain is
    /// composed; the same `Arc` is shared across all pipelines on this
    /// node. Returns `Err` if a registry is already installed —
    /// startup ordering bug, not a runtime condition. Pipelines that
    /// declare `poh.signing_key_ref` without this installed surface a
    /// clear `AeonError::state` at pipeline start (chain-only
    /// `Verify` + `TrustExtend` walks still work, since they require
    /// no signing key).
    #[cfg(feature = "processor-auth")]
    pub fn set_secret_registry(
        &self,
        registry: Arc<aeon_types::SecretRegistry>,
    ) -> Result<(), AeonError> {
        self.secret_registry.set(registry).map_err(|_| {
            AeonError::state(
                "PipelineSupervisor: secret registry already installed",
            )
        })
    }

    /// EO-2: install the node-wide L2 body store root. Intended to be
    /// called once by `cmd_serve` after `bootstrap_multi` finishes,
    /// using the same path the cluster crate's
    /// `L2SegmentTransferProvider` is configured with. Returns `Err`
    /// if already set — startup ordering bug. Tests / benches that
    /// don't exercise L2-requiring durability modes simply leave it
    /// unset; pipelines that declare an L2-requiring mode without a
    /// root installed surface a clear `AeonError::state` at
    /// pipeline start rather than silently passing through.
    pub fn install_l2_root(
        &self,
        root: std::path::PathBuf,
    ) -> Result<(), AeonError> {
        self.l2_root.set(root).map_err(|_| {
            AeonError::state("supervisor: L2 root already installed")
        })
    }

    /// S3: install the node-wide data-context KEK used to wrap per-
    /// segment / per-value DEKs. Intended to be called once by
    /// `cmd_serve` after the secret registry is configured; the Arc is
    /// shared across all pipelines on this node. Returns `Err` if a
    /// KEK is already installed — startup ordering bug.
    ///
    /// If this is never called, pipelines with
    /// `encryption.at_rest = required` will fail to start (by design);
    /// pipelines with `off` / `optional` still start, running plaintext.
    pub fn set_data_context_kek(&self, kek: Arc<KekHandle>) -> Result<(), AeonError> {
        self.data_context_kek.set(kek).map_err(|_| {
            AeonError::state("PipelineSupervisor: data-context KEK already installed")
        })
    }

    /// Currently-installed data-context KEK, if any. Returns a clone
    /// of the shared `Arc` so callers can hold it past the supervisor's
    /// borrow.
    pub fn data_context_kek(&self) -> Option<Arc<KekHandle>> {
        self.data_context_kek.get().cloned()
    }

    /// G11.c: install the shared `InstalledPohChainRegistry`. Called once
    /// in `cmd_serve` after the cluster-side `PartitionTransferDriver`
    /// is built, so the same registry instance the driver writes to is
    /// the one `create_poh_state` reads from on pipeline start. Returns
    /// `Err` if already installed — startup ordering bug, not a runtime
    /// condition.
    #[cfg(all(feature = "processor-auth", feature = "cluster"))]
    pub fn set_poh_installed_chains(
        &self,
        registry: Arc<crate::partition_install::InstalledPohChainRegistry>,
    ) -> Result<(), AeonError> {
        self.poh_installed_chains.set(registry).map_err(|_| {
            AeonError::state(
                "PipelineSupervisor: poh_installed_chains registry already installed",
            )
        })
    }

    pub fn connectors(&self) -> &ConnectorRegistry {
        &self.connectors
    }

    /// Shared write-gate registry. Cluster-side cutover coordinator uses
    /// this to look up `(pipeline, partition)` gates during handover.
    pub fn gate_registry(&self) -> Arc<WriteGateRegistry> {
        Arc::clone(&self.gate_registry)
    }

    /// CL-6c.4: shared registry of *live* `Arc<Mutex<PohChain>>`
    /// handles. Cluster-side `PohChainExportProvider` reads from this
    /// when a peer asks for the partition's PoH state during a CL-6
    /// handover. The supervisor stamps the same Arc onto every
    /// pipeline's `PipelineConfig.poh_live_chains` so `create_poh_state`
    /// can register the chain it produces.
    #[cfg(all(feature = "processor-auth", feature = "cluster"))]
    pub fn poh_live_chains(
        &self,
    ) -> Arc<crate::partition_install::LivePohChainRegistry> {
        Arc::clone(&self.poh_live_chains)
    }

    /// Install the cluster-ownership resolver. Intended to be called once
    /// by `cmd_serve` after `ClusterNode::bootstrap_multi`. Returns `Err`
    /// if a resolver is already installed — callers treat this as a bug
    /// (startup ordering violation), not a runtime condition.
    pub fn set_ownership_resolver(
        &self,
        resolver: Arc<dyn PartitionOwnershipResolver>,
    ) -> Result<(), AeonError> {
        self.ownership.set(resolver).map_err(|_| {
            AeonError::state(
                "PipelineSupervisor: ownership resolver already installed",
            )
        })
    }

    /// Start a pipeline from its `PipelineDefinition`. Idempotent: a second
    /// `start()` for an already-running pipeline returns `Ok(())` and reuses
    /// the existing control/metrics handles.
    ///
    /// On success, the caller (REST handler / Raft applier) is responsible
    /// for installing the returned handles into `AppState.pipeline_controls`
    /// and `pipeline_metrics` so the existing `/metrics` and upgrade endpoints
    /// can find them.
    pub async fn start(
        &self,
        def: &PipelineDefinition,
    ) -> Result<(Arc<PipelineControl>, Arc<PipelineMetrics>), AeonError> {
        let name = def.name.clone();

        {
            let running = self.running.lock().await;
            if let Some(existing) = running.get(&name) {
                return Ok((Arc::clone(&existing.control), Arc::clone(&existing.metrics)));
            }
        }

        // T0: only single-source / single-sink pipelines. Multi-* support
        // arrives with the DAG topology runner in a later phase.
        let source_cfg = def
            .sources
            .first()
            .ok_or_else(|| AeonError::config(format!("pipeline '{name}' has no sources")))?;
        let sink_cfg = def
            .sinks
            .first()
            .ok_or_else(|| AeonError::config(format!("pipeline '{name}' has no sinks")))?;

        // G1: if the manifest left `partitions` empty and a cluster
        // ownership resolver is installed, fill in this node's owned
        // slice before the factory sees the config — so cluster-aware
        // sources (Kafka today) read only their local share instead of
        // silently defaulting to `[0]`. Empty result from the resolver
        // means "this node owns nothing yet" — the factory's own
        // fallback decides what to do (Kafka logs a warn + uses `[0]`).
        let resolved_source_cfg: aeon_types::registry::SourceConfig;
        let source_cfg_ref = if source_cfg.partitions.is_empty() {
            if let Some(r) = self.ownership.get() {
                let owned = r.owned_partitions().await.unwrap_or_default();
                if !owned.is_empty() {
                    resolved_source_cfg = aeon_types::registry::SourceConfig {
                        partitions: owned,
                        ..source_cfg.clone()
                    };
                    &resolved_source_cfg
                } else {
                    source_cfg
                }
            } else {
                source_cfg
            }
        } else {
            source_cfg
        };

        // S3: resolve the pipeline's declared at-rest encryption posture
        // against the node-wide data-context KEK *before* any connector /
        // processor is built. `at_rest = required` without a KEK returns
        // an error here, so a mis-configured pipeline never opens a
        // socket or a Kafka consumer. The resolved plan rides on
        // `PipelineConfig.encryption_plan` so the EO-2 L2 registry /
        // L3 wrapper can install the KEK when it constructs them.
        let encryption_plan = resolve_encryption_plan(
            &def.encryption,
            self.data_context_kek.get().cloned(),
        )
        .map_err(|e| {
            AeonError::config(format!(
                "pipeline '{name}' refused to start: {e}"
            ))
        })?;

        // S6.9: resolve the declared compliance block against the
        // just-resolved encryption plan. A GDPR/Mixed pipeline under
        // strict enforcement refuses to start when at-rest encryption
        // is not active — tombstones cannot credibly honour Art. 17
        // when segments remain plaintext on disk and in backups. Warn
        // enforcement logs but starts. Other regimes (PCI / HIPAA /
        // None) and Off enforcement short-circuit to a no-op plan.
        let erasure_plan = resolve_erasure_plan(&def.compliance, &encryption_plan)
            .map_err(|e| {
                AeonError::config(format!(
                    "pipeline '{name}' refused to start: {e}"
                ))
            })?;

        // S5: resolve the declared retention posture. A malformed
        // `hold_after_ack` string is a hard refusal to start — a
        // pipeline that would silently wait 0 seconds (or crash on
        // first GC sweep) is worse than a clear error at boot.
        let retention_plan = resolve_retention_plan(&def.durability.retention)
            .map_err(|e| {
                AeonError::config(format!(
                    "pipeline '{name}' refused to start: {e}"
                ))
            })?;

        // S4.2: cross-cut the three resolved plans against the
        // declared compliance regime. PCI / HIPAA require
        // encryption + retention; GDPR / Mixed additionally require
        // an erasure surface. Strict enforcement aborts start on any
        // unmet precondition; warn logs each finding and boots
        // anyway; off and `regime=None` short-circuit to a no-op.
        // The individual probes (S3/S5/S6.9) have already caught
        // their own local errors — this is the single authoritative
        // check that a PCI-strict pipeline isn't running plaintext.
        validate_compliance(
            &def.compliance,
            &encryption_plan,
            &retention_plan,
            &erasure_plan,
        )
        .map_err(|e| {
            // S2.5 — audit the refusal on the dedicated channel so the
            // SIEM side sees it regardless of the `tracing` subscriber
            // configuration. `regime` + `enforcement` are stable tags,
            // safe to emit; the full finding list is already in the
            // human message of `e`.
            aeon_observability::emit_audit(
                &aeon_types::audit::AuditEvent::new(
                    aeon_observability::now_unix_nanos(),
                    aeon_types::audit::AuditCategory::Compliance,
                    "compliance.validate.denied",
                    aeon_types::audit::AuditOutcome::Denied,
                )
                .with_resource(format!("pipeline/{name}"))
                .with_detail("regime", format!("{:?}", def.compliance.regime))
                .with_detail("enforcement", format!("{:?}", def.compliance.enforcement)),
            );
            AeonError::config(format!(
                "pipeline '{name}' refused to start: {e}"
            ))
        })?;

        let source = self.connectors.build_source(source_cfg_ref)?;
        let mut sink = self.connectors.build_sink(sink_cfg)?;

        let processor = build_processor(
            &def.processor.name,
            &name,
            def.processor.tier.as_deref(),
        )?;

        // G2/CL-6: install a write-freeze gate for this pipeline's owned
        // partition. T0 supervisor is single-partition (partition 0);
        // multi-partition pipelines go through `run_multi_partition` which
        // installs its own per-partition gates. The gate Arc is stamped into
        // `PipelineConfig.write_gate` so the source loop calls `try_enter`
        // before every `next_batch`.
        let partition_id = PartitionId::new(0);
        let gate = self
            .gate_registry
            .get_or_create(&name, partition_id);

        let mut pipeline_config = pipeline_config_for(def);
        pipeline_config.partition_id = partition_id;
        pipeline_config.write_gate = Some(gate);
        // V5.1: keep `poh.partition` in lock-step with `partition_id` so
        // `create_poh_state` genesises the chain with the right partition
        // id. `pipeline_config_for` initialises `poh.partition` to
        // `PartitionId::new(0)` at config-build time; this stamp is what
        // makes single-partition supervisor pipelines produce a chain
        // keyed on the actual owned partition. Multi-partition runners
        // do their own per-task rewrite in `run_multi_partition`.
        //
        // V5.1 R1: resolve `poh.signing_key_ref` against the installed
        // `SecretRegistry` and stamp the result onto
        // `pipeline_config.poh.signing_key`. Disabled / no-ref paths
        // are no-ops; ref set without a registry, with an unknown
        // scheme, or with wrong-length material is a hard refusal,
        // matching the encryption probe's "required without KEK"
        // pattern.
        #[cfg(feature = "processor-auth")]
        if let Some(poh) = pipeline_config.poh.as_mut() {
            poh.partition = partition_id;
            let registry = self.secret_registry.get().map(|r| r.as_ref());
            poh.signing_key = crate::poh_probe::resolve_poh_signing_key(
                &def.poh,
                registry,
            )?;
        }
        // EO-2: build a `PipelineL2Registry` rooted at the supervisor's
        // installed L2 root so `MaybeL2Wrapped::wrap` engages on
        // durability modes that require body persistence. Without this,
        // `pipeline_config.l2_registry` stays `None` and OrderedBatch /
        // UnorderedBatch / PerEvent silently degrade to passthrough.
        // Discovered 2026-04-25 during V3 validation: 500K events flowed
        // cleanly with `checkpoints_written_total=1` but
        // `/app/artifacts/l2body/` was empty because the L2 body spine
        // never engaged.
        //
        // Reads `encryption_plan` + `retention_plan` before they get
        // moved into `pipeline_config`.
        if def.durability.mode.requires_l2_body_store() {
            let root = self.l2_root.get().ok_or_else(|| {
                AeonError::state(format!(
                    "pipeline '{name}' declares durability.mode={:?} which \
                     requires L2 body persistence, but no L2 root is \
                     installed on this supervisor (call \
                     PipelineSupervisor::install_l2_root from cmd_serve)",
                    def.durability.mode
                ))
            })?;
            let mut registry = crate::eo2::PipelineL2Registry::new(
                crate::delivery::L2BodyStoreConfig {
                    root: Some(root.clone()),
                    segment_bytes: crate::delivery::L2BodyStoreConfig::default()
                        .segment_bytes,
                },
            );
            // S5 retention is already resolved via `retention_plan`;
            // forward the hold so segment GC honours it.
            registry = registry.with_gc_min_hold(retention_plan.l2_hold_after_ack);
            // S3: hand the data-context KEK to the registry so newly
            // opened segments are encrypted in place when at-rest is on.
            if let Some(kek) = encryption_plan.kek.as_ref() {
                registry = registry.with_kek(Arc::clone(kek));
            }
            pipeline_config.l2_registry = Some(registry);
        }
        pipeline_config.encryption_plan = Some(encryption_plan);
        pipeline_config.retention_plan = Some(retention_plan);
        // M2 / FT-3: hand the supervisor's L3 store to the per-pipeline
        // config so the sink task's `build_sink_task_ctx` can construct a
        // real `L3CheckpointStore` when the manifest declares
        // `checkpoint.backend: state_store`. Without this clone, the
        // `StateStore` arm logs "no l3_checkpoint_store provided" and
        // falls through to a `None` writer — the pre-existing root
        // cause of `aeon_pipeline_checkpoints_written_total` staying
        // at 0 across all v3-* runs.
        if let Some(l3) = self.l3_checkpoint_store.get() {
            pipeline_config.l3_checkpoint_store = Some(Arc::clone(l3));
        }
        // O1 / EO-2 P8: stamp the shared Eo2Metrics registry onto every
        // pipeline so L2/sink/ack/fallback counters all publish into one
        // place. REST /metrics reads from `self.eo2_metrics` for its
        // render_prometheus splice. Without this clone, in-process
        // increments at sites like `FallbackCheckpointStore::engage_fallback`
        // are dead because pipeline_config.eo2_metrics stays None.
        pipeline_config.eo2_metrics = Some(Arc::clone(&self.eo2_metrics));
        // G11.c: share the process-wide PoH-chain install registry so
        // `create_poh_state` can resume a chain a recent partition-transfer
        // installed on this node instead of genesising fresh.
        #[cfg(all(feature = "processor-auth", feature = "cluster"))]
        {
            pipeline_config.poh_installed_chains =
                self.poh_installed_chains.get().cloned();
            // CL-6c.4: stamp the shared live-chain registry so
            // `create_poh_state` can register the chain it produces and
            // the source-side `PohChainExportProvider` can find it on
            // the next outbound transfer.
            pipeline_config.poh_live_chains =
                Some(Arc::clone(&self.poh_live_chains));
        }
        // P5: if the installed ownership resolver exposes a change-feed,
        // subscribe the pipeline so the source loop can `reassign_partitions`
        // without tearing down the task on a CL-6 transfer commit. Resolvers
        // without a watch (tests, benches, single-node) leave this `None` and
        // the loop falls back to the legacy resolve-once-at-start behaviour.
        if let Some(resolver) = self.ownership.get() {
            pipeline_config.partition_reassign = resolver.watch();
        }

        let metrics = Arc::new(PipelineMetrics::new());
        let control = PipelineControl::new();
        let shutdown = Arc::new(AtomicBool::new(false));

        // G4: install the ack callback so sinks that observe broker acks
        // (Kafka today; HTTP/NATS/etc as they adopt) drive the
        // `outputs_acked_total` companion metric. Sinks that don't override
        // `Sink::on_ack_callback` keep the trait's no-op default — for them
        // `outputs_acked` stays at 0 and dashboards should fall back to
        // `outputs_sent`.
        Sink::on_ack_callback(&mut sink, metrics.ack_callback());

        let metrics_task = Arc::clone(&metrics);
        let control_task = Arc::clone(&control);
        let shutdown_task = Arc::clone(&shutdown);
        let name_for_task = name.clone();

        let handle = tokio::spawn(async move {
            let result = run_buffered_managed(
                source,
                processor,
                sink,
                pipeline_config,
                metrics_task,
                shutdown_task,
                None,
                control_task,
            )
            .await;

            if let Err(ref e) = result {
                tracing::error!(
                    pipeline = %name_for_task,
                    error = %e,
                    "pipeline task exited with error"
                );
            } else {
                tracing::info!(pipeline = %name_for_task, "pipeline task exited cleanly");
            }
            result
        });

        let entry = Arc::new(RunningPipeline {
            handle: Mutex::new(Some(handle)),
            shutdown,
            control: Arc::clone(&control),
            metrics: Arc::clone(&metrics),
        });

        let mut running = self.running.lock().await;
        running.insert(name.clone(), entry);
        tracing::info!(pipeline = %name, "supervisor started pipeline");

        Ok((control, metrics))
    }

    /// Signal the pipeline task to shut down and await its `JoinHandle`. No-op
    /// if the pipeline is not running.
    pub async fn stop(&self, name: &str) -> Result<(), AeonError> {
        let entry = {
            let mut running = self.running.lock().await;
            running.remove(name)
        };

        let entry = match entry {
            Some(e) => e,
            None => return Ok(()),
        };

        entry.shutdown.store(true, Ordering::Release);

        let handle_opt = {
            let mut slot = entry.handle.lock().await;
            slot.take()
        };

        if let Some(handle) = handle_opt {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(pipeline = %name, error = %e, "pipeline returned error on stop");
                }
                Err(e) => {
                    tracing::warn!(pipeline = %name, error = %e, "pipeline task join error");
                }
            }
        }

        // G2/CL-6: drop all gates owned by this pipeline so a restart
        // starts with fresh `Open` state. If a cluster-side cutover was
        // mid-flight when stop() was called, its drain future resolves on
        // the DrainGuard drop inside the source task; the `Frozen` state
        // dies with the Arc once every holder releases.
        self.gate_registry.remove_pipeline(name);

        // CL-6c.4: drop live PoH chain handles for this pipeline so a
        // subsequent re-`start()` doesn't see stale references that
        // outlive the underlying processor task.
        #[cfg(all(feature = "processor-auth", feature = "cluster"))]
        self.poh_live_chains.remove_pipeline(name);

        tracing::info!(pipeline = %name, "supervisor stopped pipeline");
        Ok(())
    }

    /// Whether a pipeline with this name is currently running on this node.
    pub async fn is_running(&self, name: &str) -> bool {
        self.running.lock().await.contains_key(name)
    }

    /// Names of all currently-running pipelines.
    pub async fn list_running(&self) -> Vec<String> {
        let running = self.running.lock().await;
        let mut v: Vec<String> = running.keys().cloned().collect();
        v.sort();
        v
    }

    /// Reconcile against the desired set of running pipelines. Used by the
    /// Raft applier on every committed `SetPipelineState` so each node
    /// converges to the same set of locally-running tasks.
    ///
    /// `desired` is the set of `(name, definition)` pairs that should be
    /// running here. Anything currently running but not in `desired` is
    /// stopped; anything in `desired` but not running is started. Same-name
    /// entries are left untouched (config changes go through stop+start).
    pub async fn reconcile(
        &self,
        desired: &[(&str, &PipelineDefinition)],
    ) -> Result<(), AeonError> {
        let want: std::collections::BTreeSet<&str> =
            desired.iter().map(|(n, _)| *n).collect();

        let to_stop: Vec<String> = {
            let running = self.running.lock().await;
            running
                .keys()
                .filter(|k| !want.contains(k.as_str()))
                .cloned()
                .collect()
        };
        for name in to_stop {
            self.stop(&name).await?;
        }

        for (name, def) in desired {
            if !self.is_running(name).await {
                self.start(def).await?;
            }
        }
        Ok(())
    }

    /// Snapshot of every running pipeline's metrics handle. The Prometheus
    /// `/metrics` handler iterates this to expose per-pipeline counters for
    /// pipelines started through either the REST path or the Raft applier —
    /// the supervisor is the single source of truth for "what's actually
    /// running on this node", so metrics discovery piggybacks on it.
    pub async fn metrics_snapshot(&self) -> Vec<(String, Arc<PipelineMetrics>)> {
        let running = self.running.lock().await;
        running
            .iter()
            .map(|(name, entry)| (name.clone(), Arc::clone(&entry.metrics)))
            .collect()
    }

    /// Per-pipeline metrics lookup. Returns `None` if `name` is not currently
    /// running on this node. Used by the `/v1/pipelines/{name}/tail` WebSocket
    /// tick loop so it picks up a fresh metrics handle after restart.
    pub async fn get_metrics(&self, name: &str) -> Option<Arc<PipelineMetrics>> {
        let running = self.running.lock().await;
        running.get(name).map(|entry| Arc::clone(&entry.metrics))
    }

    /// Test helper — seed a metrics handle under `name` without spinning up a
    /// pipeline task. Lets rest_api tests exercise the `/metrics` handler
    /// without standing up real source/sink plumbing.
    #[cfg(test)]
    pub(crate) async fn insert_metrics_for_test(
        &self,
        name: impl Into<String>,
        metrics: Arc<PipelineMetrics>,
    ) {
        let entry = Arc::new(RunningPipeline {
            handle: Mutex::new(None),
            shutdown: Arc::new(AtomicBool::new(false)),
            control: PipelineControl::new(),
            metrics,
        });
        self.running.lock().await.insert(name.into(), entry);
    }
}

fn build_processor(
    processor_name: &str,
    pipeline_name: &str,
    processor_tier: Option<&str>,
) -> Result<Box<dyn aeon_types::Processor + Send + Sync>, AeonError> {
    // V3 native row: when the manifest declares `processor.tier = "native"`,
    // load the cdylib artifact at `${AEON_PROCESSORS_DIR}/{name}.so` (or
    // `.dll` / `.dylib` per platform). The tier is carried through from
    // `ProcessorManifest.tier` via `ProcessorRef.tier`; absent or any
    // other value falls back to the legacy passthrough path. Wasm tier is
    // intentionally still passthrough-on-supervisor: the in-process Wasm
    // path lands in a follow-up atom (different fixture, different load
    // boundary).
    if processor_tier.map(|t| t.eq_ignore_ascii_case("native")) == Some(true) {
        let dir = std::env::var("AEON_PROCESSORS_DIR")
            .unwrap_or_else(|_| "/app/processors".to_string());
        let ext = if cfg!(target_os = "windows") {
            "dll"
        } else if cfg!(target_os = "macos") {
            "dylib"
        } else {
            "so"
        };
        let path = std::path::PathBuf::from(&dir).join(format!("{processor_name}.{ext}"));
        if !path.exists() {
            return Err(AeonError::state(format!(
                "supervisor: native processor artifact not found at {} \
                 (manifest declares processor.tier=native; bundle the \
                 cdylib into the runtime image or override the directory \
                 via AEON_PROCESSORS_DIR)",
                path.display()
            )));
        }
        let proc = crate::native_loader::NativeProcessor::load(&path, &[])
            .map_err(|e| AeonError::state(format!(
                "supervisor: failed to load native processor from {}: {e}",
                path.display()
            )))?;
        tracing::info!(
            pipeline = pipeline_name,
            processor = processor_name,
            artifact = %path.display(),
            "supervisor: loaded native processor"
        );
        return Ok(Box::new(proc));
    }

    if processor_name != IDENTITY_PROCESSOR {
        tracing::debug!(
            pipeline = pipeline_name,
            processor = processor_name,
            tier = processor_tier.unwrap_or("<none>"),
            "supervisor: passthrough path (tier not yet wired or absent)"
        );
    }
    Ok(Box::new(PassthroughProcessor::new(Arc::from("output"))))
}

/// Build a `PipelineConfig` from the pipeline definition. Translates the
/// declarative `DurabilityBlock` onto runtime knobs (delivery semantics,
/// flush strategy, pipeline_name label). EO-2 L2/L3 plumbing (registry,
/// capacity, metrics, ack tracker) is left at defaults — `DurabilityMode::None`
/// pipelines do not need them, and stronger-mode wiring lands in the
/// follow-up commit that exercises the EO-2 modes end-to-end.
fn pipeline_config_for(def: &PipelineDefinition) -> PipelineConfig {
    let mut cfg = PipelineConfig {
        pipeline_name: def.name.clone(),
        ..PipelineConfig::default()
    };
    // Forward the manifest's declared durability mode onto the delivery
    // config. For T0, only `None` is fully exercised end-to-end; the other
    // modes still need their L2 registry / capacity / ack tracker wiring to
    // become observable, which lands when the matrix asks for them.
    cfg.delivery.durability = def.durability.mode;
    // M2 / FT-3: translate the manifest's declared checkpoint backend
    // (`durability.checkpoint.backend` in YAML) onto the runtime config.
    // Without this, manifests declaring `state_store` silently fell
    // through to the engine's default `Wal` backend, defeating the
    // purpose of `set_l3_checkpoint_store` and leaving the L3 redb
    // file untouched (writes went to /tmp/aeon-checkpoints/pipeline.wal
    // instead). Discovered 2026-05-01 during the C2 IOChaos run —
    // the fault injection on `/app/artifacts/l3-checkpoints.redb`
    // saw zero traffic because no checkpoint writes ever hit that file.
    use aeon_types::manifest::CheckpointBackendDecl;
    cfg.delivery.checkpoint.backend = match def.durability.checkpoint.backend {
        CheckpointBackendDecl::StateStore => crate::delivery::CheckpointBackend::StateStore,
        CheckpointBackendDecl::Wal => crate::delivery::CheckpointBackend::Wal,
        CheckpointBackendDecl::None => crate::delivery::CheckpointBackend::None,
    };
    // Q1: propagate the manifest's `durability.checkpoint.interval` onto
    // `delivery.flush.interval`. Pre-Q1 this string field was parsed
    // and round-tripped through the registry but never reached the
    // runtime, so blocking-strategy pipelines stuck at the 1-second
    // default cadence regardless of what the manifest declared. Bad
    // syntax falls back to the existing default with a warn log so a
    // typoed unit doesn't block startup.
    if let Some(interval_str) = def.durability.checkpoint.interval.as_deref() {
        match crate::retention_probe::parse_hold_duration(interval_str) {
            Ok(d) => cfg.delivery.flush.interval = d,
            Err(e) => {
                tracing::warn!(
                    pipeline = %def.name,
                    interval = %interval_str,
                    error = %e,
                    "ignoring malformed durability.checkpoint.interval; using default"
                );
            }
        }
    }
    if matches!(def.durability.mode, DurabilityMode::None) {
        // Strip whatever backend the manifest declared so the
        // at-least-once baseline isn't paying for unrelated FS work
        // in the matrix. Honors the design rule that DurabilityMode::None
        // disables checkpoint persistence regardless of the
        // `checkpoint.backend` declaration.
        cfg.delivery.checkpoint.backend = crate::delivery::CheckpointBackend::None;
        cfg.delivery.checkpoint.retention = std::time::Duration::ZERO;
    }
    // V5.1: translate the manifest's PoH posture onto runtime config.
    // The supervisor stamps the real partition id into `cfg.partition_id`
    // *after* `pipeline_config_for` returns; `create_poh_state` reads
    // `config.poh.partition` first and falls back to `config.partition_id`
    // if they disagree, but mirroring the supervisor-installed partition
    // here means the chain genesises with the correct id from the start
    // for the T0 single-partition path. Multi-partition pipelines have
    // their per-partition sub-tasks rewrite this in `run_multi_partition`.
    #[cfg(feature = "processor-auth")]
    {
        cfg.poh = if def.poh.is_active() {
            Some(crate::pipeline::PohConfig {
                partition: PartitionId::new(0),
                max_recent_entries: def.poh.max_recent_entries as usize,
                // V5.1 schema preserves `signing_key_ref` but the secret
                // resolver atom that maps it onto an `Arc<SigningKey>`
                // lands separately; chain-only verification (`Verify`,
                // `TrustExtend`) works without a signing key, so the
                // unblock is real even before the resolver is wired.
                signing_key: None,
            })
        } else {
            None
        };
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector_registry::{DynSink, DynSource, SinkFactory, SourceFactory};
    use aeon_types::partition::PartitionId;
    use aeon_types::registry::{ProcessorRef, SinkConfig, SourceConfig};
    use aeon_types::{BatchResult, Event, Output, Sink, Source};
    use bytes::Bytes;
    use std::collections::BTreeMap;

    struct CountingSource {
        remaining: usize,
    }
    impl Source for CountingSource {
        async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
            if self.remaining == 0 {
                return Ok(vec![]);
            }
            let take = self.remaining.min(64);
            self.remaining -= take;
            let v = (0..take)
                .map(|i| {
                    Event::new(
                        uuid::Uuid::nil(),
                        i as i64,
                        Arc::from("t"),
                        PartitionId::new(0),
                        Bytes::from_static(b"x"),
                    )
                })
                .collect();
            Ok(v)
        }
    }

    struct CountingSourceFactory(usize);
    impl SourceFactory for CountingSourceFactory {
        fn build(&self, _cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError> {
            Ok(Box::new(CountingSource { remaining: self.0 }))
        }
    }

    struct CapturingSink {
        seen: Arc<std::sync::atomic::AtomicU64>,
    }
    impl Sink for CapturingSink {
        async fn write_batch(
            &mut self,
            outputs: Vec<Output>,
        ) -> Result<BatchResult, AeonError> {
            self.seen
                .fetch_add(outputs.len() as u64, Ordering::Relaxed);
            Ok(BatchResult::all_delivered(
                outputs.iter().map(|_| uuid::Uuid::nil()).collect(),
            ))
        }
        async fn flush(&mut self) -> Result<(), AeonError> {
            Ok(())
        }
    }

    struct CapturingSinkFactory(Arc<std::sync::atomic::AtomicU64>);
    impl SinkFactory for CapturingSinkFactory {
        fn build(&self, _cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError> {
            Ok(Box::new(CapturingSink {
                seen: Arc::clone(&self.0),
            }))
        }
    }

    fn def_for(name: &str) -> PipelineDefinition {
        PipelineDefinition::new(
            name,
            SourceConfig {
                source_type: "count".into(),
                topic: None,
                partitions: vec![],
                config: BTreeMap::new(),
            },
            ProcessorRef::new(IDENTITY_PROCESSOR, "0.0.0"),
            SinkConfig {
                sink_type: "capture".into(),
                topic: None,
                config: BTreeMap::new(),
            },
            0,
        )
    }

    // V5.1: pipeline_config_for must translate `def.poh` onto runtime
    // `PipelineConfig.poh`. Disabled is the legacy default — `cfg.poh`
    // stays None. Enabled produces a `PohConfig` with the manifest's
    // `max_recent_entries`, ready for `create_poh_state` to genesise
    // a per-partition chain.
    #[cfg(feature = "processor-auth")]
    #[test]
    fn pipeline_config_for_default_poh_is_disabled() {
        let def = def_for("p");
        let cfg = pipeline_config_for(&def);
        assert!(cfg.poh.is_none());
    }

    #[cfg(feature = "processor-auth")]
    #[test]
    fn pipeline_config_for_enabled_poh_populates_pipeline_config() {
        let mut def = def_for("p");
        def.poh = aeon_types::poh::PohBlock {
            enabled: true,
            max_recent_entries: 4096,
            signing_key_ref: None,
        };
        let cfg = pipeline_config_for(&def);
        let poh = cfg.poh.expect("PohConfig must be populated when enabled");
        assert_eq!(poh.max_recent_entries, 4096);
        // Partition mirrors the supervisor-stamped value at start(); the
        // builder sets a placeholder of 0 here.
        assert_eq!(poh.partition.as_u16(), 0);
        assert!(poh.signing_key.is_none());
    }

    // V5.1 R1: a manifest declaring `signing_key_ref` without an installed
    // SecretRegistry must hard-fail at start() — not silently strip the
    // reference and run unsigned. Mirrors the encryption probe's
    // "required-without-KEK" behaviour. Idempotent registry install is
    // also covered.
    #[cfg(feature = "processor-auth")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_refuses_pipeline_with_signing_key_ref_when_no_registry() {
        let mut reg = ConnectorRegistry::new();
        reg.register_source("count", Arc::new(CountingSourceFactory(10)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink("capture", Arc::new(CapturingSinkFactory(counter)));

        let sup = PipelineSupervisor::new(Arc::new(reg));
        let mut def = def_for("poh-needs-key");
        def.poh = aeon_types::poh::PohBlock {
            enabled: true,
            max_recent_entries: 1024,
            signing_key_ref: Some("${ENV:AEON_POH_REQUIRED_KEY}".into()),
        };
        let err = match sup.start(&def).await {
            Err(e) => e,
            Ok(_) => panic!("start must refuse without registry"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("SecretRegistry"), "got {msg}");
    }

    #[cfg(feature = "processor-auth")]
    #[test]
    fn set_secret_registry_is_once_only() {
        let sup = PipelineSupervisor::new(Arc::new(ConnectorRegistry::new()));
        let r1 = Arc::new(aeon_types::SecretRegistry::default_local());
        sup.set_secret_registry(r1).expect("first install");
        let r2 = Arc::new(aeon_types::SecretRegistry::default_local());
        let err = match sup.set_secret_registry(r2) {
            Err(e) => e,
            Ok(_) => panic!("second install must fail"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("already installed"), "got {msg}");
    }

    /// Poll until `cond()` returns true or `max_iters` × 10ms elapse.
    /// Cheaper and more readable than sprinkling raw `sleep` loops.
    async fn wait_until<F: Fn() -> bool>(cond: F, max_iters: u32) -> bool {
        for _ in 0..max_iters {
            if cond() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cond()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_runs_source_through_sink_via_passthrough() {
        let mut reg = ConnectorRegistry::new();
        reg.register_source("count", Arc::new(CountingSourceFactory(500)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink(
            "capture",
            Arc::new(CapturingSinkFactory(Arc::clone(&counter))),
        );

        let sup = PipelineSupervisor::new(Arc::new(reg));
        let def = def_for("p1");
        let (_ctrl, _metrics) = sup.start(&def).await.expect("start");

        // CountingSource exhausts at 500 — poll the sink-side counter until
        // we observe every event landed, then stop so the JoinHandle joins.
        let ok = wait_until(|| counter.load(Ordering::Relaxed) >= 500, 500).await;
        assert!(
            ok,
            "expected 500 outputs, got {}",
            counter.load(Ordering::Relaxed)
        );
        sup.stop("p1").await.expect("stop");
        assert!(!sup.is_running("p1").await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_is_idempotent() {
        let mut reg = ConnectorRegistry::new();
        reg.register_source("count", Arc::new(CountingSourceFactory(10_000)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink("capture", Arc::new(CapturingSinkFactory(counter)));

        let sup = PipelineSupervisor::new(Arc::new(reg));
        let def = def_for("p1");
        let (c1, m1) = sup.start(&def).await.expect("start 1");
        let (c2, m2) = sup.start(&def).await.expect("start 2");
        assert!(Arc::ptr_eq(&c1, &c2));
        assert!(Arc::ptr_eq(&m1, &m2));
        sup.stop("p1").await.expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_starts_and_stops_to_match_desired() {
        let mut reg = ConnectorRegistry::new();
        // Bounded but large — shutdown-flag path drains in well under a second
        // and we never inspect the counter in this test anyway.
        reg.register_source("count", Arc::new(CountingSourceFactory(100_000)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink("capture", Arc::new(CapturingSinkFactory(counter)));

        let sup = PipelineSupervisor::new(Arc::new(reg));

        let a = def_for("a");
        let b = def_for("b");
        let c = def_for("c");

        sup.reconcile(&[("a", &a), ("b", &b)])
            .await
            .expect("reconcile 1");
        let mut got = sup.list_running().await;
        got.sort();
        assert_eq!(got, vec!["a".to_string(), "b".to_string()]);

        sup.reconcile(&[("b", &b), ("c", &c)])
            .await
            .expect("reconcile 2");
        let mut got = sup.list_running().await;
        got.sort();
        assert_eq!(got, vec!["b".to_string(), "c".to_string()]);

        sup.reconcile(&[]).await.expect("reconcile drain");
        assert!(sup.list_running().await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_source_type_returns_config_error() {
        let reg = ConnectorRegistry::new();
        let sup = PipelineSupervisor::new(Arc::new(reg));
        let def = def_for("p1");
        match sup.start(&def).await {
            Err(AeonError::Config { .. }) => {}
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    // ─── G1: cluster-ownership-aware source partition defaulting ───

    /// Source factory that records the `SourceConfig` it was handed so
    /// tests can assert the supervisor filled `partitions` from the
    /// installed ownership resolver before the factory built.
    struct RecordingSourceFactory {
        seen_partitions: Arc<std::sync::Mutex<Option<Vec<u16>>>>,
        remaining: usize,
    }
    impl SourceFactory for RecordingSourceFactory {
        fn build(&self, cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError> {
            *self.seen_partitions.lock().expect("lock") = Some(cfg.partitions.clone());
            Ok(Box::new(CountingSource {
                remaining: self.remaining,
            }))
        }
    }

    /// Stub resolver that returns a fixed owned-partitions vec.
    struct StubOwnershipResolver(Option<Vec<u16>>);
    impl crate::connector_registry::PartitionOwnershipResolver
        for StubOwnershipResolver
    {
        fn owned_partitions<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<Vec<u16>>> + Send + 'a>,
        > {
            let out = self.0.clone();
            Box::pin(async move { out })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_partitions_are_filled_from_ownership_resolver() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let mut reg = ConnectorRegistry::new();
        reg.register_source(
            "count",
            Arc::new(RecordingSourceFactory {
                seen_partitions: Arc::clone(&seen),
                remaining: 100,
            }),
        );
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink("capture", Arc::new(CapturingSinkFactory(counter)));

        let sup = PipelineSupervisor::new(Arc::new(reg));
        sup.set_ownership_resolver(Arc::new(StubOwnershipResolver(Some(vec![
            5, 11, 17,
        ]))))
        .expect("install resolver");

        let def = def_for("p1"); // partitions: vec![]
        let _ = sup.start(&def).await.expect("start");

        let got = seen
            .lock()
            .expect("lock")
            .clone()
            .expect("factory was called");
        assert_eq!(
            got,
            vec![5, 11, 17],
            "supervisor must fill empty partitions from resolver"
        );

        sup.stop("p1").await.expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_partitions_are_not_overridden_by_resolver() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let mut reg = ConnectorRegistry::new();
        reg.register_source(
            "count",
            Arc::new(RecordingSourceFactory {
                seen_partitions: Arc::clone(&seen),
                remaining: 50,
            }),
        );
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink("capture", Arc::new(CapturingSinkFactory(counter)));

        let sup = PipelineSupervisor::new(Arc::new(reg));
        sup.set_ownership_resolver(Arc::new(StubOwnershipResolver(Some(vec![
            99,
        ]))))
        .expect("install resolver");

        let mut def = def_for("p1");
        def.sources[0].partitions = vec![1, 2, 3]; // explicit

        let _ = sup.start(&def).await.expect("start");

        let got = seen.lock().expect("lock").clone().expect("called");
        assert_eq!(
            got,
            vec![1, 2, 3],
            "explicit partitions must win over resolver output"
        );
        sup.stop("p1").await.expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_partitions_without_resolver_pass_through_unchanged() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let mut reg = ConnectorRegistry::new();
        reg.register_source(
            "count",
            Arc::new(RecordingSourceFactory {
                seen_partitions: Arc::clone(&seen),
                remaining: 10,
            }),
        );
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink("capture", Arc::new(CapturingSinkFactory(counter)));

        let sup = PipelineSupervisor::new(Arc::new(reg));
        // No resolver installed — factory sees the empty list and
        // applies its own fallback (Kafka → [0] + warn).

        let def = def_for("p1");
        let _ = sup.start(&def).await.expect("start");

        let got = seen.lock().expect("lock").clone().expect("called");
        assert!(
            got.is_empty(),
            "no resolver → supervisor must pass partitions through unchanged, got {got:?}"
        );
        sup.stop("p1").await.expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolver_returning_none_preserves_empty_partitions() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let mut reg = ConnectorRegistry::new();
        reg.register_source(
            "count",
            Arc::new(RecordingSourceFactory {
                seen_partitions: Arc::clone(&seen),
                remaining: 10,
            }),
        );
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink("capture", Arc::new(CapturingSinkFactory(counter)));

        let sup = PipelineSupervisor::new(Arc::new(reg));
        sup.set_ownership_resolver(Arc::new(StubOwnershipResolver(None)))
            .expect("install resolver");

        let def = def_for("p1");
        let _ = sup.start(&def).await.expect("start");

        // Resolver said "I know nothing" — supervisor falls through,
        // factory sees an empty list and applies its own default.
        let got = seen.lock().expect("lock").clone().expect("called");
        assert!(got.is_empty(), "None from resolver must not fill partitions");
        sup.stop("p1").await.expect("stop");
    }

    #[tokio::test]
    async fn ownership_resolver_install_is_once_only() {
        let sup = PipelineSupervisor::new(Arc::new(ConnectorRegistry::new()));
        sup.set_ownership_resolver(Arc::new(StubOwnershipResolver(Some(vec![0]))))
            .expect("first install ok");
        let err = sup
            .set_ownership_resolver(Arc::new(StubOwnershipResolver(Some(vec![1]))))
            .expect_err("second install must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("already installed"),
            "error must explain the reason, got: {msg}"
        );
    }

    // ─── S3: at-rest encryption probe wired into pipeline start ───

    /// Build a real data-context `KekHandle` for tests. Mirrors the
    /// pattern in `encryption_probe::tests` / `l2_body::tests` — unique
    /// env var per call, hex-encoded 32-byte key, registered HexEnv
    /// provider.
    fn test_data_context_kek() -> Arc<aeon_crypto::kek::KekHandle> {
        use aeon_types::{
            SecretBytes, SecretError, SecretProvider, SecretRef, SecretRegistry,
            SecretScheme,
        };
        use std::sync::atomic::AtomicU64;

        struct HexEnv;
        impl SecretProvider for HexEnv {
            fn scheme(&self) -> SecretScheme {
                SecretScheme::Env
            }
            fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError> {
                let v = std::env::var(path)
                    .map_err(|_| SecretError::EnvNotSet(path.to_string()))?;
                let bytes: Vec<u8> = (0..v.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&v[i..i + 2], 16).unwrap_or(0))
                    .collect();
                Ok(SecretBytes::new(bytes))
            }
        }

        static N: AtomicU64 = AtomicU64::new(0);
        let var = format!(
            "AEON_SUP_TEST_KEK_{}",
            N.fetch_add(1, Ordering::Relaxed)
        );
        let hex: String = (0..32).map(|_| "42".to_string()).collect();
        // SAFETY: test-only env mutation, unique var per call.
        unsafe { std::env::set_var(&var, &hex) };
        let mut reg = SecretRegistry::empty();
        reg.register(Arc::new(HexEnv));
        Arc::new(aeon_crypto::kek::KekHandle::new(
            aeon_crypto::kek::KekDomain::DataContext,
            "test-sup-kek",
            SecretRef::env(&var),
            Arc::new(reg),
        ))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_refuses_when_at_rest_required_but_no_kek_installed() {
        let mut reg = ConnectorRegistry::new();
        reg.register_source("count", Arc::new(CountingSourceFactory(10)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink("capture", Arc::new(CapturingSinkFactory(counter)));

        let sup = PipelineSupervisor::new(Arc::new(reg));
        let mut def = def_for("p-req-no-kek");
        def.encryption.at_rest = aeon_types::AtRestEncryption::Required;

        // No `set_data_context_kek` call — the probe must refuse.
        let msg = match sup.start(&def).await {
            Err(e) => format!("{e}"),
            Ok(_) => panic!("required + no KEK must refuse to start"),
        };
        assert!(
            msg.contains("refused to start")
                && msg.contains("no data-context KEK"),
            "error must cite the missing KEK, got: {msg}"
        );
        assert!(
            !sup.is_running("p-req-no-kek").await,
            "refused pipeline must not show as running"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_succeeds_when_at_rest_required_and_kek_installed() {
        let mut reg = ConnectorRegistry::new();
        reg.register_source("count", Arc::new(CountingSourceFactory(50)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink(
            "capture",
            Arc::new(CapturingSinkFactory(Arc::clone(&counter))),
        );

        let sup = PipelineSupervisor::new(Arc::new(reg));
        sup.set_data_context_kek(test_data_context_kek())
            .expect("install kek");

        let mut def = def_for("p-req-kek");
        def.encryption.at_rest = aeon_types::AtRestEncryption::Required;

        let (_ctrl, _metrics) =
            sup.start(&def).await.expect("required + KEK must start");

        let ok = wait_until(|| counter.load(Ordering::Relaxed) >= 50, 500).await;
        assert!(
            ok,
            "expected 50 outputs under encrypted pipeline, got {}",
            counter.load(Ordering::Relaxed)
        );
        sup.stop("p-req-kek").await.expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_succeeds_with_at_rest_off_regardless_of_kek() {
        // The historical default path — manifest leaves `encryption`
        // inert (at_rest=off), no KEK installed — must continue to work
        // after the probe wiring. This is the S3 backward-compat gate.
        let mut reg = ConnectorRegistry::new();
        reg.register_source("count", Arc::new(CountingSourceFactory(20)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink(
            "capture",
            Arc::new(CapturingSinkFactory(Arc::clone(&counter))),
        );

        let sup = PipelineSupervisor::new(Arc::new(reg));
        // Explicitly no KEK installed.

        let def = def_for("p-off"); // encryption defaults to Off
        let (_ctrl, _metrics) =
            sup.start(&def).await.expect("off + no KEK must still start");

        let ok = wait_until(|| counter.load(Ordering::Relaxed) >= 20, 500).await;
        assert!(ok, "expected 20 outputs on plaintext pipeline");
        sup.stop("p-off").await.expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_succeeds_with_at_rest_optional_and_no_kek() {
        // `Optional` = use-KEK-if-available, plaintext otherwise. Must
        // not refuse to start; must run to completion even without a
        // KEK installed.
        let mut reg = ConnectorRegistry::new();
        reg.register_source("count", Arc::new(CountingSourceFactory(15)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink(
            "capture",
            Arc::new(CapturingSinkFactory(Arc::clone(&counter))),
        );

        let sup = PipelineSupervisor::new(Arc::new(reg));
        let mut def = def_for("p-opt-no-kek");
        def.encryption.at_rest = aeon_types::AtRestEncryption::Optional;

        let (_ctrl, _metrics) =
            sup.start(&def).await.expect("optional + no KEK must start");
        let ok = wait_until(|| counter.load(Ordering::Relaxed) >= 15, 500).await;
        assert!(ok, "expected 15 outputs on optional-but-plaintext pipeline");
        sup.stop("p-opt-no-kek").await.expect("stop");
    }

    // ─── S5: retention probe wired into pipeline start ───

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_refuses_on_malformed_hold_after_ack() {
        let mut reg = ConnectorRegistry::new();
        reg.register_source("count", Arc::new(CountingSourceFactory(5)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink("capture", Arc::new(CapturingSinkFactory(counter)));

        let sup = PipelineSupervisor::new(Arc::new(reg));
        let mut def = def_for("p-bad-hold");
        def.durability.retention.l2_body.hold_after_ack =
            Some("notaduration".into());

        let msg = match sup.start(&def).await {
            Err(e) => format!("{e}"),
            Ok(_) => panic!("malformed hold_after_ack must refuse to start"),
        };
        assert!(
            msg.contains("refused to start") && msg.contains("retention"),
            "error must cite retention parse failure, got: {msg}"
        );
        assert!(
            !sup.is_running("p-bad-hold").await,
            "refused pipeline must not show as running"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_succeeds_with_valid_retention_fields() {
        let mut reg = ConnectorRegistry::new();
        reg.register_source("count", Arc::new(CountingSourceFactory(10)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink(
            "capture",
            Arc::new(CapturingSinkFactory(Arc::clone(&counter))),
        );

        let sup = PipelineSupervisor::new(Arc::new(reg));
        let mut def = def_for("p-ret-ok");
        def.durability.retention.l2_body.hold_after_ack = Some("30s".into());
        def.durability.retention.l3_ack.max_records = Some(5_000);

        let (_ctrl, _metrics) = sup
            .start(&def)
            .await
            .expect("valid retention must start");
        let ok = wait_until(|| counter.load(Ordering::Relaxed) >= 10, 500).await;
        assert!(ok, "expected 10 outputs under retention-configured pipeline");
        sup.stop("p-ret-ok").await.expect("stop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_succeeds_with_inert_retention() {
        // Default manifest — no retention knobs set — must run as before.
        let mut reg = ConnectorRegistry::new();
        reg.register_source("count", Arc::new(CountingSourceFactory(7)));
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        reg.register_sink(
            "capture",
            Arc::new(CapturingSinkFactory(Arc::clone(&counter))),
        );

        let sup = PipelineSupervisor::new(Arc::new(reg));
        let def = def_for("p-ret-inert");
        assert!(def.durability.retention.is_default());

        let (_ctrl, _metrics) =
            sup.start(&def).await.expect("inert retention must start");
        let ok = wait_until(|| counter.load(Ordering::Relaxed) >= 7, 500).await;
        assert!(ok, "inert retention must not change behaviour");
        sup.stop("p-ret-inert").await.expect("stop");
    }

    #[tokio::test]
    async fn data_context_kek_install_is_once_only() {
        let sup = PipelineSupervisor::new(Arc::new(ConnectorRegistry::new()));
        sup.set_data_context_kek(test_data_context_kek())
            .expect("first install ok");
        let err = sup
            .set_data_context_kek(test_data_context_kek())
            .expect_err("second install must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("already installed"),
            "error must explain the reason, got: {msg}"
        );
    }
}
