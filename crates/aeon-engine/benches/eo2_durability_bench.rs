//! EO-2 Durability Benchmarks — P10
//!
//! Measures the cost of the durability stack across five dimensions:
//!
//! 1. **Per-event overhead by durability mode** — `None` vs `OrderedBatch`
//!    through a blackhole sink, isolating L2 write + checkpoint cost.
//! 2. **L2 body-store append throughput** — raw `L2BodyStore::append` at
//!    various payload sizes to establish the write ceiling.
//! 3. **Checkpoint cadence fsync amortisation** — pipeline throughput at
//!    1ms / 10ms / 100ms / 500ms flush intervals.
//! 4. **Capacity backpressure overhead** — pipeline with vs without
//!    `PipelineCapacity` tracking enabled.
//! 5. **Content-hash dedup cost (poll sources)** — `ContentHashDedup::
//!    check_and_mark` ns/event for first-seen and repeat-lookup paths at
//!    multiple payload sizes. Target: ~30 ns/event per EO-2 design §4.3.
//!
//! Run: `cargo bench -p aeon-engine --bench eo2_durability_bench`

#![allow(clippy::field_reassign_with_default)]

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

/// Push sources now loop on empty (P5.d), so bench pipelines need a watcher
/// that flips `shutdown` once the run has ingested all events.
fn shutdown_after_target(
    metrics: Arc<PipelineMetrics>,
    shutdown: Arc<AtomicBool>,
    target: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if metrics
                .events_received
                .load(std::sync::atomic::Ordering::Relaxed)
                >= target
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
                shutdown.store(true, std::sync::atomic::Ordering::Release);
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
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
                        let target = events.len() as u64;
                        let source = OneShotPushSource {
                            batch: Some(events),
                        };
                        let processor = PassthroughProcessor::new(Arc::from("out"));
                        let sink = BlackholeSink::new();
                        let metrics = Arc::new(PipelineMetrics::new());
                        let shutdown = Arc::new(AtomicBool::new(false));
                        let stopper = shutdown_after_target(
                            Arc::clone(&metrics),
                            Arc::clone(&shutdown),
                            target,
                        );
                        run_buffered(source, processor, sink, config, metrics, shutdown, None)
                            .await
                            .unwrap();
                        stopper.await.unwrap();
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
                                kek: None,
                                gc_min_hold: std::time::Duration::ZERO,
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

// ── Benchmark 2b: L2 body-store append throughput, encrypted ──────────
//
// Mirrors benchmark 2 but seals each event with the S3 at-rest cipher
// (AES-256-GCM, per-segment DEK wrapped by a data-context KEK). The
// delta between this group and `eo2_l2_append` is the per-event
// encryption tax; S3 design budget says a few hundred ns at 256 B.

fn l2_append_throughput_encrypted(c: &mut Criterion) {
    use aeon_crypto::kek::{KekDomain, KekHandle};
    use aeon_types::{
        SecretBytes, SecretError, SecretProvider, SecretRef, SecretRegistry, SecretScheme,
    };

    struct HexEnv;
    impl SecretProvider for HexEnv {
        fn scheme(&self) -> SecretScheme {
            SecretScheme::Env
        }
        fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError> {
            let v = std::env::var(path).map_err(|_| SecretError::EnvNotSet(path.to_string()))?;
            let bytes: Vec<u8> = (0..v.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&v[i..i + 2], 16).unwrap_or(0))
                .collect();
            Ok(SecretBytes::new(bytes))
        }
    }

    // One stable KEK reused across bench samples — each segment still
    // gets a fresh DEK, so this does not cheat on nonce uniqueness.
    unsafe {
        std::env::set_var(
            "AEON_BENCH_L2_KEK",
            "4242424242424242424242424242424242424242424242424242424242424242",
        );
    }
    let mut reg = SecretRegistry::empty();
    reg.register(Arc::new(HexEnv));
    let kek = Arc::new(KekHandle::new(
        KekDomain::DataContext,
        "bench-l2-kek",
        SecretRef::env("AEON_BENCH_L2_KEK"),
        Arc::new(reg),
    ));

    let mut group = c.benchmark_group("eo2_l2_append_encrypted");

    for &payload_size in &[64usize, 256, 1024, 4096] {
        let event_count = 10_000usize;
        group.throughput(Throughput::Elements(event_count as u64));

        group.bench_function(BenchmarkId::new("payload_bytes", payload_size), |b| {
            b.iter_batched(
                || {
                    let events = make_events(event_count, payload_size);
                    let tmp = tempfile::tempdir().unwrap();
                    let dir = tmp.path().join("l2-bench-enc");
                    std::fs::create_dir_all(&dir).unwrap();
                    let store = L2BodyStore::open(
                        dir,
                        aeon_engine::l2_body::L2BodyConfig {
                            segment_bytes: 256 * 1024 * 1024,
                            kek: Some(Arc::clone(&kek)),
                            gc_min_hold: std::time::Duration::ZERO,
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
        });
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
                            let target = events.len() as u64;
                            let source = OneShotPushSource {
                                batch: Some(events),
                            };
                            let processor = PassthroughProcessor::new(Arc::from("out"));
                            let sink = BlackholeSink::new();
                            let metrics = Arc::new(PipelineMetrics::new());
                            let shutdown = Arc::new(AtomicBool::new(false));
                            let stopper = shutdown_after_target(
                                Arc::clone(&metrics),
                                Arc::clone(&shutdown),
                                target,
                            );
                            run_buffered(
                                source, processor, sink, config, metrics, shutdown, None,
                            )
                            .await
                            .unwrap();
                            stopper.await.unwrap();
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
                        let target = events.len() as u64;
                        let source = OneShotPushSource {
                            batch: Some(events),
                        };
                        let processor = PassthroughProcessor::new(Arc::from("out"));
                        let sink = BlackholeSink::new();
                        let metrics = Arc::new(PipelineMetrics::new());
                        let shutdown = Arc::new(AtomicBool::new(false));
                        let stopper = shutdown_after_target(
                            Arc::clone(&metrics),
                            Arc::clone(&shutdown),
                            target,
                        );
                        run_buffered(source, processor, sink, config, metrics, shutdown, None)
                            .await
                            .unwrap();
                        stopper.await.unwrap();
                    });
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

/// Bench 5 — content-hash dedup cost per payload.
///
/// Measures `ContentHashDedup::check_and_mark` under two access patterns:
/// - **first_seen**: every payload is unique; we pay hash + insert.
/// - **repeat_lookup**: every payload is a re-lookup of a warm hash; we
///   pay hash + map-get only.
///
/// Payload sizes span the realistic poll-source range (64 B .. 4 KiB).
fn content_hash_dedup(c: &mut Criterion) {
    use aeon_engine::eo2_content_hash::{ContentHashAlgorithm, ContentHashDedup};

    let mut group = c.benchmark_group("content_hash_dedup");

    for size in [64usize, 256, 1024, 4096] {
        group.throughput(Throughput::Bytes(size as u64));

        // First-seen: every call gets a distinct payload, so every call
        // hashes + inserts. Amortised by pre-building a Vec of payloads
        // so only the hash+insert is timed.
        group.bench_with_input(
            BenchmarkId::new("first_seen", size),
            &size,
            |b, &sz| {
                b.iter_batched(
                    || {
                        // Each iteration gets a fresh dedup and a pool of
                        // 1 000 distinct payloads. Use the event-index so
                        // every byte string is unique without hashing cost
                        // before the measurement starts.
                        let d = ContentHashDedup::new(
                            ContentHashAlgorithm::Xxhash3_64,
                            Duration::from_secs(300),
                        );
                        let payloads: Vec<Vec<u8>> = (0..1_000)
                            .map(|i| {
                                let mut v = vec![b'x'; sz];
                                v[..8].copy_from_slice(&(i as u64).to_le_bytes());
                                v
                            })
                            .collect();
                        (d, payloads)
                    },
                    |(d, payloads)| {
                        for p in &payloads {
                            let _ = d.check_and_mark(p);
                        }
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        // Repeat-lookup: the dedup is pre-warmed with the same payload,
        // so every check_and_mark is a hash + get (duplicate detected).
        // Isolates the read path cost.
        group.bench_with_input(
            BenchmarkId::new("repeat_lookup", size),
            &size,
            |b, &sz| {
                b.iter_batched(
                    || {
                        let d = ContentHashDedup::new(
                            ContentHashAlgorithm::Xxhash3_64,
                            Duration::from_secs(300),
                        );
                        let payload = vec![b'x'; sz];
                        // Pre-warm — first call inserts, benchmark hits repeat path.
                        let _ = d.check_and_mark(&payload);
                        (d, payload)
                    },
                    |(d, payload)| {
                        for _ in 0..1_000 {
                            let _ = d.check_and_mark(&payload);
                        }
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    durability_mode_overhead,
    l2_append_throughput,
    l2_append_throughput_encrypted,
    checkpoint_cadence,
    capacity_tracking_overhead,
    content_hash_dedup,
);
criterion_main!(benches);
