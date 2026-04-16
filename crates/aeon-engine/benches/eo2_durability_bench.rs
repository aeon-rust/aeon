//! EO-2 Durability Benchmarks — P10
//!
//! Measures the cost of the durability stack across four dimensions:
//!
//! 1. **Per-event overhead by durability mode** — `None` vs `OrderedBatch`
//!    through a blackhole sink, isolating L2 write + checkpoint cost.
//! 2. **L2 body-store append throughput** — raw `L2BodyStore::append` at
//!    various payload sizes to establish the write ceiling.
//! 3. **Checkpoint cadence fsync amortisation** — pipeline throughput at
//!    1ms / 10ms / 100ms / 500ms flush intervals.
//! 4. **Capacity backpressure overhead** — pipeline with vs without
//!    `PipelineCapacity` tracking enabled.
//!
//! Run: `cargo bench -p aeon-engine --bench eo2_durability_bench`

use aeon_connectors::BlackholeSink;
use aeon_engine::delivery::L2BodyStoreConfig;
use aeon_engine::eo2::PipelineL2Registry;
use aeon_engine::l2_body::L2BodyStore;
use aeon_engine::{PassthroughProcessor, PipelineConfig, PipelineMetrics, run_buffered};
use aeon_types::event::Event;
use aeon_types::{DurabilityMode, PartitionId, SourceKind};
use bytes::Bytes;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

fn make_events(n: usize, payload_size: usize) -> Vec<Event> {
    let src: Arc<str> = Arc::from("bench-source");
    let payload = Bytes::from(vec![b'x'; payload_size]);
    (0..n)
        .map(|i| {
            Event::new(
                uuid::Uuid::now_v7(),
                i as i64,
                Arc::clone(&src),
                PartitionId::new(0),
                payload.clone(),
            )
        })
        .collect()
}

struct OneShotPushSource {
    batch: Option<Vec<Event>>,
}

impl aeon_types::traits::Source for OneShotPushSource {
    fn next_batch(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Vec<Event>, aeon_types::AeonError>> + Send {
        let b = self.batch.take().unwrap_or_default();
        async move { Ok(b) }
    }
    fn source_kind(&self) -> SourceKind {
        SourceKind::Push
    }
}

// ── Benchmark 1: Per-event overhead by durability mode ─────────────────

fn durability_mode_overhead(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("eo2_durability_mode");
    let event_count = 10_000usize;
    group.throughput(Throughput::Elements(event_count as u64));

    for mode_name in ["none", "ordered_batch"] {
        group.bench_function(BenchmarkId::new("mode", mode_name), |b| {
            b.iter_batched(
                || {
                    let events = make_events(event_count, 256);
                    let tmp = tempfile::tempdir().unwrap();
                    let registry = PipelineL2Registry::new(L2BodyStoreConfig {
                        root: Some(tmp.path().to_path_buf()),
                        segment_bytes: 64 * 1024 * 1024,
                    });

                    let mut config = PipelineConfig::default();
                    config.pipeline_name = "eo2-bench".into();
                    config.delivery.flush.interval = Duration::from_millis(10);
                    match mode_name {
                        "none" => {
                            config.delivery.durability = DurabilityMode::None;
                        }
                        "ordered_batch" => {
                            config.delivery.durability = DurabilityMode::OrderedBatch;
                            config.l2_registry = Some(registry.clone());
                        }
                        _ => unreachable!(),
                    }
                    (events, config, tmp)
                },
                |(events, config, _tmp)| {
                    rt.block_on(async {
                        let source = OneShotPushSource {
                            batch: Some(events),
                        };
                        let processor = PassthroughProcessor::new(Arc::from("out"));
                        let sink = BlackholeSink::new();
                        let metrics = Arc::new(PipelineMetrics::new());
                        let shutdown = Arc::new(AtomicBool::new(false));
                        run_buffered(source, processor, sink, config, metrics, shutdown, None)
                            .await
                            .unwrap();
                    });
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

// ── Benchmark 2: L2 body-store raw append throughput ───────────────────

fn l2_append_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("eo2_l2_append");

    for &payload_size in &[64usize, 256, 1024, 4096] {
        let event_count = 10_000usize;
        group.throughput(Throughput::Elements(event_count as u64));

        group.bench_function(
            BenchmarkId::new("payload_bytes", payload_size),
            |b| {
                b.iter_batched(
                    || {
                        let events = make_events(event_count, payload_size);
                        let tmp = tempfile::tempdir().unwrap();
                        let dir = tmp.path().join("l2-bench");
                        std::fs::create_dir_all(&dir).unwrap();
                        let store = L2BodyStore::open(
                            dir,
                            aeon_engine::l2_body::L2BodyConfig {
                                segment_bytes: 256 * 1024 * 1024,
                            },
                        )
                        .unwrap();
                        (events, store, tmp)
                    },
                    |(events, mut store, _tmp)| {
                        for event in &events {
                            store.append(event).unwrap();
                        }
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }

    group.finish();
}

// ── Benchmark 3: Checkpoint cadence / fsync amortisation ───────────────

fn checkpoint_cadence(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("eo2_checkpoint_cadence");
    let event_count = 10_000usize;
    group.throughput(Throughput::Elements(event_count as u64));

    for &interval_ms in &[1u64, 10, 100, 500] {
        group.bench_function(
            BenchmarkId::new("flush_interval_ms", interval_ms),
            |b| {
                b.iter_batched(
                    || {
                        let events = make_events(event_count, 256);
                        let tmp = tempfile::tempdir().unwrap();
                        let registry = PipelineL2Registry::new(L2BodyStoreConfig {
                            root: Some(tmp.path().to_path_buf()),
                            segment_bytes: 64 * 1024 * 1024,
                        });

                        let mut config = PipelineConfig::default();
                        config.pipeline_name = "ckpt-cadence-bench".into();
                        config.delivery.durability = DurabilityMode::OrderedBatch;
                        config.l2_registry = Some(registry);
                        config.delivery.flush.interval =
                            Duration::from_millis(interval_ms);
                        (events, config, tmp)
                    },
                    |(events, config, _tmp)| {
                        rt.block_on(async {
                            let source = OneShotPushSource {
                                batch: Some(events),
                            };
                            let processor = PassthroughProcessor::new(Arc::from("out"));
                            let sink = BlackholeSink::new();
                            let metrics = Arc::new(PipelineMetrics::new());
                            let shutdown = Arc::new(AtomicBool::new(false));
                            run_buffered(
                                source, processor, sink, config, metrics, shutdown, None,
                            )
                            .await
                            .unwrap();
                        });
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }

    group.finish();
}

// ── Benchmark 4: Capacity tracking overhead ────────────────────────────

fn capacity_tracking_overhead(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("eo2_capacity_overhead");
    let event_count = 10_000usize;
    group.throughput(Throughput::Elements(event_count as u64));

    for with_capacity in [false, true] {
        let label = if with_capacity {
            "with_capacity"
        } else {
            "without_capacity"
        };

        group.bench_function(BenchmarkId::new("tracking", label), |b| {
            b.iter_batched(
                || {
                    let events = make_events(event_count, 256);
                    let tmp = tempfile::tempdir().unwrap();
                    let registry = PipelineL2Registry::new(L2BodyStoreConfig {
                        root: Some(tmp.path().to_path_buf()),
                        segment_bytes: 64 * 1024 * 1024,
                    });

                    let mut config = PipelineConfig::default();
                    config.pipeline_name = "cap-bench".into();
                    config.delivery.durability = DurabilityMode::OrderedBatch;
                    config.l2_registry = Some(registry);
                    config.delivery.flush.interval = Duration::from_millis(10);

                    if with_capacity {
                        let node =
                            aeon_engine::eo2_backpressure::NodeCapacity::new(None);
                        let caps =
                            aeon_engine::eo2_backpressure::CapacityLimits::from_config(
                                None,
                                Some(1024 * 1024 * 1024),
                                1,
                            );
                        config.eo2_capacity = Some(
                            aeon_engine::eo2_backpressure::PipelineCapacity::new(
                                caps, node,
                            ),
                        );
                    }
                    (events, config, tmp)
                },
                |(events, config, _tmp)| {
                    rt.block_on(async {
                        let source = OneShotPushSource {
                            batch: Some(events),
                        };
                        let processor = PassthroughProcessor::new(Arc::from("out"));
                        let sink = BlackholeSink::new();
                        let metrics = Arc::new(PipelineMetrics::new());
                        let shutdown = Arc::new(AtomicBool::new(false));
                        run_buffered(source, processor, sink, config, metrics, shutdown, None)
                            .await
                            .unwrap();
                    });
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    durability_mode_overhead,
    l2_append_throughput,
    checkpoint_cadence,
    capacity_tracking_overhead,
);
criterion_main!(benches);
