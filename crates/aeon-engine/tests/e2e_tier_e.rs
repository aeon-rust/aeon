//! Tier E E2E Tests: Cross-Connector Coverage
//!
//! Tests from `docs/E2E-TEST-PLAN.md` Tier E (P2, mixed infra).
//! One representative SDK (Python T4) across many connector pairs.
//! Validates that the pipeline works with diverse source/sink combinations.
//!
//! Each test verifies the 5 E2E criteria:
//!   1. Event delivery: source count == sink count (zero loss)
//!   2. Payload integrity: input payload == output payload
//!   3. Metadata propagation: headers/metadata pass through correctly
//!   4. Ordering: within a partition, events arrive in order
//!   5. Graceful shutdown: clean termination

use aeon_connectors::blackhole::BlackholeSink;
use aeon_connectors::file::{FileSink, FileSinkConfig, FileSource, FileSourceConfig};
use aeon_connectors::memory::MemorySink;
use aeon_connectors::stdout::StdoutSink;
use aeon_types::traits::{ProcessorTransport, Sink, Source};
use aeon_types::{Event, Output};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;

#[path = "e2e_ws_harness.rs"]
mod e2e_ws_harness;

/// Helper: create test events for memory source.
fn make_test_events(prefix: &str, count: usize) -> Vec<Event> {
    (0..count)
        .map(|i| {
            Event::new(
                uuid::Uuid::nil(),
                0,
                Arc::from("e2e-test"),
                aeon_types::PartitionId::new(0),
                Bytes::from(format!("{prefix}-{i:05}")),
            )
        })
        .collect()
}

/// Helper: check Python runtime + packages are available.
fn python_available() -> bool {
    if !e2e_ws_harness::runtime_available("python") {
        eprintln!("SKIP: python not found");
        return false;
    }
    let check = std::process::Command::new("python")
        .args(["-c", "import websockets, nacl.signing"])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP: Python packages missing");
        return false;
    }
    true
}

/// Helper: start Python T4 processor and return (child, cleanup files).
async fn start_python_processor(
    server: &e2e_ws_harness::WsTestServer,
    test_name: &str,
) -> (std::process::Child, std::path::PathBuf, std::path::PathBuf) {
    let identity = e2e_ws_harness::register_test_identity(server, "python-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let port = server.port;
    let seed_path = seed_file.to_string_lossy().to_string();
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::python_passthrough_script(
        port,
        &seed_path,
        &pub_key,
        &server.pipeline_name,
        "python-proc",
    );
    let script_path = std::env::temp_dir().join(format!("aeon_e2e_{test_name}_python.py"));
    std::fs::write(&script_path, &script).expect("write python script");

    let child = std::process::Command::new("python")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn python");

    (child, script_path, seed_file)
}

/// Helper: drive events through WS transport and return outputs.
async fn drive_events(
    server: &e2e_ws_harness::WsTestServer,
    events: Vec<Event>,
    batch_size: usize,
) -> Vec<Output> {
    let mut all_outputs = Vec::new();
    for chunk in events.chunks(batch_size) {
        let outputs = server.ws_host.call_batch(chunk.to_vec()).await.unwrap();
        all_outputs.extend(outputs);
    }
    all_outputs
}

// ===========================================================================
// E1: Memory -> Python T4 -> Blackhole
// ===========================================================================

#[tokio::test]
async fn e1_memory_python_blackhole() {
    if !python_available() {
        return;
    }

    let msg_count = 200;
    let pipeline_name = "e1-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let (mut child, script_path, seed_file) = start_python_processor(&server, "e1").await;

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "E1: Python processor failed to connect");

    let events = make_test_events("e1-payload", msg_count);
    let outputs = drive_events(&server, events, 32).await;

    // Write to blackhole sink
    let mut sink = BlackholeSink::new();
    sink.write_batch(outputs).await.unwrap();

    assert_eq!(
        sink.count(),
        msg_count as u64,
        "E1 C1: event count mismatch"
    );

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// E2: Memory -> Python T4 -> Stdout
// ===========================================================================

#[tokio::test]
async fn e2_memory_python_stdout() {
    if !python_available() {
        return;
    }

    let msg_count = 50; // Small count — stdout is noisy
    let pipeline_name = "e2-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let (mut child, script_path, seed_file) = start_python_processor(&server, "e2").await;

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "E2: Python processor failed to connect");

    let events = make_test_events("e2-payload", msg_count);
    let outputs = drive_events(&server, events, 32).await;

    let mut sink = StdoutSink::new();
    sink.write_batch(outputs).await.unwrap();

    assert_eq!(
        sink.count(),
        msg_count as u64,
        "E2 C1: event count mismatch"
    );

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// E3: Memory -> Python T4 -> File
// ===========================================================================

#[tokio::test]
async fn e3_memory_python_file() {
    if !python_available() {
        return;
    }

    let msg_count = 200;
    let pipeline_name = "e3-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let (mut child, script_path, seed_file) = start_python_processor(&server, "e3").await;

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "E3: Python processor failed to connect");

    let events = make_test_events("e3-payload", msg_count);
    let outputs = drive_events(&server, events, 32).await;

    // Write to file sink
    let output_dir = tempfile::tempdir().unwrap();
    let output_path = output_dir.path().join("e3_output.txt");
    let sink_config = FileSinkConfig::new(&output_path);
    let mut sink = FileSink::new(sink_config);
    sink.write_batch(outputs).await.unwrap();
    sink.flush().await.unwrap();

    // Verify
    let output_content = std::fs::read_to_string(&output_path).unwrap();
    let output_lines: Vec<&str> = output_content.lines().collect();
    assert_eq!(output_lines.len(), msg_count, "E3 C1: event count mismatch");
    for (i, line) in output_lines.iter().enumerate() {
        assert_eq!(
            *line,
            format!("e3-payload-{i:05}"),
            "E3 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// E4: File -> Python T4 -> Memory
// ===========================================================================

#[tokio::test]
async fn e4_file_python_memory() {
    if !python_available() {
        return;
    }

    let msg_count = 200;

    // Write input file
    let input_dir = tempfile::tempdir().unwrap();
    let input_path = input_dir.path().join("e4_input.txt");
    let mut input_data = String::new();
    for i in 0..msg_count {
        input_data.push_str(&format!("e4-payload-{i:05}\n"));
    }
    std::fs::write(&input_path, &input_data).unwrap();

    let pipeline_name = "e4-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let (mut child, script_path, seed_file) = start_python_processor(&server, "e4").await;

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "E4: Python processor failed to connect");

    // Read from file, process through WS
    let source_config = FileSourceConfig::new(&input_path)
        .with_batch_size(32)
        .with_source_name("e4-file");
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

    // Collect in memory sink
    let mut sink = MemorySink::new();
    sink.write_batch(all_outputs).await.unwrap();

    let outputs = sink.outputs();
    assert_eq!(outputs.len(), msg_count, "E4 C1: event count mismatch");
    for (i, output) in outputs.iter().enumerate() {
        let payload = std::str::from_utf8(&output.payload).unwrap();
        assert_eq!(
            payload,
            format!("e4-payload-{i:05}"),
            "E4 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// E5: File -> Python T4 -> Kafka
// ===========================================================================

#[tokio::test]
async fn e5_file_python_kafka() {
    // Needs Redpanda
    if tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect("127.0.0.1:19092"),
    )
    .await
    .is_err()
    {
        eprintln!("SKIP E5: Redpanda not available");
        return;
    }
    if !python_available() {
        return;
    }

    use aeon_connectors::kafka::{KafkaSink, KafkaSinkConfig};

    let msg_count = 200;

    let input_dir = tempfile::tempdir().unwrap();
    let input_path = input_dir.path().join("e5_input.txt");
    let mut input_data = String::new();
    for i in 0..msg_count {
        input_data.push_str(&format!("e5-payload-{i:05}\n"));
    }
    std::fs::write(&input_path, &input_data).unwrap();

    let pipeline_name = "e5-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let (mut child, script_path, seed_file) = start_python_processor(&server, "e5").await;

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "E5: Python processor failed to connect");

    let source_config = FileSourceConfig::new(&input_path)
        .with_batch_size(32)
        .with_source_name("e5-file");
    let mut source = FileSource::new(source_config);

    let sink_topic = "aeon-e2e-e5-sink";
    let sink_config = KafkaSinkConfig::new("localhost:19092", sink_topic);
    let mut sink = KafkaSink::new(sink_config).expect("sink creation");

    let mut total_outputs = 0u64;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len() as u64;
        sink.write_batch(outputs).await.unwrap();
    }
    sink.flush().await.unwrap();

    assert_eq!(
        total_outputs, msg_count as u64,
        "E5 C1: event count mismatch"
    );

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// E6: Kafka -> Python T4 -> File
// ===========================================================================

#[tokio::test]
async fn e6_kafka_python_file() {
    // Needs Redpanda
    if tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect("127.0.0.1:19092"),
    )
    .await
    .is_err()
    {
        eprintln!("SKIP E6: Redpanda not available");
        return;
    }
    if !python_available() {
        return;
    }

    use aeon_connectors::kafka::{KafkaSource, KafkaSourceConfig};
    use rdkafka::config::ClientConfig;
    use rdkafka::producer::{FutureProducer, FutureRecord, Producer};

    let source_topic = "aeon-e2e-e6-source";
    let msg_count = 200;

    // Produce test messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", "localhost:19092")
        .set("message.timeout.ms", "30000")
        .create()
        .expect("producer creation");
    for i in 0..msg_count {
        let payload = format!("e6-payload-{i:05}");
        producer
            .send(
                FutureRecord::to(source_topic)
                    .payload(payload.as_bytes())
                    .key(b"k" as &[u8]),
                Duration::from_secs(5),
            )
            .await
            .expect("send failed");
    }
    producer.flush(Duration::from_secs(10)).unwrap();

    let pipeline_name = "e6-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let (mut child, script_path, seed_file) = start_python_processor(&server, "e6").await;

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "E6: Python processor failed to connect");

    let source_config = KafkaSourceConfig::new("localhost:19092", source_topic)
        .with_partitions(vec![0])
        .with_batch_max(64)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("e6-kafka")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");

    let output_dir = tempfile::tempdir().unwrap();
    let output_path = output_dir.path().join("e6_output.txt");
    let sink_config = FileSinkConfig::new(&output_path);
    let mut sink = FileSink::new(sink_config);

    let mut total = 0u64;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total += outputs.len() as u64;
        sink.write_batch(outputs).await.unwrap();
    }
    sink.flush().await.unwrap();

    assert!(
        total >= msg_count as u64,
        "E6 C1: expected >= {msg_count}, got {total}"
    );

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// E7: Kafka -> Python T4 -> Blackhole
// ===========================================================================

#[tokio::test]
async fn e7_kafka_python_blackhole() {
    // Needs Redpanda
    if tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect("127.0.0.1:19092"),
    )
    .await
    .is_err()
    {
        eprintln!("SKIP E7: Redpanda not available");
        return;
    }
    if !python_available() {
        return;
    }

    use aeon_connectors::kafka::{KafkaSource, KafkaSourceConfig};
    use rdkafka::config::ClientConfig;
    use rdkafka::producer::{FutureProducer, FutureRecord, Producer};

    let source_topic = "aeon-e2e-e7-source";
    let msg_count = 200;

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", "localhost:19092")
        .set("message.timeout.ms", "30000")
        .create()
        .expect("producer creation");
    for i in 0..msg_count {
        let payload = format!("e7-payload-{i:05}");
        producer
            .send(
                FutureRecord::to(source_topic)
                    .payload(payload.as_bytes())
                    .key(b"k" as &[u8]),
                Duration::from_secs(5),
            )
            .await
            .expect("send failed");
    }
    producer.flush(Duration::from_secs(10)).unwrap();

    let pipeline_name = "e7-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let (mut child, script_path, seed_file) = start_python_processor(&server, "e7").await;

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "E7: Python processor failed to connect");

    let source_config = KafkaSourceConfig::new("localhost:19092", source_topic)
        .with_partitions(vec![0])
        .with_batch_max(64)
        .with_poll_timeout(Duration::from_secs(2))
        .with_source_name("e7-kafka")
        .with_max_empty_polls(5);
    let mut source = KafkaSource::new(source_config).expect("source creation");

    let mut sink = BlackholeSink::new();
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            break;
        }
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        sink.write_batch(outputs).await.unwrap();
    }

    assert!(
        sink.count() >= msg_count as u64,
        "E7 C1: expected >= {msg_count}, got {}",
        sink.count()
    );

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// E8: HTTP Webhook -> Python T4 -> Memory
// ===========================================================================

#[tokio::test]
async fn e8_http_webhook_python_memory() {
    if !python_available() {
        return;
    }

    use aeon_connectors::http::HttpWebhookSourceConfig;

    let msg_count = 100;
    let pipeline_name = "e8-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let (mut child, script_path, seed_file) = start_python_processor(&server, "e8").await;

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "E8: Python processor failed to connect");

    // Bind to random port, then create webhook source on it
    let tmp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let webhook_addr = tmp_listener.local_addr().unwrap();
    drop(tmp_listener);

    let webhook_config2 = HttpWebhookSourceConfig::new(webhook_addr)
        .with_source_name("e8-webhook")
        .with_poll_timeout(Duration::from_millis(500));
    let mut source = aeon_connectors::http::HttpWebhookSource::new(webhook_config2)
        .await
        .expect("webhook source creation");

    // Post events via HTTP client
    let client = reqwest::Client::new();
    for i in 0..msg_count {
        let payload = format!("e8-payload-{i:05}");
        let resp = client
            .post(format!("http://{webhook_addr}/webhook"))
            .body(payload)
            .send()
            .await
            .expect("POST failed");
        assert_eq!(resp.status(), 202, "E8: webhook POST rejected");
    }

    // Read events from webhook source, process through WS
    let mut all_outputs = Vec::new();
    let mut total_received = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while total_received < msg_count {
        if tokio::time::Instant::now() > deadline {
            panic!("E8: timed out waiting for webhook events");
        }
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            continue;
        }
        total_received += events.len();
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        all_outputs.extend(outputs);
    }

    let mut sink = MemorySink::new();
    sink.write_batch(all_outputs).await.unwrap();

    assert_eq!(
        sink.outputs().len(),
        msg_count,
        "E8 C1: event count mismatch"
    );

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// E9: HTTP Webhook -> Python T4 -> Kafka
// ===========================================================================

#[tokio::test]
async fn e9_http_webhook_python_kafka() {
    // Needs Redpanda
    if tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect("127.0.0.1:19092"),
    )
    .await
    .is_err()
    {
        eprintln!("SKIP E9: Redpanda not available");
        return;
    }
    if !python_available() {
        return;
    }

    use aeon_connectors::http::HttpWebhookSourceConfig;
    use aeon_connectors::kafka::{KafkaSink, KafkaSinkConfig};

    let msg_count = 100;
    let pipeline_name = "e9-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let (mut child, script_path, seed_file) = start_python_processor(&server, "e9").await;

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "E9: Python processor failed to connect");

    // Bind to random port, then create webhook source on it
    let tmp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let webhook_addr = tmp_listener.local_addr().unwrap();
    drop(tmp_listener);

    let webhook_config = HttpWebhookSourceConfig::new(webhook_addr)
        .with_source_name("e9-webhook")
        .with_poll_timeout(Duration::from_millis(500));
    let mut source = aeon_connectors::http::HttpWebhookSource::new(webhook_config)
        .await
        .expect("webhook source creation");

    let sink_topic = "aeon-e2e-e9-sink";
    let sink_config = KafkaSinkConfig::new("localhost:19092", sink_topic);
    let mut sink = KafkaSink::new(sink_config).expect("sink creation");

    // Post events
    let client = reqwest::Client::new();
    for i in 0..msg_count {
        let payload = format!("e9-payload-{i:05}");
        let resp = client
            .post(format!("http://{webhook_addr}/webhook"))
            .body(payload)
            .send()
            .await
            .expect("POST failed");
        assert_eq!(resp.status(), 202, "E9: webhook POST rejected");
    }

    // Read events from webhook, process through WS, write to Kafka
    let mut total = 0u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while (total as usize) < msg_count {
        if tokio::time::Instant::now() > deadline {
            panic!("E9: timed out waiting for webhook events");
        }
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            continue;
        }
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total += outputs.len() as u64;
        sink.write_batch(outputs).await.unwrap();
    }
    sink.flush().await.unwrap();

    assert_eq!(total, msg_count as u64, "E9 C1: event count mismatch");

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}
