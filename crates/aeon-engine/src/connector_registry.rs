//! ConnectorRegistry — runtime-addressable factory of Source/Sink connectors.
//!
//! Each connector crate registers a factory keyed by its `source_type` /
//! `sink_type` string (e.g. `"kafka"`, `"memory"`, `"blackhole"`). The
//! supervisor hands a `SourceConfig` / `SinkConfig` to the registry and
//! receives a concrete connector, type-erased behind `BoxedSourceAdapter` /
//! `BoxedSinkAdapter` so the supervisor can keep a uniform
//! `HashMap<String, ...>` without caring which connector implementation
//! it is running.
//!
//! The adapters re-implement `Source` / `Sink` so they feed back into the
//! generic `run_buffered_managed<S: Source, _, _>` pipeline runner. The hot
//! path stays monomorphized per concrete connector — only the supervisor
//! boundary carries a vtable (one indirection per `next_batch` /
//! `write_batch`, negligible vs the batch itself).
//!
//! The dyn-compatible shim (`DynSource` / `DynSink`) is required because
//! the `Source`/`Sink` traits use `impl Future` in their method signatures
//! (Rust 2024 edition native async-in-traits), which is not object-safe on
//! its own.

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
