use aeon_connectors::{BlackholeSink, MemorySource};
use aeon_engine::{PassthroughProcessor, PipelineMetrics, run};
use aeon_types::{Event, PartitionId};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn make_events(count: usize, payload_size: usize) -> Vec<Event> {
    let source: Arc<str> = Arc::from("bench-source");
    let payload = Bytes::from(vec![b'x'; payload_size]);
    (0..count)
        .map(|i| {
            Event::new(
                uuid::Uuid::nil(),
                i as i64,
                Arc::clone(&source),
                PartitionId::new(0),
                payload.clone(), // Bytes clone = Arc increment, not data copy
            )
        })
        .collect()
}

fn blackhole_throughput(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("blackhole_pipeline");

    // Benchmark at different event counts and batch sizes
    for &event_count in &[10_000u64, 100_000, 1_000_000] {
        for &batch_size in &[64usize, 256, 1024] {
            group.throughput(Throughput::Elements(event_count));
            group.bench_with_input(
                BenchmarkId::new(
                    format!("events_{event_count}"),
                    format!("batch_{batch_size}"),
                ),
                &(event_count as usize, batch_size),
                |b, &(count, batch)| {
                    let events = make_events(count, 256);
                    b.iter(|| {
                        rt.block_on(async {
                            let mut source = MemorySource::new(events.clone(), batch);
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
    }

    group.finish();
}

fn blackhole_per_event(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("per_event_overhead");

    // Measure per-event overhead at different payload sizes
    for &payload_size in &[64usize, 256, 1024] {
        let event_count = 100_000usize;
        group.throughput(Throughput::Elements(event_count as u64));
        group.bench_with_input(
            BenchmarkId::new("passthrough", format!("{payload_size}B_payload")),
            &payload_size,
            |b, &psize| {
                let events = make_events(event_count, psize);
                b.iter(|| {
                    rt.block_on(async {
                        let mut source = MemorySource::new(events.clone(), 1024);
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

criterion_group!(benches, blackhole_throughput, blackhole_per_event);
criterion_main!(benches);
