//! T3 WebTransport transport client for AWPP.
//!
//! Connects to Aeon via HTTP/3 + QUIC (WebTransport), performs AWPP handshake
//! over the control stream, and processes batches over bidirectional data streams.
//!
//! WebTransport provides lower latency than WebSocket due to QUIC's 0-RTT
//! connection establishment and multiplexed streams.

use aeon_types::AeonError;
use wtransport::{ClientConfig, Endpoint};

use crate::auth;
use crate::wire::{self, Accepted, Challenge, Heartbeat, Register};
use crate::{BatchProcessFn, ProcessFn, ProcessorConfig, SessionInfo};

/// Run a processor over T3 WebTransport with per-event processing.
pub async fn run_webtransport(
    config: ProcessorConfig,
    process_fn: ProcessFn,
) -> Result<(), AeonError> {
    let wrapper: Box<dyn Fn(Vec<crate::ProcessEvent>) -> Vec<Vec<crate::ProcessOutput>> + Send> =
        Box::new(move |events| events.into_iter().map(process_fn).collect());
    run_webtransport_inner(config, wrapper).await
}

/// Run a processor over T3 WebTransport with batch processing.
pub async fn run_webtransport_batch(
    config: ProcessorConfig,
    process_fn: BatchProcessFn,
) -> Result<(), AeonError> {
    let wrapper: Box<dyn Fn(Vec<crate::ProcessEvent>) -> Vec<Vec<crate::ProcessOutput>> + Send> =
        Box::new(process_fn);
    run_webtransport_inner(config, wrapper).await
}

/// Internal WebTransport processing loop.
async fn run_webtransport_inner(
    config: ProcessorConfig,
    process_fn: Box<dyn Fn(Vec<crate::ProcessEvent>) -> Vec<Vec<crate::ProcessOutput>> + Send>,
) -> Result<(), AeonError> {
    // Parse URL to extract host for WebTransport.
    let url = config.url.clone();

    #[cfg(feature = "webtransport-insecure")]
    let wt_config = ClientConfig::builder()
        .with_bind_default()
        .with_no_cert_validation()
        .build();

    #[cfg(not(feature = "webtransport-insecure"))]
    let wt_config = ClientConfig::builder()
        .with_bind_default()
        .with_native_certs()
        .build();

    let connection = Endpoint::client(wt_config)
        .map_err(|e| AeonError::state(format!("WebTransport endpoint: {e}")))?
        .connect(&url)
        .await
        .map_err(|e| AeonError::state(format!("WebTransport connect: {e}")))?;

    tracing::info!(url = %url, "Connected to Aeon via WebTransport");

    // Open control stream (bidirectional stream 0).
    let (mut ctrl_send, mut ctrl_recv) = connection
        .open_bi()
        .await
        .map_err(|e| AeonError::state(format!("Open control stream: {e}")))?
        .await
        .map_err(|e| AeonError::state(format!("Await control stream: {e}")))?;

    // Step 1: Receive Challenge over control stream.
    let challenge_json = read_length_prefixed(&mut ctrl_recv).await?;
    let challenge: Challenge = serde_json::from_str(&challenge_json)
        .map_err(|e| AeonError::state(format!("Parse challenge: {e}")))?;

    if challenge.msg_type != "challenge" {
        return Err(AeonError::state(format!(
            "Expected challenge, got: {}",
            challenge.msg_type
        )));
    }
    tracing::debug!(nonce_len = challenge.nonce.len(), "Received challenge");

    // Step 2: Send Register.
    let register = build_register(&config, &challenge)?;
    let register_json = serde_json::to_string(&register)
        .map_err(|e| AeonError::state(format!("Serialize register: {e}")))?;
    write_length_prefixed(&mut ctrl_send, register_json.as_bytes()).await?;

    // Step 3: Receive Accepted or Rejected.
    let response_json = read_length_prefixed(&mut ctrl_recv).await?;
    let msg_type = wire::parse_control_type(&response_json)?;

    let session = match msg_type.as_str() {
        "accepted" => {
            let accepted: Accepted = serde_json::from_str(&response_json)
                .map_err(|e| AeonError::state(format!("Parse accepted: {e}")))?;
            SessionInfo {
                session_id: accepted.session_id,
                codec: accepted.transport_codec,
                heartbeat_interval_ms: accepted.heartbeat_interval_ms,
                batch_signing: accepted.batch_signing,
            }
        }
        "rejected" => {
            let rejected: wire::Rejected = serde_json::from_str(&response_json)
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
    };

    tracing::info!(
        session_id = %session.session_id,
        codec = %session.codec,
        "Session accepted via WebTransport"
    );

    // Start heartbeat task on control stream.
    let heartbeat_interval = std::time::Duration::from_millis(session.heartbeat_interval_ms);
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval);
        loop {
            interval.tick().await;
            let hb = Heartbeat::now();
            let json = serde_json::to_string(&hb).unwrap_or_default();
            if write_length_prefixed(&mut ctrl_send, json.as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Processing loop: accept bidirectional data streams from server.
    let codec = session.codec.clone();
    let batch_signing = session.batch_signing;
    let signing_key = config.signing_key.clone();

    loop {
        match connection.accept_bi().await {
            Ok(stream) => {
                let (mut send, mut recv) = stream;
                let codec = codec.clone();
                let signing_key = signing_key.clone();

                // Read batch request from data stream.
                let mut data = Vec::new();
                let mut buf = [0u8; 65536];
                loop {
                    match recv.read(&mut buf).await {
                        Ok(Some(n)) => data.extend_from_slice(&buf[..n]),
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(err = %e, "Data stream read error");
                            break;
                        }
                    }
                }

                if data.is_empty() {
                    continue;
                }

                // Decode and process.
                match wire::decode_batch_request(&data, &codec) {
                    Ok((batch_id, events)) => {
                        let outputs = process_fn(events);
                        match wire::encode_batch_response(
                            batch_id,
                            &outputs,
                            &codec,
                            &signing_key,
                            batch_signing,
                        ) {
                            Ok(response) => {
                                if let Err(e) = send.write_all(&response).await {
                                    tracing::warn!(err = %e, "Data stream write error");
                                }
                                let _ = send.finish().await;
                            }
                            Err(e) => {
                                tracing::warn!(err = %e, "Failed to encode response");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "Failed to decode batch request");
                    }
                }
            }
            Err(e) => {
                tracing::info!(err = %e, "Connection closed or error accepting stream");
                break;
            }
        }
    }

    heartbeat_handle.abort();
    tracing::info!("WebTransport processor disconnected");
    Ok(())
}

/// Build the AWPP Register message for WebTransport.
fn build_register(config: &ProcessorConfig, challenge: &Challenge) -> Result<Register, AeonError> {
    let pk = config.signing_key.verifying_key();
    let public_key = auth::format_public_key(&pk);
    let challenge_signature = auth::sign_challenge(&config.signing_key, &challenge.nonce)?;

    Ok(Register {
        msg_type: "register".into(),
        protocol: challenge.protocol.clone(),
        transport: "webtransport".into(),
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

/// Read a length-prefixed message from a QUIC stream.
///
/// Format: `[4B len LE][message bytes]`
async fn read_length_prefixed(recv: &mut wtransport::RecvStream) -> Result<String, AeonError> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| AeonError::state(format!("Read length prefix: {e}")))?;

    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 1024 * 1024 {
        return Err(AeonError::state(format!(
            "Control message too large: {len} bytes"
        )));
    }

    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| AeonError::state(format!("Read control message: {e}")))?;

    String::from_utf8(buf).map_err(|e| AeonError::state(format!("Control message not UTF-8: {e}")))
}

/// Write a length-prefixed message to a QUIC stream.
///
/// Format: `[4B len LE][message bytes]`
async fn write_length_prefixed(
    send: &mut wtransport::SendStream,
    data: &[u8],
) -> Result<(), AeonError> {
    let len = (data.len() as u32).to_le_bytes();
    send.write_all(&len)
        .await
        .map_err(|e| AeonError::state(format!("Write length prefix: {e}")))?;
    send.write_all(data)
        .await
        .map_err(|e| AeonError::state(format!("Write control message: {e}")))?;
    Ok(())
}
