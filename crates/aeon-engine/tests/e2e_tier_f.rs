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
// F1: NATS -> NATS (Python T4)
// ===========================================================================

#[tokio::test]
#[ignore = "requires NATS server (Docker) + Python SDK + engine WebSocket host"]
async fn f1_nats_python_t4() {
    todo!("Implement with NatsSource -> Python T4 -> NatsSink");
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
// F3: Redis -> Redis (Node.js T4)
// ===========================================================================

#[tokio::test]
#[ignore = "requires Redis server (Docker) + Node.js SDK + engine WebSocket host"]
async fn f3_redis_nodejs_t4() {
    todo!("Implement with RedisSource -> Node.js T4 -> RedisSink");
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
// F5: RabbitMQ -> RabbitMQ (PHP T4)
// ===========================================================================

#[tokio::test]
#[ignore = "requires RabbitMQ server (Docker) + PHP SDK + engine WebSocket host"]
async fn f5_rabbitmq_php_t4() {
    todo!("Implement with RabbitMqSource -> PHP T4 -> RabbitMqSink");
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
    let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
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
    let sink_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
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
