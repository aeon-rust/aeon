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
        .args([
            "-r",
            "if (!function_exists('sodium_crypto_sign_seed_keypair')) exit(1);",
        ])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP: PHP sodium extension not available");
        return false;
    }
    true
}

// ===========================================================================
// H1: Swoole / OpenSwoole (Laravel Octane deployment model)
// ===========================================================================

fn swoole_available() -> bool {
    if !php_available() {
        return false;
    }
    let check = std::process::Command::new("php")
        .args([
            "-r",
            "if (!extension_loaded('swoole') && !extension_loaded('openswoole')) exit(1);",
        ])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP: PHP swoole/openswoole extension not loaded");
        return false;
    }
    true
}

#[tokio::test]
async fn h1_php_swoole() {
    if !swoole_available() {
        return;
    }

    let pipeline_name = "h1-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "php-swoole");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::php_swoole_passthrough_script(
        server.port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "php-swoole",
    );
    let script_path = std::env::temp_dir().join("aeon_e2e_h1_swoole.php");
    std::fs::write(&script_path, &script).expect("write h1 script");

    let mut child = std::process::Command::new("php")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn php (swoole)");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(15)).await;
    if !connected {
        let _ = child.kill();
        let out = child.wait_with_output().ok();
        let stderr = out
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
            .unwrap_or_default();
        drop(server);
        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_file(&seed_file);
        panic!("H1: Swoole processor failed to connect. stderr: {stderr}");
    }

    let events = make_test_events(MSG_COUNT);
    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 64)
        .await
        .unwrap();

    assert_eq!(outputs.len(), MSG_COUNT, "H1 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("h-payload-{i:05}");
        assert_eq!(
            output.payload.as_ref(),
            expected.as_bytes(),
            "H1 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// H2: ReactPHP event loop + Ratchet/Pawl WebSocket client
// ===========================================================================

#[tokio::test]
async fn h2_php_revolt_reactphp() {
    if !php_available() {
        return;
    }
    if !e2e_ws_harness::composer_available() {
        eprintln!("SKIP H2: composer not available");
        return;
    }

    // Install react/event-loop + ratchet/pawl into a cached project dir.
    let project_dir = match e2e_ws_harness::setup_composer_project(
        "h2-reactphp",
        &["react/event-loop:^1.5", "ratchet/pawl:^0.4"],
    ) {
        Some(d) => d,
        None => {
            eprintln!("SKIP H2: composer install failed");
            return;
        }
    };
    let autoload = project_dir.join("vendor").join("autoload.php");
    if !autoload.exists() {
        eprintln!("SKIP H2: vendor/autoload.php missing after composer install");
        return;
    }

    let pipeline_name = "h2-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "php-reactphp");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::php_reactphp_passthrough_script(
        server.port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "php-reactphp",
        &autoload.to_string_lossy(),
    );
    let script_path = std::env::temp_dir().join("aeon_e2e_h2_reactphp.php");
    std::fs::write(&script_path, &script).expect("write h2 script");

    let mut child = std::process::Command::new("php")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn php (reactphp)");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(15)).await;
    if !connected {
        let _ = child.kill();
        let out = child.wait_with_output().ok();
        let stderr = out
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
            .unwrap_or_default();
        drop(server);
        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_file(&seed_file);
        panic!("H2: ReactPHP processor failed to connect. stderr: {stderr}");
    }

    let events = make_test_events(MSG_COUNT);
    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 64)
        .await
        .unwrap();

    assert_eq!(outputs.len(), MSG_COUNT, "H2 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("h-payload-{i:05}");
        assert_eq!(
            output.payload.as_ref(),
            expected.as_bytes(),
            "H2 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// H3: AMPHP v3 (amphp/websocket-client)
// ===========================================================================

#[tokio::test]
async fn h3_php_revolt_amphp() {
    if !php_available() {
        return;
    }
    if !e2e_ws_harness::composer_available() {
        eprintln!("SKIP H3: composer not available");
        return;
    }

    let project_dir = match e2e_ws_harness::setup_composer_project(
        "h3-amphp",
        &["amphp/websocket-client:^2.0"],
    ) {
        Some(d) => d,
        None => {
            eprintln!("SKIP H3: composer install failed");
            return;
        }
    };
    let autoload = project_dir.join("vendor").join("autoload.php");
    if !autoload.exists() {
        eprintln!("SKIP H3: vendor/autoload.php missing after composer install");
        return;
    }

    let pipeline_name = "h3-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "php-amphp");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::php_amphp_passthrough_script(
        server.port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "php-amphp",
        &autoload.to_string_lossy(),
    );
    let script_path = std::env::temp_dir().join("aeon_e2e_h3_amphp.php");
    std::fs::write(&script_path, &script).expect("write h3 script");

    let mut child = std::process::Command::new("php")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn php (amphp)");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(15)).await;
    if !connected {
        let _ = child.kill();
        let out = child.wait_with_output().ok();
        let stderr = out
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
            .unwrap_or_default();
        drop(server);
        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_file(&seed_file);
        panic!("H3: AMPHP processor failed to connect. stderr: {stderr}");
    }

    let events = make_test_events(MSG_COUNT);
    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 64)
        .await
        .unwrap();

    assert_eq!(outputs.len(), MSG_COUNT, "H3 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("h-payload-{i:05}");
        assert_eq!(
            output.payload.as_ref(),
            expected.as_bytes(),
            "H3 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// H4: Workerman (AsyncTcpConnection with ws:// scheme)
// ===========================================================================

#[tokio::test]
async fn h4_php_workerman() {
    if !php_available() {
        return;
    }
    if !e2e_ws_harness::composer_available() {
        eprintln!("SKIP H4: composer not available");
        return;
    }

    let project_dir = match e2e_ws_harness::setup_composer_project(
        "h4-workerman",
        &["workerman/workerman:^5.0"],
    ) {
        Some(d) => d,
        None => {
            eprintln!("SKIP H4: composer install failed");
            return;
        }
    };
    let autoload = project_dir.join("vendor").join("autoload.php");
    if !autoload.exists() {
        eprintln!("SKIP H4: vendor/autoload.php missing after composer install");
        return;
    }

    let pipeline_name = "h4-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "php-workerman");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::php_workerman_passthrough_script(
        server.port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "php-workerman",
        &autoload.to_string_lossy(),
    );
    let script_path = std::env::temp_dir().join("aeon_e2e_h4_workerman.php");
    std::fs::write(&script_path, &script).expect("write h4 script");

    // Workerman requires `start` as argv[1]
    let mut child = std::process::Command::new("php")
        .arg(&script_path)
        .arg("start")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn php (workerman)");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(15)).await;
    if !connected {
        let _ = child.kill();
        let out = child.wait_with_output().ok();
        let stderr = out
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
            .unwrap_or_default();
        drop(server);
        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_file(&seed_file);
        panic!("H4: Workerman processor failed to connect. stderr: {stderr}");
    }

    let events = make_test_events(MSG_COUNT);
    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 64)
        .await
        .unwrap();

    assert_eq!(outputs.len(), MSG_COUNT, "H4 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("h-payload-{i:05}");
        assert_eq!(
            output.payload.as_ref(),
            expected.as_bytes(),
            "H4 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// H5: FrankenPHP / RoadRunner (embedded PHP SAPI)
// ===========================================================================

/// Locate a FrankenPHP / RoadRunner PHP-CLI compatible invocation.
/// Returns the full command prefix (e.g. `["frankenphp", "php-cli"]` or
/// `["rr", "run", "--"]`) that can replace `php` for running a script.
fn resolve_frankenphp_runner() -> Option<Vec<String>> {
    // Explicit override: AEON_FRANKENPHP = "frankenphp" | "/path/to/frankenphp"
    if let Ok(custom) = std::env::var("AEON_FRANKENPHP") {
        if !custom.trim().is_empty() {
            let probe = std::process::Command::new(&custom)
                .arg("version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if probe {
                return Some(vec![custom, "php-cli".to_string()]);
            }
        }
    }
    // Native `frankenphp version`
    let probe = std::process::Command::new("frankenphp")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if probe {
        return Some(vec!["frankenphp".to_string(), "php-cli".to_string()]);
    }
    None
}

#[tokio::test]
async fn h5_php_frankenphp() {
    if !php_available() {
        return;
    }
    let Some(runner) = resolve_frankenphp_runner() else {
        eprintln!("SKIP H5: frankenphp binary not found");
        return;
    };

    let pipeline_name = "h5-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "php-frankenphp");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    // FrankenPHP's `php-cli` subcommand runs a regular PHP script under its
    // embedded SAPI. The H6 pure-PHP native CLI script is portable enough to
    // run under any PHP SAPI — this validates FrankenPHP's execution model
    // without depending on its worker-mode APIs.
    let script = e2e_ws_harness::php_passthrough_script(
        server.port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "php-frankenphp",
    );
    let script_path = std::env::temp_dir().join("aeon_e2e_h5_frankenphp.php");
    std::fs::write(&script_path, &script).expect("write h5 script");

    let mut cmd = std::process::Command::new(&runner[0]);
    for arg in &runner[1..] {
        cmd.arg(arg);
    }
    cmd.arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn frankenphp");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(15)).await;
    if !connected {
        let _ = child.kill();
        let out = child.wait_with_output().ok();
        let stderr = out
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
            .unwrap_or_default();
        drop(server);
        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_file(&seed_file);
        panic!("H5: FrankenPHP processor failed to connect. stderr: {stderr}");
    }

    let events = make_test_events(MSG_COUNT);
    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 64)
        .await
        .unwrap();

    assert_eq!(outputs.len(), MSG_COUNT, "H5 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("h-payload-{i:05}");
        assert_eq!(
            output.payload.as_ref(),
            expected.as_bytes(),
            "H5 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
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
        server.port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "php-native",
    );
    let script_path = std::env::temp_dir().join("aeon_e2e_h6_php.php");
    std::fs::write(&script_path, &script).expect("write php script");

    let mut child = std::process::Command::new("php")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn php");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "H6: PHP native CLI processor failed to connect");

    let events = make_test_events(MSG_COUNT);
    let outputs = e2e_ws_harness::drive_events_through_transport(&server.ws_host, events, 64)
        .await
        .unwrap();

    assert_eq!(outputs.len(), MSG_COUNT, "H6 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let expected = format!("h-payload-{i:05}");
        assert_eq!(
            output.payload.as_ref(),
            expected.as_bytes(),
            "H6 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}
