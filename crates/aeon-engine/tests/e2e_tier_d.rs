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

#[tokio::test]
#[ignore = "requires TLS certs + Python SDK with WebTransport support + engine WT host"]
async fn d1_python_wt_t3() {
    todo!("Implement with engine WebTransport host + Python SDK (T3)");
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
            // NOTE: the SDK's `ProcessEvent.id` is a `String`, but `uuid::Uuid`
            // serializes as raw 16 bytes in msgpack (non-human-readable format).
            // That means msgpack on this data stream fails to decode events —
            // matching the existing convention in A10/C8/F6, this test uses
            // `json` (where `Uuid` serializes as a string and round-trips
            // cleanly). Switching the SDK envelope to `uuid::Uuid` is tracked
            // as a follow-up.
            let config = ProcessorConfig::new("rust-net-wt-proc", url)
                .pipeline(pipeline_name)
                .signing_key_from_seed(&seed)
                .codec("json");
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
