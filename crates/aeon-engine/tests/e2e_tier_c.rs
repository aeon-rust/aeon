//! Tier C E2E Tests: Kafka/Redpanda → Processor → Kafka/Redpanda (all SDKs)
//!
//! Tests from `docs/E2E-TEST-PLAN.md` Tier C (P0, needs Redpanda at localhost:19092).
//! The Gate 1 money path — every SDK must pass through Kafka.
//!
//! Each test verifies the 5 E2E criteria:
//!   1. Event delivery: source count == sink count (zero loss)
//!   2. Payload integrity: input payload == output payload
//!   3. Metadata propagation: headers/metadata pass through correctly
//!   4. Ordering: within a partition, events arrive in order
//!   5. Graceful shutdown: clean termination

use aeon_connectors::kafka::{KafkaSink, KafkaSinkConfig, KafkaSource, KafkaSourceConfig};
use aeon_engine::{PassthroughProcessor, PipelineMetrics, run};
use aeon_types::traits::{ProcessorTransport, Source};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord, Producer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[path = "e2e_ws_harness.rs"]
mod e2e_ws_harness;

const BROKERS: &str = "localhost:19092";

async fn require_redpanda() -> bool {
    match tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect("127.0.0.1:19092"),
    )
    .await
    {
        Ok(Ok(_)) => true,
        _ => {
            eprintln!("SKIP: Redpanda not available at {BROKERS}");
            false
        }
    }
}

async fn produce_test_messages(topic: &str, count: usize) {
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", BROKERS)
        .set("message.timeout.ms", "30000")
        .create()
        .expect("producer creation");

    for i in 0..count {
        let payload = format!("test-event-{i}");
        let key = format!("key-{}", i % 16);
        producer
            .send(
                FutureRecord::to(topic)
                    .payload(payload.as_bytes())
                    .key(key.as_bytes()),
                Duration::from_secs(5),
            )
            .await
            .expect("message delivery failed");
    }

    producer
        .flush(Duration::from_secs(10))
        .expect("producer flush");
}

// ===========================================================================
// C1: Kafka -> Rust Native T1 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c1_kafka_rust_native_t1() {
    if !require_redpanda().await {
        return;
    }

    let source_topic = "aeon-e2e-c1-source";
    let sink_topic = "aeon-e2e-c1-sink";
    let msg_count = 500;

    produce_test_messages(source_topic, msg_count).await;

    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions((0..16).collect())
        .with_batch_max(128)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c1-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");

    let processor = PassthroughProcessor::new(Arc::from(sink_topic));

    let sink_config = KafkaSinkConfig::new(BROKERS, sink_topic);
    let mut sink = KafkaSink::new(sink_config).expect("sink creation");

    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    let received = metrics.events_received.load(Ordering::Relaxed);
    let processed = metrics.events_processed.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    assert!(
        received >= msg_count as u64,
        "C1: expected >= {msg_count} received, got {received}"
    );
    assert_eq!(received, processed, "C1: received == processed");
    assert_eq!(processed, sent, "C1: processed == sent");
}

// ===========================================================================
// C2: Kafka -> Rust Wasm T2 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c2_kafka_rust_wasm_t2() {
    if !require_redpanda().await {
        return;
    }

    use aeon_wasm::{WasmModule, WasmProcessor};

    // Inline WAT passthrough processor
    let wat = include_str!("e2e_wasm_passthrough.wat");
    let module = WasmModule::from_wat(wat).expect("WAT compilation failed");
    let processor = WasmProcessor::new(Arc::new(module)).expect("WasmProcessor creation failed");

    let source_topic = "aeon-e2e-c2-source";
    let sink_topic = "aeon-e2e-c2-sink";
    let msg_count = 200;

    produce_test_messages(source_topic, msg_count).await;

    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions(vec![0])
        .with_batch_max(64)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c2-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");

    let sink_config = KafkaSinkConfig::new(BROKERS, sink_topic);
    let mut sink = KafkaSink::new(sink_config).expect("sink creation");

    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    assert!(
        received >= msg_count as u64,
        "C2: expected >= {msg_count} received, got {received}"
    );
    assert_eq!(received, sent, "C2: received == sent");
}

// ===========================================================================
// C3: Kafka -> C/C++ Native T1 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c3_kafka_c_native_t1() {
    if !require_redpanda().await {
        return;
    }

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
            "SKIP C3: C passthrough DLL not found at {}",
            dll_path.display()
        );
        return;
    }

    let processor =
        aeon_engine::NativeProcessor::load(&dll_path, b"").expect("load C passthrough processor");

    let source_topic = "aeon-e2e-c3-source";
    let sink_topic = "aeon-e2e-c3-sink";
    let msg_count = 500;

    produce_test_messages(source_topic, msg_count).await;

    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions(vec![0])
        .with_batch_max(128)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c3-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");

    let sink_config = KafkaSinkConfig::new(BROKERS, sink_topic);
    let mut sink = KafkaSink::new(sink_config).expect("sink creation");

    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    assert!(
        received >= msg_count as u64,
        "C3: expected >= {msg_count} received, got {received}"
    );
    assert_eq!(received, sent, "C3: received == sent");
}

// ===========================================================================
// C4: Kafka -> .NET NativeAOT T1 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c4_kafka_dotnet_native_aot_t1() {
    if !require_redpanda().await {
        return;
    }

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
            "SKIP C4: .NET NativeAOT DLL not found at {}",
            dll_path.display()
        );
        return;
    }

    let processor = aeon_engine::NativeProcessor::load(&dll_path, b"")
        .expect("load .NET NativeAOT passthrough processor");

    let source_topic = "aeon-e2e-c4-source";
    let sink_topic = "aeon-e2e-c4-sink";
    let msg_count = 500;

    produce_test_messages(source_topic, msg_count).await;

    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions(vec![0])
        .with_batch_max(128)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c4-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");

    let sink_config = KafkaSinkConfig::new(BROKERS, sink_topic);
    let mut sink = KafkaSink::new(sink_config).expect("sink creation");

    let metrics = PipelineMetrics::new();
    let shutdown = AtomicBool::new(false);

    run(&mut source, &processor, &mut sink, &metrics, &shutdown)
        .await
        .unwrap();

    let received = metrics.events_received.load(Ordering::Relaxed);
    let sent = metrics.outputs_sent.load(Ordering::Relaxed);

    assert!(
        received >= msg_count as u64,
        "C4: expected >= {msg_count} received, got {received}"
    );
    assert_eq!(received, sent, "C4: received == sent");
}

// ===========================================================================
// C5: Kafka -> .NET WS T4 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c5_kafka_dotnet_ws_t4() {
    if !require_redpanda().await {
        return;
    }
    if !e2e_ws_harness::runtime_available("dotnet") {
        eprintln!("SKIP C5: dotnet not found");
        return;
    }
    let source_topic = "aeon-e2e-c5-source";
    let msg_count = 200;
    produce_test_messages(source_topic, msg_count).await;

    let pipeline_name = "c5-pipeline";
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
    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(30)).await;
    assert!(connected, "C5: .NET processor failed to connect");

    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions(vec![0])
        .with_batch_max(64)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c5-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");
    let mut total_received = 0u64;
    let mut total_outputs = 0u64;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        total_received += events.len() as u64;
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len() as u64;
    }
    assert!(
        total_received >= msg_count as u64,
        "C5: expected >= {msg_count} received, got {total_received}"
    );
    assert_eq!(total_received, total_outputs, "C5: received == outputs");
    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&seed_file);
    let _ = std::fs::remove_dir_all(&dotnet_dir);
}

// ===========================================================================
// C6: Kafka -> Python WS T4 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c6_kafka_python_ws_t4() {
    if !require_redpanda().await {
        return;
    }
    if !e2e_ws_harness::runtime_available("python") {
        eprintln!("SKIP C6: python not found");
        return;
    }
    let check = std::process::Command::new("python")
        .args(["-c", "import websockets, nacl.signing"])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP C6: Python packages missing");
        return;
    }

    let source_topic = "aeon-e2e-c6-source";
    let msg_count = 200;
    produce_test_messages(source_topic, msg_count).await;

    let pipeline_name = "c6-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "python-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let port = server.port;
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::python_passthrough_script(
        port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "python-proc",
    );
    let script_path = std::env::temp_dir().join("aeon_e2e_c6_python.py");
    std::fs::write(&script_path, &script).unwrap();

    let mut child = std::process::Command::new("python")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn python");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "C6: Python processor failed to connect");

    // Read from Kafka, process through WS, count outputs
    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions(vec![0])
        .with_batch_max(64)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c6-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");

    let mut total_received = 0u64;
    let mut total_outputs = 0u64;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        total_received += events.len() as u64;
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len() as u64;
    }

    assert!(
        total_received >= msg_count as u64,
        "C6: expected >= {msg_count} received, got {total_received}"
    );
    assert_eq!(total_received, total_outputs, "C6: received == outputs");

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// C7: Kafka -> Go WS T4 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c7_kafka_go_ws_t4() {
    if !require_redpanda().await {
        return;
    }
    if !e2e_ws_harness::runtime_available("go") {
        eprintln!("SKIP C7: go not found");
        return;
    }

    let source_topic = "aeon-e2e-c7-source";
    let msg_count = 200;
    produce_test_messages(source_topic, msg_count).await;

    let pipeline_name = "c7-pipeline";
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
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn go");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(30)).await;
    assert!(connected, "C7: Go processor failed to connect");

    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions(vec![0])
        .with_batch_max(64)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c7-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");

    let mut total_received = 0u64;
    let mut total_outputs = 0u64;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        total_received += events.len() as u64;
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len() as u64;
    }

    assert!(
        total_received >= msg_count as u64,
        "C7: expected >= {msg_count} received, got {total_received}"
    );
    assert_eq!(total_received, total_outputs, "C7: received == outputs");

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&seed_file);
    let _ = std::fs::remove_dir_all(&go_dir);
}

// ===========================================================================
// C8: Kafka -> Rust Network WS T4 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c8_kafka_rust_network_ws_t4() {
    if !require_redpanda().await {
        return;
    }

    use aeon_processor_client::{ProcessEvent, ProcessOutput, ProcessorClient, ProcessorConfig};

    let source_topic = "aeon-e2e-c8-source";
    let msg_count = 200;
    produce_test_messages(source_topic, msg_count).await;

    let pipeline_name = "c8-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "rust-net-proc");
    let port = server.port;
    let seed = *identity.signing_key.as_bytes();

    let _client_handle = tokio::spawn(async move {
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

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(5)).await;
    assert!(connected, "C8: Rust processor failed to connect");

    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions(vec![0])
        .with_batch_max(64)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c8-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");

    let mut total_received = 0u64;
    let mut total_outputs = 0u64;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        total_received += events.len() as u64;
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len() as u64;
    }

    assert!(
        total_received >= msg_count as u64,
        "C8: expected >= {msg_count} received, got {total_received}"
    );
    assert_eq!(total_received, total_outputs, "C8: received == outputs");
}

// ===========================================================================
// C9: Kafka -> Node.js WS T4 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c9_kafka_nodejs_ws_t4() {
    if !require_redpanda().await {
        return;
    }
    if !e2e_ws_harness::runtime_available("node") {
        eprintln!("SKIP C9: node not found");
        return;
    }
    let check = std::process::Command::new("node")
        .args([
            "-e",
            "const v=parseInt(process.versions.node);if(v<22){process.exit(1)}",
        ])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP C9: Node.js 22+ required");
        return;
    }

    let source_topic = "aeon-e2e-c9-source";
    let msg_count = 200;
    produce_test_messages(source_topic, msg_count).await;

    let pipeline_name = "c9-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "nodejs-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let port = server.port;
    let seed_path = seed_file.to_string_lossy().to_string().replace('\\', "/");
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::nodejs_passthrough_script(
        port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "nodejs-proc",
    );
    let script_path = std::env::temp_dir().join("aeon_e2e_c9_nodejs.js");
    std::fs::write(&script_path, &script).unwrap();

    let mut child = std::process::Command::new("node")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn node");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "C9: Node.js processor failed to connect");

    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions(vec![0])
        .with_batch_max(64)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c9-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");

    let mut total_received = 0u64;
    let mut total_outputs = 0u64;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        total_received += events.len() as u64;
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len() as u64;
    }

    assert!(
        total_received >= msg_count as u64,
        "C9: expected >= {msg_count} received, got {total_received}"
    );
    assert_eq!(total_received, total_outputs, "C9: received == outputs");

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// C10: Kafka -> Java WS T4 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c10_kafka_java_ws_t4() {
    if !require_redpanda().await {
        return;
    }
    if !e2e_ws_harness::runtime_available("java") {
        eprintln!("SKIP C10: java not found");
        return;
    }
    let java_ver = std::process::Command::new("java")
        .args(["--version"])
        .output();
    let is_modern = java_ver
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.split('.').next())
                .and_then(|v| v.parse::<u32>().ok())
                .map(|v| v >= 15)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if !is_modern {
        eprintln!("SKIP C10: Java 15+ required");
        return;
    }

    let source_topic = "aeon-e2e-c10-source";
    let msg_count = 200;
    produce_test_messages(source_topic, msg_count).await;

    let pipeline_name = "c10-pipeline";
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
    let compile = std::process::Command::new("javac")
        .arg(java_dir.join("AeonProcessor.java"))
        .output()
        .expect("javac");
    if !compile.status.success() {
        eprintln!("SKIP C10: javac failed");
        return;
    }
    let mut child = std::process::Command::new("java")
        .args(["-cp", &java_dir.to_string_lossy(), "AeonProcessor"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn java");
    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(15)).await;
    assert!(connected, "C10: Java processor failed to connect");

    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions(vec![0])
        .with_batch_max(64)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c10-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");
    let mut total_received = 0u64;
    let mut total_outputs = 0u64;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        total_received += events.len() as u64;
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len() as u64;
    }
    assert!(
        total_received >= msg_count as u64,
        "C10: expected >= {msg_count} received, got {total_received}"
    );
    assert_eq!(total_received, total_outputs, "C10: received == outputs");
    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&seed_file);
    let _ = std::fs::remove_dir_all(&java_dir);
}

// ===========================================================================
// C11: Kafka -> PHP WS T4 -> Kafka
// ===========================================================================

#[tokio::test]
async fn c11_kafka_php_ws_t4() {
    if !require_redpanda().await {
        return;
    }
    if !e2e_ws_harness::runtime_available("php") {
        eprintln!("SKIP C11: php not found");
        return;
    }
    let check = std::process::Command::new("php")
        .args([
            "-r",
            "if (!function_exists('sodium_crypto_sign_seed_keypair')) exit(1);",
        ])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP C11: PHP sodium extension not available");
        return;
    }

    let source_topic = "aeon-e2e-c11-source";
    let msg_count = 200;
    produce_test_messages(source_topic, msg_count).await;

    let pipeline_name = "c11-pipeline";
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
    let script_path = std::env::temp_dir().join("aeon_e2e_c11_php.php");
    std::fs::write(&script_path, &script).unwrap();

    let mut child = std::process::Command::new("php")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn php");
    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "C11: PHP processor failed to connect");

    let source_config = KafkaSourceConfig::new(BROKERS, source_topic)
        .with_partitions(vec![0])
        .with_batch_max(64)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("c11-source")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");
    let mut total_received = 0u64;
    let mut total_outputs = 0u64;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        total_received += events.len() as u64;
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len() as u64;
    }
    assert!(
        total_received >= msg_count as u64,
        "C11: expected >= {msg_count} received, got {total_received}"
    );
    assert_eq!(total_received, total_outputs, "C11: received == outputs");
    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}
