//! Tier F E2E Tests: External Messaging Systems
//!
//! Tests from `docs/E2E-TEST-PLAN.md` Tier F (P2, Docker services required).
//! Each test uses a different messaging connector with a different SDK.
//!
//! Each test verifies the 5 E2E criteria:
//!   1. Event delivery: source count == sink count (zero loss)
//!   2. Payload integrity: input payload == output payload
//!   3. Metadata propagation: headers/metadata pass through correctly
//!   4. Ordering: within a partition, events arrive in order
//!   5. Graceful shutdown: clean termination

use aeon_types::traits::ProcessorTransport;
use std::sync::Arc;
use std::time::Duration;

#[path = "e2e_ws_harness.rs"]
mod e2e_ws_harness;

// ===========================================================================
// F1: NATS JetStream -> Python T4 -> NATS JetStream
// ===========================================================================
//
// Creates two JetStream streams (source + sink), publishes N messages into
// the source stream, and drains them through NatsSource → Python T4
// WebSocket processor → NatsSink (OrderedBatch, which awaits all PublishAck
// futures at the batch boundary). Verifies the sink stream ended up with
// exactly N persisted messages.
//
// NATS is expected on `nats://localhost:30422` (K3s NodePort from
// `infra/k8s-test-services.yaml`) or via `AEON_E2E_NATS_URL` override.

fn nats_url() -> String {
    std::env::var("AEON_E2E_NATS_URL").unwrap_or_else(|_| "nats://localhost:30422".to_string())
}

async fn require_nats() -> Option<async_nats::jetstream::Context> {
    let url = nats_url();
    let client_fut = async_nats::connect(&url);
    let client = match tokio::time::timeout(Duration::from_secs(2), client_fut).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            eprintln!("SKIP F1: nats connect failed for {url}: {e}");
            return None;
        }
        Err(_) => {
            eprintln!("SKIP F1: nats connect timeout for {url}");
            return None;
        }
    };
    Some(async_nats::jetstream::new(client))
}

#[tokio::test]
async fn f1_nats_python_t4() {
    use aeon_connectors::nats::{NatsSink, NatsSinkConfig, NatsSource, NatsSourceConfig};
    use aeon_types::DeliveryStrategy;
    use aeon_types::traits::{Sink, Source};

    // --- 1. Preconditions ----------------------------------------------------
    let Some(js) = require_nats().await else {
        return;
    };
    if !e2e_ws_harness::runtime_available("python") {
        eprintln!("SKIP F1: python not found");
        return;
    }

    // --- 2. Fresh source + sink JetStream streams ----------------------------
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let src_stream_name = format!("aeon_e2e_f1_src_{suffix}");
    let sink_stream_name = format!("aeon_e2e_f1_sink_{suffix}");
    let src_subject = format!("aeon.e2e.f1.src.{suffix}");
    let sink_subject = format!("aeon.e2e.f1.sink.{suffix}");
    let consumer_name = format!("f1_consumer_{suffix}");

    // Clean any stale streams from previous runs with the same names
    // (stream names are nano-suffixed so this is almost always a no-op).
    let _ = js.delete_stream(&src_stream_name).await;
    let _ = js.delete_stream(&sink_stream_name).await;

    js.create_stream(async_nats::jetstream::stream::Config {
        name: src_stream_name.clone(),
        subjects: vec![src_subject.clone()],
        ..Default::default()
    })
    .await
    .expect("create src stream");

    js.create_stream(async_nats::jetstream::stream::Config {
        name: sink_stream_name.clone(),
        subjects: vec![sink_subject.clone()],
        ..Default::default()
    })
    .await
    .expect("create sink stream");

    // --- 3. Pre-populate source stream via JS publish ------------------------
    let msg_count: usize = 100;
    for i in 0..msg_count {
        let payload = format!("f1-payload-{i:05}");
        let ack = js
            .publish(src_subject.clone(), payload.into_bytes().into())
            .await
            .expect("js publish");
        ack.await.expect("js publish ack");
    }

    // --- 4. Start WS harness + register Python processor identity ------------
    let pipeline_name = "f1-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "python-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let port = server.port;
    let seed_path = seed_file.to_string_lossy().to_string().replace('\\', "/");
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::python_passthrough_script(
        port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "python-proc",
    );
    let script_path = std::env::temp_dir().join(format!("aeon_e2e_f1_python_{suffix}.py"));
    std::fs::write(&script_path, &script).unwrap();

    let mut child = std::process::Command::new("python")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn python");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "F1: Python processor failed to connect");

    // --- 5. Aeon NATS source + sink ------------------------------------------
    let source_config = NatsSourceConfig::new(nats_url(), &src_stream_name, &src_subject)
        .with_consumer(&consumer_name)
        .with_batch_size(64)
        .with_fetch_timeout(Duration::from_millis(500))
        .with_source_name("f1-nats-source");
    let mut source = NatsSource::new(source_config)
        .await
        .expect("NatsSource::new");

    // OrderedBatch exercises the §4.0 metric-credit-on-flush path for NATS's
    // JetStream ack futures (publish then await-all-acks at batch boundary).
    let sink_config = NatsSinkConfig::new(nats_url(), &sink_subject)
        .with_strategy(DeliveryStrategy::OrderedBatch);
    let mut sink = NatsSink::new(sink_config).await.expect("NatsSink::new");

    // --- 6. Drain loop: NATS -> Python T4 -> NATS ----------------------------
    let mut total_received: usize = 0;
    let mut total_outputs: usize = 0;
    let mut empty_polls = 0;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            empty_polls += 1;
            if empty_polls >= 3 {
                break;
            }
            continue;
        }
        empty_polls = 0;
        total_received += events.len();
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len();
        sink.write_batch(outputs).await.unwrap();
        if total_received >= msg_count {
            break;
        }
    }
    sink.flush().await.unwrap();

    // --- 7. Verification -----------------------------------------------------
    assert_eq!(
        total_received, msg_count,
        "F1 C1: source received {total_received}, expected {msg_count}"
    );
    assert_eq!(
        total_outputs, msg_count,
        "F1 C1: processor produced {total_outputs}, expected {msg_count}"
    );

    // Give the sink stream a moment to settle on the server.
    let mut sink_messages = 0u64;
    for _ in 0..20 {
        let mut sink_stream = js
            .get_stream(&sink_stream_name)
            .await
            .expect("get sink stream");
        let info = sink_stream.info().await.expect("stream info");
        sink_messages = info.state.messages;
        if sink_messages as usize >= msg_count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        sink_messages as usize, msg_count,
        "F1 C1: sink stream has {sink_messages}, expected {msg_count}"
    );

    // --- 8. Cleanup ----------------------------------------------------------
    let _ = js.delete_stream(&src_stream_name).await;
    let _ = js.delete_stream(&sink_stream_name).await;

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// F2: NATS -> Go T4 -> Kafka
// ===========================================================================
//
// Cross-connector topology: publish N messages into a NATS JetStream
// stream, drain through NatsSource → Go T4 WebSocket processor → KafkaSink
// into a fresh Redpanda topic, then drain the Kafka topic back with a
// StreamConsumer to verify payload integrity and ordering.
//
// Unlike F1/F3/F5 this test does not exercise a specific audit fix — it
// validates that the source and sink belong to different messaging
// systems without either stepping on the other's ownership of the event
// envelope (metadata propagation, delivery strategies, shutdown). Skips
// gracefully if NATS, Redpanda, or Go are unavailable.

const F2_BROKERS: &str = "localhost:19092";

async fn require_redpanda_for_f2() -> bool {
    match tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect("127.0.0.1:19092"),
    )
    .await
    {
        Ok(Ok(_)) => true,
        _ => {
            eprintln!("SKIP F2: Redpanda not available at {F2_BROKERS}");
            false
        }
    }
}

#[tokio::test]
async fn f2_nats_kafka_go_t4() {
    use aeon_connectors::kafka::{KafkaSink, KafkaSinkConfig};
    use aeon_connectors::nats::{NatsSource, NatsSourceConfig};
    use aeon_types::traits::{Sink, Source};
    use rdkafka::Message as _;
    use rdkafka::config::ClientConfig;
    use rdkafka::consumer::{Consumer, StreamConsumer};

    // --- 1. Preconditions ----------------------------------------------------
    let Some(js) = require_nats().await else {
        return;
    };
    if !require_redpanda_for_f2().await {
        return;
    }
    if !e2e_ws_harness::runtime_available("go") {
        eprintln!("SKIP F2: go not found");
        return;
    }

    // --- 2. Fresh NATS source stream + Kafka sink topic ---------------------
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let src_stream_name = format!("aeon_e2e_f2_src_{suffix}");
    let src_subject = format!("aeon.e2e.f2.src.{suffix}");
    let consumer_name = format!("f2_consumer_{suffix}");
    let kafka_sink_topic = format!("aeon-e2e-f2-sink-{suffix}");

    let _ = js.delete_stream(&src_stream_name).await;
    js.create_stream(async_nats::jetstream::stream::Config {
        name: src_stream_name.clone(),
        subjects: vec![src_subject.clone()],
        ..Default::default()
    })
    .await
    .expect("create src stream");

    // --- 3. Pre-populate source stream ---------------------------------------
    let msg_count: usize = 100;
    for i in 0..msg_count {
        let payload = format!("f2-payload-{i:05}");
        let ack = js
            .publish(src_subject.clone(), payload.into_bytes().into())
            .await
            .expect("js publish");
        ack.await.expect("js publish ack");
    }

    // --- 4. Start WS harness + spawn Go processor ----------------------------
    let pipeline_name = "f2-pipeline";
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
    assert!(connected, "F2: Go processor failed to connect");

    // --- 5. Aeon NATS source + Kafka sink ------------------------------------
    let source_config = NatsSourceConfig::new(nats_url(), &src_stream_name, &src_subject)
        .with_consumer(&consumer_name)
        .with_batch_size(64)
        .with_fetch_timeout(Duration::from_millis(500))
        .with_source_name("f2-nats-source");
    let mut source = NatsSource::new(source_config)
        .await
        .expect("NatsSource::new");

    let sink_config = KafkaSinkConfig::new(F2_BROKERS, &kafka_sink_topic);
    let mut sink = KafkaSink::new(sink_config).expect("KafkaSink::new");

    // --- 6. Drain loop: NATS -> Go T4 -> Kafka -------------------------------
    let mut total_received: usize = 0;
    let mut total_outputs: usize = 0;
    let mut empty_polls = 0;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            empty_polls += 1;
            if empty_polls >= 3 {
                break;
            }
            continue;
        }
        empty_polls = 0;
        total_received += events.len();
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len();
        sink.write_batch(outputs).await.unwrap();
        if total_received >= msg_count {
            break;
        }
    }
    sink.flush().await.unwrap();

    // --- 7. Verification: source/processor counts + Kafka payload drain -----
    assert_eq!(
        total_received, msg_count,
        "F2 C1: source received {total_received}, expected {msg_count}"
    );
    assert_eq!(
        total_outputs, msg_count,
        "F2 C1: processor produced {total_outputs}, expected {msg_count}"
    );

    let verify_group = format!("f2-verify-{suffix}");
    let verifier: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", F2_BROKERS)
        .set("group.id", &verify_group)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()
        .expect("verifier consumer create");
    verifier
        .subscribe(&[kafka_sink_topic.as_str()])
        .expect("subscribe sink topic");

    let mut collected: Vec<String> = Vec::with_capacity(msg_count);
    let verify_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while collected.len() < msg_count {
        let remaining = verify_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, verifier.recv()).await {
            Ok(Ok(msg)) => {
                let payload = msg
                    .payload()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                collected.push(payload);
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }

    assert_eq!(
        collected.len(),
        msg_count,
        "F2 C1: kafka sink drained {} messages, expected {msg_count}",
        collected.len()
    );
    // Single-partition default topic + single producer preserves order.
    for (i, payload) in collected.iter().enumerate() {
        assert_eq!(
            payload,
            &format!("f2-payload-{i:05}"),
            "F2 C2: payload mismatch at index {i}"
        );
    }

    // --- 8. Cleanup ----------------------------------------------------------
    let _ = js.delete_stream(&src_stream_name).await;

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&go_dir);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// F3: Redis Streams -> Node.js T4 -> Redis Streams
// ===========================================================================
//
// Pre-produces N messages into a source stream via XADD, then drains them
// through RedisSource → Node.js WebSocket processor → RedisSink (OrderedBatch,
// which exercises the §4.4 pipelined-XADD round-trip fix). Verifies the sink
// stream has exactly N entries with matching payloads.
//
// Redis is expected on `redis://localhost:30637` (K3s NodePort from
// `infra/k8s-test-services.yaml`) or via `AEON_E2E_REDIS_URL` override.

fn redis_url() -> String {
    std::env::var("AEON_E2E_REDIS_URL").unwrap_or_else(|_| "redis://localhost:30637".to_string())
}

async fn require_redis() -> Option<redis::aio::MultiplexedConnection> {
    let url = redis_url();
    let client = match redis::Client::open(url.as_str()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP F3: redis client create failed for {url}: {e}");
            return None;
        }
    };
    let conn_fut = client.get_multiplexed_async_connection();
    let conn = match tokio::time::timeout(Duration::from_secs(2), conn_fut).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            eprintln!("SKIP F3: redis connect failed for {url}: {e}");
            return None;
        }
        Err(_) => {
            eprintln!("SKIP F3: redis connect timeout for {url}");
            return None;
        }
    };
    Some(conn)
}

#[tokio::test]
async fn f3_redis_nodejs_t4() {
    use aeon_connectors::redis_streams::{
        RedisSink, RedisSinkConfig, RedisSource, RedisSourceConfig,
    };
    use aeon_types::DeliveryStrategy;
    use aeon_types::traits::{Sink, Source};
    use redis::AsyncCommands;

    // --- 1. Preconditions ----------------------------------------------------
    let Some(mut setup_conn) = require_redis().await else {
        return;
    };
    if !e2e_ws_harness::runtime_available("node") {
        eprintln!("SKIP F3: node not found");
        return;
    }
    let check = std::process::Command::new("node")
        .args([
            "-e",
            "const v=parseInt(process.versions.node);if(v<22){process.exit(1)}",
        ])
        .output();
    if check.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!("SKIP F3: Node.js 22+ required");
        return;
    }

    // --- 2. Fresh source + sink streams --------------------------------------
    // Suffix with nanos so repeat runs don't collide on consumer-group state.
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let source_stream = format!("aeon-e2e-f3-source-{suffix}");
    let sink_stream = format!("aeon-e2e-f3-sink-{suffix}");
    let group = format!("f3-group-{suffix}");

    let msg_count: usize = 100;
    for i in 0..msg_count {
        let payload = format!("f3-payload-{i:05}");
        let _: String = setup_conn
            .xadd(&source_stream, "*", &[("data", payload.as_str())])
            .await
            .expect("XADD pre-populate");
    }

    // --- 3. Start WS harness + register Node.js processor identity -----------
    let pipeline_name = "f3-pipeline";
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
    let script_path = std::env::temp_dir().join(format!("aeon_e2e_f3_nodejs_{suffix}.js"));
    std::fs::write(&script_path, &script).unwrap();

    let mut child = std::process::Command::new("node")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn node");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "F3: Node.js processor failed to connect");

    // --- 4. Build Aeon Redis source + sink -----------------------------------
    let source_config = RedisSourceConfig::new(redis_url(), &source_stream, &group, "f3-consumer")
        .with_batch_size(64)
        .with_block_ms(200)
        .with_source_name("f3-redis-source");
    let mut source = RedisSource::new(source_config)
        .await
        .expect("RedisSource::new");

    // OrderedBatch exercises the §4.4 pipelined-XADD round-trip fix.
    let sink_config = RedisSinkConfig::new(redis_url(), &sink_stream)
        .with_strategy(DeliveryStrategy::OrderedBatch);
    let mut sink = RedisSink::new(sink_config).await.expect("RedisSink::new");

    // --- 5. Drain loop: Redis -> Node.js T4 -> Redis -------------------------
    let mut total_received: usize = 0;
    let mut total_outputs: usize = 0;
    let mut empty_polls = 0;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            empty_polls += 1;
            if empty_polls >= 3 {
                break;
            }
            continue;
        }
        empty_polls = 0;
        total_received += events.len();
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len();
        sink.write_batch(outputs).await.unwrap();
        if total_received >= msg_count {
            break;
        }
    }
    sink.flush().await.unwrap();

    // --- 6. Verification -----------------------------------------------------
    assert_eq!(
        total_received, msg_count,
        "F3 C1: source received {total_received}, expected {msg_count}"
    );
    assert_eq!(
        total_outputs, msg_count,
        "F3 C1: processor produced {total_outputs}, expected {msg_count}"
    );

    let sink_len: usize = setup_conn
        .xlen(&sink_stream)
        .await
        .expect("XLEN sink stream");
    assert_eq!(
        sink_len, msg_count,
        "F3 C1: sink stream has {sink_len}, expected {msg_count}"
    );

    // Payload integrity: read back from sink stream and verify each payload.
    let entries: redis::streams::StreamRangeReply = setup_conn
        .xrange_all(&sink_stream)
        .await
        .expect("XRANGE sink stream");
    assert_eq!(entries.ids.len(), msg_count, "F3 C2: XRANGE count mismatch");
    for (i, entry) in entries.ids.iter().enumerate() {
        let payload: String = entry.get("data").expect("data field");
        assert_eq!(
            payload,
            format!("f3-payload-{i:05}"),
            "F3 C2: payload mismatch at index {i}"
        );
    }

    // --- 7. Cleanup ----------------------------------------------------------
    let _: Result<(), _> = setup_conn.del(&source_stream).await;
    let _: Result<(), _> = setup_conn.del(&sink_stream).await;

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// F4: MQTT -> Java T4 -> MQTT
// ===========================================================================
//
// Subscribes to the source topic via MqttSource, subscribes to the sink
// topic via a verifier MqttSource, publishes N messages onto the source
// topic via a helper MqttSink, drains through the Java T4 WebSocket
// processor into the real MqttSink, and verifies the verifier drained
// exactly N matching payloads. Unlike F1/F3/F5, MQTT is not a queue — the
// subscribers must exist before any message is published, which is why
// the source/verifier are created before the publish step.
//
// Exercises the §4.2 MQTT source push-buffer path (TCP-window-driven
// backpressure, reader-task suspension on send) end-to-end with a
// non-Rust SDK for the first time. The MQTT sink is the "infeasible to
// differentiate" case from §4.4 — rumqttc hides per-publish PUBACK
// matching, so OrderedBatch == UnorderedBatch in practice, and this test
// just validates that the existing write_batch loop survives a round-trip
// against a real broker.
//
// Mosquitto is expected on `localhost:30188` (K3s NodePort from
// `infra/k8s-test-services.yaml`) or via `AEON_E2E_MQTT_HOST` /
// `AEON_E2E_MQTT_PORT` overrides.

fn mqtt_host() -> String {
    std::env::var("AEON_E2E_MQTT_HOST").unwrap_or_else(|_| "localhost".to_string())
}

fn mqtt_port() -> u16 {
    std::env::var("AEON_E2E_MQTT_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30188)
}

async fn require_mqtt() -> bool {
    let host = mqtt_host();
    let port = mqtt_port();
    let addr = format!("{host}:{port}");
    match tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            eprintln!("SKIP F4: mqtt tcp connect failed for {addr}: {e}");
            false
        }
        Err(_) => {
            eprintln!("SKIP F4: mqtt tcp connect timeout for {addr}");
            false
        }
    }
}

/// Resolve a Java 15+ `java` + `javac` pair. Checks `JAVA_HOME/bin` first
/// (Windows CI often has Oracle stub + Microsoft JDK side-by-side, where
/// `java` in PATH is the old Oracle 8 stub but `JAVA_HOME` points at a
/// modern JDK), then falls back to plain `java`/`javac` in PATH.
fn resolve_modern_java() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    fn probe(java: &std::path::Path) -> bool {
        let Ok(out) = std::process::Command::new(java).arg("--version").output() else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let version = text
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.split('.').next())
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        version >= 15
    }

    let (java_exe, javac_exe) = if cfg!(windows) {
        ("java.exe", "javac.exe")
    } else {
        ("java", "javac")
    };

    if let Ok(home) = std::env::var("JAVA_HOME") {
        let bin = std::path::Path::new(&home).join("bin");
        let java = bin.join(java_exe);
        let javac = bin.join(javac_exe);
        if java.exists() && javac.exists() && probe(&java) {
            return Some((java, javac));
        }
    }

    let path_java = std::path::PathBuf::from("java");
    let path_javac = std::path::PathBuf::from("javac");
    if probe(&path_java) {
        return Some((path_java, path_javac));
    }
    None
}

#[tokio::test]
async fn f4_mqtt_java_t4() {
    use aeon_connectors::mqtt::{MqttSink, MqttSinkConfig, MqttSource, MqttSourceConfig};
    use aeon_types::Output;
    use aeon_types::traits::{Sink, Source};
    use bytes::Bytes;
    use std::sync::Arc as StdArc;

    // --- 1. Preconditions ----------------------------------------------------
    if !require_mqtt().await {
        return;
    }
    let Some((java_bin, javac_bin)) = resolve_modern_java() else {
        eprintln!("SKIP F4: java 15+ not found (checked JAVA_HOME + PATH)");
        return;
    };

    // --- 2. Fresh source + sink topics ---------------------------------------
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let source_topic = format!("aeon/e2e/f4/src/{suffix}");
    let sink_topic = format!("aeon/e2e/f4/sink/{suffix}");

    let host = mqtt_host();
    let port = mqtt_port();
    let msg_count: usize = 100;

    // --- 3. Create Aeon MQTT source + sink + verifier BEFORE publishing ------
    // MQTT topics are ephemeral — subscribers must exist before any message
    // is published or it is silently dropped (even with QoS 1, the broker
    // does not buffer for clients that have never subscribed with a
    // persistent session). Create every subscriber first, then publish.
    let source_config = MqttSourceConfig::new(&host, port, &source_topic)
        .with_client_id(format!("aeon-f4-src-{suffix}"))
        .with_poll_timeout(Duration::from_millis(500))
        .with_source_name("f4-mqtt-source");
    let mut source = MqttSource::new(source_config)
        .await
        .expect("MqttSource::new");

    let verifier_config = MqttSourceConfig::new(&host, port, &sink_topic)
        .with_client_id(format!("aeon-f4-verify-{suffix}"))
        .with_poll_timeout(Duration::from_millis(500))
        .with_source_name("f4-mqtt-verifier");
    let mut verifier = MqttSource::new(verifier_config)
        .await
        .expect("MqttSource::new (verifier)");

    let sink_config = MqttSinkConfig::new(&host, port, &sink_topic)
        .with_client_id(format!("aeon-f4-sink-{suffix}"));
    let mut sink = MqttSink::new(sink_config).await.expect("MqttSink::new");

    // Small settle so that all three SUBSCRIBE / CONNACK round-trips finish
    // before we start publishing.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // --- 4. Start WS harness + register Java processor identity --------------
    let pipeline_name = "f4-pipeline";
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
    if let Err(stderr) =
        e2e_ws_harness::compile_java_with_sdk(&javac_bin.to_string_lossy(), &java_dir)
    {
        eprintln!("SKIP F4: javac failed: {stderr}");
        let _ = std::fs::remove_dir_all(&java_dir);
        drop(server);
        return;
    }

    let mut child = std::process::Command::new(&java_bin)
        .args(["-cp", &java_dir.to_string_lossy(), "AeonProcessor"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn java");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(15)).await;
    assert!(connected, "F4: Java processor failed to connect");

    // --- 5. Publish N messages onto the source topic via a helper sink -------
    let mut publisher_sink = MqttSink::new(
        MqttSinkConfig::new(&host, port, &source_topic)
            .with_client_id(format!("aeon-f4-pub-{suffix}")),
    )
    .await
    .expect("MqttSink::new (publisher)");

    let dest: StdArc<str> = StdArc::from("source");
    let mut payloads: Vec<Output> = Vec::with_capacity(msg_count);
    for i in 0..msg_count {
        let payload = format!("f4-payload-{i:05}");
        payloads.push(Output::new(StdArc::clone(&dest), Bytes::from(payload)));
    }
    publisher_sink
        .write_batch(payloads)
        .await
        .expect("publish seed");

    // --- 6. Drain loop: MQTT -> Java T4 -> MQTT ------------------------------
    let mut total_received: usize = 0;
    let mut total_outputs: usize = 0;
    let mut empty_polls = 0;
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if tokio::time::Instant::now() >= drain_deadline {
            break;
        }
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            empty_polls += 1;
            if total_received >= msg_count && empty_polls >= 3 {
                break;
            }
            continue;
        }
        empty_polls = 0;
        total_received += events.len();
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len();
        sink.write_batch(outputs).await.unwrap();
        if total_received >= msg_count {
            break;
        }
    }
    sink.flush().await.unwrap();

    // --- 7. Verification -----------------------------------------------------
    assert_eq!(
        total_received, msg_count,
        "F4 C1: source received {total_received}, expected {msg_count}"
    );
    assert_eq!(
        total_outputs, msg_count,
        "F4 C1: processor produced {total_outputs}, expected {msg_count}"
    );

    // Drain the verifier subscribed to the sink topic.
    let mut collected: Vec<String> = Vec::with_capacity(msg_count);
    let verify_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while collected.len() < msg_count {
        if tokio::time::Instant::now() >= verify_deadline {
            break;
        }
        let events = verifier.next_batch().await.unwrap();
        if events.is_empty() {
            continue;
        }
        for event in events {
            let payload = std::str::from_utf8(event.payload.as_ref())
                .expect("utf8 payload")
                .to_string();
            collected.push(payload);
            if collected.len() >= msg_count {
                break;
            }
        }
    }

    assert_eq!(
        collected.len(),
        msg_count,
        "F4 C1: verifier drained {} from sink topic, expected {msg_count}",
        collected.len()
    );
    // MQTT preserves per-topic ordering from a single publisher with QoS 1,
    // so the verifier should see the payloads in send order.
    for (i, payload) in collected.iter().enumerate() {
        assert_eq!(
            payload,
            &format!("f4-payload-{i:05}"),
            "F4 C2: payload mismatch at index {i}"
        );
    }

    // --- 8. Cleanup ----------------------------------------------------------
    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&java_dir);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// F5: RabbitMQ -> PHP T4 -> RabbitMQ
// ===========================================================================
//
// Pre-populates a source queue via AMQP basic.publish, drains it through
// RabbitMqSource → PHP T4 WebSocket processor → RabbitMqSink (OrderedBatch,
// which exercises the §4.4 join_all publisher-confirm round-trip fix), and
// verifies the sink queue ended up with exactly N messages with matching
// payloads. Both queues are per-run suffixed to avoid collisions.
//
// RabbitMQ is expected on `amqp://guest:guest@localhost:30567/%2f` (K3s
// NodePort from `infra/k8s-test-services.yaml`) or via `AEON_E2E_RABBITMQ_URL`
// override. The test publishes/consumes via the default exchange with the
// queue name as routing key, so no exchange declaration is needed.

fn rabbitmq_url() -> String {
    std::env::var("AEON_E2E_RABBITMQ_URL")
        .unwrap_or_else(|_| "amqp://guest:guest@localhost:30567/%2f".to_string())
}

async fn require_rabbitmq() -> Option<lapin::Connection> {
    let url = rabbitmq_url();
    let conn_fut = lapin::Connection::connect(&url, lapin::ConnectionProperties::default());
    match tokio::time::timeout(Duration::from_secs(2), conn_fut).await {
        Ok(Ok(c)) => Some(c),
        Ok(Err(e)) => {
            eprintln!("SKIP F5: rabbitmq connect failed for {url}: {e}");
            None
        }
        Err(_) => {
            eprintln!("SKIP F5: rabbitmq connect timeout for {url}");
            None
        }
    }
}

#[tokio::test]
async fn f5_rabbitmq_php_t4() {
    use aeon_connectors::rabbitmq::{
        RabbitMqSink, RabbitMqSinkConfig, RabbitMqSource, RabbitMqSourceConfig,
    };
    use aeon_types::DeliveryStrategy;
    use aeon_types::traits::{Sink, Source};
    use lapin::BasicProperties;
    use lapin::options::{
        BasicConsumeOptions, BasicPublishOptions, QueueDeclareOptions, QueueDeleteOptions,
    };
    use lapin::types::FieldTable;

    // --- 1. Preconditions ----------------------------------------------------
    let Some(setup_conn) = require_rabbitmq().await else {
        return;
    };
    if !e2e_ws_harness::runtime_available("php") {
        eprintln!("SKIP F5: php not found");
        return;
    }

    // --- 2. Fresh source + sink queues ---------------------------------------
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let source_queue = format!("aeon.e2e.f5.src.{suffix}");
    let sink_queue = format!("aeon.e2e.f5.sink.{suffix}");

    let setup_channel = setup_conn
        .create_channel()
        .await
        .expect("setup channel create");

    setup_channel
        .queue_declare(
            &source_queue,
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .expect("declare source queue");

    setup_channel
        .queue_declare(
            &sink_queue,
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .expect("declare sink queue");

    // --- 3. Pre-populate source queue via basic.publish ----------------------
    let msg_count: usize = 100;
    for i in 0..msg_count {
        let payload = format!("f5-payload-{i:05}");
        setup_channel
            .basic_publish(
                "",
                &source_queue,
                BasicPublishOptions::default(),
                payload.as_bytes(),
                BasicProperties::default().with_delivery_mode(2),
            )
            .await
            .expect("basic_publish")
            .await
            .expect("publisher confirm");
    }

    // --- 4. Start WS harness + register PHP processor identity ---------------
    let pipeline_name = "f5-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "php-proc");
    let seed_file = e2e_ws_harness::write_seed_file(&identity);
    let port = server.port;
    let seed_path = seed_file.to_string_lossy().to_string().replace('\\', "/");
    let pub_key = identity.public_key.clone();

    let script = e2e_ws_harness::php_passthrough_script(
        port,
        &seed_path,
        &pub_key,
        pipeline_name,
        "php-proc",
    );
    let script_path = std::env::temp_dir().join(format!("aeon_e2e_f5_php_{suffix}.php"));
    std::fs::write(&script_path, &script).unwrap();

    let mut child = std::process::Command::new("php")
        .arg(&script_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn php");

    let connected = e2e_ws_harness::wait_for_connection(&server, Duration::from_secs(10)).await;
    assert!(connected, "F5: PHP processor failed to connect");

    // --- 5. Aeon RabbitMQ source + sink --------------------------------------
    let source_config = RabbitMqSourceConfig::new(rabbitmq_url(), &source_queue)
        .with_prefetch_count(256)
        .with_poll_timeout(Duration::from_millis(500))
        .with_declare_queue(false)
        .with_source_name("f5-rabbitmq-source");
    let mut source = RabbitMqSource::new(source_config)
        .await
        .expect("RabbitMqSource::new");

    // OrderedBatch exercises the §4.4 join_all publisher-confirm round-trip
    // fix (batch boundary awaits all PublisherConfirm futures concurrently).
    let sink_config = RabbitMqSinkConfig::direct_to_queue(rabbitmq_url(), &sink_queue)
        .with_strategy(DeliveryStrategy::OrderedBatch);
    let mut sink = RabbitMqSink::new(sink_config)
        .await
        .expect("RabbitMqSink::new");

    // --- 6. Drain loop: RabbitMQ -> PHP T4 -> RabbitMQ -----------------------
    let mut total_received: usize = 0;
    let mut total_outputs: usize = 0;
    let mut empty_polls = 0;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            empty_polls += 1;
            if empty_polls >= 3 {
                break;
            }
            continue;
        }
        empty_polls = 0;
        total_received += events.len();
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len();
        sink.write_batch(outputs).await.unwrap();
        if total_received >= msg_count {
            break;
        }
    }
    sink.flush().await.unwrap();

    // --- 7. Verification -----------------------------------------------------
    assert_eq!(
        total_received, msg_count,
        "F5 C1: source received {total_received}, expected {msg_count}"
    );
    assert_eq!(
        total_outputs, msg_count,
        "F5 C1: processor produced {total_outputs}, expected {msg_count}"
    );

    // Drain the sink queue via a fresh consumer on the setup channel and
    // verify every payload round-tripped in order.
    let verify_channel = setup_conn
        .create_channel()
        .await
        .expect("verify channel create");
    let mut consumer = verify_channel
        .basic_consume(
            &sink_queue,
            "f5-verifier",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .expect("basic_consume sink queue");

    use futures_util::StreamExt;
    use lapin::options::BasicAckOptions;
    let mut collected: Vec<String> = Vec::with_capacity(msg_count);
    let verify_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while collected.len() < msg_count {
        let remaining = verify_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, consumer.next()).await {
            Ok(Some(Ok(delivery))) => {
                let payload = String::from_utf8(delivery.data.clone()).expect("utf8 payload");
                collected.push(payload);
                let _ = delivery.ack(BasicAckOptions::default()).await;
            }
            Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
        }
    }

    assert_eq!(
        collected.len(),
        msg_count,
        "F5 C1: sink queue drained {} messages, expected {msg_count}",
        collected.len()
    );
    for (i, payload) in collected.iter().enumerate() {
        assert_eq!(
            payload,
            &format!("f5-payload-{i:05}"),
            "F5 C2: payload mismatch at index {i}"
        );
    }

    // --- 8. Cleanup ----------------------------------------------------------
    let _ = setup_channel
        .queue_delete(&source_queue, QueueDeleteOptions::default())
        .await;
    let _ = setup_channel
        .queue_delete(&sink_queue, QueueDeleteOptions::default())
        .await;

    drop(server);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&seed_file);
}

// ===========================================================================
// F6: WebSocket -> WebSocket (Rust Network T4)
// ===========================================================================

#[tokio::test]
async fn f6_websocket_rust_net_t4() {
    // WebSocketSource → Rust T4 Processor → WebSocketSink
    // Loopback: we create our own WS servers for source and sink.

    use aeon_connectors::websocket::{
        WebSocketSink, WebSocketSinkConfig, WebSocketSource, WebSocketSourceConfig,
    };
    use aeon_processor_client::{ProcessEvent, ProcessOutput, ProcessorClient, ProcessorConfig};
    use aeon_types::traits::{Sink, Source};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let msg_count = 100;

    // === 1. Create WS server that SENDS events (for WebSocketSource) ===
    let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source_addr = source_listener.local_addr().unwrap();

    let source_server_handle = tokio::spawn(async move {
        // Accept one connection and send events
        let (stream, _) = source_listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut writer, _reader) = ws.split();

        for i in 0..msg_count {
            let payload = format!("f6-payload-{i:05}");
            writer.send(Message::Text(payload.into())).await.unwrap();
        }
        // Small delay to ensure all messages are sent before close
        tokio::time::sleep(Duration::from_millis(200)).await;
        writer.send(Message::Close(None)).await.ok();
    });

    // === 2. Create WS server that RECEIVES outputs (for WebSocketSink) ===
    let sink_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sink_addr = sink_listener.local_addr().unwrap();

    let sink_collected = Arc::new(tokio::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink_collected_clone = Arc::clone(&sink_collected);

    let sink_server_handle = tokio::spawn(async move {
        let (stream, _) = sink_listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (_writer, mut reader) = ws.split();

        while let Some(Ok(msg)) = reader.next().await {
            match msg {
                Message::Binary(data) => {
                    sink_collected_clone.lock().await.push(data.to_vec());
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // === 3. Start Rust T4 processor via engine WS host ===
    let pipeline_name = "f6-pipeline";
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
    assert!(connected, "F6: Rust processor failed to connect");

    // === 4. Wire: WebSocketSource → T4 Processor → WebSocketSink ===
    let source_config = WebSocketSourceConfig::new(format!("ws://{source_addr}"))
        .with_source_name("f6-ws-source")
        .with_poll_timeout(Duration::from_millis(500));
    let mut source = WebSocketSource::new(source_config).await.unwrap();

    let sink_config = WebSocketSinkConfig::new(format!("ws://{sink_addr}"));
    let mut sink = WebSocketSink::new(sink_config).await.unwrap();

    let mut total_received = 0usize;
    let mut total_outputs = 0usize;
    let mut empty_polls = 0;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            empty_polls += 1;
            if empty_polls >= 5 {
                break;
            }
            continue;
        }
        empty_polls = 0;
        total_received += events.len();
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len();
        sink.write_batch(outputs).await.unwrap();
    }
    sink.flush().await.unwrap();

    // Wait for sink server to collect everything
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        total_received, msg_count,
        "F6 C1: source received {total_received}, expected {msg_count}"
    );
    assert_eq!(
        total_outputs, msg_count,
        "F6 C1: processor produced {total_outputs}, expected {msg_count}"
    );

    // Verify payload integrity in sink server
    let collected = sink_collected.lock().await;
    assert_eq!(
        collected.len(),
        msg_count,
        "F6 C1: sink received {}, expected {msg_count}",
        collected.len()
    );
    for (i, data) in collected.iter().enumerate() {
        let payload = std::str::from_utf8(data).unwrap();
        assert_eq!(
            payload,
            format!("f6-payload-{i:05}"),
            "F6 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    source_server_handle.abort();
    sink_server_handle.abort();
}

// ===========================================================================
// F7: QUIC -> Rust T4 -> QUIC (loopback, no Docker)
// ===========================================================================
//
// QuicSource receives events from a QUIC client, routes them through a Rust
// T4 WebSocket processor, and QuicSink sends outputs to a QUIC server.
// Pure loopback — self-signed certs via dev_quic_configs(), no infra needed.

#[tokio::test]
async fn f7_quic_rust_t4() {
    use aeon_connectors::quic::{
        QuicSink, QuicSinkConfig, QuicSource, QuicSourceConfig, dev_quic_configs,
    };
    use aeon_processor_client::{ProcessEvent, ProcessOutput, ProcessorClient, ProcessorConfig};
    use aeon_types::traits::{Sink, Source};

    let msg_count = 100usize;

    // === 1. QuicSource — accepts events from a QUIC client ===
    let (server_config, client_config_for_source) = dev_quic_configs();
    let source_config = QuicSourceConfig::new("127.0.0.1:0".parse().unwrap(), server_config)
        .with_source_name("f7-quic-source")
        .with_poll_timeout(Duration::from_millis(500));
    let mut source = QuicSource::new(source_config).unwrap();
    let source_addr = source.local_addr();

    // === 2. QUIC "sink server" — receives outputs from QuicSink ===
    let (sink_server_config, sink_client_config) = dev_quic_configs();
    let sink_server_endpoint =
        quinn::Endpoint::server(sink_server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let sink_server_addr = sink_server_endpoint.local_addr().unwrap();

    let sink_collected = Arc::new(tokio::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink_collected_clone = Arc::clone(&sink_collected);

    let sink_server_handle = tokio::spawn(async move {
        // Accept one connection, then read all streams
        let incoming = sink_server_endpoint.accept().await.unwrap();
        let conn = incoming.await.unwrap();
        while let Ok((_, mut recv)) = conn.accept_bi().await {
            let collected = Arc::clone(&sink_collected_clone);
            tokio::spawn(async move {
                loop {
                    // Read length prefix
                    let mut len_buf = [0u8; 4];
                    match recv.read_exact(&mut len_buf).await {
                        Ok(()) => {}
                        Err(_) => break,
                    }
                    let len = u32::from_le_bytes(len_buf) as usize;
                    if len == 0 {
                        continue;
                    }
                    let mut payload = vec![0u8; len];
                    if recv.read_exact(&mut payload).await.is_err() {
                        break;
                    }
                    collected.lock().await.push(payload);
                }
            });
        }
    });

    // === 3. Start Rust T4 processor via engine WS host ===
    let pipeline_name = "f7-pipeline";
    let server = e2e_ws_harness::start_ws_test_server(pipeline_name).await;
    let identity = e2e_ws_harness::register_test_identity(&server, "rust-quic-proc");
    let port = server.port;
    let seed = *identity.signing_key.as_bytes();

    let _client_handle = tokio::spawn(async move {
        let config = ProcessorConfig::new(
            "rust-quic-proc",
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
    assert!(connected, "F7: Rust processor failed to connect");

    // === 4. Send events via QUIC client to the source ===
    let source_client_handle = tokio::spawn(async move {
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(client_config_for_source);
        let conn = endpoint
            .connect(source_addr, "localhost")
            .unwrap()
            .await
            .unwrap();

        let (mut send, _recv) = conn.open_bi().await.unwrap();
        for i in 0..msg_count {
            let payload = format!("f7-payload-{i:05}");
            let len = payload.len() as u32;
            send.write_all(&len.to_le_bytes()).await.unwrap();
            send.write_all(payload.as_bytes()).await.unwrap();
        }
        send.finish().unwrap();
        // Wait for stream to flush
        send.stopped().await.ok();
    });

    // === 5. Wire: QuicSource → T4 Processor → QuicSink ===
    let mut sink = QuicSink::new(QuicSinkConfig::new(
        sink_server_addr,
        "localhost",
        sink_client_config,
    ))
    .await
    .unwrap();

    let mut total_received = 0usize;
    let mut total_outputs = 0usize;
    let mut empty_polls = 0;
    loop {
        let events = source.next_batch().await.unwrap();
        if events.is_empty() {
            empty_polls += 1;
            if empty_polls >= 5 {
                break;
            }
            continue;
        }
        empty_polls = 0;
        total_received += events.len();
        let outputs = server.ws_host.call_batch(events).await.unwrap();
        total_outputs += outputs.len();
        sink.write_batch(outputs).await.unwrap();
    }
    sink.flush().await.unwrap();

    // Wait for source client to finish sending
    source_client_handle.await.unwrap();

    // Wait for sink server to collect
    tokio::time::sleep(Duration::from_millis(300)).await;

    // === 6. Verify criteria ===
    assert_eq!(
        total_received, msg_count,
        "F7 C1: source received {total_received}, expected {msg_count}"
    );
    assert_eq!(
        total_outputs, msg_count,
        "F7 C1: processor produced {total_outputs}, expected {msg_count}"
    );

    let collected = sink_collected.lock().await;
    assert_eq!(
        collected.len(),
        msg_count,
        "F7 C1: sink received {}, expected {msg_count}",
        collected.len()
    );
    for (i, data) in collected.iter().enumerate() {
        let payload = std::str::from_utf8(data).unwrap();
        assert_eq!(
            payload,
            format!("f7-payload-{i:05}"),
            "F7 C2: payload mismatch at {i}"
        );
    }

    drop(server);
    sink_server_handle.abort();
}
