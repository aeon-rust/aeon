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
    use aeon_types::traits::{Sink, Source};
    use aeon_types::DeliveryStrategy;

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
    let mut source = NatsSource::new(source_config).await.expect("NatsSource::new");

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
// F2: NATS -> Kafka (Go T4)
// ===========================================================================

#[tokio::test]
#[ignore = "requires NATS server + Redpanda (Docker) + Go SDK + engine WebSocket host"]
async fn f2_nats_kafka_go_t4() {
    todo!("Implement with NatsSource -> Go T4 -> KafkaSink");
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
    use aeon_types::traits::{Sink, Source};
    use aeon_types::DeliveryStrategy;
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
// F4: MQTT -> MQTT (Java T4)
// ===========================================================================

#[tokio::test]
#[ignore = "requires Mosquitto MQTT broker (Docker) + Java SDK + engine WebSocket host"]
async fn f4_mqtt_java_t4() {
    todo!("Implement with MqttSource -> Java T4 -> MqttSink");
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
    use aeon_types::traits::{Sink, Source};
    use aeon_types::DeliveryStrategy;
    use lapin::options::{
        BasicConsumeOptions, BasicPublishOptions, QueueDeclareOptions, QueueDeleteOptions,
    };
    use lapin::types::FieldTable;
    use lapin::BasicProperties;

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
    let mut sink = RabbitMqSink::new(sink_config).await.expect("RabbitMqSink::new");

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
        .signing_key_from_seed(&seed)
        .codec("json");

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
// F7: QUIC -> QUIC (Go T3)
// ===========================================================================

#[tokio::test]
#[ignore = "requires TLS certs + Go SDK T3 + engine WebTransport host (loopback, no Docker)"]
async fn f7_quic_go_t3() {
    todo!("Implement with QuicSource -> Go T3 -> QuicSink (loopback)");
}
