//! Benchmarks for Event and Output creation, cloning, and conversion.
//!
//! Measures per-operation cost of the canonical envelope types on the hot path.

use aeon_types::{Event, Output, PartitionId};
use bytes::Bytes;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::sync::Arc;

fn event_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_creation");
    let source: Arc<str> = Arc::from("bench-source");

    for &payload_size in &[0usize, 64, 256, 1024, 4096] {
        let payload = Bytes::from(vec![b'x'; payload_size]);
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("new", format!("{payload_size}B")),
            &payload_size,
            |b, _| {
                b.iter(|| {
                    std::hint::black_box(Event::new(
                        uuid::Uuid::nil(),
                        1234567890,
                        Arc::clone(&source),
                        PartitionId::new(0),
                        payload.clone(), // Bytes clone = Arc increment
                    ));
                });
            },
        );
    }

    group.finish();
}

fn event_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_clone");
    let source: Arc<str> = Arc::from("bench-source");

    for &payload_size in &[64usize, 256, 1024] {
        let payload = Bytes::from(vec![b'x'; payload_size]);
        let event = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::clone(&source),
            PartitionId::new(0),
            payload,
        );

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("clone", format!("{payload_size}B")),
            &event,
            |b, event| {
                b.iter(|| {
                    std::hint::black_box(event.clone());
                });
            },
        );
    }

    group.finish();
}

fn event_with_metadata(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_metadata");
    let source: Arc<str> = Arc::from("bench-source");
    let payload = Bytes::from(vec![b'x'; 256]);

    for &header_count in &[0usize, 1, 4, 8] {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("with_headers", format!("{header_count}_headers")),
            &header_count,
            |b, &count| {
                let headers: Vec<(Arc<str>, Arc<str>)> = (0..count)
                    .map(|i| (Arc::from(format!("k{i}")), Arc::from(format!("v{i}"))))
                    .collect();
                b.iter(|| {
                    let mut event = Event::new(
                        uuid::Uuid::nil(),
                        0,
                        Arc::clone(&source),
                        PartitionId::new(0),
                        payload.clone(),
                    );
                    for (k, v) in &headers {
                        event.metadata.push((Arc::clone(k), Arc::clone(v)));
                    }
                    std::hint::black_box(event);
                });
            },
        );
    }

    group.finish();
}

fn output_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("output_creation");
    let dest: Arc<str> = Arc::from("bench-dest");

    for &payload_size in &[64usize, 256, 1024] {
        let payload = Bytes::from(vec![b'x'; payload_size]);
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("new", format!("{payload_size}B")),
            &payload_size,
            |b, _| {
                b.iter(|| {
                    std::hint::black_box(
                        Output::new(Arc::clone(&dest), payload.clone())
                            .with_key(Bytes::from_static(b"key-1")),
                    );
                });
            },
        );
    }

    group.finish();
}

fn output_into_event(c: &mut Criterion) {
    let mut group = c.benchmark_group("output_into_event");
    let dest: Arc<str> = Arc::from("dest");
    let source: Arc<str> = Arc::from("chained");

    for &header_count in &[0usize, 2, 4] {
        let payload = Bytes::from(vec![b'x'; 256]);
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("convert", format!("{header_count}_headers")),
            &header_count,
            |b, &count| {
                b.iter_batched(
                    || {
                        let mut output = Output::new(Arc::clone(&dest), payload.clone());
                        for i in 0..count {
                            output = output.with_header(
                                Arc::from(format!("k{i}")),
                                Arc::from(format!("v{i}")),
                            );
                        }
                        output
                    },
                    |output| {
                        std::hint::black_box(output.into_event(
                            uuid::Uuid::nil(),
                            42,
                            Arc::clone(&source),
                            PartitionId::new(0),
                        ));
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bytes_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("bytes_ops");

    // Bytes::clone (Arc increment — should be ~1-2ns)
    let data = Bytes::from(vec![b'x'; 1024]);
    group.bench_function("clone_1024B", |b| {
        b.iter(|| {
            std::hint::black_box(data.clone());
        });
    });

    // Bytes::slice (zero-copy sub-range)
    group.bench_function("slice_256B_from_1024B", |b| {
        b.iter(|| {
            std::hint::black_box(data.slice(128..384));
        });
    });

    // Bytes::copy_from_slice (the copy we do in KafkaSource)
    let raw = vec![b'x'; 256];
    group.bench_function("copy_from_slice_256B", |b| {
        b.iter(|| {
            std::hint::black_box(Bytes::copy_from_slice(&raw));
        });
    });

    let raw_1k = vec![b'x'; 1024];
    group.bench_function("copy_from_slice_1024B", |b| {
        b.iter(|| {
            std::hint::black_box(Bytes::copy_from_slice(&raw_1k));
        });
    });

    group.finish();
}

fn batch_event_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_creation");
    let source: Arc<str> = Arc::from("bench");
    let payload = Bytes::from(vec![b'x'; 256]);

    for &batch_size in &[64usize, 256, 1024] {
        group.throughput(Throughput::Elements(batch_size as u64));
        group.bench_with_input(
            BenchmarkId::new("events", format!("batch_{batch_size}")),
            &batch_size,
            |b, &size| {
                b.iter(|| {
                    let events: Vec<Event> = (0..size)
                        .map(|i| {
                            Event::new(
                                uuid::Uuid::nil(),
                                i as i64,
                                Arc::clone(&source),
                                PartitionId::new((i % 16) as u16),
                                payload.clone(),
                            )
                        })
                        .collect();
                    std::hint::black_box(events);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    event_creation,
    event_clone,
    event_with_metadata,
    output_creation,
    output_into_event,
    bytes_operations,
    batch_event_creation,
);
criterion_main!(benches);
