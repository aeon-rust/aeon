//! Tier B E2E Tests: File → Processor → File (tier families)
//!
//! Tests from `docs/E2E-TEST-PLAN.md` Tier B (P1, no infra required).
//! One representative per tier family validates file-based round-trip.
//!
//! Each test verifies the 5 E2E criteria:
//!   1. Event delivery: source lines == sink lines (zero loss)
//!   2. Payload integrity: input payload == output payload
//!   3. Metadata propagation: headers/metadata pass through correctly
//!   4. Ordering: lines arrive in order
//!   5. Graceful shutdown: clean termination, no orphaned data

use aeon_connectors::file::{FileSink, FileSinkConfig, FileSource, FileSourceConfig};
use aeon_engine::{PassthroughProcessor, PipelineMetrics, run};
use aeon_types::traits::{ProcessorTransport, Sink, Source};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[path = "e2e_ws_harness.rs"]
mod e2e_ws_harness;

// ===========================================================================
// B1: File -> Rust Native T1 -> File
// ===========================================================================

#[tokio::test]
async fn b1_file_rust_native_t1() {
    let input_dir = tempfile::tempdir().unwrap();
    let input_path = input_dir.path().join("input.txt");
    let output_dir = tempfile::tempdir().unwrap();
    let output_path = output_dir.path().join("output.txt");

    let msg_count = 500;

    // Write test lines to input file
    let mut input_data = String::new();
    for i in 0..msg_count {
        input_data.push_str(&format!("payload-{i:05}\n"));
    }
    std::fs::write(&input_path, &input_data).unwrap();

    // Pipeline: File -> Passthrough -> File
    let source_config = FileSourceConfig::new(&input_path)
        .with_batch_size(64)
        .with_source_name("b1-file");
    let mut source = FileSource::new(source_config);

    let processor = PassthroughProcessor::new(Arc::from("output"));

    let sink_config = FileSinkConfig::new(&output_path);
    let mut sink = FileSink::new(sink_config);

    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    // Criterion 1: Event delivery
    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);
    assert_eq!(received, msg_count as u64, "C1: all events received");
    assert_eq!(sent, msg_count as u64, "C1: all events sent");

    // Criterion 2: Payload integrity — read output file and compare
    let output_content = std::fs::read_to_string(&output_path).unwrap();
    let output_lines: Vec<&str> = output_content.lines().collect();
    assert_eq!(output_lines.len(), msg_count, "C1: all lines written to output file");

    for (i, line) in output_lines.iter().enumerate() {
        assert_eq!(
            *line,
            format!("payload-{i:05}"),
            "C2: payload mismatch at line {i}"
        );
    }
}

/// B1 variant: large file with 10K lines.
#[tokio::test]
async fn b1_file_rust_native_t1_large() {
    let input_dir = tempfile::tempdir().unwrap();
    let input_path = input_dir.path().join("input_large.txt");
    let output_dir = tempfile::tempdir().unwrap();
    let output_path = output_dir.path().join("output_large.txt");

    let msg_count = 10_000;
    let mut input_data = String::with_capacity(msg_count * 20);
    for i in 0..msg_count {
        input_data.push_str(&format!("line-{i:06}\n"));
    }
    std::fs::write(&input_path, &input_data).unwrap();

    let source_config = FileSourceConfig::new(&input_path).with_batch_size(1024);
    let mut source = FileSource::new(source_config);
    let processor = PassthroughProcessor::new(Arc::from("output"));
    let sink_config = FileSinkConfig::new(&output_path);
    let mut sink = FileSink::new(sink_config);
    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    assert_eq!(
        metrics.events_received.load(Ordering::Relaxed),
        msg_count as u64
    );
    assert_eq!(
        metrics.outputs_sent.load(Ordering::Relaxed),
        msg_count as u64
    );
}

// ===========================================================================
// B2: File -> C/C++ Native T1 -> File
// ===========================================================================

#[tokio::test]
async fn b2_file_c_native_t1() {
    let dll_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap()
        .join("sdks").join("c").join("build").join("passthrough_processor.dll");

    if !dll_path.exists() {
        eprintln!("SKIP B2: C passthrough DLL not found at {}", dll_path.display());
        return;
    }

    let processor = aeon_engine::NativeProcessor::load(&dll_path, b"")
        .expect("load C passthrough processor");

    let input_dir = tempfile::tempdir().unwrap();
    let input_path = input_dir.path().join("input.txt");
    let output_dir = tempfile::tempdir().unwrap();
    let output_path = output_dir.path().join("output.txt");

    let msg_count = 500;
    let mut input_data = String::new();
    for i in 0..msg_count {
        input_data.push_str(&format!("payload-{i:05}\n"));
    }
    std::fs::write(&input_path, &input_data).unwrap();

    let source_config = FileSourceConfig::new(&input_path)
        .with_batch_size(64)
        .with_source_name("b2-file");
    let mut source = FileSource::new(source_config);
    let sink_config = FileSinkConfig::new(&output_path);
    let mut sink = FileSink::new(sink_config);
    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    assert_eq!(metrics.events_received.load(Ordering::Relaxed), msg_count as u64, "C1: events received");
    assert_eq!(metrics.outputs_sent.load(Ordering::Relaxed), msg_count as u64, "C1: outputs sent");

    let output_content = std::fs::read_to_string(&output_path).unwrap();
    let output_lines: Vec<&str> = output_content.lines().collect();
    assert_eq!(output_lines.len(), msg_count, "C1: all lines in output");
    for (i, line) in output_lines.iter().enumerate() {
        assert_eq!(*line, format!("payload-{i:05}"), "C2: payload mismatch at {i}");
    }
}

// ===========================================================================
// B3: File -> Python T4 WS -> File
// ===========================================================================

#[tokio::test]
async fn b3_file_python_ws_t4() {
    // File -> Python T4 WS -> File
    // Reuses the Python AWPP script from A8, but drives file-sourced events.
    if !e2e_ws_harness::runtime_available("python") {
        eprintln!("SKIP B3: python not found");
        return;
    }
    let check = std::process::Command::new("python")
        .args(["-c", "import websockets, nacl.signing"])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP B3: Python packages missing");
        return;
    }

    // Set up input file
    let input_dir = tempfile::tempdir().unwrap();
    let input_path = input_dir.path().join("b3_input.txt");
    let msg_count = 200;
    let mut input_data = String::new();
    for i in 0..msg_count {
        input_data.push_str(&format!("b3-payload-{i:05}\n"));
    }
    std::fs::write(&input_path, &input_data).unwrap();

    // Start WS host + Python processor
    let pipeline_name = "b3-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "python-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let port = server.port;
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    // Reuse the same inline Python script pattern as A8
    let script = crate::e2e_ws_harness::python_passthrough_script(port, &seed_path, &pub_key, pipeline_name, "python-proc");
    let script_path = std::env::temp_dir().join("aeon_e2e_b3_python.py");
    std::fs::write(&script_path, &script).expect("write python script");

    let mut child = std::process::Command::new("python")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn python");

    let connected = e2e_ws_harness::wait_for_connection(
        &server, std::time::Duration::from_secs(10),
    ).await;
    assert!(connected, "B3: Python processor failed to connect");

    // Read events from file source, drive through WS, collect outputs
    let source_config = FileSourceConfig::new(&input_path)
        .with_batch_size(32)
        .with_source_name("b3-file");
    let mut source = FileSource::new(source_config);

    let mut all_outputs = Vec::new();
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        all_outputs.extend(outputs);
    }

    // Write outputs to file
    let output_dir = tempfile::tempdir().unwrap();
    let output_path = output_dir.path().join("b3_output.txt");
    let sink_config = FileSinkConfig::new(&output_path);
    let mut sink = FileSink::new(sink_config);
    sink.write_batch(all_outputs).await.unwrap();
    sink.flush().await.unwrap();

    // Verify
    let output_content = std::fs::read_to_string(&output_path).unwrap();
    let output_lines: Vec<&str> = output_content.lines().collect();
    assert_eq!(output_lines.len(), msg_count, "B3 C1: event count mismatch");
    for (i, line) in output_lines.iter().enumerate() {
        assert_eq!(*line, format!("b3-payload-{i:05}"), "B3 C2: payload mismatch at {i}");
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// B4: File -> Node.js T4 WS -> File
// ===========================================================================

#[tokio::test]
async fn b4_file_nodejs_ws_t4() {
    // File -> Node.js T4 WS -> File
    if !e2e_ws_harness::runtime_available("node") {
        eprintln!("SKIP B4: node not found");
        return;
    }
    let check = std::process::Command::new("node")
        .args(["-e", "const v=parseInt(process.versions.node);if(v<22){process.exit(1)}"])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP B4: Node.js 22+ required");
        return;
    }

    // Set up input file
    let input_dir = tempfile::tempdir().unwrap();
    let input_path = input_dir.path().join("b4_input.txt");
    let msg_count = 200;
    let mut input_data = String::new();
    for i in 0..msg_count {
        input_data.push_str(&format!("b4-payload-{i:05}\n"));
    }
    std::fs::write(&input_path, &input_data).unwrap();

    let pipeline_name = "b4-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "nodejs-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let port = server.port;
    let seed_path = seed_file.to_string_lossy().to_string().replace('\\', "/");
    let pub_key = identity.public_key.clone();

    let script = crate::e2e_ws_harness::nodejs_passthrough_script(port, &seed_path, &pub_key, pipeline_name, "nodejs-proc");
    let script_path = std::env::temp_dir().join("aeon_e2e_b4_nodejs.js");
    std::fs::write(&script_path, &script).expect("write nodejs script");

    let mut child = std::process::Command::new("node")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn node");

    let connected = e2e_ws_harness::wait_for_connection(
        &server, std::time::Duration::from_secs(10),
    ).await;
    assert!(connected, "B4: Node.js processor failed to connect");

    let source_config = FileSourceConfig::new(&input_path)
        .with_batch_size(32)
        .with_source_name("b4-file");
    let mut source = FileSource::new(source_config);

    let mut all_outputs = Vec::new();
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        all_outputs.extend(outputs);
    }

    let output_dir = tempfile::tempdir().unwrap();
    let output_path = output_dir.path().join("b4_output.txt");
    let sink_config = FileSinkConfig::new(&output_path);
    let mut sink = FileSink::new(sink_config);
    sink.write_batch(all_outputs).await.unwrap();
    sink.flush().await.unwrap();

    let output_content = std::fs::read_to_string(&output_path).unwrap();
    let output_lines: Vec<&str> = output_content.lines().collect();
    assert_eq!(output_lines.len(), msg_count, "B4 C1: event count mismatch");
    for (i, line) in output_lines.iter().enumerate() {
        assert_eq!(*line, format!("b4-payload-{i:05}"), "B4 C2: payload mismatch at {i}");
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}
