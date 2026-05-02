//! ConnectorRegistry — runtime-addressable factory of Source/Sink connectors.
//!
//! ## Dispatch tiers (per CLAUDE.md rule 9)
//!
//! Aeon mandates static dispatch on the per-event hot path and accepts
//! `dyn Trait` only at coarse-grained boundaries. This module is one
//! such boundary, and the dispatch story across it is:
//!
//! 1. **Connector construction** (cold, init-time): YAML manifest →
//!    `ConnectorRegistry::build_source(&cfg) -> Box<dyn DynSource>`.
//!    Vtable cost: one per pipeline start, paid once.
//! 2. **Adapter wrap** (cold, init-time): `BoxedSourceAdapter`
//!    re-implements `Source` so the trait-object connector can be fed
//!    into the generic runner. The adapter's `Source::next_batch`
//!    forwards to the inner `Box<dyn DynSource>::next_batch_dyn`.
//!    Vtable cost: one per *batch* (not per event) — at typical
//!    `batch_size = 1024`, this is ~1000× amortised vs per-event.
//! 3. **Pipeline loop** (hot, per-batch): `run_buffered_managed<S: Source, ...>`
//!    monomorphizes on `S = BoxedSourceAdapter`. From this point on,
//!    every per-event op (the inner SPSC ring writes, the per-event
//!    `L2WritingSource` wrap, the `Output` construction) is static
//!    dispatch. The trait object lives only inside the adapter,
//!    invisible to the loop.
//!
//! In other words: **one vtable hop per batch; zero per event**.
//!
//! ## Why the DynSource shim exists
//!
//! The `Source`/`Sink` traits in `aeon-types::traits` use `impl Future`
//! in their method signatures (Rust 2024 edition native async-in-traits).
//! This makes them ergonomic for connector authors but not directly
//! object-safe — `Box<dyn Source>` won't compile because the trait isn't
//! dyn-compatible. The `DynSource` / `DynSink` shims wrap `next_batch`
//! to return `Pin<Box<dyn Future<...>>>`, which IS object-safe, at the
//! cost of one heap allocation per batch (again, amortised against
//! 256–1024 events per batch).
//!
//! ## When to construct directly vs go through the registry
//!
//! Production / cluster code MUST use the registry path so config
//! validation, feature probing, and auth setup all happen uniformly.
//! Tests and benches MAY construct concrete connector types directly
//! (e.g. `MemorySource::new(events, batch_size)`) — those types
//! implement `Source` / `Sink` and feed into `run_buffered<S: Source, ...>`
//! without any vtable hop at all.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aeon_types::delivery::BatchResult;
use aeon_types::error::AeonError;
use aeon_types::event::{Event, Output};
use aeon_types::registry::{SinkConfig, SourceConfig};
use aeon_types::traits::{Sink, SinkAckCallback, Source, SourceKind};

// ─── Dyn-compatible shims ──────────────────────────────────────────────────

/// Object-safe mirror of `Source`. Produced by every `Source` via blanket
/// impl; carried at the registry boundary as `Box<dyn DynSource>`.
pub trait DynSource: Send + Sync {
    fn next_batch_boxed<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Event>, AeonError>> + Send + 'a>>;

    fn source_kind(&self) -> SourceKind;

    fn supports_broker_event_time(&self) -> bool;

    fn pause_boxed<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    fn resume_boxed<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    fn reassign_partitions_boxed<'a>(
        &'a mut self,
        partitions: &'a [u16],
    ) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + 'a>>;

    fn broker_coordinated_partitions(&self) -> bool;
}

impl<S: Source + 'static> DynSource for S {
    fn next_batch_boxed<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Event>, AeonError>> + Send + 'a>> {
        Box::pin(self.next_batch())
    }

    fn source_kind(&self) -> SourceKind {
        Source::source_kind(self)
    }

    fn supports_broker_event_time(&self) -> bool {
        Source::supports_broker_event_time(self)
    }

    fn pause_boxed<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(self.pause())
    }

    fn resume_boxed<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(self.resume())
    }

    fn reassign_partitions_boxed<'a>(
        &'a mut self,
        partitions: &'a [u16],
    ) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + 'a>> {
        Box::pin(self.reassign_partitions(partitions))
    }

    fn broker_coordinated_partitions(&self) -> bool {
        Source::broker_coordinated_partitions(self)
    }
}

/// Object-safe mirror of `Sink`.
pub trait DynSink: Send + Sync {
    fn write_batch_boxed<'a>(
        &'a mut self,
        outputs: Vec<Output>,
    ) -> Pin<Box<dyn Future<Output = Result<BatchResult, AeonError>> + Send + 'a>>;

    fn flush_boxed<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + 'a>>;

    /// Forward `Sink::on_ack_callback` through the dyn boundary.
    fn on_ack_callback_dyn(&mut self, cb: SinkAckCallback);
}

impl<S: Sink + 'static> DynSink for S {
    fn write_batch_boxed<'a>(
        &'a mut self,
        outputs: Vec<Output>,
    ) -> Pin<Box<dyn Future<Output = Result<BatchResult, AeonError>> + Send + 'a>> {
        Box::pin(self.write_batch(outputs))
    }

    fn flush_boxed<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AeonError>> + Send + 'a>> {
        Box::pin(self.flush())
    }

    fn on_ack_callback_dyn(&mut self, cb: SinkAckCallback) {
        Sink::on_ack_callback(self, cb);
    }
}

// ─── Adapters: Box<dyn> → trait impl ───────────────────────────────────────

/// Wraps `Box<dyn DynSource>` into a value that implements the original
/// `Source` trait, so it can be fed to generic pipeline runners.
pub struct BoxedSourceAdapter(pub Box<dyn DynSource>);

impl Source for BoxedSourceAdapter {
    fn next_batch(
        &mut self,
    ) -> impl Future<Output = Result<Vec<Event>, AeonError>> + Send {
        self.0.next_batch_boxed()
    }

    fn source_kind(&self) -> SourceKind {
        self.0.source_kind()
    }

    fn supports_broker_event_time(&self) -> bool {
        self.0.supports_broker_event_time()
    }

    fn pause(&mut self) -> impl Future<Output = ()> + Send {
        self.0.pause_boxed()
    }

    fn resume(&mut self) -> impl Future<Output = ()> + Send {
        self.0.resume_boxed()
    }

    fn reassign_partitions(
        &mut self,
        partitions: &[u16],
    ) -> impl Future<Output = Result<(), AeonError>> + Send {
        let owned = partitions.to_vec();
        async move { self.0.reassign_partitions_boxed(&owned).await }
    }

    fn broker_coordinated_partitions(&self) -> bool {
        self.0.broker_coordinated_partitions()
    }
}

/// Wraps `Box<dyn DynSink>` into a value that implements the original
/// `Sink` trait.
pub struct BoxedSinkAdapter(pub Box<dyn DynSink>);

impl Sink for BoxedSinkAdapter {
    fn write_batch(
        &mut self,
        outputs: Vec<Output>,
    ) -> impl Future<Output = Result<BatchResult, AeonError>> + Send {
        self.0.write_batch_boxed(outputs)
    }

    fn flush(&mut self) -> impl Future<Output = Result<(), AeonError>> + Send {
        self.0.flush_boxed()
    }

    fn on_ack_callback(&mut self, cb: SinkAckCallback) {
        self.0.on_ack_callback_dyn(cb);
    }
}

// ─── Factories ─────────────────────────────────────────────────────────────

/// Builds a source from a `SourceConfig`. One implementation per connector
/// type — registered under the connector's `source_type` key.
pub trait SourceFactory: Send + Sync {
    fn build(&self, cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError>;
}

/// Builds a sink from a `SinkConfig`. One implementation per connector type.
pub trait SinkFactory: Send + Sync {
    fn build(&self, cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError>;
}

// ─── Partition-ownership resolver ──────────────────────────────────────────

/// Supplies the set of partition ids this node currently owns (per the
/// cluster's replicated `PartitionTable`). Queried by the supervisor when
/// a source manifest leaves its `partitions` list empty — lets cluster-
/// aware sources (Kafka today, other partitioned pulls tomorrow) read
/// only the slice this node is responsible for rather than silently
/// falling back to `[0]` (the G1 under-read from DOKS Session A).
///
/// Keyless by design: the current runtime hosts one pipeline per node.
/// Multi-pipeline support (P5) will add a pipeline parameter.
///
/// Method returns a boxed future for dyn-compat (mirrors the pattern
/// used by `CutoverCoordinator`). Returning `None` means "this node has
/// no committed ownership yet" — the caller must decide whether to
/// fall back (single-node / pre-cluster paths) or fail loudly.
pub trait PartitionOwnershipResolver: Send + Sync {
    fn owned_partitions<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Option<Vec<u16>>> + Send + 'a>>;

    /// P5: optional change-feed of the owned-partitions slice for this node.
    ///
    /// When `Some`, the supervisor clones the receiver into each pipeline's
    /// `PipelineConfig.partition_reassign` and the source loop re-assigns
    /// the live source on every committed ownership change — no pipeline
    /// restart. `None` keeps the legacy "resolve once at start" behaviour
    /// (single-node tests, pre-cluster benches).
    ///
    /// Default: returns `None`. Cluster-backed resolvers that expose a Raft
    /// watch (see `ClusterPartitionOwnership`) override this.
    fn watch(&self) -> Option<tokio::sync::watch::Receiver<Vec<u16>>> {
        None
    }
}

// ─── Registry ──────────────────────────────────────────────────────────────

/// Maps connector-type strings to their factories. Constructed by
/// `cmd_serve` with the set of connectors compiled into the binary;
/// handed as `Arc<ConnectorRegistry>` to the supervisor.
#[derive(Default)]
pub struct ConnectorRegistry {
    sources: HashMap<String, Arc<dyn SourceFactory>>,
    sinks: HashMap<String, Arc<dyn SinkFactory>>,
}

impl ConnectorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a source factory under `source_type`. Overwrites any existing
    /// entry — last registration wins, which lets callers override defaults.
    pub fn register_source(&mut self, source_type: &str, factory: Arc<dyn SourceFactory>) {
        self.sources.insert(source_type.to_string(), factory);
    }

    /// Register a sink factory under `sink_type`.
    pub fn register_sink(&mut self, sink_type: &str, factory: Arc<dyn SinkFactory>) {
        self.sinks.insert(sink_type.to_string(), factory);
    }

    /// Build a source from config. Returns `Config` error if the type is
    /// unknown — the supervisor converts this into a pipeline-start failure.
    pub fn build_source(
        &self,
        cfg: &SourceConfig,
    ) -> Result<BoxedSourceAdapter, AeonError> {
        let factory = self.sources.get(&cfg.source_type).ok_or_else(|| {
            AeonError::config(format!(
                "unknown source type '{}' — registered: [{}]",
                cfg.source_type,
                self.source_types().join(", ")
            ))
        })?;
        Ok(BoxedSourceAdapter(factory.build(cfg)?))
    }

    /// Build a sink from config.
    pub fn build_sink(&self, cfg: &SinkConfig) -> Result<BoxedSinkAdapter, AeonError> {
        let factory = self.sinks.get(&cfg.sink_type).ok_or_else(|| {
            AeonError::config(format!(
                "unknown sink type '{}' — registered: [{}]",
                cfg.sink_type,
                self.sink_types().join(", ")
            ))
        })?;
        Ok(BoxedSinkAdapter(factory.build(cfg)?))
    }

    /// List of registered source type keys — used in error messages.
    pub fn source_types(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sources.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn sink_types(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sinks.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn has_source(&self, source_type: &str) -> bool {
        self.sources.contains_key(source_type)
    }

    pub fn has_sink(&self, sink_type: &str) -> bool {
        self.sinks.contains_key(sink_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::partition::PartitionId;
    use bytes::Bytes;
    use std::collections::BTreeMap;
    use std::sync::Arc as StdArc;

    struct FixedSource {
        events: Vec<Event>,
    }
    impl Source for FixedSource {
        async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
            Ok(std::mem::take(&mut self.events))
        }
    }

    struct FixedSourceFactory;
    impl SourceFactory for FixedSourceFactory {
        fn build(&self, _cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError> {
            Ok(Box::new(FixedSource {
                events: vec![Event::new(
                    uuid::Uuid::nil(),
                    0,
                    StdArc::from("test"),
                    PartitionId::new(0),
                    Bytes::from_static(b"hello"),
                )],
            }))
        }
    }

    struct DroppingSink;
    impl Sink for DroppingSink {
        async fn write_batch(
            &mut self,
            outputs: Vec<Output>,
        ) -> Result<BatchResult, AeonError> {
            Ok(BatchResult::all_delivered(
                outputs.iter().map(|_| uuid::Uuid::nil()).collect(),
            ))
        }
        async fn flush(&mut self) -> Result<(), AeonError> {
            Ok(())
        }
    }

    struct DroppingSinkFactory;
    impl SinkFactory for DroppingSinkFactory {
        fn build(&self, _cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError> {
            Ok(Box::new(DroppingSink))
        }
    }

    #[tokio::test]
    async fn registry_builds_and_runs_a_source_through_the_adapter() {
        let mut reg = ConnectorRegistry::new();
        reg.register_source("fixed", Arc::new(FixedSourceFactory));
        let cfg = SourceConfig {
            source_type: "fixed".into(),
            topic: None,
            partitions: vec![],
            config: BTreeMap::new(),
        };
        let mut src = reg.build_source(&cfg).unwrap();
        let batch = src.next_batch().await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].payload.as_ref(), b"hello");
    }

    #[tokio::test]
    async fn registry_builds_and_runs_a_sink_through_the_adapter() {
        let mut reg = ConnectorRegistry::new();
        reg.register_sink("drop", Arc::new(DroppingSinkFactory));
        let cfg = SinkConfig {
            sink_type: "drop".into(),
            topic: None,
            config: BTreeMap::new(),
        };
        let mut sink = reg.build_sink(&cfg).unwrap();
        let result = sink
            .write_batch(vec![Output::new(
                StdArc::from("dest"),
                Bytes::from_static(b"x"),
            )])
            .await
            .unwrap();
        assert_eq!(result.delivered.len(), 1);
    }

    #[test]
    fn unknown_type_returns_config_error() {
        let reg = ConnectorRegistry::new();
        let cfg = SourceConfig {
            source_type: "missing".into(),
            topic: None,
            partitions: vec![],
            config: BTreeMap::new(),
        };
        match reg.build_source(&cfg) {
            Err(AeonError::Config { .. }) => {}
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    /// G4: a sink that records every ack-callback invocation so we can
    /// assert the trait default and the dyn-forward both work.
    struct AckSpySink {
        cb: Option<SinkAckCallback>,
    }
    impl Sink for AckSpySink {
        async fn write_batch(
            &mut self,
            outputs: Vec<Output>,
        ) -> Result<BatchResult, AeonError> {
            // Fire the installed callback for every output so the test can
            // observe the count that bubbles up through `BoxedSinkAdapter`.
            if let Some(cb) = self.cb.as_ref() {
                cb(outputs.len());
            }
            Ok(BatchResult::all_delivered(
                outputs.iter().map(|_| uuid::Uuid::nil()).collect(),
            ))
        }
        async fn flush(&mut self) -> Result<(), AeonError> {
            Ok(())
        }
        fn on_ack_callback(&mut self, cb: SinkAckCallback) {
            self.cb = Some(cb);
        }
    }

    #[tokio::test]
    async fn boxed_sink_adapter_forwards_on_ack_callback_through_dyn_boundary() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        // Wrap an AckSpySink behind the same dyn-erasure path the supervisor uses.
        let inner: Box<dyn DynSink> = Box::new(AckSpySink { cb: None });
        let mut adapter = BoxedSinkAdapter(inner);

        // Counter shared with the callback so we can verify forwarding.
        let counter = StdArc::new(AtomicUsize::new(0));
        let counter_for_cb = StdArc::clone(&counter);
        let cb: SinkAckCallback =
            StdArc::new(move |n| { counter_for_cb.fetch_add(n, AtomicOrdering::Relaxed); });

        // Install through the adapter — must reach the inner spy via
        // `DynSink::on_ack_callback_dyn` → `Sink::on_ack_callback`.
        Sink::on_ack_callback(&mut adapter, cb);

        // Drive a batch through the adapter — the spy fires the callback
        // with `outputs.len()`, which the counter should now reflect.
        let outputs: Vec<Output> = (0..5)
            .map(|_| Output::new(StdArc::from("dest"), Bytes::from_static(b"x")))
            .collect();
        let result = adapter.write_batch(outputs).await.unwrap();
        assert_eq!(result.delivered.len(), 5);
        assert_eq!(counter.load(AtomicOrdering::Relaxed), 5);
    }

    #[tokio::test]
    async fn dyn_sink_default_on_ack_callback_is_no_op_for_non_overriding_sinks() {
        // DroppingSink does not override `on_ack_callback`; installing a
        // callback through the dyn boundary must not crash and the callback
        // simply never fires.
        let inner: Box<dyn DynSink> = Box::new(DroppingSink);
        let mut adapter = BoxedSinkAdapter(inner);

        let fired = StdArc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_for_cb = StdArc::clone(&fired);
        let cb: SinkAckCallback = StdArc::new(move |_| {
            fired_for_cb.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        Sink::on_ack_callback(&mut adapter, cb);
        let _ = adapter
            .write_batch(vec![Output::new(
                StdArc::from("dest"),
                Bytes::from_static(b"x"),
            )])
            .await
            .unwrap();
        assert!(
            !fired.load(std::sync::atomic::Ordering::Relaxed),
            "default trait impl must not fire the engine callback"
        );
    }
}
