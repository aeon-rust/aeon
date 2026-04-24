//! Multi-runtime processor benchmark.
//!
//! Compares Rust-native vs Rust→Wasm vs AssemblyScript→Wasm processors,
//! all implementing the same JSON enrichment workload.
//!
//! Run from workspace root:
//!   cargo bench -p aeon-sample-rust-native --bench multi_runtime

use std::sync::Arc;

use aeon_sample_rust_native::JsonEnrichProcessor;
use aeon_types::Processor;
use aeon_types::event::Event;
use aeon_types::partition::PartitionId;
use aeon_wasm::{WasmConfig, WasmModule, WasmProcessor};
use bytes::Bytes;
use criterion::{Criterion, black_box, criterion_group, criterion_main};

/// Test JSON payload
fn make_json_event(i: usize) -> Event {
    let payload = format!(
        "{{\"user_id\":\"u-{i}\",\"action\":\"click\",\"page\":\"/dashboard\",\"ts\":1234567890}}"
    );
    Event::new(
        uuid::Uuid::nil(),
        1234567890,
        Arc::from("bench"),
        PartitionId::new(0),
        Bytes::from(payload),
    )
}

fn load_rust_wasm() -> WasmProcessor {
    let default_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../rust-wasm/target/wasm32-unknown-unknown/release/aeon_sample_rust_wasm.wasm"
    );
    let path = std::env::var("AEON_BENCH_RUST_WASM").unwrap_or_else(|_| default_path.to_string());
    let wasm_bytes = std::fs::read(&path)
        .unwrap_or_else(|_| panic!("Cannot read Rust Wasm at {path}. Build first: cd samples/processors/rust-wasm && cargo build --target wasm32-unknown-unknown --release"));

    let config = WasmConfig {
        max_fuel: Some(10_000_000),
        ..Default::default()
    };
    let module = WasmModule::from_bytes(&wasm_bytes, config).unwrap();
    WasmProcessor::new(Arc::new(module)).unwrap()
}

fn load_assemblyscript_wasm() -> WasmProcessor {
    let default_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../assemblyscript-wasm/build/processor.wasm"
    );
    let path = std::env::var("AEON_BENCH_AS_WASM").unwrap_or_else(|_| default_path.to_string());
    let wasm_bytes = std::fs::read(&path)
        .unwrap_or_else(|_| panic!("Cannot read AS Wasm at {path}. Build first: cd samples/processors/assemblyscript-wasm && npm run asbuild:release"));

    let config = WasmConfig {
        max_fuel: Some(10_000_000),
        ..Default::default()
    };
    let module = WasmModule::from_bytes(&wasm_bytes, config).unwrap();
    WasmProcessor::new(Arc::new(module)).unwrap()
}

fn bench_single_event(c: &mut Criterion) {
    let mut group = c.benchmark_group("processor/single_event");

    // Rust-native
    let native = JsonEnrichProcessor::new("user_id", "enriched", "rust-native");
    group.bench_function("rust-native", |b| {
        b.iter(|| {
            let event = make_json_event(42);
            let outputs = native.process(black_box(event)).unwrap();
            black_box(outputs);
        });
    });

    // Rust→Wasm
    let rust_wasm = load_rust_wasm();
    group.bench_function("rust-wasm", |b| {
        b.iter(|| {
            let event = make_json_event(42);
            let outputs = Processor::process(&rust_wasm, black_box(event)).unwrap();
            black_box(outputs);
        });
    });

    // AssemblyScript→Wasm
    let as_wasm = load_assemblyscript_wasm();
    group.bench_function("assemblyscript-wasm", |b| {
        b.iter(|| {
            let event = make_json_event(42);
            let outputs = Processor::process(&as_wasm, black_box(event)).unwrap();
            black_box(outputs);
        });
    });

    group.finish();
}

fn bench_batch_100(c: &mut Criterion) {
    let mut group = c.benchmark_group("processor/batch_100");

    // Rust-native
    let native = JsonEnrichProcessor::new("user_id", "enriched", "rust-native");
    group.bench_function("rust-native", |b| {
        b.iter(|| {
            let events: Vec<Event> = (0..100).map(make_json_event).collect();
            let outputs = native.process_batch(black_box(events)).unwrap();
            black_box(outputs);
        });
    });

    // Rust→Wasm
    let rust_wasm = load_rust_wasm();
    group.bench_function("rust-wasm", |b| {
        b.iter(|| {
            let events: Vec<Event> = (0..100).map(make_json_event).collect();
            let outputs = Processor::process_batch(&rust_wasm, black_box(events)).unwrap();
            black_box(outputs);
        });
    });

    // AssemblyScript→Wasm
    let as_wasm = load_assemblyscript_wasm();
    group.bench_function("assemblyscript-wasm", |b| {
        b.iter(|| {
            let events: Vec<Event> = (0..100).map(make_json_event).collect();
            let outputs = Processor::process_batch(&as_wasm, black_box(events)).unwrap();
            black_box(outputs);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_single_event, bench_batch_100);
criterion_main!(benches);
