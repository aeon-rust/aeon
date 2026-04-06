//! State store benchmarks — L1 DashMap, L2 mmap, L3 redb read/write latency.

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

#[cfg(feature = "redb")]
fn l3_get_put(c: &mut Criterion) {
    use aeon_state::{L3Store, RedbConfig, RedbStore};

    let mut group = c.benchmark_group("l3_redb");

    let dir = tempfile::tempdir().unwrap();

    // Single key put
    group.throughput(Throughput::Elements(1));
    group.bench_function("put_256B", |b| {
        let path = dir.path().join("bench_put.redb");
        let store = RedbStore::open(RedbConfig {
            path,
            sync_writes: false,
        })
        .unwrap();
        let value = vec![b'x'; 256];
        b.iter(|| {
            store.put(b"key", &value).unwrap();
        });
    });

    // Single key get (existing)
    group.bench_function("get_existing_256B", |b| {
        let path = dir.path().join("bench_get.redb");
        let store = RedbStore::open(RedbConfig {
            path,
            sync_writes: false,
        })
        .unwrap();
        let value = vec![b'x'; 256];
        store.put(b"key", &value).unwrap();
        b.iter(|| {
            std::hint::black_box(store.get(b"key").unwrap());
        });
    });

    // Get missing key
    group.bench_function("get_missing", |b| {
        let path = dir.path().join("bench_miss.redb");
        let store = RedbStore::open(RedbConfig {
            path,
            sync_writes: false,
        })
        .unwrap();
        b.iter(|| {
            std::hint::black_box(store.get(b"missing").unwrap());
        });
    });

    group.finish();
}

#[cfg(feature = "redb")]
fn l3_batch_operations(c: &mut Criterion) {
    use aeon_state::{BatchOp, L3Store, RedbConfig, RedbStore};

    let mut group = c.benchmark_group("l3_redb_batch");
    let dir = tempfile::tempdir().unwrap();

    for &count in &[100usize, 1000] {
        let value = vec![b'x'; 256];

        group.throughput(Throughput::Elements(count as u64));

        // Batch write (single transaction)
        group.bench_with_input(
            BenchmarkId::new("write_batch", format!("{count}_keys")),
            &count,
            |b, _| {
                let path = dir.path().join(format!("bench_batch_{count}.redb"));
                let store = RedbStore::open(RedbConfig {
                    path,
                    sync_writes: false,
                })
                .unwrap();
                let ops: Vec<_> = (0..count)
                    .map(|i| {
                        (
                            BatchOp::Put,
                            format!("key:{i}").into_bytes(),
                            Some(value.clone()),
                        )
                    })
                    .collect();
                b.iter(|| {
                    store.write_batch(&ops).unwrap();
                });
            },
        );

        // Batch read (pre-populated)
        group.bench_with_input(
            BenchmarkId::new("read", format!("{count}_keys")),
            &count,
            |b, _| {
                let path = dir.path().join(format!("bench_read_{count}.redb"));
                let store = RedbStore::open(RedbConfig {
                    path,
                    sync_writes: false,
                })
                .unwrap();
                let keys: Vec<Vec<u8>> = (0..count)
                    .map(|i| format!("key:{i}").into_bytes())
                    .collect();
                for key in &keys {
                    store.put(key, &value).unwrap();
                }
                b.iter(|| {
                    for key in &keys {
                        std::hint::black_box(store.get(key).unwrap());
                    }
                });
            },
        );
    }

    group.finish();
}

fn tiered_read_through(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("tiered");
    group.throughput(Throughput::Elements(1));

    // L1-only (baseline)
    group.bench_function("l1_only_get", |b| {
        let store = aeon_state::TieredStore::new();
        rt.block_on(async { store.put(b"key", &vec![b'x'; 256]).await.unwrap() });
        b.iter(|| {
            rt.block_on(async {
                std::hint::black_box(store.get(b"key").await.unwrap());
            });
        });
    });

    group.finish();
}

#[cfg(feature = "redb")]
criterion_group!(
    benches,
    l1_get_put,
    l1_batch_operations,
    l3_get_put,
    l3_batch_operations,
    tiered_read_through
);

#[cfg(not(feature = "redb"))]
criterion_group!(
    benches,
    l1_get_put,
    l1_batch_operations,
    tiered_read_through
);

criterion_main!(benches);
