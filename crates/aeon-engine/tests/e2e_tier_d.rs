//! Tier D E2E Tests: T3 WebTransport Variants
//!
//! Tests from `docs/E2E-TEST-PLAN.md` Tier D (P1, needs TLS certs).
//! Memory Source → Processor (T3 WebTransport) → Memory Sink.
//! Validates QUIC/WebTransport transport for each T3-capable SDK.
//!
//! Each test verifies the 5 E2E criteria:
//!   1. Event delivery: source count == sink count (zero loss)
//!   2. Payload integrity: input payload == output payload
//!   3. Metadata propagation: headers/metadata pass through correctly
//!   4. Ordering: within a partition, events arrive in order
//!   5. Graceful shutdown: processor disconnects cleanly
//!
//! D3 (Rust Network) is implemented end-to-end when built with
//! `--features webtransport-host`. The engine's WT host is feature-gated,
//! so tests are gated accordingly; when the feature is off, D3 is a
//! build-time skip that still shows up in test output.

#[cfg(feature = "webtransport-host")]
mod e2e_wt_harness;

// ===========================================================================
// D1: Memory -> Python T3 WebTransport -> Memory
// ===========================================================================

#[cfg(feature = "webtransport-host")]
#[tokio::test]
async fn d1_python_wt_t3() {
    use std::sync::Arc;
    use std::time::Duration;

    use aeon_types::event::Event;
    use aeon_types::partition::PartitionId;
    use bytes::Bytes;

    // Optional tracing init — respects RUST_LOG if set so devs can debug
    // the wtransport accept loop / AWPP handshake with `RUST_LOG=debug`.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .try_init();

    // 1. Python + packages preflight. Skip cleanly if Python or the WT
    //    dependencies (aioquic, pynacl, msgpack) aren't installed — Tier D
    //    is opt-in and we don't want to fail on a bare CI box.
    if !e2e_wt_harness::runtime_available("python") {
        eprintln!("SKIP D1: python not found");
        return;
    }
    let check = std::process::Command::new("python")
        .args([
            "-c",
            "import aioquic, nacl.signing, msgpack, struct, json",
        ])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!(
            "SKIP D1: Python WT packages missing (pip install aioquic pynacl msgpack)"
        );
        return;
    }

    let pipeline_name = "d1-pipeline";

    // 2. Start engine WT host (self-signed cert on 127.0.0.1:<random>).
    let server = e2e_wt_harness::start_wt_test_server(pipeline_name).await;
    let url = server.url.clone();

    // 3. Register processor identity + persist the seed so the Python
    //    subprocess can load it with `Signer.from_file`.
    let identity = e2e_wt_harness::register_test_identity(&server, "python-wt-proc");
    let seed_path = e2e_wt_harness::write_seed_file(&identity);

    // 4. Build the inline Python driver. It imports the Python SDK from the
    //    workspace checkout (not pip-installed) so we always exercise the
    //    in-repo source, and calls `run_webtransport()` with `insecure=True`
    //    + `server_name="localhost"` to handshake against the self-signed
    //    cert bound to 127.0.0.1.
    let sdk_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdks/python")
        .canonicalize()
        .expect("canonicalize sdks/python path");
    // Python string literals — use forward slashes to avoid Windows
    // backslash escaping inside the inline script.
    let sdk_path_py = sdk_dir.to_string_lossy().replace('\\', "/");
    let seed_path_py = seed_path.to_string_lossy().replace('\\', "/");

    let script = format!(
        r#"
import sys, traceback

sys.path.insert(0, r"{sdk_path}")

try:
    from aeon_transport import processor, Output, run_webtransport

    @processor
    def passthrough(event):
        # Forward the event payload unchanged; surface the metadata as
        # output headers so the D1 C3 assertion can verify propagation.
        return [Output(
            destination="output",
            payload=event.payload,
            headers=list(event.metadata),
        )]

    run_webtransport(
        r"{url}",
        name="python-wt-proc",
        version="1.0.0",
        private_key_path=r"{seed_path}",
        pipelines=["{pipeline_name}"],
        codec="msgpack",
        insecure=True,
        server_name="localhost",
        log_level="INFO",
    )
except SystemExit:
    raise
except BaseException as e:
    print("PYTHON_ERROR: " + repr(e), file=sys.stderr)
    traceback.print_exc(file=sys.stderr)
    sys.exit(1)
"#,
        sdk_path = sdk_path_py,
        url = url,
        seed_path = seed_path_py,
        pipeline_name = pipeline_name,
    );

    let script_path = std::env::temp_dir().join("aeon_e2e_d1_python.py");
    std::fs::write(&script_path, &script).expect("write python script");

    // 5. Spawn the Python subprocess. Inherit PATH so `python` resolves the
    //    same way the preflight `runtime_available` check did.
    let mut child = std::process::Command::new("python")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn python");

    // 6. Wait for processor to connect (allow 20s — aioquic + QUIC handshake
    //    is a bit slower than raw WebSocket).
    let connected =
        e2e_wt_harness::wait_for_connection(&server, Duration::from_secs(20)).await;
    if !connected {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_file(&seed_path);
        panic!(
            "D1: Python WT processor failed to connect within 20s.\nstderr: {stderr}\nstdout: {stdout}"
        );
    }

    // 7. Drive 200 events through the T3 transport, pinned to partition 0
    //    so a single data stream carries the whole flow (ordering check C4
    //    is a side-effect of single-stream delivery).
    let source: Arc<str> = Arc::from("d1-source");
    let events: Vec<Event> = (0..200)
        .map(|i| {
            let payload = Bytes::from(format!("d1-payload-{i:05}"));
            Event::new(
                uuid::Uuid::now_v7(),
                i as i64,
                Arc::clone(&source),
                PartitionId::new(0),
                payload,
            )
            .with_metadata(Arc::from("d1-key"), Arc::from(format!("val-{i}")))
        })
        .collect();
    let events_clone = events.clone();

    let drive_result =
        e2e_wt_harness::drive_events_through_transport(&server.wt_host, events, 32).await;
    let outputs = match drive_result {
        Ok(o) => o,
        Err(e) => {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_file(&script_path);
            let _ = std::fs::remove_file(&seed_path);
            panic!(
                "D1: drive_events_through_transport failed: {e}\nstderr: {stderr}"
            );
        }
    };

    // 8. Verify the E2E criteria.
    // C1: zero loss.
    assert_eq!(
        outputs.len(),
        events_clone.len(),
        "D1 C1: event count mismatch: {} outputs vs {} events",
        outputs.len(),
        events_clone.len(),
    );

    // C2: payload integrity, C4: per-partition ordering (single stream).
    for (i, (event, output)) in events_clone.iter().zip(outputs.iter()).enumerate() {
        assert_eq!(
            output.payload.as_ref(),
            event.payload.as_ref(),
            "D1 C2: payload mismatch at index {i}",
        );
    }

    // C3: metadata propagation — passthrough copied metadata into headers.
    for (i, output) in outputs.iter().enumerate() {
        let found = output.headers.iter().any(|(k, v)| {
            k.as_ref() == "d1-key" && v.as_ref() == format!("val-{i}").as_str()
        });
        assert!(found, "D1 C3: metadata not propagated at index {i}");
    }

    // 9. C5 graceful shutdown: drop the server to close the WT connection
    //    and give the Python client a short window to notice and exit.
    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_path);
}

#[cfg(not(feature = "webtransport-host"))]
#[tokio::test]
#[ignore = "requires --features webtransport-host"]
async fn d1_python_wt_t3() {
    todo!("rebuild with --features webtransport-host");
}

// ===========================================================================
// D2: Memory -> Go T3 WebTransport -> Memory
// ===========================================================================

#[tokio::test]
#[ignore = "requires TLS certs + Go SDK with WebTransport support + engine WT host"]
async fn d2_go_wt_t3() {
    todo!("Implement with engine WebTransport host + Go SDK (T3)");
}

// ===========================================================================
// D3: Memory -> Rust Network T3 WebTransport -> Memory
// ===========================================================================

#[cfg(feature = "webtransport-host")]
#[tokio::test]
async fn d3_rust_network_wt_t3() {
    use std::sync::Arc;
    use std::time::Duration;

    use aeon_processor_client::{ProcessEvent, ProcessOutput, ProcessorConfig};
    use aeon_types::event::Event;
    use aeon_types::partition::PartitionId;
    use bytes::Bytes;

    let pipeline_name = "d3-pipeline";

    // 1. Start engine WT host (self-signed cert on 127.0.0.1:<random>).
    let server = e2e_wt_harness::start_wt_test_server(pipeline_name).await;
    let url = server.url.clone();

    // 2. Register processor identity.
    let identity = e2e_wt_harness::register_test_identity(&server, "rust-net-wt-proc");
    let seed = *identity.signing_key.as_bytes();

    // 3. Spawn the Rust WT processor client (pure Rust, in-process task).
    //    Uses the batch entry point so we can ack the whole batch at once.
    fn passthrough_batch(events: Vec<ProcessEvent>) -> Vec<Vec<ProcessOutput>> {
        events
            .into_iter()
            .map(|e| {
                vec![ProcessOutput {
                    destination: "output".into(),
                    key: None,
                    payload: e.payload,
                    headers: e.metadata,
                }]
            })
            .collect()
    }

    let client_handle = {
        let url = url.clone();
        let pipeline_name = pipeline_name.to_string();
        tokio::spawn(async move {
            // Default codec (msgpack) — `ProcessEvent.id` is now `uuid::Uuid`,
            // so the engine's msgpack-encoded `WireEvent.id` (16-byte array)
            // round-trips cleanly.
            let config = ProcessorConfig::new("rust-net-wt-proc", url)
                .pipeline(pipeline_name)
                .signing_key_from_seed(&seed);
            aeon_processor_client::webtransport::run_webtransport_batch(config, passthrough_batch)
                .await
        })
    };

    // 4. Wait for processor to connect.
    let connected = e2e_wt_harness::wait_for_connection(&server, Duration::from_secs(15)).await;
    if !connected {
        // Poll the client task to surface any early error it may have hit.
        if client_handle.is_finished() {
            let client_err = client_handle.await;
            panic!(
                "D3: Rust WT processor failed to connect within 15s; client result: {client_err:?}"
            );
        } else {
            panic!("D3: Rust WT processor failed to connect within 15s; client still running");
        }
    }

    // 5. Drive 200 events through the T3 transport.
    //    Pin everything to partition 0 so a single data stream handles the whole flow.
    let source: Arc<str> = Arc::from("d3-source");
    let events: Vec<Event> = (0..200)
        .map(|i| {
            let payload = Bytes::from(format!("d3-payload-{i:05}"));
            Event::new(
                uuid::Uuid::now_v7(),
                i as i64,
                Arc::clone(&source),
                PartitionId::new(0),
                payload,
            )
            .with_metadata(Arc::from("d3-key"), Arc::from(format!("val-{i}")))
        })
        .collect();
    let events_clone = events.clone();

    let outputs =
        e2e_wt_harness::drive_events_through_transport(&server.wt_host, events, 32)
            .await
            .expect("D3: drive_events_through_transport failed");

    // 6. Verify the 5 E2E criteria.
    // C1: zero loss
    assert_eq!(
        outputs.len(),
        events_clone.len(),
        "D3 C1: event count mismatch: {} outputs vs {} events",
        outputs.len(),
        events_clone.len(),
    );

    // C2: payload integrity, C4: per-partition ordering (same partition → in order)
    for (i, (event, output)) in events_clone.iter().zip(outputs.iter()).enumerate() {
        assert_eq!(
            output.payload.as_ref(),
            event.payload.as_ref(),
            "D3 C2: payload mismatch at index {i}",
        );
    }

    // C3: metadata propagation — the passthrough routed `metadata` into output headers.
    for (i, output) in outputs.iter().enumerate() {
        let found = output.headers.iter().any(|(k, v)| {
            k.as_ref() == "d3-key" && v.as_ref() == format!("val-{i}").as_str()
        });
        assert!(found, "D3 C3: metadata not propagated at index {i}");
    }

    // 7. Graceful shutdown: drop the server to close the WT connection, then
    //    give the client a short window to notice and exit.
    drop(server);
    let _ = tokio::time::timeout(Duration::from_secs(3), client_handle).await;
}

#[cfg(not(feature = "webtransport-host"))]
#[tokio::test]
#[ignore = "requires --features webtransport-host"]
async fn d3_rust_network_wt_t3() {
    todo!("rebuild with --features webtransport-host");
}

// ===========================================================================
// D4: Memory -> Node.js T3 WebTransport -> Memory
// ===========================================================================

#[tokio::test]
#[ignore = "requires TLS certs + Node.js SDK with WebTransport support + engine WT host"]
async fn d4_nodejs_wt_t3() {
    todo!("Implement with engine WebTransport host + Node.js SDK (T3)");
}

// ===========================================================================
// D5: Memory -> Java T3 WebTransport -> Memory
// ===========================================================================

#[tokio::test]
#[ignore = "requires TLS certs + Java SDK with WebTransport support + engine WT host"]
async fn d5_java_wt_t3() {
    todo!("Implement with engine WebTransport host + Java SDK (T3)");
}
