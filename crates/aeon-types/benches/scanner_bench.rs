//! Benchmarks for SIMD scanner vs naive byte scanning.
//!
//! Compares memchr-based scanning against standard library methods
//! to validate SIMD acceleration on the hot path.

use aeon_types::scanner;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

fn make_json_payload(size: usize) -> Vec<u8> {
    // Create a realistic JSON payload with a field near the end
    let mut payload = Vec::with_capacity(size);
    payload.extend_from_slice(b"{\"timestamp\":1234567890,\"source\":\"sensor-42\",");
    // Pad with filler fields
    while payload.len() < size - 50 {
        payload.extend_from_slice(b"\"filler\":\"xxxxxxxxxxxxxxxxxxxx\",");
    }
    payload.extend_from_slice(b"\"event_type\":\"click\"}");
    payload.truncate(size);
    payload
}

fn bench_find_byte(c: &mut Criterion) {
    let mut group = c.benchmark_group("find_byte");

    for &size in &[256usize, 1024, 4096] {
        let data = vec![b'x'; size];
        // Needle at the end
        let mut data_end = data.clone();
        *data_end.last_mut().unwrap() = b'Z';

        group.throughput(Throughput::Bytes(size as u64));

        // memchr (SIMD)
        group.bench_with_input(
            BenchmarkId::new("memchr", format!("{size}B")),
            &data_end,
            |b, data| {
                b.iter(|| std::hint::black_box(scanner::find_byte(b'Z', data)));
            },
        );

        // Naive: slice.iter().position()
        group.bench_with_input(
            BenchmarkId::new("naive_iter", format!("{size}B")),
            &data_end,
            |b, data| {
                b.iter(|| std::hint::black_box(data.iter().position(|&b| b == b'Z')));
            },
        );
    }

    group.finish();
}

fn bench_find_substring(c: &mut Criterion) {
    let mut group = c.benchmark_group("find_substring");

    for &size in &[256usize, 1024, 4096] {
        let payload = make_json_payload(size);

        group.throughput(Throughput::Bytes(size as u64));

        // memchr::memmem (SIMD)
        group.bench_with_input(
            BenchmarkId::new("memmem", format!("{size}B")),
            &payload,
            |b, data| {
                b.iter(|| std::hint::black_box(scanner::find_bytes(b"event_type", data)));
            },
        );

        // Naive: windows().position()
        group.bench_with_input(
            BenchmarkId::new("naive_windows", format!("{size}B")),
            &payload,
            |b, data| {
                b.iter(|| std::hint::black_box(data.windows(10).position(|w| w == b"event_type")));
            },
        );
    }

    group.finish();
}

fn bench_json_field_extract(c: &mut Criterion) {
    let mut group = c.benchmark_group("json_field_extract");

    for &size in &[256usize, 1024, 4096] {
        let payload = make_json_payload(size);

        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(
            BenchmarkId::new("memchr_extract", format!("{size}B")),
            &payload,
            |b, data| {
                b.iter(|| std::hint::black_box(scanner::json_field_value("event_type", data)));
            },
        );
    }

    group.finish();
}

fn bench_precompiled_finder(c: &mut Criterion) {
    let mut group = c.benchmark_group("precompiled_finder");
    let finder = scanner::BytesFinder::new(b"event_type");

    for &size in &[256usize, 1024, 4096] {
        let payload = make_json_payload(size);

        group.throughput(Throughput::Bytes(size as u64));

        // Pre-compiled finder (amortized setup)
        group.bench_with_input(
            BenchmarkId::new("precompiled", format!("{size}B")),
            &payload,
            |b, data| {
                b.iter(|| std::hint::black_box(finder.find(data)));
            },
        );

        // One-shot find (setup + search each time)
        group.bench_with_input(
            BenchmarkId::new("oneshot", format!("{size}B")),
            &payload,
            |b, data| {
                b.iter(|| std::hint::black_box(scanner::find_bytes(b"event_type", data)));
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_find_byte,
    bench_find_substring,
    bench_json_field_extract,
    bench_precompiled_finder,
);
criterion_main!(benches);
