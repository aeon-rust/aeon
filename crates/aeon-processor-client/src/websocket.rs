//! T4 WebSocket transport client for AWPP.
//!
//! Connects to Aeon via WebSocket, performs AWPP handshake, and enters
//! the processing loop. Handles heartbeats, batch decode/encode, and
//! reconnection transparently.

use aeon_types::AeonError;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::auth;
use crate::wire::{self, Accepted, Challenge, Heartbeat, Register};
use crate::{BatchProcessFn, ProcessFn, ProcessorConfig, SessionInfo};

/// Run a processor over T4 WebSocket with per-event processing.
pub async fn run_websocket(
    config: ProcessorConfig,
    process_fn: ProcessFn,
) -> Result<(), AeonError> {
    let wrapper: Box<dyn Fn(Vec<crate::ProcessEvent>) -> Vec<Vec<crate::ProcessOutput>> + Send> =
        Box::new(move |events| events.into_iter().map(process_fn).collect());
    run_websocket_inner(config, wrapper).await
}

/// Run a processor over T4 WebSocket with batch processing.
pub async fn run_websocket_batch(
    config: ProcessorConfig,
    process_fn: BatchProcessFn,
) -> Result<(), AeonError> {
    let wrapper: Box<dyn Fn(Vec<crate::ProcessEvent>) -> Vec<Vec<crate::ProcessOutput>> + Send> =
        Box::new(process_fn);
    run_websocket_inner(config, wrapper).await
}

/// Internal WebSocket processing loop.
async fn run_websocket_inner(
    config: ProcessorConfig,
    process_fn: Box<dyn Fn(Vec<crate::ProcessEvent>) -> Vec<Vec<crate::ProcessOutput>> + Send>,
) -> Result<(), AeonError> {
    let (ws_stream, _response) = tokio_tungstenite::connect_async(&config.url)
        .await
        .map_err(|e| AeonError::state(format!("WebSocket connect failed: {e}")))?;

    tracing::info!(url = %config.url, "Connected to Aeon");

    let (mut sink, mut stream) = ws_stream.split();

    // Step 1: Receive Challenge.
    let challenge = recv_challenge(&mut stream).await?;
    tracing::debug!(nonce_len = challenge.nonce.len(), "Received challenge");

    // Step 2: Send Register.
    let register = build_register(&config, &challenge)?;
    let register_json = serde_json::to_string(&register)
        .map_err(|e| AeonError::state(format!("Serialize register: {e}")))?;
    sink.send(Message::Text(register_json.into()))
        .await
        .map_err(|e| AeonError::state(format!("Send register: {e}")))?;

    // Step 3: Receive Accepted or Rejected.
    let session = recv_accepted(&mut stream).await?;
    tracing::info!(
        session_id = %session.session_id,
        codec = %session.codec,
        heartbeat_ms = session.heartbeat_interval_ms,
        "Session accepted"
    );

    // Start heartbeat task.
    let heartbeat_interval = std::time::Duration::from_millis(session.heartbeat_interval_ms);
    let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::mpsc::channel::<Message>(4);

    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval);
        loop {
            interval.tick().await;
            let hb = Heartbeat::now();
            let json = serde_json::to_string(&hb).unwrap_or_default();
            if heartbeat_tx.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    // Processing loop.
    let codec = session.codec.clone();
    let batch_signing = session.batch_signing;
    let signing_key = config.signing_key.clone();

    loop {
        tokio::select! {
            // Heartbeat to send.
            Some(hb_msg) = heartbeat_rx.recv() => {
                if let Err(e) = sink.send(hb_msg).await {
                    tracing::warn!("Heartbeat send failed: {e}");
                    break;
                }
            }

            // Message from Aeon.
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        // Data frame: parse routing header + batch.
                        match wire::parse_data_frame(&data) {
                            Ok((pipeline, partition, batch_data)) => {
                                match wire::decode_batch_request(batch_data, &codec) {
                                    Ok((batch_id, events)) => {
                                        let outputs = process_fn(events);
                                        let response = wire::encode_batch_response(
                                            batch_id,
                                            &outputs,
                                            &codec,
                                            &signing_key,
                                            batch_signing,
                                        )?;
                                        let frame = wire::build_data_frame(
                                            &pipeline,
                                            partition,
                                            &response,
                                        );
                                        sink.send(Message::Binary(frame.into()))
                                            .await
                                            .map_err(|e| AeonError::state(
                                                format!("Send response: {e}")
                                            ))?;
                                    }
                                    Err(e) => {
                                        tracing::warn!(batch_err = %e, "Failed to decode batch");
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(frame_err = %e, "Failed to parse data frame");
                            }
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        // Control message.
                        match wire::parse_control_type(&text) {
                            Ok(ref t) if t == "heartbeat" => {
                                // Server heartbeat — acknowledged by our heartbeat loop.
                            }
                            Ok(ref t) if t == "drain" => {
                                tracing::info!("Received drain signal, shutting down");
                                break;
                            }
                            Ok(ref t) if t == "error" => {
                                tracing::error!(msg = %text, "Server error");
                                break;
                            }
                            Ok(t) => {
                                tracing::debug!(msg_type = %t, "Unknown control message");
                            }
                            Err(e) => {
                                tracing::warn!(err = %e, "Invalid control message");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        tracing::info!("Server closed connection");
                        break;
                    }
                    Some(Ok(_)) => {} // Ping/Pong handled by tungstenite
                    Some(Err(e)) => {
                        tracing::error!(err = %e, "WebSocket error");
                        break;
                    }
                    None => {
                        tracing::info!("WebSocket stream ended");
                        break;
                    }
                }
            }
        }
    }

    heartbeat_handle.abort();
    tracing::info!("Processor disconnected");
    Ok(())
}

/// Receive and parse the AWPP Challenge message.
async fn recv_challenge<S>(stream: &mut S) -> Result<Challenge, AeonError>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                let challenge: Challenge = serde_json::from_str(&text)
                    .map_err(|e| AeonError::state(format!("Parse challenge: {e}")))?;
                if challenge.msg_type != "challenge" {
                    return Err(AeonError::state(format!(
                        "Expected challenge, got: {}",
                        challenge.msg_type
                    )));
                }
                return Ok(challenge);
            }
            Some(Ok(_)) => continue, // skip non-text
            Some(Err(e)) => return Err(AeonError::state(format!("WS recv challenge: {e}"))),
            None => return Err(AeonError::state("Connection closed before challenge")),
        }
    }
}

/// Build the AWPP Register message.
fn build_register(config: &ProcessorConfig, challenge: &Challenge) -> Result<Register, AeonError> {
    let pk = config.signing_key.verifying_key();
    let public_key = auth::format_public_key(&pk);
    let challenge_signature = auth::sign_challenge(&config.signing_key, &challenge.nonce)?;

    Ok(Register {
        msg_type: "register".into(),
        protocol: challenge.protocol.clone(),
        transport: "websocket".into(),
        name: config.name.clone(),
        version: config.version.clone(),
        public_key,
        challenge_signature,
        oauth_token: None,
        capabilities: vec!["batch".into()],
        max_batch_size: None,
        transport_codec: config.codec.clone(),
        requested_pipelines: config.pipelines.clone(),
        binding: config.binding.clone(),
    })
}

/// Receive and parse the AWPP Accepted or Rejected message.
async fn recv_accepted<S>(stream: &mut S) -> Result<SessionInfo, AeonError>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                let msg_type = wire::parse_control_type(&text)?;
                match msg_type.as_str() {
                    "accepted" => {
                        let accepted: Accepted = serde_json::from_str(&text)
                            .map_err(|e| AeonError::state(format!("Parse accepted: {e}")))?;
                        return Ok(SessionInfo {
                            session_id: accepted.session_id,
                            codec: accepted.transport_codec,
                            heartbeat_interval_ms: accepted.heartbeat_interval_ms,
                            batch_signing: accepted.batch_signing,
                        });
                    }
                    "rejected" => {
                        let rejected: wire::Rejected = serde_json::from_str(&text)
                            .map_err(|e| AeonError::state(format!("Parse rejected: {e}")))?;
                        return Err(AeonError::state(format!(
                            "Registration rejected: {} — {}",
                            rejected.code, rejected.message
                        )));
                    }
                    other => {
                        return Err(AeonError::state(format!(
                            "Expected accepted/rejected, got: {other}"
                        )));
                    }
                }
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(AeonError::state(format!("WS recv accepted: {e}"))),
            None => return Err(AeonError::state("Connection closed before accepted")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_register_message() {
        let config =
            ProcessorConfig::new("test-proc", "ws://localhost:4471/api/v1/processors/connect")
                .pipeline("my-pipeline")
                .version("2.0.0");

        let challenge = Challenge {
            msg_type: "challenge".into(),
            protocol: "awpp/1".into(),
            nonce: hex::encode([0xABu8; 32]),
            oauth_required: false,
        };

        let reg = build_register(&config, &challenge).unwrap();

        assert_eq!(reg.msg_type, "register");
        assert_eq!(reg.protocol, "awpp/1");
        assert_eq!(reg.transport, "websocket");
        assert_eq!(reg.name, "test-proc");
        assert_eq!(reg.version, "2.0.0");
        assert!(reg.public_key.starts_with("ed25519:"));
        assert_eq!(reg.challenge_signature.len(), 128);
        assert_eq!(reg.requested_pipelines, vec!["my-pipeline"]);
        assert_eq!(reg.binding, "dedicated");
        assert_eq!(reg.transport_codec, "msgpack");
    }

    #[test]
    fn parse_accepted_json() {
        let json = r#"{
            "type": "accepted",
            "session_id": "550e8400-e29b-41d4-a716-446655440000",
            "pipelines": [{"name": "p1", "partitions": [0, 1], "batch_size": 100}],
            "wire_format": "binary/v1",
            "transport_codec": "msgpack",
            "heartbeat_interval_ms": 10000,
            "batch_signing": true
        }"#;

        let accepted: Accepted = serde_json::from_str(json).unwrap();
        assert_eq!(accepted.session_id, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(accepted.pipelines.len(), 1);
        assert_eq!(accepted.pipelines[0].partitions, vec![0, 1]);
        assert_eq!(accepted.heartbeat_interval_ms, 10000);
        assert!(accepted.batch_signing);
    }

    #[test]
    fn parse_rejected_json() {
        let json = r#"{
            "type": "rejected",
            "code": "AUTH_FAILED",
            "message": "ED25519 verification failed"
        }"#;

        let rejected: wire::Rejected = serde_json::from_str(json).unwrap();
        assert_eq!(rejected.code, "AUTH_FAILED");
    }
}
