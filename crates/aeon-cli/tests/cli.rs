//! Integration tests for the `aeon` CLI binary.
//!
//! Historically aeon-cli had zero tests, which let regressions like ZD-1/2/3 ship
//! (wrong serde casing, missing route, placeholder SHA-512). These tests exercise
//! the CLI end-to-end via `assert_cmd`, including a tiny in-process TCP mock for
//! commands that talk to the Aeon REST API.

use assert_cmd::Command;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

// ── smoke tests (no network, no fs) ─────────────────────────────────────

#[test]
fn version_prints_package_version() {
    Command::cargo_bin("aeon")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_lists_core_subcommands() {
    let assert = Command::cargo_bin("aeon")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for expected in [
        "new",
        "build",
        "validate",
        "processor",
        "pipeline",
        "apply",
        "doctor",
    ] {
        assert!(
            out.contains(expected),
            "expected `{expected}` in --help output"
        );
    }
}

// ── scaffolding ─────────────────────────────────────────────────────────

#[test]
fn new_scaffolds_wasm_rust_project() {
    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("aeon")
        .unwrap()
        .current_dir(tmp.path())
        .args(["new", "myproc", "--runtime", "wasm", "--lang", "rust"])
        .assert()
        .success();

    let root = tmp.path().join("myproc");
    assert!(root.join("Cargo.toml").is_file(), "Cargo.toml missing");
    assert!(root.join("src/lib.rs").is_file(), "src/lib.rs missing");
    assert!(
        root.join(".cargo/config.toml").is_file(),
        ".cargo/config.toml missing"
    );

    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    assert!(cargo.contains("crate-type = [\"cdylib\"]"));
    let lib = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(lib.contains("aeon_processor!"));
}

#[test]
fn new_rejects_path_traversal_in_name() {
    let tmp = tempfile::tempdir().unwrap();
    Command::cargo_bin("aeon")
        .unwrap()
        .current_dir(tmp.path())
        .args(["new", "../evil"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("must not contain"));
}

// ── validate ────────────────────────────────────────────────────────────

#[test]
fn validate_missing_file_fails_cleanly() {
    Command::cargo_bin("aeon")
        .unwrap()
        .args(["validate", "/definitely/does/not/exist.wasm"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("error:"))
        .stderr(predicates::str::contains("file not found"));
}

#[test]
fn validate_unknown_extension_gives_hint() {
    // DX-4: a clear error + actionable hint for wrong artifact type.
    let tmp = tempfile::tempdir().unwrap();
    let bogus = tmp.path().join("processor.exe");
    std::fs::write(&bogus, b"not a real binary").unwrap();

    Command::cargo_bin("aeon")
        .unwrap()
        .args(["validate", bogus.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown file extension"))
        .stderr(predicates::str::contains("hint:"));
}

#[test]
fn processor_list_against_unreachable_server_hints_at_server() {
    // DX-4: connection refused → hint to run `aeon serve` / `aeon doctor`.
    // Bind then drop to get a guaranteed-closed port.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    Command::cargo_bin("aeon")
        .unwrap()
        .args(["processor", "--api", &format!("http://{addr}"), "list"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("error:"))
        .stderr(predicates::str::contains("hint:"))
        .stderr(predicates::str::contains("aeon doctor"));
}

// ── REST-talking commands use a tiny TCP mock ───────────────────────────

/// Minimal single-shot HTTP mock. Binds 127.0.0.1:0, serves one request,
/// returns (url, captured_body). The captured body is the raw request payload.
///
/// We intentionally avoid adding `httpmock`/`wiremock` as deps — the CLI only
/// sends simple ureq requests and all we need is to capture the request body
/// and return a canned JSON response.
struct Mock {
    url: String,
    body_rx: mpsc::Receiver<(String, String)>, // (request_line, body)
    _handle: thread::JoinHandle<()>,
}

impl Mock {
    fn spawn(response_status: &'static str, response_body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let (tx, rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).ok();

            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();

            // Read headers, capture Content-Length
            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }

            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                reader.read_exact(&mut body).unwrap();
            }
            let body_str = String::from_utf8_lossy(&body).into_owned();
            tx.send((request_line.trim().to_string(), body_str)).ok();

            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                status = response_status,
                len = response_body.len(),
                body = response_body,
            );
            sock.write_all(resp.as_bytes()).ok();
            sock.flush().ok();
        });

        Mock {
            url,
            body_rx: rx,
            _handle: handle,
        }
    }

    fn captured(&self) -> (String, String) {
        self.body_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("mock received no request")
    }
}

#[test]
fn processor_register_sends_kebab_case_type_and_available_status() {
    // Write a dummy .wasm artifact so `std::fs::read` succeeds.
    let tmp = tempfile::tempdir().unwrap();
    let artifact = tmp.path().join("foo.wasm");
    std::fs::write(&artifact, b"\0asm\x01\x00\x00\x00").unwrap();

    let mock = Mock::spawn("201 Created", r#"{"status":"registered"}"#);

    Command::cargo_bin("aeon")
        .unwrap()
        .args([
            "processor",
            "--api",
            &mock.url,
            "register",
            "myproc",
            "--version",
            "1.0.0",
            "--description",
            "test",
            "--artifact",
            artifact.to_str().unwrap(),
        ])
        .assert()
        .success();

    let (request_line, body) = mock.captured();
    assert!(
        request_line.starts_with("POST /api/v1/processors"),
        "got: {request_line}"
    );
    let json: serde_json::Value = serde_json::from_str(&body).expect("body must be JSON");

    // ZD-2 regression guard: processor_type must be kebab-case, not PascalCase.
    let ptype = json["version"]["processor_type"].as_str().unwrap();
    assert_eq!(ptype, "wasm", "processor_type must be lowercase kebab-case");
    let status = json["version"]["status"].as_str().unwrap();
    assert_eq!(status, "available", "status must be kebab-case");

    // Basic shape
    assert_eq!(json["name"], "myproc");
    assert_eq!(json["version"]["version"], "1.0.0");
    assert!(json["version"]["sha512"].is_string());
    assert!(json["version"]["size_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn processor_register_marks_native_so_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let artifact = tmp.path().join("foo.so");
    std::fs::write(&artifact, b"not-really-an-elf").unwrap();

    let mock = Mock::spawn("201 Created", r#"{"status":"registered"}"#);

    Command::cargo_bin("aeon")
        .unwrap()
        .args([
            "processor",
            "--api",
            &mock.url,
            "register",
            "foo",
            "--version",
            "0.1.0",
            "--artifact",
            artifact.to_str().unwrap(),
            "--platform",
            "x86_64-linux",
        ])
        .assert()
        .success();

    let (_, body) = mock.captured();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["version"]["processor_type"], "native-so");
    assert_eq!(json["version"]["platform"], "x86_64-linux");
}

#[test]
fn processor_list_parses_empty_response() {
    let mock = Mock::spawn("200 OK", "[]");
    Command::cargo_bin("aeon")
        .unwrap()
        .args(["processor", "--api", &mock.url, "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("No processors registered"));
}

#[test]
fn processor_list_renders_rows() {
    let mock = Mock::spawn(
        "200 OK",
        r#"[{"name":"alpha","version_count":2,"latest_version":"1.2.0"}]"#,
    );
    Command::cargo_bin("aeon")
        .unwrap()
        .args(["processor", "--api", &mock.url, "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("alpha"))
        .stdout(predicates::str::contains("1.2.0"));
}
