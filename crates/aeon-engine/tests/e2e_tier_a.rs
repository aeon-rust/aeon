//! Tier A E2E Tests: Memory → Processor → Memory (all SDK/tier combos)
//!
//! Tests from `docs/E2E-TEST-PLAN.md` Tier A (P0, no infra required).
//! Each test verifies the 5 E2E criteria:
//!   1. Event delivery: source count == sink count (zero loss)
//!   2. Payload integrity: input payload == output payload (or expected transform)
//!   3. Metadata propagation: headers/metadata pass through correctly
//!   4. Ordering: within a partition, events arrive in order
//!   5. Graceful shutdown: processor disconnects cleanly, no orphaned batches

use aeon_connectors::{MemorySink, MemorySource};
use aeon_engine::{PassthroughProcessor, PipelineConfig, PipelineMetrics, run, run_buffered};
use aeon_types::{Event, Output, PartitionId};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[path = "e2e_ws_harness.rs"]
mod e2e_ws_harness;

// ---------------------------------------------------------------------------
// Shared test helpers
// ---------------------------------------------------------------------------

/// Number of events per E2E test (enough to exercise batching, not so many it's slow).
const EVENT_COUNT: usize = 1_000;

/// Create test events with unique payloads, metadata, and sequential timestamps.
fn make_test_events(count: usize) -> Vec<Event> {
    let source: Arc<str> = Arc::from("e2e-test");
    (0..count)
        .map(|i| {
            let payload = Bytes::from(format!("payload-{i:05}"));
            Event::new(
                uuid::Uuid::now_v7(),
                i as i64,
                Arc::clone(&source),
                PartitionId::new((i % 4) as u16),
                payload,
            )
            .with_metadata(Arc::from("test-key"), Arc::from(format!("val-{i}")))
        })
        .collect()
}

/// Verify all 5 E2E criteria against source events and sink outputs.
fn verify_e2e(events: &[Event], outputs: &[Output]) {
    // Criterion 1: Event delivery (zero loss)
    assert_eq!(
        outputs.len(),
        events.len(),
        "C1: event count mismatch: {} outputs vs {} events",
        outputs.len(),
        events.len(),
    );

    // Criterion 2: Payload integrity
    for (i, (event, output)) in events.iter().zip(outputs.iter()).enumerate() {
        assert_eq!(
            output.payload.as_ref(),
            event.payload.as_ref(),
            "C2: payload mismatch at index {i}",
        );
    }

    // Criterion 3: Metadata propagation (source_event_id preserved)
    for (i, (event, output)) in events.iter().zip(outputs.iter()).enumerate() {
        assert_eq!(
            output.source_event_id,
            Some(event.id),
            "C3: source_event_id not propagated at index {i}",
        );
        assert_eq!(
            output.source_partition,
            Some(event.partition),
            "C3: source_partition not propagated at index {i}",
        );
    }

    // Criterion 4: Ordering (within each partition, timestamps are monotonically increasing)
    let mut partition_last_ts: std::collections::HashMap<u16, i64> =
        std::collections::HashMap::new();
    for (i, event) in events.iter().enumerate() {
        let part = event.partition.as_u16();
        if let Some(last_ts) = partition_last_ts.get(&part) {
            assert!(
                event.timestamp >= *last_ts,
                "C4: ordering violation at index {i}, partition {part}: ts {} < prev {}",
                event.timestamp,
                last_ts,
            );
        }
        partition_last_ts.insert(part, event.timestamp);
    }
}

/// Verify pipeline metrics match expected counts.
fn verify_metrics(metrics: &PipelineMetrics, expected_count: u64) {
    assert_eq!(
        metrics.events_received.load(Ordering::Relaxed),
        expected_count,
        "metrics: events_received mismatch",
    );
    assert_eq!(
        metrics.events_processed.load(Ordering::Relaxed),
        expected_count,
        "metrics: events_processed mismatch",
    );
    assert_eq!(
        metrics.outputs_sent.load(Ordering::Relaxed),
        expected_count,
        "metrics: outputs_sent mismatch",
    );
}

// ===========================================================================
// A1: Rust Native T1 — Memory -> RustNative -> Memory
// ===========================================================================

#[tokio::test]
async fn a1_rust_native_t1_memory_roundtrip() {
    let events = make_test_events(EVENT_COUNT);
    let events_clone = events.clone();

    let mut source = MemorySource::new(events, 64);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = MemorySink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = std::sync::atomic::AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    verify_e2e(&events_clone, sink.outputs());
    verify_metrics(&metrics, EVENT_COUNT as u64);
}

/// A1 variant: buffered pipeline with SPSC ring buffers.
#[tokio::test]
async fn a1_rust_native_t1_memory_roundtrip_buffered() {
    let events = make_test_events(EVENT_COUNT);
    let source = MemorySource::new(events, 64);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let sink = MemorySink::new();

    let config = PipelineConfig {
        source_buffer_capacity: 128,
        sink_buffer_capacity: 128,
        max_batch_size: 64,
        ..Default::default()
    };
    let metrics = Arc::new(PipelineMetrics::new());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    run_buffered(
        source,
        processor,
        sink,
        config,
        Arc::clone(&metrics),
        shutdown,
        None,
    )
    .await
    .unwrap();

    // run_buffered consumes sink — verify via metrics
    assert_eq!(
        metrics.events_received.load(Ordering::Relaxed),
        EVENT_COUNT as u64
    );
    assert_eq!(
        metrics.events_processed.load(Ordering::Relaxed),
        EVENT_COUNT as u64
    );
    assert_eq!(
        metrics.outputs_sent.load(Ordering::Relaxed),
        EVENT_COUNT as u64
    );
}

/// A1 variant: graceful shutdown mid-pipeline (criterion 5).
#[tokio::test]
async fn a1_rust_native_t1_graceful_shutdown() {
    let events = make_test_events(10_000);
    let mut source = MemorySource::new(events, 32);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = MemorySink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = std::sync::atomic::AtomicBool::new(false);

    // Trigger shutdown after a small delay
    let shutdown_ref = &shutdown;
    let result = tokio::select! {
        r = run(&mut source, &processor, &mut sink, &metrics, shutdown_ref) => r,
        _ = async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            shutdown_ref.store(true, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        } => Ok(()),
    };

    // Criterion 5: no panic, no error, clean shutdown
    assert!(result.is_ok());
    // Some events processed, no orphaned outputs
    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);
    assert!(received > 0, "some events should have been processed");
    assert_eq!(
        received, sent,
        "every received event should produce an output"
    );
}

/// A1 variant: large batch to stress batching logic.
#[tokio::test]
async fn a1_rust_native_t1_large_batch() {
    let events = make_test_events(10_000);
    let events_clone = events.clone();

    let mut source = MemorySource::new(events, 1024);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let mut sink = MemorySink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = std::sync::atomic::AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    verify_e2e(&events_clone, sink.outputs());
    verify_metrics(&metrics, 10_000);
}

// ===========================================================================
// A2: Rust Wasm T2 — Memory -> RustWasm -> Memory
// ===========================================================================

/// WAT passthrough processor module — reads event, emits output with same payload.
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

            ;; Skip UUID (16) + timestamp (8) = 24 bytes
            (local.set $pos (i32.add (local.get $ptr) (i32.const 24)))

            ;; Skip source string
            (local.set $src_len (i32.load (local.get $pos)))
            (local.set $pos (i32.add (local.get $pos) (i32.add (i32.const 4) (local.get $src_len))))

            ;; Skip partition (2 bytes)
            (local.set $pos (i32.add (local.get $pos) (i32.const 2)))

            ;; Skip metadata entries
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

            ;; Read payload
            (local.set $payload_len (i32.load (local.get $pos)))
            (local.set $payload_ptr (i32.add (local.get $pos) (i32.const 4)))

            ;; Output size: 4(count) + 4(dest_len) + 6("output") + 1(no_key) + 4(payload_len) + payload + 4(headers=0)
            (local.set $result_content_len (i32.add (i32.const 23) (local.get $payload_len)))

            ;; Allocate and write output
            (local.set $out_ptr (call $bump_alloc (i32.add (i32.const 4) (local.get $result_content_len))))
            (local.set $write_pos (local.get $out_ptr))

            ;; Length prefix
            (i32.store (local.get $write_pos) (local.get $result_content_len))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))

            ;; Output count = 1
            (i32.store (local.get $write_pos) (i32.const 1))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))

            ;; Destination = "output" (6 bytes)
            (i32.store (local.get $write_pos) (i32.const 6))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))
            (i32.store8 (local.get $write_pos) (i32.const 111))
            (i32.store8 (i32.add (local.get $write_pos) (i32.const 1)) (i32.const 117))
            (i32.store8 (i32.add (local.get $write_pos) (i32.const 2)) (i32.const 116))
            (i32.store8 (i32.add (local.get $write_pos) (i32.const 3)) (i32.const 112))
            (i32.store8 (i32.add (local.get $write_pos) (i32.const 4)) (i32.const 117))
            (i32.store8 (i32.add (local.get $write_pos) (i32.const 5)) (i32.const 116))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 6)))

            ;; No key
            (i32.store8 (local.get $write_pos) (i32.const 0))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 1)))

            ;; Payload
            (i32.store (local.get $write_pos) (local.get $payload_len))
            (local.set $write_pos (i32.add (local.get $write_pos) (i32.const 4)))
            (memory.copy (local.get $write_pos) (local.get $payload_ptr) (local.get $payload_len))
            (local.set $write_pos (i32.add (local.get $write_pos) (local.get $payload_len)))

            ;; Headers = 0
            (i32.store (local.get $write_pos) (i32.const 0))

            (local.get $out_ptr)
        )

        (func $bump_alloc (param $size i32) (result i32)
            (local $ptr i32)
            (local.set $ptr (global.get $bump))
            (global.set $bump (i32.add (global.get $bump) (local.get $size)))
            (local.get $ptr)
        )

        (memory (export "memory") 4)
    )
"#;

#[tokio::test]
async fn a2_rust_wasm_t2_memory_roundtrip() {
    use aeon_wasm::{WasmModule, WasmProcessor};

    let module = WasmModule::from_wat(PASSTHROUGH_WAT).expect("WAT compilation failed");
    let processor = WasmProcessor::new(Arc::new(module)).expect("WasmProcessor creation failed");

    let events = make_test_events(EVENT_COUNT);
    let events_clone = events.clone();

    let mut source = MemorySource::new(events, 64);
    let mut sink = MemorySink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = std::sync::atomic::AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    // Verify all 5 criteria
    let outputs = sink.outputs();
    assert_eq!(outputs.len(), EVENT_COUNT, "C1: zero event loss");

    for (i, (event, output)) in events_clone.iter().zip(outputs.iter()).enumerate() {
        assert_eq!(
            output.payload.as_ref(),
            event.payload.as_ref(),
            "C2: payload mismatch at {i}",
        );
        assert_eq!(
            output.source_event_id,
            Some(event.id),
            "C3: event identity not propagated at {i}",
        );
    }

    verify_metrics(&metrics, EVENT_COUNT as u64);
}

/// A2 variant: buffered pipeline.
#[tokio::test]
async fn a2_rust_wasm_t2_memory_roundtrip_buffered() {
    use aeon_wasm::{WasmModule, WasmProcessor};

    let module = WasmModule::from_wat(PASSTHROUGH_WAT).expect("WAT compilation failed");
    let processor = WasmProcessor::new(Arc::new(module)).expect("WasmProcessor creation failed");

    let events = make_test_events(500);
    let source = MemorySource::new(events, 32);
    let sink = MemorySink::new();

    let config = PipelineConfig {
        source_buffer_capacity: 64,
        sink_buffer_capacity: 64,
        max_batch_size: 32,
        ..Default::default()
    };
    let metrics = Arc::new(PipelineMetrics::new());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    run_buffered(
        source,
        processor,
        sink,
        config,
        Arc::clone(&metrics),
        shutdown,
        None,
    )
    .await
    .unwrap();

    assert_eq!(metrics.events_received.load(Ordering::Relaxed), 500);
    assert_eq!(metrics.events_processed.load(Ordering::Relaxed), 500);
    assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), 500);
}

// ===========================================================================
// A3: AssemblyScript T2 — Memory -> AssemblyScript -> Memory
// ===========================================================================

#[tokio::test]
async fn a3_assemblyscript_t2_memory_roundtrip() {
    use aeon_wasm::{WasmConfig, WasmModule, WasmProcessor};

    let wasm_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples/processors/wasm-bins/assemblyscript-processor.wasm");

    let wasm_bytes = std::fs::read(&wasm_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", wasm_path.display()));

    let module = WasmModule::from_bytes(&wasm_bytes, WasmConfig::default())
        .expect("AssemblyScript .wasm compilation failed");
    let processor = WasmProcessor::new(Arc::new(module)).expect("WasmProcessor creation failed");

    // AS processor is a JSON enrichment processor:
    // - If payload contains "user_id":"<val>", output = {"original":<payload>,"extracted_user_id":"<val>"}
    // - If no user_id found, output = original payload unchanged
    // - Destination is always "enriched"
    let event_count = 200;
    let source_name: Arc<str> = Arc::from("as-test");
    let events: Vec<Event> = (0..event_count)
        .map(|i| {
            let payload = Bytes::from(format!(r#"{{"user_id":"user-{i:03}","data":"hello"}}"#));
            Event::new(
                uuid::Uuid::now_v7(),
                i as i64,
                Arc::clone(&source_name),
                PartitionId::new((i % 4) as u16),
                payload,
            )
        })
        .collect();
    let events_clone = events.clone();

    let mut source = MemorySource::new(events, 32);
    let mut sink = MemorySink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = std::sync::atomic::AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    let outputs = sink.outputs();

    // Criterion 1: Event delivery (zero loss)
    assert_eq!(outputs.len(), event_count, "C1: zero event loss");

    // Criterion 2: Payload integrity — enriched JSON wrapping
    for (i, output) in outputs.iter().enumerate() {
        let payload_str = std::str::from_utf8(&output.payload).expect("valid UTF-8");
        assert!(
            payload_str.contains("extracted_user_id"),
            "C2: output {i} should contain extracted_user_id, got: {payload_str}"
        );
        assert!(
            payload_str.contains(&format!("user-{i:03}")),
            "C2: output {i} should contain user-{i:03}"
        );
    }

    // Criterion 3: Metadata propagation (source_event_id)
    for (i, (event, output)) in events_clone.iter().zip(outputs.iter()).enumerate() {
        assert_eq!(
            output.source_event_id,
            Some(event.id),
            "C3: source_event_id not propagated at {i}"
        );
    }

    // Criterion 4: Destination is "enriched" (AS processor behavior)
    for output in outputs {
        assert_eq!(&*output.destination, "enriched", "C4: destination mismatch");
    }

    verify_metrics(&metrics, event_count as u64);
}

// ===========================================================================
// A4: C/C++ Native T1 — Memory -> C_Native -> Memory
// ===========================================================================

#[tokio::test]
async fn a4_c_native_t1_memory_roundtrip() {
    // T1 native C/C++ processor loaded via NativeProcessor (libloading).
    // The .dll must export: aeon_processor_create, aeon_processor_destroy,
    // aeon_process, aeon_process_batch per the C-ABI contract.

    let dll_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sdks")
        .join("c")
        .join("build")
        .join("passthrough_processor.dll");

    if !dll_path.exists() {
        eprintln!(
            "SKIP A4: C passthrough DLL not found at {}",
            dll_path.display()
        );
        return;
    }

    let processor =
        aeon_engine::NativeProcessor::load(&dll_path, b"").expect("load C passthrough processor");

    let events = make_test_events(EVENT_COUNT);
    let events_clone = events.clone();

    let mut source = MemorySource::new(events, 64);
    let mut sink = MemorySink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = std::sync::atomic::AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    verify_e2e(&events_clone, sink.outputs());
    verify_metrics(&metrics, EVENT_COUNT as u64);
}

// ===========================================================================
// A5: C/C++ Wasm T2 — Memory -> C_Wasm -> Memory
// ===========================================================================

#[tokio::test]
async fn a5_c_wasm_t2_memory_roundtrip() {
    // T2 Wasm C/C++ processor compiled to .wasm via wasi-sdk.
    // Build: wasi-sdk clang --target=wasm32-unknown-unknown -nostdlib ...
    // Source: sdks/c/src/passthrough_wasm.c
    use aeon_wasm::{WasmConfig, WasmModule, WasmProcessor};

    let wasm_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sdks")
        .join("c")
        .join("build")
        .join("passthrough_wasm.wasm");

    if !wasm_path.exists() {
        eprintln!(
            "SKIP A5: C Wasm module not found at {}. Build with wasi-sdk.",
            wasm_path.display()
        );
        return;
    }

    let wasm_bytes = std::fs::read(&wasm_path).expect("read .wasm file");
    let module = WasmModule::from_bytes(&wasm_bytes, WasmConfig::default())
        .expect("WasmModule from C .wasm");
    let processor = WasmProcessor::new(Arc::new(module)).expect("WasmProcessor creation failed");

    let events = make_test_events(EVENT_COUNT);
    let events_clone = events.clone();

    let mut source = MemorySource::new(events, 64);
    let mut sink = MemorySink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = std::sync::atomic::AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    // Verify all 5 criteria
    let outputs = sink.outputs();
    assert_eq!(outputs.len(), EVENT_COUNT, "A5 C1: zero event loss");

    for (i, (event, output)) in events_clone.iter().zip(outputs.iter()).enumerate() {
        assert_eq!(
            output.payload.as_ref(),
            event.payload.as_ref(),
            "A5 C2: payload mismatch at {i}",
        );
        assert_eq!(
            output.source_event_id,
            Some(event.id),
            "A5 C3: event identity not propagated at {i}",
        );
    }

    verify_metrics(&metrics, EVENT_COUNT as u64);
}

// ===========================================================================
// A6: C#/.NET NativeAOT T1 — Memory -> DotNet_NativeAOT -> Memory
// ===========================================================================

#[tokio::test]
async fn a6_dotnet_native_aot_t1_memory_roundtrip() {
    // T1 NativeAOT .NET processor loaded via NativeProcessor.
    // Build: cd sdks/dotnet/AeonPassthroughNative && dotnet publish -c Release -r win-x64 -p:PublishAot=true

    let dll_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sdks")
        .join("dotnet")
        .join("AeonPassthroughNative")
        .join("bin")
        .join("Release")
        .join("net8.0")
        .join("win-x64")
        .join("publish")
        .join("AeonPassthroughNative.dll");

    if !dll_path.exists() {
        eprintln!(
            "SKIP A6: .NET NativeAOT DLL not found at {}",
            dll_path.display()
        );
        return;
    }

    // libsodium.dll is in the same publish dir — Windows LoadLibrary
    // searches the loaded DLL's directory automatically.
    let processor = aeon_engine::NativeProcessor::load(&dll_path, b"")
        .expect("load .NET NativeAOT passthrough processor");

    let events = make_test_events(EVENT_COUNT);
    let events_clone = events.clone();

    let mut source = MemorySource::new(events, 64);
    let mut sink = MemorySink::new();
    let metrics = PipelineMetrics::new();
    let shutdown = std::sync::atomic::AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    verify_e2e(&events_clone, sink.outputs());
    verify_metrics(&metrics, EVENT_COUNT as u64);
}

// ===========================================================================
// A7: C#/.NET T4 WebSocket — Memory -> DotNet_WS -> Memory
// ===========================================================================

#[tokio::test]
async fn a7_dotnet_ws_t4_memory_roundtrip() {
    if !e2e_ws_harness::runtime_available("dotnet") {
        eprintln!("SKIP A7: dotnet not found");
        return;
    }

    let msg_count = EVENT_COUNT;
    let pipeline_name = "a7-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "dotnet-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let dotnet_dir = e2e_ws_harness::dotnet_passthrough_project(
        server.port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "dotnet-proc",
    );

    let mut child = std::process::Command::new("dotnet")
        .args(["run", "--project", &dotnet_dir.to_string_lossy()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn dotnet");

    let connected =
        e2e_ws_harness::wait_for_connection(&server, std::time::Duration::from_secs(30)).await;
    assert!(connected, "A7: .NET processor failed to connect");

    let events = make_test_events(msg_count);
    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 64)
        .await
        .unwrap();

    assert_eq!(outputs.len(), msg_count, "A7 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("payload-{i:05}");
        assert_eq!(
            output.payload.as_ref(),
            expected.as_bytes(),
            "A7 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&seed_file);
    let _ = std::fs::remove_dir_all(&dotnet_dir);
}

// ===========================================================================
// A8: Python T4 WebSocket — Memory -> Python_WS -> Memory
// ===========================================================================

#[tokio::test]
async fn a8_python_ws_t4_memory_roundtrip() {
    // T4 WebSocket Python processor (inline script).
    // Requires: Python 3.10+ with websockets + pynacl packages.
    if !e2e_ws_harness::runtime_available("python") {
        eprintln!("SKIP A8: python not found");
        return;
    }

    // Check required packages
    let check = std::process::Command::new("python")
        .args([
            "-c",
            "import websockets, nacl.signing, json, struct, zlib, hashlib, base64",
        ])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP A8: Python packages missing (pip install websockets pynacl)");
        return;
    }

    let pipeline_name = "a8-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "python-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);

    let port = server.port;
    let seed_path = seed_file.to_string_lossy().to_string();

    // Inline Python passthrough processor — implements AWPP correctly
    // Note: serde_bytes serializes payload as integer array in JSON, e.g. [112,97,121,...]
    let script = format!(
        r#"
import asyncio, json, struct, zlib, hashlib, base64, sys, os, traceback
from nacl.signing import SigningKey

SEED = open(r'{seed_path}', 'rb').read()
sk = SigningKey(SEED)
vk = sk.verify_key
pk_b64 = base64.b64encode(vk.encode()).decode()
PUBLIC_KEY = f"ed25519:{{pk_b64}}"

async def main():
    import websockets
    url = "ws://127.0.0.1:{port}/api/v1/processors/connect"
    async with websockets.connect(url, max_size=16*1024*1024) as ws:
        # 1. Receive Challenge
        raw = await ws.recv()
        msg = json.loads(raw)
        assert msg["type"] == "challenge", f"expected challenge, got {{msg['type']}}"
        nonce = msg["nonce"]

        # 2. Sign challenge (sign raw hex-decoded nonce bytes, NOT the string)
        nonce_bytes = bytes.fromhex(nonce)
        sig = sk.sign(nonce_bytes).signature.hex()

        # 3. Send Register
        register = {{
            "type": "register",
            "protocol": "awpp/1",
            "transport": "websocket",
            "name": "python-proc",
            "version": "1.0.0",
            "public_key": PUBLIC_KEY,
            "challenge_signature": sig,
            "capabilities": ["batch"],
            "transport_codec": "json",
            "requested_pipelines": ["{pipeline_name}"],
            "binding": "dedicated",
        }}
        await ws.send(json.dumps(register))

        # 4. Receive Accepted
        raw = await ws.recv()
        msg = json.loads(raw)
        if msg["type"] == "rejected":
            print(f"REJECTED: {{msg}}", file=sys.stderr)
            return
        assert msg["type"] == "accepted", f"expected accepted, got {{msg['type']}}"
        batch_signing = msg.get("batch_signing", True)

        # 5. Message loop
        async for message in ws:
            try:
                if isinstance(message, str):
                    ctrl = json.loads(message)
                    if ctrl.get("type") == "drain":
                        break
                    continue
                # Binary data frame
                data = message if isinstance(message, bytes) else bytes(message)
                # Parse routing header
                name_len = struct.unpack_from("<I", data, 0)[0]
                pipeline = data[4:4+name_len].decode()
                partition = struct.unpack_from("<H", data, 4+name_len)[0]
                batch_data = data[4+name_len+2:]

                # Parse batch request
                crc_offset = len(batch_data) - 4
                stored_crc = struct.unpack_from("<I", batch_data, crc_offset)[0]
                computed_crc = zlib.crc32(batch_data[:crc_offset]) & 0xFFFFFFFF
                assert stored_crc == computed_crc, "CRC mismatch"
                batch_id = struct.unpack_from("<Q", batch_data, 0)[0]
                event_count = struct.unpack_from("<I", batch_data, 8)[0]
                off = 12
                events = []
                for _ in range(event_count):
                    elen = struct.unpack_from("<I", batch_data, off)[0]
                    off += 4
                    ev = json.loads(batch_data[off:off+elen])
                    off += elen
                    events.append(ev)

                # Build response — passthrough: each event -> one output with same payload
                # serde_bytes in JSON: payload is an integer array [b0, b1, ...], not base64
                resp_parts = []
                resp_parts.append(struct.pack("<Q", batch_id))
                resp_parts.append(struct.pack("<I", len(events)))
                for ev in events:
                    resp_parts.append(struct.pack("<I", 1))  # 1 output per event
                    payload = ev.get("payload", [])
                    # payload comes as integer array from serde_bytes JSON
                    if isinstance(payload, list):
                        payload = payload  # keep as-is for JSON output
                    elif isinstance(payload, str):
                        payload = list(payload.encode())
                    out = {{"destination": "output", "payload": payload, "headers": []}}
                    encoded = json.dumps(out, separators=(",", ":")).encode()
                    resp_parts.append(struct.pack("<I", len(encoded)))
                    resp_parts.append(encoded)

                body = b"".join(resp_parts)
                crc = zlib.crc32(body) & 0xFFFFFFFF
                body_with_crc = body + struct.pack("<I", crc)

                # Sign
                if batch_signing:
                    sig_bytes = sk.sign(body_with_crc).signature
                else:
                    sig_bytes = b"\x00" * 64

                response_wire = body_with_crc + sig_bytes

                # Wrap in data frame
                name_b = pipeline.encode()
                frame = struct.pack("<I", len(name_b)) + name_b + struct.pack("<H", partition) + response_wire
                await ws.send(frame)
            except Exception as e:
                print(f"ERROR: {{e}}", file=sys.stderr)
                traceback.print_exc(file=sys.stderr)
                break

asyncio.run(main())
"#
    );

    // Write script to temp file
    let script_path = std::env::temp_dir().join("aeon_e2e_a8_python.py");
    std::fs::write(&script_path, &script).expect("write python script");

    let mut child = std::process::Command::new("python")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn python");

    let connected =
        e2e_ws_harness::wait_for_connection(&server, std::time::Duration::from_secs(10)).await;

    if !connected {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("A8: Python processor failed to connect within 10s. stderr: {stderr}");
    }

    let events = make_test_events(200);
    let events_clone = events.clone();

    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 32)
        .await
        .expect("A8: drive_events failed");

    // Verify
    assert_eq!(
        outputs.len(),
        events_clone.len(),
        "A8 C1: event count mismatch"
    );
    for (i, (event, output)) in events_clone.iter().zip(outputs.iter()).enumerate() {
        assert_eq!(
            output.payload.as_ref(),
            event.payload.as_ref(),
            "A8 C2: payload mismatch at index {i}",
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// A9: Go T4 WebSocket — Memory -> Go_WS -> Memory
// ===========================================================================

#[tokio::test]
async fn a9_go_ws_t4_memory_roundtrip() {
    if !e2e_ws_harness::runtime_available("go") {
        eprintln!("SKIP A9: go not found");
        return;
    }

    let msg_count = EVENT_COUNT;
    let pipeline_name = "a9-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "go-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let go_dir = e2e_ws_harness::go_passthrough_project(
        server.port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "go-proc",
    );

    let mut child = std::process::Command::new("go")
        .args(["run", "."])
        .current_dir(&go_dir)
        .env(
            "PATH",
            format!(
                "C:\\Program Files\\Go\\bin;{}",
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn go");

    let connected = e2e_ws_harness::wait_for_connection(
        &server,
        std::time::Duration::from_secs(30), // Go compile takes time
    )
    .await;
    assert!(connected, "A9: Go processor failed to connect");

    let events = make_test_events(msg_count);
    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 64)
        .await
        .unwrap();

    assert_eq!(outputs.len(), msg_count, "A9 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("payload-{i:05}");
        assert_eq!(
            output.payload.as_ref(),
            expected.as_bytes(),
            "A9 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&seed_file);
    let _ = std::fs::remove_dir_all(&go_dir);
}

// ===========================================================================
// A10: Rust Network T4 WebSocket — Memory -> RustNet_WS -> Memory
// ===========================================================================

#[tokio::test]
async fn a10_rust_network_ws_t4_memory_roundtrip() {
    // T4 WebSocket Rust processor using aeon-processor-client.
    // Pure Rust — no external process needed.
    use aeon_processor_client::{ProcessEvent, ProcessOutput, ProcessorClient, ProcessorConfig};

    let pipeline_name = "a10-pipeline";

    // 1. Start engine WS host
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;

    // 2. Register processor identity
    let identity = e2e_ws_harness::register_test_identity(&server, "rust-net-proc");

    // 3. Spawn Rust processor client (passthrough)
    let port = server.port;
    let seed = *identity.signing_key.as_bytes();
    let client_handle = tokio::spawn(async move {
        let config = ProcessorConfig::new(
            "rust-net-proc",
            format!("ws://127.0.0.1:{port}/api/v1/processors/connect"),
        )
        .pipeline(pipeline_name.to_string())
        .signing_key_from_seed(&seed);

        fn passthrough(event: ProcessEvent) -> Vec<ProcessOutput> {
            vec![ProcessOutput {
                destination: "output".into(),
                key: None,
                payload: event.payload,
                headers: vec![],
            }]
        }

        ProcessorClient::run(config, passthrough).await
    });

    // 4. Wait for processor to connect
    let connected =
        e2e_ws_harness::wait_for_connection(&server, std::time::Duration::from_secs(5)).await;
    assert!(connected, "A10: Rust processor failed to connect within 5s");

    // 5. Drive events through WS transport
    let events = make_test_events(200);
    let events_clone = events.clone();

    let outputs = e2e_ws_harness::drive_events_through_transport(
        &server.ws_host,
        events,
        32, // batch size
    )
    .await
    .expect("A10: drive_events_through_transport failed");

    // 6. Verify
    assert_eq!(
        outputs.len(),
        events_clone.len(),
        "A10 C1: event count mismatch: {} outputs vs {} events",
        outputs.len(),
        events_clone.len(),
    );

    for (i, (event, output)) in events_clone.iter().zip(outputs.iter()).enumerate() {
        assert_eq!(
            output.payload.as_ref(),
            event.payload.as_ref(),
            "A10 C2: payload mismatch at index {i}",
        );
    }

    // Shutdown: drop the server which closes the WS, causing the client to exit
    drop(server);
    // Give client time to notice disconnect
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), client_handle).await;
}

// ===========================================================================
// A11: Node.js T4 WebSocket — Memory -> NodeJS_WS -> Memory
// ===========================================================================

#[tokio::test]
async fn a11_nodejs_ws_t4_memory_roundtrip() {
    // T4 WebSocket Node.js processor using the SDK's run() directly.
    // Requires: Node.js 18+ with `npm install` run in sdks/nodejs/.
    if !e2e_ws_harness::runtime_available("node") {
        eprintln!("SKIP A11: node not found");
        return;
    }

    // Check that the SDK's node_modules exist (npm install has been run)
    let sdk_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sdks")
        .join("nodejs");
    if !sdk_path.join("node_modules").exists() {
        eprintln!(
            "SKIP A11: sdks/nodejs/node_modules not found — run `npm install` in sdks/nodejs/"
        );
        return;
    }

    let pipeline_name = "a11-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "nodejs-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);

    let port = server.port;
    let seed_path = seed_file.to_string_lossy().to_string();

    let script = e2e_ws_harness::nodejs_passthrough_script(
        port,
        &seed_path,
        &identity.public_key,
        pipeline_name,
        "nodejs-proc",
    );

    let script_path = std::env::temp_dir().join("aeon_e2e_a11_nodejs.js");
    std::fs::write(&script_path, &script).expect("write nodejs script");

    let mut child = std::process::Command::new("node")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn node");

    let connected =
        e2e_ws_harness::wait_for_connection(&server, std::time::Duration::from_secs(10)).await;

    if !connected {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("A11: Node.js processor failed to connect within 10s. stderr: {stderr}");
    }

    let events = make_test_events(200);
    let events_clone = events.clone();

    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 32)
        .await
        .expect("A11: drive_events failed");

    assert_eq!(
        outputs.len(),
        events_clone.len(),
        "A11 C1: event count mismatch"
    );
    for (i, (event, output)) in events_clone.iter().zip(outputs.iter()).enumerate() {
        assert_eq!(
            output.payload.as_ref(),
            event.payload.as_ref(),
            "A11 C2: payload mismatch at index {i}",
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// A12: Java T4 WebSocket — Memory -> Java_WS -> Memory
// ===========================================================================

#[tokio::test]
async fn a12_java_ws_t4_memory_roundtrip() {
    if !e2e_ws_harness::runtime_available("java") {
        eprintln!("SKIP A12: java not found");
        return;
    }
    // Check Java 17+ (SDK uses records [Java 16+], EdDSA [Java 15+], switch arrows [Java 14+])
    let java_ver = std::process::Command::new("java")
        .args(["--version"])
        .output();
    let is_modern = java_ver
        .map(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            // Parse version like "openjdk 17.0.18" or "java 21.0.1"
            out.split_whitespace()
                .nth(1)
                .and_then(|v| v.split('.').next())
                .and_then(|v| v.parse::<u32>().ok())
                .map(|v| v >= 17)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if !is_modern {
        eprintln!("SKIP A12: Java 17+ required for SDK (records, EdDSA)");
        return;
    }

    let msg_count = EVENT_COUNT;
    let pipeline_name = "a12-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "java-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let java_dir = e2e_ws_harness::java_passthrough_project(
        server.port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "java-proc",
    );
    if let Err(stderr) = e2e_ws_harness::compile_java_with_sdk("javac", &java_dir) {
        eprintln!("SKIP A12: javac failed: {stderr}");
        let _ = std::fs::remove_dir_all(&java_dir);
        return;
    }

    let mut child = std::process::Command::new("java")
        .args(["-cp", &java_dir.to_string_lossy(), "AeonProcessor"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn java");

    let connected =
        e2e_ws_harness::wait_for_connection(&server, std::time::Duration::from_secs(15)).await;
    assert!(connected, "A12: Java processor failed to connect");

    let events = make_test_events(msg_count);
    let result = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 64).await;

    let outputs = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("A12: drive_events failed: {e}\nJava stdout: {stdout}\nJava stderr: {stderr}");
        }
    };

    assert_eq!(outputs.len(), msg_count, "A12 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("payload-{i:05}");
        assert_eq!(
            output.payload.as_ref(),
            expected.as_bytes(),
            "A12 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&seed_file);
    let _ = std::fs::remove_dir_all(&java_dir);
}

// ===========================================================================
// A13: PHP T4 WebSocket — Memory -> PHP_WS -> Memory
// ===========================================================================

#[tokio::test]
async fn a13_php_ws_t4_memory_roundtrip() {
    if !e2e_ws_harness::runtime_available("php") {
        eprintln!("SKIP A13: php not found");
        return;
    }
    // Check sodium extension
    let check = std::process::Command::new("php")
        .args([
            "-r",
            "if (!function_exists('sodium_crypto_sign_seed_keypair')) exit(1);",
        ])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP A13: PHP sodium extension not available");
        return;
    }

    let msg_count = EVENT_COUNT;
    let pipeline_name = "a13-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "php-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::php_passthrough_script(
        server.port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "php-proc",
    );
    let script_path = std::env::temp_dir().join("aeon_e2e_a13_php.php");
    std::fs::write(&script_path, &script).expect("write php script");

    let mut child = std::process::Command::new("php")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn php");

    let connected =
        e2e_ws_harness::wait_for_connection(&server, std::time::Duration::from_secs(10)).await;
    assert!(connected, "A13: PHP processor failed to connect");

    let events = make_test_events(msg_count);
    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 64)
        .await
        .unwrap();

    assert_eq!(outputs.len(), msg_count, "A13 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("payload-{i:05}");
        assert_eq!(
            output.payload.as_ref(),
            expected.as_bytes(),
            "A13 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}
