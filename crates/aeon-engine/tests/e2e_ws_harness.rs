//! T4 WebSocket Test Harness
//!
//! Provides reusable infrastructure for all T4 E2E tests:
//! - Starts an axum server with WebSocket processor host on a random port
//! - Registers ED25519 processor identities
//! - Drives events through the WebSocket transport
//! - Supports both in-process Rust clients and external SDK processes

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use aeon_engine::identity_store::ProcessorIdentityStore;
use aeon_engine::pipeline_manager::PipelineManager;
use aeon_engine::registry::ProcessorRegistry;
use aeon_engine::rest_api::{AppState, api_router};
use aeon_engine::transport::session::PipelineResolver;
use aeon_engine::transport::websocket_host::{WebSocketHostConfig, WebSocketProcessorHost};
use aeon_types::awpp::PipelineAssignment;
use aeon_types::error::AeonError;
use aeon_types::event::{Event, Output};
use aeon_types::processor_identity::{PipelineScope, ProcessorIdentity};
use aeon_types::traits::ProcessorTransport;

/// Simple pipeline resolver that assigns all 16 partitions to any requested pipeline.
struct TestPipelineResolver {
    pipeline_name: String,
}

impl PipelineResolver for TestPipelineResolver {
    fn resolve(
        &self,
        requested_pipelines: &[String],
        _processor_name: &str,
    ) -> Result<Vec<PipelineAssignment>, AeonError> {
        let mut assignments = Vec::new();
        for name in requested_pipelines {
            if name == &self.pipeline_name || self.pipeline_name.is_empty() {
                assignments.push(PipelineAssignment {
                    name: name.clone(),
                    partitions: (0..16).collect(),
                    batch_size: 128,
                });
            }
        }
        if assignments.is_empty() {
            for name in requested_pipelines {
                assignments.push(PipelineAssignment {
                    name: name.clone(),
                    partitions: (0..16).collect(),
                    batch_size: 128,
                });
            }
        }
        Ok(assignments)
    }
}

/// Running WebSocket test server with all needed state.
pub struct WsTestServer {
    pub port: u16,
    pub ws_host: Arc<WebSocketProcessorHost>,
    pub identity_store: Arc<ProcessorIdentityStore>,
    pub pipeline_name: String,
    _server_handle: tokio::task::JoinHandle<()>,
}

/// ED25519 identity registered for a test processor.
pub struct TestIdentity {
    pub signing_key: ed25519_dalek::SigningKey,
    pub public_key: String,
    pub fingerprint: String,
}

/// Start a WebSocket test server on a random port.
pub async fn start_ws_test_server(pipeline_name: &str) -> WsTestServer {
    let identity_store = Arc::new(ProcessorIdentityStore::new());
    let pipeline_resolver: Arc<dyn PipelineResolver> = Arc::new(TestPipelineResolver {
        pipeline_name: pipeline_name.to_string(),
    });

    let ws_config = WebSocketHostConfig {
        identity_store: Arc::clone(&identity_store),
        pipeline_resolver,
        oauth_required: false,
        heartbeat_interval: Duration::from_secs(30),
        handshake_timeout: Duration::from_secs(10),
        batch_timeout: Duration::from_secs(30),
        processor_name: "test-processor".into(),
        processor_version: "1.0.0".into(),
        pipeline_name: pipeline_name.to_string(),
        pipeline_codec: None,
        max_inflight_batches: aeon_engine::transport::session::DEFAULT_MAX_INFLIGHT_BATCHES,
    };

    let ws_host = Arc::new(WebSocketProcessorHost::new(ws_config));

    let dir = std::env::temp_dir().join(format!(
        "aeon-e2e-ws-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::create_dir_all(&dir);

    let state = Arc::new(AppState {
        registry: Arc::new(ProcessorRegistry::new(&dir).unwrap()),
        pipelines: Arc::new(PipelineManager::new()),
        delivery_ledgers: dashmap::DashMap::new(),
        pipeline_controls: dashmap::DashMap::new(),
        identities: Arc::clone(&identity_store),
        #[cfg(feature = "processor-auth")]
        authenticator: None,
        #[cfg(not(feature = "processor-auth"))]
        api_token: None,
        #[cfg(feature = "websocket-host")]
        ws_host: Some(Arc::clone(&ws_host)),
    });

    let router = api_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to random port");
    let port = listener.local_addr().unwrap().port();

    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    WsTestServer {
        port,
        ws_host,
        identity_store,
        pipeline_name: pipeline_name.to_string(),
        _server_handle: server_handle,
    }
}

/// Register an ED25519 keypair for a test processor.
pub fn register_test_identity(server: &WsTestServer, processor_name: &str) -> TestIdentity {
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use sha2::{Digest, Sha256};

    let mut rng = rand::thread_rng();
    let signing_key = SigningKey::generate(&mut rng);
    let verifying_key = signing_key.verifying_key();

    let pk_b64 = base64::engine::general_purpose::STANDARD.encode(verifying_key.as_bytes());
    let public_key = format!("ed25519:{pk_b64}");

    let hash = Sha256::digest(verifying_key.as_bytes());
    let fingerprint = format!("SHA256:{}", hex::encode(hash));

    let identity = ProcessorIdentity {
        public_key: public_key.clone(),
        fingerprint: fingerprint.clone(),
        processor_name: processor_name.to_string(),
        allowed_pipelines: PipelineScope::AllMatchingPipelines,
        max_instances: 4,
        registered_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
        registered_by: "e2e-test".into(),
        revoked_at: None,
    };

    server
        .identity_store
        .register(identity)
        .expect("register test identity");

    TestIdentity {
        signing_key,
        public_key,
        fingerprint,
    }
}

/// Wait for a T4 processor to connect (polls session count).
pub async fn wait_for_connection(server: &WsTestServer, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if server.ws_host.session_count() > 0 {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Drive events through the T4 WebSocket transport and collect outputs.
pub async fn drive_events_through_transport(
    ws_host: &WebSocketProcessorHost,
    events: Vec<Event>,
    batch_size: usize,
) -> Result<Vec<Output>, AeonError> {
    let mut all_outputs = Vec::new();

    for chunk in events.chunks(batch_size) {
        let batch = chunk.to_vec();
        let outputs = ws_host.call_batch(batch).await?;
        all_outputs.extend(outputs);
    }

    Ok(all_outputs)
}

/// Write the 32-byte ED25519 seed to a temp file (for external SDK processes).
pub fn write_seed_file(identity: &TestIdentity) -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("aeon-e2e-key-{}.bin", &identity.fingerprint[7..23]));
    std::fs::write(&path, identity.signing_key.as_bytes()).expect("write seed file");
    path
}

// ── External SDK Process Helpers ───────────────────────────────────────

/// Check if a runtime command exists (returns true if executable is found).
pub fn runtime_available(command: &str) -> bool {
    // Go uses `go version` not `go --version`
    let args = if command == "go" {
        vec!["version"]
    } else {
        vec!["--version"]
    };
    std::process::Command::new(command)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Spawn an external SDK process with the processor script.
/// Returns the child process handle.
pub fn spawn_sdk_process(
    command: &str,
    args: &[&str],
    env_vars: &[(&str, String)],
) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(command);
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    for (key, val) in env_vars {
        cmd.env(key, val);
    }

    cmd.spawn()
}

/// Run a standard T4 SDK test:
/// 1. Check runtime availability
/// 2. Start WS server + register identity
/// 3. Spawn SDK process
/// 4. Wait for connection
/// 5. Drive events
/// 6. Verify outputs
/// 7. Clean up
pub async fn run_sdk_t4_test(
    test_name: &str,
    pipeline_name: &str,
    processor_name: &str,
    runtime_cmd: &str,
    script_args: Vec<String>,
    event_count: usize,
) -> Result<Vec<Output>, String> {
    if !runtime_available(runtime_cmd) {
        return Err(format!(
            "SKIP {test_name}: {runtime_cmd} not available on this system"
        ));
    }

    let server = start_ws_test_server(pipeline_name).await;
    let identity = register_test_identity(&server, processor_name);
    let seed_file = write_seed_file(&identity);

    // Build args with connection info
    let port = server.port;
    let env_vars = vec![
        (
            "AEON_WS_URL",
            format!("ws://127.0.0.1:{port}/api/v1/processors/connect"),
        ),
        ("AEON_PIPELINE", pipeline_name.to_string()),
        ("AEON_PROCESSOR_NAME", processor_name.to_string()),
        ("AEON_KEY_FILE", seed_file.to_string_lossy().to_string()),
        ("AEON_PUBLIC_KEY", identity.public_key.clone()),
    ];

    let args_refs: Vec<&str> = script_args.iter().map(|s| s.as_str()).collect();
    let mut child = spawn_sdk_process(runtime_cmd, &args_refs, &env_vars)
        .map_err(|e| format!("{test_name}: failed to spawn {runtime_cmd}: {e}"))?;

    // Wait for connection
    let connected = wait_for_connection(&server, Duration::from_secs(10)).await;
    if !connected {
        // Read stderr for diagnostics
        let _ = child.kill();
        let output = child
            .wait_with_output()
            .unwrap_or_else(|_| std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: vec![],
                stderr: b"(could not read output)".to_vec(),
            });
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{test_name}: SDK process failed to connect within 10s.\nstderr: {stderr}"
        ));
    }

    // Generate and drive events
    let source: Arc<str> = Arc::from("e2e-test");
    let events: Vec<Event> = (0..event_count)
        .map(|i| {
            use aeon_types::partition::PartitionId;
            let payload = bytes::Bytes::from(format!("payload-{i:05}"));
            Event::new(
                uuid::Uuid::now_v7(),
                i as i64,
                Arc::clone(&source),
                PartitionId::new((i % 4) as u16),
                payload,
            )
        })
        .collect();

    let outputs = drive_events_through_transport(&server.ws_host, events, 32)
        .await
        .map_err(|e| format!("{test_name}: drive_events failed: {e}"))?;

    // Clean up
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&seed_file);

    Ok(outputs)
}

// ── Reusable Inline SDK Scripts ────────────────────────────────────────

/// Generate inline Python passthrough processor script.
/// Requires: python with `websockets` and `nacl` packages.
pub fn python_passthrough_script(
    port: u16,
    seed_path: &str,
    _pub_key: &str,
    pipeline_name: &str,
    processor_name: &str,
) -> String {
    // Locate the in-repo Python SDK so the inline script can import it.
    let sdk_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sdks")
        .join("python");
    let sdk_path_str = sdk_path.to_string_lossy().replace('\\', "/");
    let seed_path_fixed = seed_path.replace('\\', "/");

    // Uses the Python SDK's run() directly with @processor decorator.
    // The SDK's Signer handles hex-decode nonce signing and
    // "ed25519:<base64>" public key formatting. Uses msgpack codec
    // which correctly round-trips serde_bytes payloads as binary.
    format!(
        r#"
import sys, traceback

sys.path.insert(0, r"{sdk_path}")

try:
    from aeon_transport import processor, Output, run

    @processor
    def passthrough(event):
        return [Output(
            destination="output",
            payload=event.payload,
            headers=list(event.metadata),
        )]

    run(
        "ws://127.0.0.1:{port}/api/v1/processors/connect",
        name="{processor_name}",
        version="1.0.0",
        private_key_path=r"{seed_path}",
        pipelines=["{pipeline_name}"],
        codec="msgpack",
    )
except SystemExit:
    raise
except BaseException as e:
    print("PYTHON_ERROR: " + repr(e), file=sys.stderr)
    traceback.print_exc(file=sys.stderr)
    sys.exit(1)
"#,
        sdk_path = sdk_path_str,
        port = port,
        processor_name = processor_name,
        seed_path = seed_path_fixed,
        pipeline_name = pipeline_name,
    )
}

/// Generate inline Node.js passthrough processor script.
/// Requires: Node.js 18+ with `npm install` run in sdks/nodejs/.
/// Uses the Node.js SDK's run() directly with processor() decorator.
/// The SDK's Signer handles hex-decode nonce signing and
/// "ed25519:<base64>" public key formatting. Uses msgpack codec
/// which correctly round-trips serde_bytes payloads as binary.
pub fn nodejs_passthrough_script(
    port: u16,
    seed_path: &str,
    _pub_key: &str,
    pipeline_name: &str,
    processor_name: &str,
) -> String {
    // Locate the in-repo Node.js SDK so the inline script can require it.
    let sdk_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sdks")
        .join("nodejs");
    let sdk_path_str = sdk_path.to_string_lossy().replace('\\', "/");
    let seed_path_fixed = seed_path.replace('\\', "/");

    format!(
        r#"
const aeon = require('{sdk_path}/aeon.js');

const passthrough = aeon.processor((event) => [{{
    destination: 'output',
    payload: event.payload,
    headers: event.metadata || [],
}}]);

aeon.run('ws://127.0.0.1:{port}/api/v1/processors/connect', {{
    name: '{processor_name}',
    version: '1.0.0',
    privateKeyPath: '{seed_path}',
    pipelines: ['{pipeline_name}'],
    codec: 'msgpack',
    processor: passthrough,
}}).catch((err) => {{
    console.error('NODEJS_ERROR:', err);
    process.exit(1);
}});
"#,
        sdk_path = sdk_path_str,
        port = port,
        processor_name = processor_name,
        seed_path = seed_path_fixed,
        pipeline_name = pipeline_name,
    )
}

/// Generate a Go passthrough processor project in a temp directory.
/// Returns the path to the temp directory (caller must `go run .` from there).
/// Requires: Go 1.23+ (uses the in-repo Go SDK's `aeon.Run()` directly).
pub fn go_passthrough_project(
    port: u16,
    seed_path: &str,
    _pub_key: &str,
    pipeline_name: &str,
    processor_name: &str,
) -> std::path::PathBuf {
    let tmp_dir = std::env::temp_dir().join(format!("aeon_e2e_go_{pipeline_name}"));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).expect("create go temp dir");

    // Find the absolute path to sdks/go
    let sdk_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sdks")
        .join("go");
    let sdk_path_str = sdk_path.to_string_lossy().replace('\\', "/");

    // go.mod — pin Go 1.23 to match the SDK's requirement.
    let go_mod = format!(
        r#"module aeon-e2e-go-test

go 1.23

require github.com/aeon-rust/aeon/sdks/go v0.0.0

replace github.com/aeon-rust/aeon/sdks/go => {sdk_path_str}
"#
    );
    std::fs::write(tmp_dir.join("go.mod"), &go_mod).expect("write go.mod");

    // Fix: seed_path on Windows may have backslashes
    let seed_path_fixed = seed_path.replace('\\', "/");

    // main.go — uses the Go SDK's Run() directly.
    // The SDK's AWPPPublicKey() returns "ed25519:<base64>" and
    // SignChallenge() hex-decodes the nonce before signing, matching
    // the engine's verification. Uses msgpack codec (SDK default)
    // which correctly round-trips serde_bytes payloads as binary.
    let main_go = format!(
        r#"package main

import (
	"log"

	aeon "github.com/aeon-rust/aeon/sdks/go"
)

func main() {{
	err := aeon.Run(aeon.Config{{
		URL:            "ws://127.0.0.1:{port}/api/v1/processors/connect",
		Name:           "{processor_name}",
		Version:        "1.0.0",
		PrivateKeyPath: "{seed_path}",
		Pipelines:      []string{{"{pipeline_name}"}},
		Codec:          "msgpack",
		Processor: func(e aeon.Event) []aeon.Output {{
			return []aeon.Output{{{{
				Destination: "output",
				Payload:     e.Payload,
				Headers:     e.Metadata,
			}}}}
		}},
	}})
	if err != nil {{
		log.Fatalf("Run: %v", err)
	}}
}}
"#,
        port = port,
        processor_name = processor_name,
        seed_path = seed_path_fixed,
        pipeline_name = pipeline_name,
    );

    std::fs::write(tmp_dir.join("main.go"), &main_go).expect("write main.go");

    // Copy go.sum from the SDK so `go mod tidy` doesn't need to fetch
    // checksums over the network (same pattern as go_wt_passthrough_project).
    let sdk_go_sum = sdk_path.join("go.sum");
    if sdk_go_sum.exists() {
        std::fs::copy(&sdk_go_sum, tmp_dir.join("go.sum")).expect("copy go.sum");
    }

    // Run go mod tidy to materialise transitive deps
    let output = std::process::Command::new("go")
        .args(["mod", "tidy"])
        .current_dir(&tmp_dir)
        .env(
            "PATH",
            format!(
                "{};{}",
                "C:\\Program Files\\Go\\bin",
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("go mod tidy");
    if !output.status.success() {
        eprintln!(
            "go mod tidy stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    tmp_dir
}

/// Generate a .NET passthrough processor project in a temp directory.
/// Returns the path to the temp directory (caller must `dotnet run` from there).
/// Requires: .NET 8+ SDK with NuGet access (downloads NSec.Cryptography + MessagePack).
/// Uses the in-repo C# SDK's Runner.RunAsync() directly with ProcessorRegistration.PerEvent().
/// The SDK's Signer handles hex-decode nonce signing and
/// "ed25519:<base64>" public key formatting.
pub fn dotnet_passthrough_project(
    port: u16,
    seed_path: &str,
    _pub_key: &str,
    pipeline_name: &str,
    processor_name: &str,
) -> std::path::PathBuf {
    let tmp_dir = std::env::temp_dir().join(format!("aeon_e2e_dotnet_{pipeline_name}"));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).expect("create dotnet temp dir");

    let seed_path_fixed = seed_path.replace('\\', "/");

    // Locate the in-repo .NET SDK project for ProjectReference
    let sdk_csproj = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sdks")
        .join("dotnet")
        .join("AeonProcessorSdk")
        .join("AeonProcessorSdk.csproj");
    let sdk_csproj_str = sdk_csproj.to_string_lossy().replace('\\', "/");

    // Write csproj with reference to the SDK
    let csproj = format!(
        r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Exe</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
  </PropertyGroup>
  <ItemGroup>
    <ProjectReference Include="{sdk_csproj_str}" />
  </ItemGroup>
</Project>
"#
    );
    std::fs::write(tmp_dir.join("AeonE2ETest.csproj"), &csproj).expect("write csproj");

    let program_cs = format!(
        r#"
using Aeon.ProcessorSdk;

var seed = File.ReadAllBytes(@"{seed_path_fixed}");

var config = new RunnerConfig
{{
    Name = "{processor_name}",
    Version = "1.0.0",
    PrivateKey = seed,
    Pipelines = new List<string> {{ "{pipeline_name}" }},
    CodecName = "json",
    Processor = ProcessorRegistration.PerEvent(e => new List<Output>
    {{
        new Output {{ Destination = "output", Payload = e.Payload, Headers = new() }}
    }}),
}};

await Runner.RunAsync(
    "ws://127.0.0.1:{port}/api/v1/processors/connect",
    config);
"#
    );

    std::fs::write(tmp_dir.join("Program.cs"), &program_cs).expect("write Program.cs");

    // Restore packages
    let output = std::process::Command::new("dotnet")
        .args(["restore"])
        .current_dir(&tmp_dir)
        .output()
        .expect("dotnet restore");
    if !output.status.success() {
        eprintln!(
            "dotnet restore stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    tmp_dir
}

/// Generate a Java passthrough processor project in a temp directory.
/// Returns the path to the temp directory (caller must compile and run).
/// Requires: Java 21+ (uses the in-repo Java SDK's `Runner.run()` directly).
/// The SDK's Signer handles hex-decode nonce signing and
/// "ed25519:<base64>" public key formatting. Uses json codec
/// (Java SDK uses stdlib-only JSON — no msgpack dependency).
pub fn java_passthrough_project(
    port: u16,
    seed_path: &str,
    _pub_key: &str,
    pipeline_name: &str,
    processor_name: &str,
) -> std::path::PathBuf {
    let tmp_dir = std::env::temp_dir().join(format!("aeon_e2e_java_{pipeline_name}"));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).expect("create java temp dir");

    let seed_path_fixed = seed_path.replace('\\', "/");

    // Uses the Java SDK's Runner.run() directly with Processor.perEvent().
    let java_src = format!(
        r#"
import io.aeon.processor.*;
import java.io.FileInputStream;
import java.util.List;

public class AeonProcessor {{
    public static void main(String[] args) throws Exception {{
        byte[] seed = new FileInputStream("{seed_path_fixed}").readAllBytes();

        var config = new Runner.RunnerConfig(
            "{processor_name}",
            "1.0.0",
            seed,
            List.of("{pipeline_name}"),
            "json",
            Processor.perEvent(event -> List.of(
                new Output("output", event.payload(), null, List.of(), null, null, null)
            )),
            null
        );

        Runner.run("ws://127.0.0.1:{port}/api/v1/processors/connect", config);
    }}
}}
"#
    );

    std::fs::write(tmp_dir.join("AeonProcessor.java"), &java_src).expect("write Java source");
    tmp_dir
}

/// Compile a Java project that uses the in-repo Java SDK.
/// `javac_cmd` is the javac binary (e.g. "javac" or an absolute path).
/// `project_dir` is the directory containing AeonProcessor.java.
/// Returns Ok(()) on success, Err(stderr) on failure.
pub fn compile_java_with_sdk(javac_cmd: &str, project_dir: &std::path::Path) -> Result<(), String> {
    let sdk_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sdks")
        .join("java")
        .join("src")
        .join("main")
        .join("java");
    let sdk_src_str = sdk_src.to_string_lossy().replace('\\', "/");
    let java_src = project_dir
        .join("AeonProcessor.java")
        .to_string_lossy()
        .replace('\\', "/");

    let compile = std::process::Command::new(javac_cmd)
        .args([
            "-d",
            &project_dir.to_string_lossy(),
            "-sourcepath",
            &sdk_src_str,
            &java_src,
        ])
        .output()
        .map_err(|e| format!("javac spawn failed: {e}"))?;

    if compile.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&compile.stderr).to_string())
    }
}

/// Generate inline PHP passthrough processor script.
/// Requires: PHP 8.1+ with sodium extension (built-in on PHP 8.4).
/// Uses raw stream_socket_client for WebSocket (no Composer packages needed).
pub fn php_passthrough_script(
    port: u16,
    seed_path: &str,
    _pub_key: &str,
    pipeline_name: &str,
    processor_name: &str,
) -> String {
    let seed_path_fixed = seed_path.replace('\\', "/");
    format!(
        r#"<?php
// Minimal AWPP T4 WebSocket processor in pure PHP (sodium + stream_socket_client)
$seed = file_get_contents('{seed_path_fixed}');
$kp = sodium_crypto_sign_seed_keypair($seed);
$sk = sodium_crypto_sign_secretkey($kp);
$pk = sodium_crypto_sign_publickey($kp);
$pubKeyStr = "ed25519:" . base64_encode($pk);

function ws_connect($host, $port, $path) {{
    $sock = stream_socket_client("tcp://$host:$port", $errno, $errstr, 10);
    if (!$sock) die("connect failed: $errstr ($errno)\n");
    $key = base64_encode(random_bytes(16));
    $req = "GET $path HTTP/1.1\r\nHost: $host:$port\r\n" .
           "Upgrade: websocket\r\nConnection: Upgrade\r\n" .
           "Sec-WebSocket-Key: $key\r\nSec-WebSocket-Version: 13\r\n\r\n";
    fwrite($sock, $req);
    $resp = '';
    while (($line = fgets($sock)) !== false) {{
        $resp .= $line;
        if (trim($line) === '') break;
    }}
    if (strpos($resp, '101') === false) die("WS upgrade failed: $resp\n");
    return $sock;
}}

function ws_read($sock) {{
    $h = fread($sock, 2);
    if ($h === false || strlen($h) < 2) return null;
    $opcode = ord($h[0]) & 0x0F;
    $len = ord($h[1]) & 0x7F;
    if ($len == 126) {{ $ext = fread($sock, 2); $len = unpack('n', $ext)[1]; }}
    elseif ($len == 127) {{ $ext = fread($sock, 8); $len = unpack('J', $ext)[1]; }}
    $data = '';
    while (strlen($data) < $len) {{
        $chunk = fread($sock, $len - strlen($data));
        if ($chunk === false) break;
        $data .= $chunk;
    }}
    return [$opcode, $data];
}}

function ws_send($sock, $opcode, $data) {{
    $len = strlen($data);
    $mask = random_bytes(4);
    $frame = chr(0x80 | $opcode);
    if ($len < 126) $frame .= chr(0x80 | $len);
    elseif ($len < 65536) $frame .= chr(0x80 | 126) . pack('n', $len);
    else $frame .= chr(0x80 | 127) . pack('J', $len);
    $frame .= $mask;
    for ($i = 0; $i < $len; $i++) $frame .= chr(ord($data[$i]) ^ ord($mask[$i % 4]));
    fwrite($sock, $frame);
}}

$ws = ws_connect('127.0.0.1', {port}, '/api/v1/processors/connect');

// Read challenge
[$op, $raw] = ws_read($ws);
$challenge = json_decode($raw, true);
$nonce = hex2bin($challenge['nonce']);
$sig = sodium_crypto_sign_detached($nonce, $sk);

// Send register
$register = json_encode([
    'type' => 'register', 'protocol' => 'awpp/1', 'transport' => 'websocket',
    'name' => '{processor_name}', 'version' => '1.0.0',
    'public_key' => $pubKeyStr, 'challenge_signature' => bin2hex($sig),
    'capabilities' => ['batch'], 'transport_codec' => 'json',
    'requested_pipelines' => ['{pipeline_name}'], 'binding' => 'dedicated',
]);
ws_send($ws, 1, $register);

// Read accepted
[$op, $raw] = ws_read($ws);
$accepted = json_decode($raw, true);
if ($accepted['type'] !== 'accepted') {{ fwrite(STDERR, "Not accepted: $raw\n"); exit(1); }}
$batchSigning = $accepted['batch_signing'] ?? true;

// Message loop
while (true) {{
    $msg = ws_read($ws);
    if ($msg === null) break;
    [$op, $data] = $msg;
    if ($op === 1) {{ // Text control
        $ctrl = json_decode($data, true);
        if (($ctrl['type'] ?? '') === 'drain') break;
        continue;
    }}
    if ($op === 8) break; // Close
    if ($op !== 2) continue; // Not binary

    // Parse data frame
    $nameLen = unpack('V', substr($data, 0, 4))[1];
    $pipeline = substr($data, 4, $nameLen);
    $partition = unpack('v', substr($data, 4 + $nameLen, 2))[1];
    $bd = substr($data, 4 + $nameLen + 2);

    $batchId = unpack('P', substr($bd, 0, 8))[1]; // unsigned 64-bit LE
    $eventCount = unpack('V', substr($bd, 8, 4))[1];

    $off = 12;
    $payloads = [];
    for ($i = 0; $i < $eventCount; $i++) {{
        $eLen = unpack('V', substr($bd, $off, 4))[1]; $off += 4;
        $ev = json_decode(substr($bd, $off, $eLen), true); $off += $eLen;
        $payloads[] = $ev['payload'] ?? [];
    }}

    // Build response
    $body = pack('P', $batchId) . pack('V', count($payloads));
    foreach ($payloads as $p) {{
        $body .= pack('V', 1); // 1 output per event
        $out = json_encode(['destination' => 'output', 'payload' => $p, 'headers' => []], JSON_UNESCAPED_SLASHES);
        $body .= pack('V', strlen($out)) . $out;
    }}
    $crc = pack('V', crc32($body));
    $bwc = $body . $crc;

    if ($batchSigning) {{
        $sigBytes = sodium_crypto_sign_detached($bwc, $sk);
    }} else {{
        $sigBytes = str_repeat("\0", 64);
    }}

    // Build data frame
    $nameB = $pipeline;
    $hdr = pack('V', strlen($nameB)) . $nameB . pack('v', $partition);
    $frame = $hdr . $bwc . $sigBytes;
    ws_send($ws, 2, $frame);
}}
fclose($ws);
"#
    )
}

// ── Composer helpers for PHP async adapter tests ───────────────────────

/// Resolve a Composer invocation usable for `<prefix> install|require ...`.
/// Tries, in order:
/// 1. `AEON_COMPOSER` env var (path to composer.phar, invoked via `php`)
/// 2. `composer` / `composer.bat` on PATH
/// 3. `$HOME/.local/bin/composer` or `%USERPROFILE%\.local\bin\composer` (phar)
pub fn composer_command() -> Option<Vec<String>> {
    // 1. Explicit override via env var
    if let Ok(path) = std::env::var("AEON_COMPOSER") {
        if std::path::Path::new(&path).exists() {
            return Some(vec!["php".to_string(), path]);
        }
    }
    // 2. Native `composer` on PATH
    let native = if cfg!(windows) {
        "composer.bat"
    } else {
        "composer"
    };
    let probe = std::process::Command::new(native)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if probe {
        return Some(vec![native.to_string()]);
    }
    // 3. User-local phar install
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let phar = std::path::Path::new(&home)
        .join(".local")
        .join("bin")
        .join("composer");
    if phar.exists() {
        return Some(vec!["php".to_string(), phar.to_string_lossy().to_string()]);
    }
    None
}

/// Returns true if Composer can be invoked in some form on this machine.
pub fn composer_available() -> bool {
    composer_command().is_some()
}

/// Create (or reuse) a Composer project with the given packages.
/// `project_name` is used as a stable subdirectory under the temp dir so
/// repeated test runs can skip the download step once vendor/ is in place.
/// Each package is `name:constraint`, e.g. `react/event-loop:^1.5`.
/// Returns the project directory (containing `vendor/autoload.php`) or None
/// if Composer is unavailable or the install failed.
pub fn setup_composer_project(project_name: &str, packages: &[&str]) -> Option<std::path::PathBuf> {
    let prefix = composer_command()?;
    let dir = std::env::temp_dir().join(format!("aeon-e2e-{project_name}"));
    let autoload = dir.join("vendor").join("autoload.php");
    if autoload.exists() {
        return Some(dir);
    }
    let _ = std::fs::create_dir_all(&dir);

    // Write a minimal composer.json
    let deps: Vec<String> = packages
        .iter()
        .map(|p| {
            let mut parts = p.splitn(2, ':');
            let name = parts.next().unwrap_or("");
            let constraint = parts.next().unwrap_or("*");
            format!("    \"{name}\": \"{constraint}\"")
        })
        .collect();
    let composer_json = format!(
        "{{\n  \"require\": {{\n{}\n  }},\n  \"config\": {{ \"preferred-install\": \"dist\" }}\n}}\n",
        deps.join(",\n")
    );
    if std::fs::write(dir.join("composer.json"), composer_json).is_err() {
        return None;
    }

    let mut cmd = std::process::Command::new(&prefix[0]);
    for arg in &prefix[1..] {
        cmd.arg(arg);
    }
    cmd.arg("install")
        .arg("--no-interaction")
        .arg("--no-progress")
        .arg("--quiet")
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let output = cmd.output().ok()?;
    if !output.status.success() {
        eprintln!(
            "composer install failed for {project_name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    if !autoload.exists() {
        return None;
    }
    Some(dir)
}

/// Generate a ReactPHP + Ratchet/Pawl AWPP T4 processor script.
/// Requires a Composer project with `react/event-loop` + `ratchet/pawl`.
/// `vendor_autoload` must be the absolute path to `vendor/autoload.php`.
pub fn php_reactphp_passthrough_script(
    port: u16,
    seed_path: &str,
    _pub_key: &str,
    pipeline_name: &str,
    processor_name: &str,
    vendor_autoload: &str,
) -> String {
    let seed_path_fixed = seed_path.replace('\\', "/");
    let autoload_fixed = vendor_autoload.replace('\\', "/");
    format!(
        r#"<?php
// H2: ReactPHP event loop + Ratchet/Pawl WebSocket client — AWPP T4 processor
require '{autoload_fixed}';

use Ratchet\Client\Connector;
use React\EventLoop\Loop;
use Ratchet\RFC6455\Messaging\Frame;
use Ratchet\RFC6455\Messaging\MessageInterface;

$seed = file_get_contents('{seed_path_fixed}');
$kp = sodium_crypto_sign_seed_keypair($seed);
$sk = sodium_crypto_sign_secretkey($kp);
$pk = sodium_crypto_sign_publickey($kp);
$pubKeyStr = "ed25519:" . base64_encode($pk);

$loop = Loop::get();
$connector = new Connector($loop);
$batchSigning = true;

$connector('ws://127.0.0.1:{port}/api/v1/processors/connect', [], [])->then(
    function ($conn) use ($sk, $pubKeyStr, &$batchSigning, $loop) {{
        $conn->on('message', function (MessageInterface $msg) use ($conn, $sk, $pubKeyStr, &$batchSigning, $loop) {{
            $data = (string)$msg;
            if ($msg->isBinary()) {{
                // Parse AWPP data frame
                $nameLen = unpack('V', substr($data, 0, 4))[1];
                $pipeline = substr($data, 4, $nameLen);
                $partition = unpack('v', substr($data, 4 + $nameLen, 2))[1];
                $bd = substr($data, 4 + $nameLen + 2);
                $batchId = unpack('P', substr($bd, 0, 8))[1];
                $eventCount = unpack('V', substr($bd, 8, 4))[1];
                $off = 12;
                $payloads = [];
                for ($i = 0; $i < $eventCount; $i++) {{
                    $eLen = unpack('V', substr($bd, $off, 4))[1]; $off += 4;
                    $ev = json_decode(substr($bd, $off, $eLen), true); $off += $eLen;
                    $payloads[] = $ev['payload'] ?? [];
                }}
                // Build passthrough response
                $body = pack('P', $batchId) . pack('V', count($payloads));
                foreach ($payloads as $p) {{
                    $body .= pack('V', 1);
                    $out = json_encode(['destination' => 'output', 'payload' => $p, 'headers' => []], JSON_UNESCAPED_SLASHES);
                    $body .= pack('V', strlen($out)) . $out;
                }}
                $crc = pack('V', crc32($body));
                $bwc = $body . $crc;
                $sigBytes = $batchSigning ? sodium_crypto_sign_detached($bwc, $sk) : str_repeat("\0", 64);
                $nameB = $pipeline;
                $hdr = pack('V', strlen($nameB)) . $nameB . pack('v', $partition);
                $frame = $hdr . $bwc . $sigBytes;
                $conn->send(new Frame($frame, true, Frame::OP_BINARY));
            }} else {{
                // Text control frame
                $ctrl = json_decode($data, true);
                $type = $ctrl['type'] ?? '';
                if ($type === 'challenge') {{
                    $nonce = hex2bin($ctrl['nonce']);
                    $sig = sodium_crypto_sign_detached($nonce, $sk);
                    $register = json_encode([
                        'type' => 'register', 'protocol' => 'awpp/1', 'transport' => 'websocket',
                        'name' => '{processor_name}', 'version' => '1.0.0',
                        'public_key' => $pubKeyStr, 'challenge_signature' => bin2hex($sig),
                        'capabilities' => ['batch'], 'transport_codec' => 'json',
                        'requested_pipelines' => ['{pipeline_name}'], 'binding' => 'dedicated',
                    ]);
                    $conn->send($register);
                }} elseif ($type === 'accepted') {{
                    $batchSigning = $ctrl['batch_signing'] ?? true;
                }} elseif ($type === 'drain') {{
                    $conn->close();
                    $loop->stop();
                }}
            }}
        }});
        $conn->on('close', function ($code = null, $reason = null) use ($loop) {{
            $loop->stop();
        }});
    }},
    function ($e) use ($loop) {{
        fwrite(STDERR, "pawl connect failed: " . $e->getMessage() . "\n");
        $loop->stop();
        exit(1);
    }}
);

$loop->run();
"#
    )
}

/// Generate an AMPHP v3 (`amphp/websocket-client:^2`) AWPP T4 processor script.
/// Requires a Composer project with `amphp/websocket-client` installed.
/// `vendor_autoload` must be the absolute path to `vendor/autoload.php`.
pub fn php_amphp_passthrough_script(
    port: u16,
    seed_path: &str,
    _pub_key: &str,
    pipeline_name: &str,
    processor_name: &str,
    vendor_autoload: &str,
) -> String {
    let seed_path_fixed = seed_path.replace('\\', "/");
    let autoload_fixed = vendor_autoload.replace('\\', "/");
    format!(
        r#"<?php
// H3: AMPHP v3 + amphp/websocket-client — AWPP T4 processor
require '{autoload_fixed}';

use Amp\Websocket\Client\WebsocketHandshake;
use function Amp\Websocket\Client\connect;

$seed = file_get_contents('{seed_path_fixed}');
$kp = sodium_crypto_sign_seed_keypair($seed);
$sk = sodium_crypto_sign_secretkey($kp);
$pk = sodium_crypto_sign_publickey($kp);
$pubKeyStr = "ed25519:" . base64_encode($pk);

$handshake = new WebsocketHandshake('ws://127.0.0.1:{port}/api/v1/processors/connect');
$conn = connect($handshake);
$batchSigning = true;

try {{
    while ($message = $conn->receive()) {{
        $data = $message->buffer();
        if ($message->isBinary()) {{
            // AWPP data frame
            $nameLen = unpack('V', substr($data, 0, 4))[1];
            $pipeline = substr($data, 4, $nameLen);
            $partition = unpack('v', substr($data, 4 + $nameLen, 2))[1];
            $bd = substr($data, 4 + $nameLen + 2);
            $batchId = unpack('P', substr($bd, 0, 8))[1];
            $eventCount = unpack('V', substr($bd, 8, 4))[1];
            $off = 12;
            $payloads = [];
            for ($i = 0; $i < $eventCount; $i++) {{
                $eLen = unpack('V', substr($bd, $off, 4))[1]; $off += 4;
                $ev = json_decode(substr($bd, $off, $eLen), true); $off += $eLen;
                $payloads[] = $ev['payload'] ?? [];
            }}
            $body = pack('P', $batchId) . pack('V', count($payloads));
            foreach ($payloads as $p) {{
                $body .= pack('V', 1);
                $out = json_encode(['destination' => 'output', 'payload' => $p, 'headers' => []], JSON_UNESCAPED_SLASHES);
                $body .= pack('V', strlen($out)) . $out;
            }}
            $crc = pack('V', crc32($body));
            $bwc = $body . $crc;
            $sigBytes = $batchSigning ? sodium_crypto_sign_detached($bwc, $sk) : str_repeat("\0", 64);
            $nameB = $pipeline;
            $hdr = pack('V', strlen($nameB)) . $nameB . pack('v', $partition);
            $frame = $hdr . $bwc . $sigBytes;
            $conn->sendBinary($frame);
        }} else {{
            $ctrl = json_decode($data, true);
            $type = $ctrl['type'] ?? '';
            if ($type === 'challenge') {{
                $nonce = hex2bin($ctrl['nonce']);
                $sig = sodium_crypto_sign_detached($nonce, $sk);
                $register = json_encode([
                    'type' => 'register', 'protocol' => 'awpp/1', 'transport' => 'websocket',
                    'name' => '{processor_name}', 'version' => '1.0.0',
                    'public_key' => $pubKeyStr, 'challenge_signature' => bin2hex($sig),
                    'capabilities' => ['batch'], 'transport_codec' => 'json',
                    'requested_pipelines' => ['{pipeline_name}'], 'binding' => 'dedicated',
                ]);
                $conn->sendText($register);
            }} elseif ($type === 'accepted') {{
                $batchSigning = $ctrl['batch_signing'] ?? true;
            }} elseif ($type === 'drain') {{
                $conn->close();
                break;
            }}
        }}
    }}
}} catch (\Throwable $e) {{
    fwrite(STDERR, "amphp error: " . $e->getMessage() . "\n");
    exit(1);
}}
"#
    )
}

/// Generate a Workerman (`workerman/workerman:^5.0`) AWPP T4 processor script.
/// Uses `AsyncTcpConnection` with the native `ws://` protocol. Requires PHP
/// 8.1+ and a Composer project with workerman/workerman installed. Must be
/// invoked as `php <script> start` on all platforms (Workerman argv contract).
pub fn php_workerman_passthrough_script(
    port: u16,
    seed_path: &str,
    _pub_key: &str,
    pipeline_name: &str,
    processor_name: &str,
    vendor_autoload: &str,
) -> String {
    let seed_path_fixed = seed_path.replace('\\', "/");
    let autoload_fixed = vendor_autoload.replace('\\', "/");
    format!(
        r#"<?php
// H4: Workerman AsyncTcpConnection (ws://) — AWPP T4 processor
require '{autoload_fixed}';

use Workerman\Worker;
use Workerman\Connection\AsyncTcpConnection;
use Workerman\Protocols\Websocket;

$GLOBALS['h4_seed'] = file_get_contents('{seed_path_fixed}');
$GLOBALS['h4_kp'] = sodium_crypto_sign_seed_keypair($GLOBALS['h4_seed']);
$GLOBALS['h4_sk'] = sodium_crypto_sign_secretkey($GLOBALS['h4_kp']);
$GLOBALS['h4_pk'] = sodium_crypto_sign_publickey($GLOBALS['h4_kp']);
$GLOBALS['h4_pub'] = "ed25519:" . base64_encode($GLOBALS['h4_pk']);
$GLOBALS['h4_batch_signing'] = true;

$worker = new Worker();
$worker->count = 1;
$worker->onWorkerStart = function () {{
    $conn = new AsyncTcpConnection('ws://127.0.0.1:{port}/api/v1/processors/connect');
    // Default to text frames; we flip websocketType just before sending binary.
    $conn->websocketType = Websocket::BINARY_TYPE_BLOB;
    $conn->onMessage = function ($conn, $data) {{
        $sk = $GLOBALS['h4_sk'];
        $pubKeyStr = $GLOBALS['h4_pub'];
        // Workerman hands us the decoded frame body regardless of opcode.
        // Control frames are JSON starting with '{{'; data frames are binary AWPP.
        if (strlen($data) > 0 && $data[0] === '{{') {{
            $ctrl = json_decode($data, true);
            $type = $ctrl['type'] ?? '';
            if ($type === 'challenge') {{
                $nonce = hex2bin($ctrl['nonce']);
                $sig = sodium_crypto_sign_detached($nonce, $sk);
                $register = json_encode([
                    'type' => 'register', 'protocol' => 'awpp/1', 'transport' => 'websocket',
                    'name' => '{processor_name}', 'version' => '1.0.0',
                    'public_key' => $pubKeyStr, 'challenge_signature' => bin2hex($sig),
                    'capabilities' => ['batch'], 'transport_codec' => 'json',
                    'requested_pipelines' => ['{pipeline_name}'], 'binding' => 'dedicated',
                ]);
                $conn->websocketType = Websocket::BINARY_TYPE_BLOB;
                $conn->send($register);
            }} elseif ($type === 'accepted') {{
                $GLOBALS['h4_batch_signing'] = $ctrl['batch_signing'] ?? true;
            }} elseif ($type === 'drain') {{
                $conn->close();
                Worker::stopAll();
            }}
            return;
        }}
        // AWPP data frame
        $batchSigning = $GLOBALS['h4_batch_signing'];
        $nameLen = unpack('V', substr($data, 0, 4))[1];
        $pipeline = substr($data, 4, $nameLen);
        $partition = unpack('v', substr($data, 4 + $nameLen, 2))[1];
        $bd = substr($data, 4 + $nameLen + 2);
        $batchId = unpack('P', substr($bd, 0, 8))[1];
        $eventCount = unpack('V', substr($bd, 8, 4))[1];
        $off = 12;
        $payloads = [];
        for ($i = 0; $i < $eventCount; $i++) {{
            $eLen = unpack('V', substr($bd, $off, 4))[1]; $off += 4;
            $ev = json_decode(substr($bd, $off, $eLen), true); $off += $eLen;
            $payloads[] = $ev['payload'] ?? [];
        }}
        $body = pack('P', $batchId) . pack('V', count($payloads));
        foreach ($payloads as $p) {{
            $body .= pack('V', 1);
            $out = json_encode(['destination' => 'output', 'payload' => $p, 'headers' => []], JSON_UNESCAPED_SLASHES);
            $body .= pack('V', strlen($out)) . $out;
        }}
        $crc = pack('V', crc32($body));
        $bwc = $body . $crc;
        $sigBytes = $batchSigning ? sodium_crypto_sign_detached($bwc, $sk) : str_repeat("\0", 64);
        $nameB = $pipeline;
        $hdr = pack('V', strlen($nameB)) . $nameB . pack('v', $partition);
        $frame = $hdr . $bwc . $sigBytes;
        $conn->websocketType = Websocket::BINARY_TYPE_ARRAYBUFFER;
        $conn->send($frame);
    }};
    $conn->onClose = function ($conn) {{
        Worker::stopAll();
    }};
    $conn->onError = function ($conn, $code, $msg) {{
        fwrite(STDERR, "workerman error [$code]: $msg\n");
        Worker::stopAll();
    }};
    $conn->connect();
}};

Worker::runAll();
"#
    )
}

/// Generate a Swoole / OpenSwoole AWPP T4 processor script.
/// Uses `Swoole\Coroutine\Http\Client::upgrade()` to establish the WS
/// connection, then `push()` / `recv()` in a coroutine loop.
/// Requires PHP 8.1+ with either the `swoole` or `openswoole` extension loaded.
pub fn php_swoole_passthrough_script(
    port: u16,
    seed_path: &str,
    _pub_key: &str,
    pipeline_name: &str,
    processor_name: &str,
) -> String {
    let seed_path_fixed = seed_path.replace('\\', "/");
    format!(
        r#"<?php
// H1: Swoole / OpenSwoole coroutine WebSocket client — AWPP T4 processor
if (!extension_loaded('swoole') && !extension_loaded('openswoole')) {{
    fwrite(STDERR, "swoole/openswoole extension not loaded\n");
    exit(2);
}}

$seed = file_get_contents('{seed_path_fixed}');
$kp = sodium_crypto_sign_seed_keypair($seed);
$sk = sodium_crypto_sign_secretkey($kp);
$pk = sodium_crypto_sign_publickey($kp);
$pubKeyStr = "ed25519:" . base64_encode($pk);

\Swoole\Coroutine\run(function () use ($sk, $pubKeyStr) {{
    $cli = new \Swoole\Coroutine\Http\Client('127.0.0.1', {port});
    $cli->set(['timeout' => 30, 'websocket_mask' => true]);
    if (!$cli->upgrade('/api/v1/processors/connect')) {{
        fwrite(STDERR, "swoole upgrade failed: " . $cli->errMsg . "\n");
        exit(1);
    }}
    $batchSigning = true;
    while (true) {{
        $frame = $cli->recv(30);
        if ($frame === false || $frame === '' || $frame === null) break;
        if (!is_object($frame)) break;
        $data = $frame->data;
        $opcode = $frame->opcode ?? 1;
        // OPCODE_CLOSE = 8
        if ($opcode === 8) break;
        if ($opcode === 1) {{
            // Text control frame
            $ctrl = json_decode($data, true);
            $type = $ctrl['type'] ?? '';
            if ($type === 'challenge') {{
                $nonce = hex2bin($ctrl['nonce']);
                $sig = sodium_crypto_sign_detached($nonce, $sk);
                $register = json_encode([
                    'type' => 'register', 'protocol' => 'awpp/1', 'transport' => 'websocket',
                    'name' => '{processor_name}', 'version' => '1.0.0',
                    'public_key' => $pubKeyStr, 'challenge_signature' => bin2hex($sig),
                    'capabilities' => ['batch'], 'transport_codec' => 'json',
                    'requested_pipelines' => ['{pipeline_name}'], 'binding' => 'dedicated',
                ]);
                $cli->push($register, 1); // WEBSOCKET_OPCODE_TEXT
            }} elseif ($type === 'accepted') {{
                $batchSigning = $ctrl['batch_signing'] ?? true;
            }} elseif ($type === 'drain') {{
                break;
            }}
            continue;
        }}
        if ($opcode !== 2) continue; // Not binary
        // AWPP data frame
        $nameLen = unpack('V', substr($data, 0, 4))[1];
        $pipeline = substr($data, 4, $nameLen);
        $partition = unpack('v', substr($data, 4 + $nameLen, 2))[1];
        $bd = substr($data, 4 + $nameLen + 2);
        $batchId = unpack('P', substr($bd, 0, 8))[1];
        $eventCount = unpack('V', substr($bd, 8, 4))[1];
        $off = 12;
        $payloads = [];
        for ($i = 0; $i < $eventCount; $i++) {{
            $eLen = unpack('V', substr($bd, $off, 4))[1]; $off += 4;
            $ev = json_decode(substr($bd, $off, $eLen), true); $off += $eLen;
            $payloads[] = $ev['payload'] ?? [];
        }}
        $body = pack('P', $batchId) . pack('V', count($payloads));
        foreach ($payloads as $p) {{
            $body .= pack('V', 1);
            $out = json_encode(['destination' => 'output', 'payload' => $p, 'headers' => []], JSON_UNESCAPED_SLASHES);
            $body .= pack('V', strlen($out)) . $out;
        }}
        $crc = pack('V', crc32($body));
        $bwc = $body . $crc;
        $sigBytes = $batchSigning ? sodium_crypto_sign_detached($bwc, $sk) : str_repeat("\0", 64);
        $nameB = $pipeline;
        $hdr = pack('V', strlen($nameB)) . $nameB . pack('v', $partition);
        $out = $hdr . $bwc . $sigBytes;
        $cli->push($out, 2); // WEBSOCKET_OPCODE_BINARY
    }}
    $cli->close();
}});
"#
    )
}
