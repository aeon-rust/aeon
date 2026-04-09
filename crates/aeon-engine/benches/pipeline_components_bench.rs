//! Benchmarks for individual pipeline components.
//!
//! Measures: SPSC ring buffer throughput, processor dispatch,
//! pipeline overhead (direct vs buffered), batch processing.

use aeon_connectors::{BlackholeSink, MemorySource};
use aeon_engine::{PassthroughProcessor, PipelineConfig, PipelineMetrics, run, run_buffered};
use aeon_types::{Event, Output, PartitionId, Processor};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn make_events(count: usize, payload_size: usize) -> Vec<Event> {
    let source: Arc<str> = Arc::from("bench");
    let payload = Bytes::from(vec![b'x'; payload_size]);
    (0..count)
        .map(|i| {
            Event::new(
                uuid::Uuid::nil(),
                i as i64,
                Arc::clone(&source),
                PartitionId::new((i % 16) as u16),
                payload.clone(),
            )
        })
        .collect()
}

/// SPSC ring buffer: push + pop throughput.
fn spsc_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc_ringbuffer");

    for &batch_size in &[1usize, 64, 256, 1024] {
        group.throughput(Throughput::Elements(batch_size as u64));
        group.bench_with_input(
            BenchmarkId::new("push_pop_batch", format!("batch_{batch_size}")),
            &batch_size,
            |b, &size| {
                let events = make_events(size, 256);
                b.iter(|| {
                    let (mut prod, mut cons) = rtrb::RingBuffer::<Vec<Event>>::new(64);
                    prod.push(events.clone()).unwrap();
                    let batch = cons.pop().unwrap();
                    std::hint::black_box(batch);
                });
            },
        );
    }

    group.finish();
}

/// Processor::process_batch throughput (PassthroughProcessor).
fn processor_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("processor_batch");
    let processor = PassthroughProcessor::new(Arc::from("output"));

    for &batch_size in &[64usize, 256, 1024, 4096] {
        let events = make_events(batch_size, 256);
        group.throughput(Throughput::Elements(batch_size as u64));
        group.bench_with_input(
            BenchmarkId::new("passthrough", format!("batch_{batch_size}")),
            &batch_size,
            |b, _| {
                b.iter(|| {
                    let outputs = processor.process_batch(events.clone()).unwrap();
                    std::hint::black_box(outputs);
                });
            },
        );
    }

    group.finish();
}

/// Direct pipeline (run) vs buffered pipeline (run_buffered) comparison.
fn pipeline_mode_comparison(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("pipeline_mode");
    let event_count = 100_000u64;
    let batch_size = 1024usize;

    group.throughput(Throughput::Elements(event_count));

    // Direct pipeline (single task, no SPSC)
    group.bench_function("direct_100K", |b| {
        let events = make_events(event_count as usize, 256);
        b.iter(|| {
            rt.block_on(async {
                let mut source = MemorySource::new(events.clone(), batch_size);
                let processor = PassthroughProcessor::new(Arc::from("output"));
                let mut sink = BlackholeSink::new();
                let metrics = PipelineMetrics::new();
                let shutdown = AtomicBool::new(false);
                run(&mut source, &processor, &mut sink, &metrics, &shutdown)
                    .await
                    .unwrap();
            });
        });
    });

    // Buffered pipeline (3 tasks, SPSC ring buffers)
    group.bench_function("buffered_100K", |b| {
        let events = make_events(event_count as usize, 256);
        b.iter(|| {
            rt.block_on(async {
                let source = MemorySource::new(events.clone(), batch_size);
                let processor = PassthroughProcessor::new(Arc::from("output"));
                let sink = BlackholeSink::new();
                let config = PipelineConfig::default();
                let metrics = Arc::new(PipelineMetrics::new());
                let shutdown = Arc::new(AtomicBool::new(false));
                run_buffered(source, processor, sink, config, metrics, shutdown, None)
                    .await
                    .unwrap();
            });
        });
    });

    group.finish();
}

/// Batch size impact on throughput (direct pipeline).
fn batch_size_sweep(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("batch_size_sweep");
    let event_count = 100_000u64;

    group.throughput(Throughput::Elements(event_count));

    for &batch_size in &[16usize, 64, 256, 1024, 4096] {
        let events = make_events(event_count as usize, 256);
        group.bench_with_input(
            BenchmarkId::new("direct", format!("batch_{batch_size}")),
            &batch_size,
            |b, &bs| {
                b.iter(|| {
                    rt.block_on(async {
                        let mut source = MemorySource::new(events.clone(), bs);
                        let processor = PassthroughProcessor::new(Arc::from("output"));
                        let mut sink = BlackholeSink::new();
                        let metrics = PipelineMetrics::new();
                        let shutdown = AtomicBool::new(false);
                        run(&mut source, &processor, &mut sink, &metrics, &shutdown)
                            .await
                            .unwrap();
                    });
                });
            },
        );
    }

    group.finish();
}

/// Event→Output→Event conversion chain (DAG chaining hot path).
fn event_output_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_output_chain");
    let source: Arc<str> = Arc::from("chain");
    let dest: Arc<str> = Arc::from("next");

    for &batch_size in &[64usize, 256, 1024] {
        let events = make_events(batch_size, 256);
        group.throughput(Throughput::Elements(batch_size as u64));
        group.bench_with_input(
            BenchmarkId::new("chain_roundtrip", format!("batch_{batch_size}")),
            &batch_size,
            |b, _| {
                b.iter(|| {
                    // Event → Output (processor)
                    let outputs: Vec<Output> = events
                        .iter()
                        .map(|e| Output::new(Arc::clone(&dest), e.payload.clone()))
                        .collect();
                    // Output → Event (chaining)
                    let chained: Vec<Event> = outputs
                        .into_iter()
                        .enumerate()
                        .map(|(i, o)| {
                            o.into_event(
                                uuid::Uuid::nil(),
                                i as i64,
                                Arc::clone(&source),
                                PartitionId::new(0),
                            )
                        })
                        .collect();
                    std::hint::black_box(chained);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    spsc_throughput,
    processor_batch,
    pipeline_mode_comparison,
    batch_size_sweep,
    event_output_conversion,
);
criterion_main!(benches);
