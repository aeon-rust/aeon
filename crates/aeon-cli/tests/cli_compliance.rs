//! S4.3 — aeon-cli integration tests for the `compliance:` manifest block.
//!
//! Exercises the CLI end-to-end: parse a YAML manifest that declares a
//! compliance block, run `aeon apply --dry-run` / `aeon apply` against a
//! tiny in-process HTTP mock, assert the regime × enforcement combination
//! survives into the POST payload, the CLI preview line shows the tag,
//! and malformed blocks fail at `validate_pipeline_shape` before the
//! network call.

use assert_cmd::Command;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

// ── Minimal HTTP mock (copied from cli.rs — different module can't share) ─

struct Mock {
    url: String,
    body_rx: mpsc::Receiver<(String, String)>,
    _handle: thread::JoinHandle<()>,
}

impl Mock {
    fn spawn_sequence(responses: Vec<(&'static str, &'static str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let (tx, rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            for (status, body) in responses {
                let (mut sock, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                sock.set_read_timeout(Some(Duration::from_secs(5))).ok();

                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();

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

                let mut body_bytes = vec![0u8; content_length];
                if content_length > 0 {
                    reader.read_exact(&mut body_bytes).unwrap();
                }
                let body_str = String::from_utf8_lossy(&body_bytes).into_owned();
                tx.send((request_line.trim().to_string(), body_str)).ok();

                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                    status = status,
                    len = body.len(),
                    body = body,
                );
                sock.write_all(resp.as_bytes()).ok();
                sock.flush().ok();
            }
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

// ── Fixtures ────────────────────────────────────────────────────────────

/// Minimal compliance pipeline manifest with substitutable regime +
/// enforcement. Sources/sinks use `memory`/`blackhole` so the manifest
/// parses cleanly without requiring Kafka-specific fields.
fn manifest_yaml(regime: &str, enforcement: &str, extras: &str) -> String {
    format!(
        r#"pipelines:
  - name: test-compliance
    partitions: 2
    compliance:
      regime: {regime}
      enforcement: {enforcement}
{extras}
    sources:
      - name: src
        type: memory
        kind: push
        identity:
          mode: random
        event_time:
          mode: aeon_ingest
    processor:
      name: passthrough
      version: "1.0.0"
    sinks:
      - name: sink
        type: blackhole
        eos_tier: t6_fire_and_forget
"#
    )
}

fn write_manifest(body: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("manifest.yaml");
    std::fs::write(&path, body).unwrap();
    tmp
}

// ── Dry-run preview shows compliance tag ────────────────────────────────

#[test]
fn dry_run_shows_gdpr_strict_tag() {
    let yaml = manifest_yaml("gdpr", "strict", "");
    let tmp = write_manifest(&yaml);
    let path = tmp.path().join("manifest.yaml");

    // cmd_apply GETs existing pipelines first, then (on dry-run) does
    // NOT POST. So we only need the list response.
    let mock = Mock::spawn_sequence(vec![("200 OK", "[]")]);

    let assert = Command::cargo_bin("aeon")
        .unwrap()
        .args([
            "apply",
            "--file",
            path.to_str().unwrap(),
            "--api",
            &mock.url,
            "--dry-run",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("test-compliance"),
        "missing pipeline name in: {stdout}"
    );
    assert!(
        stdout.contains("compliance: gdpr/strict"),
        "missing compliance tag in: {stdout}"
    );
}

#[test]
fn dry_run_omits_tag_for_default_block() {
    // Manifest with no compliance block at all — the CLI preview should
    // stay quiet about compliance.
    let yaml = r#"pipelines:
  - name: plain-pipeline
    partitions: 1
    sources:
      - name: src
        type: memory
        kind: push
        identity:
          mode: random
        event_time:
          mode: aeon_ingest
    processor:
      name: passthrough
      version: "1.0.0"
    sinks:
      - name: sink
        type: blackhole
        eos_tier: t6_fire_and_forget
"#;
    let tmp = write_manifest(yaml);
    let path = tmp.path().join("manifest.yaml");

    let mock = Mock::spawn_sequence(vec![("200 OK", "[]")]);

    let assert = Command::cargo_bin("aeon")
        .unwrap()
        .args([
            "apply",
            "--file",
            path.to_str().unwrap(),
            "--api",
            &mock.url,
            "--dry-run",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        !stdout.contains("compliance:"),
        "inert block should not print a tag: {stdout}"
    );
}

// ── POST body carries compliance block for every regime × enforcement ───

fn assert_apply_posts_compliance(regime: &str, enforcement: &str) {
    let yaml = manifest_yaml(regime, enforcement, "");
    let tmp = write_manifest(&yaml);
    let path = tmp.path().join("manifest.yaml");

    let mock = Mock::spawn_sequence(vec![
        ("200 OK", "[]"),                         // existing pipelines
        ("201 Created", r#"{"status":"ok"}"#),    // POST create
    ]);

    Command::cargo_bin("aeon")
        .unwrap()
        .args([
            "apply",
            "--file",
            path.to_str().unwrap(),
            "--api",
            &mock.url,
        ])
        .assert()
        .success();

    // Drain GET (list) response.
    let (list_req, _) = mock.captured();
    assert!(list_req.starts_with("GET "), "expected GET first: {list_req}");

    let (post_req, post_body) = mock.captured();
    assert!(
        post_req.starts_with("POST /api/v1/pipelines"),
        "expected pipeline POST: {post_req}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&post_body).expect("POST body must be JSON");
    let compliance = json
        .get("compliance")
        .unwrap_or_else(|| panic!("missing compliance in: {post_body}"));
    assert_eq!(
        compliance["regime"], regime,
        "regime mismatch in POST body for {regime}/{enforcement}"
    );
    assert_eq!(
        compliance["enforcement"], enforcement,
        "enforcement mismatch in POST body for {regime}/{enforcement}"
    );
}

#[test]
fn posts_carry_pci_warn() {
    assert_apply_posts_compliance("pci", "warn");
}

#[test]
fn posts_carry_hipaa_strict() {
    assert_apply_posts_compliance("hipaa", "strict");
}

#[test]
fn posts_carry_gdpr_strict() {
    assert_apply_posts_compliance("gdpr", "strict");
}

#[test]
fn posts_carry_mixed_warn() {
    assert_apply_posts_compliance("mixed", "warn");
}

// ── Selectors round-trip through YAML → JSON POST body ──────────────────

#[test]
fn selectors_survive_cli_roundtrip() {
    let extras = r#"      selectors:
        - path: "$.user.ssn"
          class: pii
        - path: "$.patient.dob"
          format: message_pack
          class: phi
"#;
    let yaml = manifest_yaml("mixed", "strict", extras);
    let tmp = write_manifest(&yaml);
    let path = tmp.path().join("manifest.yaml");

    let mock = Mock::spawn_sequence(vec![
        ("200 OK", "[]"),
        ("201 Created", r#"{"status":"ok"}"#),
    ]);

    Command::cargo_bin("aeon")
        .unwrap()
        .args([
            "apply",
            "--file",
            path.to_str().unwrap(),
            "--api",
            &mock.url,
        ])
        .assert()
        .success();

    let _ = mock.captured(); // drain GET
    let (_, post_body) = mock.captured();
    let json: serde_json::Value = serde_json::from_str(&post_body).unwrap();
    let selectors = json["compliance"]["selectors"]
        .as_array()
        .expect("selectors array");
    assert_eq!(selectors.len(), 2);
    assert_eq!(selectors[0]["path"], "$.user.ssn");
    assert_eq!(selectors[0]["class"], "pii");
    assert_eq!(selectors[0]["format"], "json", "format defaults to json");
    assert_eq!(selectors[1]["path"], "$.patient.dob");
    assert_eq!(selectors[1]["class"], "phi");
    assert_eq!(selectors[1]["format"], "message_pack");
}

// ── Erasure config overrides survive the round-trip ─────────────────────

#[test]
fn erasure_max_delay_survives_cli_roundtrip() {
    let extras = r#"      erasure:
        max_delay_hours: 6
"#;
    let yaml = manifest_yaml("gdpr", "strict", extras);
    let tmp = write_manifest(&yaml);
    let path = tmp.path().join("manifest.yaml");

    let mock = Mock::spawn_sequence(vec![
        ("200 OK", "[]"),
        ("201 Created", r#"{"status":"ok"}"#),
    ]);

    Command::cargo_bin("aeon")
        .unwrap()
        .args([
            "apply",
            "--file",
            path.to_str().unwrap(),
            "--api",
            &mock.url,
        ])
        .assert()
        .success();

    let _ = mock.captured();
    let (_, post_body) = mock.captured();
    let json: serde_json::Value = serde_json::from_str(&post_body).unwrap();
    assert_eq!(json["compliance"]["erasure"]["max_delay_hours"], 6);
}

// ── validate_pipeline_shape — CLI refuses malformed blocks pre-network ──

#[test]
fn empty_selector_path_fails_before_post() {
    let extras = r#"      selectors:
        - path: ""
          class: pii
"#;
    let yaml = manifest_yaml("gdpr", "strict", extras);
    let tmp = write_manifest(&yaml);
    let path = tmp.path().join("manifest.yaml");

    // Mock must still be available for the initial GET, but we expect
    // the CLI to error before any POST. Only one response needed.
    let mock = Mock::spawn_sequence(vec![("200 OK", "[]")]);

    Command::cargo_bin("aeon")
        .unwrap()
        .args([
            "apply",
            "--file",
            path.to_str().unwrap(),
            "--api",
            &mock.url,
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("selectors[0].path"));
}

#[test]
fn gdpr_strict_with_oversized_sla_cap_fails_before_post() {
    let extras = r#"      erasure:
        max_delay_hours: 800
"#;
    let yaml = manifest_yaml("gdpr", "strict", extras);
    let tmp = write_manifest(&yaml);
    let path = tmp.path().join("manifest.yaml");

    let mock = Mock::spawn_sequence(vec![("200 OK", "[]")]);

    Command::cargo_bin("aeon")
        .unwrap()
        .args([
            "apply",
            "--file",
            path.to_str().unwrap(),
            "--api",
            &mock.url,
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("30-day SLA"));
}

// ── Env-var interpolation works inside the compliance block ─────────────

#[test]
fn env_var_interpolation_drives_regime_and_enforcement() {
    // The S1 interpolator runs on the raw YAML string before parsing, so
    // a compliance regime can come from an env var (useful for flipping
    // between `warn` in staging and `strict` in prod without manifest
    // forks).
    // SAFETY: test-local env mutation, unique var name.
    unsafe {
        std::env::set_var("AEON_TEST_COMPLIANCE_REGIME", "hipaa");
        std::env::set_var("AEON_TEST_COMPLIANCE_ENFORCEMENT", "warn");
    }
    let yaml = r#"pipelines:
  - name: env-driven
    partitions: 1
    compliance:
      regime: ${ENV:AEON_TEST_COMPLIANCE_REGIME}
      enforcement: ${ENV:AEON_TEST_COMPLIANCE_ENFORCEMENT}
    sources:
      - name: src
        type: memory
        kind: push
        identity:
          mode: random
        event_time:
          mode: aeon_ingest
    processor:
      name: passthrough
      version: "1.0.0"
    sinks:
      - name: sink
        type: blackhole
        eos_tier: t6_fire_and_forget
"#;
    let tmp = write_manifest(yaml);
    let path = tmp.path().join("manifest.yaml");

    let mock = Mock::spawn_sequence(vec![
        ("200 OK", "[]"),
        ("201 Created", r#"{"status":"ok"}"#),
    ]);

    Command::cargo_bin("aeon")
        .unwrap()
        .args([
            "apply",
            "--file",
            path.to_str().unwrap(),
            "--api",
            &mock.url,
        ])
        .assert()
        .success();

    let _ = mock.captured();
    let (_, post_body) = mock.captured();
    let json: serde_json::Value = serde_json::from_str(&post_body).unwrap();
    assert_eq!(json["compliance"]["regime"], "hipaa");
    assert_eq!(json["compliance"]["enforcement"], "warn");
}
