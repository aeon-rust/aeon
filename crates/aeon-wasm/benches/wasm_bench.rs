//! Wasm runtime benchmarks.
//!
//! Measures:
//! - Module compilation time
//! - Instance creation time
//! - WasmProcessor process() latency (passthrough)
//! - Serialization / deserialization overhead

use std::sync::Arc;

use aeon_types::event::Event;
use aeon_types::partition::PartitionId;
use aeon_wasm::{WasmConfig, WasmModule, WasmProcessor};
use bytes::Bytes;
use criterion::{Criterion, black_box, criterion_group, criterion_main};

/// Passthrough WAT processor — reads event payload, emits one output.
const PASSTHROUGH_WAT: &str = r#"
    (module
        (global $bump (mut i32) (i32.const 65536))

        (func (export "alloc") (param $size i32) (result i32)
            (local $ptr i32)
            (local.set $ptr (global.get $bump))
            (global.set $bump (i32.add (global.get $bump) (local.get $size)))
            (local.get $ptr)
        )

        (func (export "dealloc") (param $ptr i32) (param $size i32))

        (func (export "process") (param $ptr i32) (param $len i32) (result i32)
            (local $pos i32)
            (local $src_len i32)
            (local $meta_count i32)
            (local $meta_i i32)
            (local $skip_len i32)
            (local $payload_len i32)
            (local $payload_ptr i32)
            (local $out_ptr i32)
            (local $write_pos i32)
            (local $result_content_len i32)

            ;; Reset bump allocator for each call to avoid OOM
            (global.set $bump (i32.const 65536))

            (local.set $pos (i32.add (local.get $ptr) (i32.const 24)))

            (local.set $src_len (i32.load (local.get $pos)))
            (local.set $pos (i32.add (local.get $pos) (i32.add (i32.const 4) (local.get $src_len))))

            (local.set $pos (i32.add (local.get $pos) (i32.const 2)))

            (local.set $meta_count (i32.load (local.get $pos)))
            (local.set $pos (i32.add (local.get $pos) (i32.const 4)))

            (local.set $meta_i (i32.const 0))
            (block $break_meta
                (loop $loop_meta
                    (br_if $break_meta (i32.ge_u (local.get $meta_i) (local.get $meta_count)))
                    (local.set $skip_len (i32.load (local.get $pos)))
                    (local.set $pos (i32.add (local.get $pos) (i32.add (i32.const 4) (local.get $skip_len))))
                    (local.set $skip_len (i32.load (local.get $pos)))
                    (local.set $pos (i32.add (local.get $pos) (i32.add (i32.const 4) (local.get $skip_len))))
                    (local.set $meta_i (i32.add (local.get $meta_i) (i32.const 1)))
                    (br $loop_meta)
                )
            )

            (local.set $payload_len (i32.load (local.get $pos)))
            (local.set $payload_ptr (i32.add (local.get $pos) (i32.const 4)))

            (local.set $result_content_len (i32.add (i32.const 23) (local.get $payload_len)))

            (local.set $out_ptr (call $bump_alloc (i32.add (i32.const 4) (local.get $result_content_len))))
            (local.set $write_pos (local.get $out_ptr))

            (i32.store (local.get $write_pos) (local.get $result_content_len))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))

            (i32.store (local.get $write_pos) (i32.const 1))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))

            (i32.store (local.get $write_pos) (i32.const 6))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))
            (i32.store8 (local.get $write_pos) (i32.const 111))
            (i32.store8 (i32.add (local.get $write_pos) (i32.const 1)) (i32.const 117))
            (i32.store8 (i32.add (local.get $write_pos) (i32.const 2)) (i32.const 116))
            (i32.store8 (i32.add (local.get $write_pos) (i32.const 3)) (i32.const 112))
            (i32.store8 (i32.add (local.get $write_pos) (i32.const 4)) (i32.const 117))
            (i32.store8 (i32.add (local.get $write_pos) (i32.const 5)) (i32.const 116))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 6)))

            (i32.store8 (local.get $write_pos) (i32.const 0))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 1)))

            (i32.store (local.get $write_pos) (local.get $payload_len))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))

            (memory.copy
                (local.get $write_pos)
                (local.get $payload_ptr)
                (local.get $payload_len)
            )
            (local.set $write_pos (i32.add (local.get $write_pos) (local.get $payload_len)))

            (i32.store (local.get $write_pos) (i32.const 0))

            (local.get $out_ptr)
        )

        (func $bump_alloc (param $size i32) (result i32)
            (local $ptr i32)
            (local.set $ptr (global.get $bump))
            (global.set $bump (i32.add (global.get $bump) (local.get $size)))
            (local.get $ptr)
        )

        (memory (export "memory") 2)
    )
"#;

fn make_event(payload_size: usize) -> Event {
    let payload = vec![0x42u8; payload_size];
    Event::new(
        uuid::Uuid::nil(),
        1234567890,
        Arc::from("bench-source"),
        PartitionId::new(0),
        Bytes::from(payload),
    )
}

fn bench_module_compilation(c: &mut Criterion) {
    c.bench_function("wasm/compile_passthrough", |b| {
        b.iter(|| {
            let module = WasmModule::from_wat(black_box(PASSTHROUGH_WAT)).unwrap();
            black_box(module);
        });
    });
}

fn bench_instance_creation(c: &mut Criterion) {
    let module = WasmModule::from_wat(PASSTHROUGH_WAT).unwrap();

    c.bench_function("wasm/instantiate", |b| {
        b.iter(|| {
            let instance = module.instantiate().unwrap();
            black_box(instance);
        });
    });
}

fn bench_process_passthrough(c: &mut Criterion) {
    let mut group = c.benchmark_group("wasm/process_passthrough");

    for payload_size in [64, 256, 1024, 4096] {
        let module = WasmModule::from_wat_with_config(
            PASSTHROUGH_WAT,
            WasmConfig {
                max_fuel: Some(10_000_000),
                ..Default::default()
            },
        )
        .unwrap();
        let processor = WasmProcessor::new(Arc::new(module)).unwrap();

        group.bench_function(format!("{payload_size}B"), |b| {
            b.iter(|| {
                let event = make_event(payload_size);
                let outputs = processor.process(black_box(event)).unwrap();
                black_box(outputs);
            });
        });
    }

    group.finish();
}

fn bench_process_batch(c: &mut Criterion) {
    let module = WasmModule::from_wat_with_config(
        PASSTHROUGH_WAT,
        WasmConfig {
            max_fuel: Some(10_000_000),
            ..Default::default()
        },
    )
    .unwrap();
    let processor = WasmProcessor::new(Arc::new(module)).unwrap();

    let mut group = c.benchmark_group("wasm/process_batch");

    for batch_size in [1, 10, 100] {
        group.bench_function(format!("{batch_size}"), |b| {
            b.iter(|| {
                let events: Vec<Event> = (0..batch_size).map(|_| make_event(256)).collect();
                let outputs = processor.process_batch(black_box(events)).unwrap();
                black_box(outputs);
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_module_compilation,
    bench_instance_creation,
    bench_process_passthrough,
    bench_process_batch,
);
criterion_main!(benches);
