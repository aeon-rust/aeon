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

use aeon_types::durability::DurabilityMode;
use aeon_types::error::AeonError;
use aeon_types::registry::PipelineDefinition;
use aeon_types::traits::Sink;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::connector_registry::{ConnectorRegistry, PartitionOwnershipResolver};
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
        }
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

        let source = self.connectors.build_source(source_cfg_ref)?;
        let mut sink = self.connectors.build_sink(sink_cfg)?;

        let processor = build_processor(&def.processor.name, &name)?;

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
        // G11.c: share the process-wide PoH-chain install registry so
        // `create_poh_state` can resume a chain a recent partition-transfer
        // installed on this node instead of genesising fresh.
        #[cfg(all(feature = "processor-auth", feature = "cluster"))]
        {
            pipeline_config.poh_installed_chains =
                self.poh_installed_chains.get().cloned();
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
) -> Result<Box<dyn aeon_types::Processor + Send + Sync>, AeonError> {
    // T0: every name resolves to PassthroughProcessor. Real Wasm/native
    // instantiation is layered on top of this in the next phase via the
    // existing `ProcessorRegistry::load_artifact` path; until then the
    // explicit `__identity` sentinel documents intent and keeps the matrix
    // self-consistent.
    if processor_name != IDENTITY_PROCESSOR {
        tracing::debug!(
            pipeline = pipeline_name,
            processor = processor_name,
            "supervisor: T0 path resolves all processor names to PassthroughProcessor"
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
    if matches!(def.durability.mode, DurabilityMode::None) {
        // Strip the default WAL checkpoint backend so the at-least-once
        // baseline isn't paying for unrelated FS work in the matrix.
        cfg.delivery.checkpoint.backend = crate::delivery::CheckpointBackend::None;
        cfg.delivery.checkpoint.retention = std::time::Duration::ZERO;
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
}
