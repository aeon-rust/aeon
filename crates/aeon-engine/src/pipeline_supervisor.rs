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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aeon_types::durability::DurabilityMode;
use aeon_types::error::AeonError;
use aeon_types::registry::PipelineDefinition;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::connector_registry::ConnectorRegistry;
use crate::pipeline::{
    PipelineConfig, PipelineControl, PipelineMetrics, run_buffered_managed,
};
use crate::processor::PassthroughProcessor;

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
}

impl PipelineSupervisor {
    pub fn new(connectors: Arc<ConnectorRegistry>) -> Self {
        Self {
            connectors,
            running: Mutex::new(HashMap::new()),
        }
    }

    pub fn connectors(&self) -> &ConnectorRegistry {
        &self.connectors
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

        let source = self.connectors.build_source(source_cfg)?;
        let sink = self.connectors.build_sink(sink_cfg)?;

        let processor = build_processor(&def.processor.name, &name)?;

        let pipeline_config = pipeline_config_for(def);

        let metrics = Arc::new(PipelineMetrics::new());
        let control = PipelineControl::new();
        let shutdown = Arc::new(AtomicBool::new(false));

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
}
