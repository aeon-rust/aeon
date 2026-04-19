//! Integration test: full manifest → runtime bridge.
//!
//! Exercises the path every node in the cluster now uses when a
//! `SetPipelineState::Running` command commits (whether from REST or Raft):
//!
//! 1. Build a `ConnectorRegistry` with real memory/blackhole factories.
//! 2. Build a `PipelineSupervisor` over that registry.
//! 3. Call `supervisor.start(&definition)` with a memory→blackhole pipeline.
//! 4. Assert events actually flow through — the sink's counter reaches
//!    the declared event count — then call `stop()` and confirm the
//!    supervisor releases the entry.
//!
//! Covers the T0 isolation-matrix row "memory → passthrough → blackhole"
//! end-to-end. The unit tests in `pipeline_supervisor::tests` use stub
//! connectors; this test uses the same factories the binary registers
//! in `cmd_serve`, so a regression in the factory glue is caught here
//! rather than only at deployment time.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aeon_connectors::{BlackholeSink, MemorySource};
use aeon_engine::{
    ConnectorRegistry, DynSink, DynSource, PipelineSupervisor, SinkFactory, SourceFactory,
};
use aeon_types::registry::{
    PipelineDefinition, ProcessorRef, SinkConfig, SourceConfig,
};
use aeon_types::{
    AeonError, BatchResult, Event, Output, PartitionId, Sink,
};
use bytes::Bytes;

/// Factory that produces a `MemorySource` preloaded with N identical events.
/// We use nil UUIDs — the pipeline doesn't care about uniqueness at this
/// level; downstream dedup features are exercised in dedicated EO-2 tests.
struct MemorySourceFactory {
    count: usize,
    batch_size: usize,
}

impl SourceFactory for MemorySourceFactory {
    fn build(&self, _cfg: &SourceConfig) -> Result<Box<dyn DynSource>, AeonError> {
        let source_name: Arc<str> = Arc::from("memory");
        let partition = PartitionId::new(0);
        let payload = Bytes::from_static(&[0u8; 64]);
        let events: Vec<Event> = (0..self.count)
            .map(|i| {
                Event::new(
                    uuid::Uuid::nil(),
                    i as i64,
                    Arc::clone(&source_name),
                    partition,
                    payload.clone(),
                )
            })
            .collect();
        Ok(Box::new(MemorySource::new(events, self.batch_size)))
    }
}

/// Counting sink adapter: wraps a `BlackholeSink` but also bumps an
/// external counter on every write so the test can observe throughput.
struct CountingBlackholeSink {
    inner: BlackholeSink,
    counter: Arc<AtomicU64>,
}

impl Sink for CountingBlackholeSink {
    async fn write_batch(
        &mut self,
        outputs: Vec<Output>,
    ) -> Result<BatchResult, AeonError> {
        let len = outputs.len() as u64;
        let result = self.inner.write_batch(outputs).await?;
        self.counter.fetch_add(len, Ordering::Relaxed);
        Ok(result)
    }
    async fn flush(&mut self) -> Result<(), AeonError> {
        self.inner.flush().await
    }
}

struct CountingBlackholeSinkFactory {
    counter: Arc<AtomicU64>,
}

impl SinkFactory for CountingBlackholeSinkFactory {
    fn build(&self, _cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError> {
        Ok(Box::new(CountingBlackholeSink {
            inner: BlackholeSink::new(),
            counter: Arc::clone(&self.counter),
        }))
    }
}

fn pipeline_def(name: &str) -> PipelineDefinition {
    PipelineDefinition::new(
        name,
        SourceConfig {
            source_type: "memory".into(),
            topic: None,
            partitions: vec![],
            config: Default::default(),
        },
        ProcessorRef::new(aeon_engine::IDENTITY_PROCESSOR, "0.0.0"),
        SinkConfig {
            sink_type: "blackhole".into(),
            topic: None,
            config: Default::default(),
        },
        0,
    )
}

async fn wait_for<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cond()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_runs_memory_to_blackhole_end_to_end() {
    const EVENT_COUNT: usize = 10_000;

    let counter = Arc::new(AtomicU64::new(0));
    let mut reg = ConnectorRegistry::new();
    reg.register_source(
        "memory",
        Arc::new(MemorySourceFactory {
            count: EVENT_COUNT,
            batch_size: 512,
        }),
    );
    reg.register_sink(
        "blackhole",
        Arc::new(CountingBlackholeSinkFactory {
            counter: Arc::clone(&counter),
        }),
    );

    let sup = PipelineSupervisor::new(Arc::new(reg));
    let def = pipeline_def("mem-to-bh");

    let (_control, metrics) = sup.start(&def).await.expect("start");
    assert!(sup.is_running("mem-to-bh").await);

    let drained = wait_for(
        || counter.load(Ordering::Relaxed) as usize >= EVENT_COUNT,
        Duration::from_secs(10),
    )
    .await;

    sup.stop("mem-to-bh").await.expect("stop");
    assert!(!sup.is_running("mem-to-bh").await);

    assert!(
        drained,
        "expected {EVENT_COUNT} outputs, got {}",
        counter.load(Ordering::Relaxed)
    );
    // Every event should have made it through passthrough at least once.
    assert!(
        metrics.events_processed.load(Ordering::Relaxed) as usize >= EVENT_COUNT,
        "events_processed = {}, want ≥ {EVENT_COUNT}",
        metrics.events_processed.load(Ordering::Relaxed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_reconcile_stops_removed_pipeline() {
    // Two bounded pipelines with distinct counters so we can assert each
    // independently. Both run the same memory→blackhole topology.
    let counter_a = Arc::new(AtomicU64::new(0));
    let counter_b = Arc::new(AtomicU64::new(0));
    let mut reg = ConnectorRegistry::new();
    reg.register_source(
        "memory",
        Arc::new(MemorySourceFactory {
            count: 50_000,
            batch_size: 1024,
        }),
    );

    // Sink factory variant that picks a counter based on the pipeline's
    // sink topic field — a cheap way to route two pipelines through a
    // shared factory instance while still observing per-pipeline counts.
    struct RoutingSinkFactory {
        a: Arc<AtomicU64>,
        b: Arc<AtomicU64>,
    }
    impl SinkFactory for RoutingSinkFactory {
        fn build(&self, cfg: &SinkConfig) -> Result<Box<dyn DynSink>, AeonError> {
            let counter = match cfg.topic.as_deref() {
                Some("a") => Arc::clone(&self.a),
                Some("b") => Arc::clone(&self.b),
                _ => Arc::new(AtomicU64::new(0)),
            };
            Ok(Box::new(CountingBlackholeSink {
                inner: BlackholeSink::new(),
                counter,
            }))
        }
    }
    reg.register_sink(
        "blackhole",
        Arc::new(RoutingSinkFactory {
            a: Arc::clone(&counter_a),
            b: Arc::clone(&counter_b),
        }),
    );

    let sup = PipelineSupervisor::new(Arc::new(reg));

    let mut a = pipeline_def("a");
    a.sinks[0].topic = Some("a".into());
    let mut b = pipeline_def("b");
    b.sinks[0].topic = Some("b".into());

    sup.reconcile(&[("a", &a), ("b", &b)])
        .await
        .expect("reconcile 1");
    let mut running = sup.list_running().await;
    running.sort();
    assert_eq!(running, vec!["a".to_string(), "b".to_string()]);

    // Remove "a" — reconcile should stop it. "b" should keep running.
    sup.reconcile(&[("b", &b)]).await.expect("reconcile 2");
    assert!(!sup.is_running("a").await);
    assert!(sup.is_running("b").await);

    // Tear "b" down explicitly so the test doesn't leak tasks.
    sup.reconcile(&[]).await.expect("reconcile 3");
    assert!(sup.list_running().await.is_empty());
}
