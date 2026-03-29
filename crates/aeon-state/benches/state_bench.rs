//! State store benchmarks — L1 DashMap read/write latency.

use aeon_state::L1Store;
use aeon_types::StateOps;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

fn l1_get_put(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("l1_store");

    // Single key put
    group.throughput(Throughput::Elements(1));
    group.bench_function("put_256B", |b| {
        let store = L1Store::new();
        let value = vec![b'x'; 256];
        b.iter(|| {
            rt.block_on(async {
                store.put(b"key", &value).await.unwrap();
            });
        });
    });

    // Single key get (existing)
    group.bench_function("get_existing_256B", |b| {
        let store = L1Store::new();
        let value = vec![b'x'; 256];
        rt.block_on(async { store.put(b"key", &value).await.unwrap() });
        b.iter(|| {
            rt.block_on(async {
                std::hint::black_box(store.get(b"key").await.unwrap());
            });
        });
    });

    // Get missing key
    group.bench_function("get_missing", |b| {
        let store = L1Store::new();
        b.iter(|| {
            rt.block_on(async {
                std::hint::black_box(store.get(b"missing").await.unwrap());
            });
        });
    });

    // Delete
    group.bench_function("delete_existing", |b| {
        let store = L1Store::new();
        let value = vec![b'x'; 256];
        b.iter(|| {
            rt.block_on(async {
                store.put(b"key", &value).await.unwrap();
                store.delete(b"key").await.unwrap();
            });
        });
    });

    group.finish();
}

fn l1_batch_operations(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("l1_batch");

    for &count in &[100usize, 1000, 10000] {
        let value = vec![b'x'; 256];
        let keys: Vec<Vec<u8>> = (0..count)
            .map(|i| format!("key:{i}").into_bytes())
            .collect();

        group.throughput(Throughput::Elements(count as u64));

        // Batch put
        group.bench_with_input(
            BenchmarkId::new("put", format!("{count}_keys")),
            &count,
            |b, _| {
                b.iter(|| {
                    let store = L1Store::new();
                    rt.block_on(async {
                        for key in &keys {
                            store.put(key, &value).await.unwrap();
                        }
                    });
                });
            },
        );

        // Batch get (pre-populated)
        group.bench_with_input(
            BenchmarkId::new("get", format!("{count}_keys")),
            &count,
            |b, _| {
                let store = L1Store::new();
                rt.block_on(async {
                    for key in &keys {
                        store.put(key, &value).await.unwrap();
                    }
                });
                b.iter(|| {
                    rt.block_on(async {
                        for key in &keys {
                            std::hint::black_box(store.get(key).await.unwrap());
                        }
                    });
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, l1_get_put, l1_batch_operations);
criterion_main!(benches);
