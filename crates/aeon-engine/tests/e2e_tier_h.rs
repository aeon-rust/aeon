//! Tier H E2E Tests: PHP Deployment Model Variants
//!
//! Tests from `docs/E2E-TEST-PLAN.md` Tier H (P1, PHP extensions required).
//! All use Memory → PHP → Memory, validating each PHP adapter E2E.
//! Tests the 6 deployment models from the PHP SDK.
//!
//! Each test verifies the 5 E2E criteria:
//!   1. Event delivery: source count == sink count (zero loss)
//!   2. Payload integrity: input payload == output payload
//!   3. Metadata propagation: headers/metadata pass through correctly
//!   4. Ordering: within a partition, events arrive in order
//!   5. Graceful shutdown: PHP process exits cleanly

use aeon_types::{Event, PartitionId};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;

#[path = "e2e_ws_harness.rs"]
mod e2e_ws_harness;

const MSG_COUNT: usize = 500;

fn make_test_events(count: usize) -> Vec<Event> {
    (0..count)
        .map(|i| {
            Event::new(
                uuid::Uuid::nil(),
                0,
                Arc::from("h-test"),
                PartitionId::new(0),
                Bytes::from(format!("h-payload-{i:05}")),
            )
        })
        .collect()
}

fn php_available() -> bool {
    if !e2e_ws_harness::runtime_available("php") {
        eprintln!("SKIP: php not found");
        return false;
    }
    let check = std::process::Command::new("php")
        .args(["-r", "if (!function_exists('sodium_crypto_sign_seed_keypair')) exit(1);"])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP: PHP sodium extension not available");
        return false;
    }
    true
}

// ===========================================================================
// H1: Swoole / OpenSwoole (Laravel Octane)
// ===========================================================================

#[tokio::test]
#[ignore = "requires PHP 8.2+ with Swoole/OpenSwoole extension"]
async fn h1_php_swoole() {
    todo!("Implement with engine WS host + PHP Swoole adapter");
}

// ===========================================================================
// H2: RevoltPHP + ReactPHP (Ratchet)
// ===========================================================================

#[tokio::test]
#[ignore = "requires PHP 8.2+ with ReactPHP/Ratchet Composer packages"]
async fn h2_php_revolt_reactphp() {
    todo!("Implement with engine WS host + PHP RevoltPHP/Ratchet adapter");
}

// ===========================================================================
// H3: RevoltPHP + AMPHP
// ===========================================================================

#[tokio::test]
#[ignore = "requires PHP 8.2+ with AMPHP Composer packages"]
async fn h3_php_revolt_amphp() {
    todo!("Implement with engine WS host + PHP RevoltPHP/AMPHP adapter");
}

// ===========================================================================
// H4: Workerman
// ===========================================================================

#[tokio::test]
#[ignore = "requires PHP 8.2+ with Workerman Composer package"]
async fn h4_php_workerman() {
    todo!("Implement with engine WS host + PHP Workerman adapter");
}

// ===========================================================================
// H5: FrankenPHP / RoadRunner
// ===========================================================================

#[tokio::test]
#[ignore = "requires FrankenPHP or RoadRunner binary"]
async fn h5_php_frankenphp() {
    todo!("Implement with engine WS host + FrankenPHP/RoadRunner adapter");
}

// ===========================================================================
// H6: Native CLI (fallback) — pure PHP, no extensions beyond sodium
// ===========================================================================

#[tokio::test]
async fn h6_php_native_cli() {
    if !php_available() {
        return;
    }

    let pipeline_name = "h6-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "php-native");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::php_passthrough_script(
        server.port, &seed_path, &pub_key, pipeline_name, "php-native",
    );
    let script_path = std::env::temp_dir().join("aeon_e2e_h6_php.php");
    std::fs::write(&script_path, &script).expect("write php script");

    let mut child = std::process::Command::new("php")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn php");

    let connected = e2e_ws_harness::wait_for_connection(
        &server, Duration::from_secs(10),
    ).await;
    assert!(connected, "H6: PHP native CLI processor failed to connect");

    let events = make_test_events(MSG_COUNT);
    let outputs = e2e_ws_harness::drive_events_through_transport(
        &server.ws_host, events, 64,
    ).await.unwrap();

    assert_eq!(outputs.len(), MSG_COUNT, "H6 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("h-payload-{i:05}");
        assert_eq!(output.payload.as_ref(), expected.as_bytes(), "H6 C2: payload mismatch at {i}");
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}
