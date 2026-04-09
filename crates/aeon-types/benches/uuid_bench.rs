use criterion::{Criterion, criterion_group, criterion_main};

fn uuid_pool_path(c: &mut Criterion) {
    let mut generator = aeon_types::CoreLocalUuidGenerator::new(0);
    // Give fill thread time to fill the pool
    std::thread::sleep(std::time::Duration::from_millis(50));

    c.bench_function("uuid_pool_pop", |b| {
        b.iter(|| {
            std::hint::black_box(generator.next_uuid());
        });
    });
}

fn uuid_fallback_path(c: &mut Criterion) {
    let mut generator = aeon_types::CoreLocalUuidGenerator::new_fallback_only(0);

    c.bench_function("uuid_fallback_inline", |b| {
        b.iter(|| {
            std::hint::black_box(generator.next_uuid());
        });
    });
}

fn uuid_stdlib_v7(c: &mut Criterion) {
    c.bench_function("uuid_stdlib_now_v7", |b| {
        b.iter(|| {
            std::hint::black_box(uuid::Uuid::now_v7());
        });
    });
}

criterion_group!(benches, uuid_pool_path, uuid_fallback_path, uuid_stdlib_v7);
criterion_main!(benches);
