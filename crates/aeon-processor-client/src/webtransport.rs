//! T3 WebTransport transport client for AWPP.
//!
//! Connects to Aeon via HTTP/3 + QUIC (WebTransport), performs AWPP handshake
//! over the control stream, and processes batches over bidirectional data streams.
//!
//! WebTransport provides lower latency than WebSocket due to QUIC's 0-RTT
//! connection establishment and multiplexed streams.

use std::sync::Arc;

use aeon_types::AeonError;
use wtransport::{ClientConfig, Endpoint};

/// Log the webtransport-insecure warning at most once per process.
#[cfg(feature = "webtransport-insecure")]
fn warn_once_webtransport_insecure() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "⚠ webtransport-insecure feature is ENABLED: server certificates will NOT be \
             validated. This is intended for local development ONLY. Rebuild without the \
             `webtransport-insecure` feature before deploying to production."
        );
    }
}

use crate::auth;
use crate::wire::{self, Accepted, Challenge, Heartbeat, Register};
use crate::{BatchProcessFn, ProcessFn, ProcessorConfig, SessionInfo};

/// Shared process function type used across spawned per-stream tasks.
/// Must be `Send + Sync` because data-stream workers are spawned on the tokio runtime.
type SharedProcessFn =
    Arc<dyn Fn(Vec<crate::ProcessEvent>) -> Vec<Vec<crate::ProcessOutput>> + Send + Sync>;

/// Run a processor over T3 WebTransport with per-event processing.
pub async fn run_webtransport(
    config: ProcessorConfig,
    process_fn: ProcessFn,
) -> Result<(), AeonError> {
    let wrapper: SharedProcessFn =
        Arc::new(move |events| events.into_iter().map(process_fn).collect());
    run_webtransport_inner(config, wrapper).await
}

/// Run a processor over T3 WebTransport with batch processing.
pub async fn run_webtransport_batch(
    config: ProcessorConfig,
    process_fn: BatchProcessFn,
) -> Result<(), AeonError> {
    let wrapper: SharedProcessFn = Arc::new(process_fn);
    run_webtransport_inner(config, wrapper).await
}

/// Internal WebTransport processing loop.
async fn run_webtransport_inner(
    config: ProcessorConfig,
    process_fn: SharedProcessFn,
) -> Result<(), AeonError> {
    // Parse URL to extract host for WebTransport.
    let url = config.url.clone();

    #[cfg(feature = "webtransport-insecure")]
    let wt_config = {
        // FT-8: compile-time insecure mode. Log once per process at connect
        // time so operators notice when a "dev" binary is running in prod.
        warn_once_webtransport_insecure();
        ClientConfig::builder()
            .with_bind_default()
            .with_no_cert_validation()
            .build()
    };

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

    let (session, pipelines) = match msg_type.as_str() {
        "accepted" => {
            let accepted: Accepted = serde_json::from_str(&response_json)
                .map_err(|e| AeonError::state(format!("Parse accepted: {e}")))?;
            let pipelines = accepted.pipelines.clone();
            (
                SessionInfo {
                    session_id: accepted.session_id,
                    codec: accepted.transport_codec,
                    heartbeat_interval_ms: accepted.heartbeat_interval_ms,
                    batch_signing: accepted.batch_signing,
                },
                pipelines,
            )
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
        pipeline_count = pipelines.len(),
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

    // Open one long-lived bidirectional data stream per (pipeline, partition)
    // assignment. The client opens the stream, writes a routing header, then
    // loops on length-prefixed batch frames (reads requests, writes responses).
    // This matches the server's `handle_wt_session` → `accept_bi` + routing-header
    // protocol in `aeon-engine/src/transport/webtransport_host.rs`.
    let codec = session.codec.clone();
    let batch_signing = session.batch_signing;
    let signing_key = config.signing_key.clone();

    let mut stream_tasks = tokio::task::JoinSet::new();
    for assignment in &pipelines {
        for &partition in &assignment.partitions {
            let pipeline_name = assignment.name.clone();
            let (mut send, recv) = connection
                .open_bi()
                .await
                .map_err(|e| AeonError::state(format!("Open data stream: {e}")))?
                .await
                .map_err(|e| AeonError::state(format!("Await data stream: {e}")))?;

            // Routing header: [4B name_len LE][name][2B partition LE]
            let name_bytes = pipeline_name.as_bytes();
            let mut header = Vec::with_capacity(4 + name_bytes.len() + 2);
            header.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            header.extend_from_slice(name_bytes);
            header.extend_from_slice(&partition.to_le_bytes());
            send.write_all(&header)
                .await
                .map_err(|e| AeonError::state(format!("Write routing header: {e}")))?;

            tracing::debug!(
                pipeline = %pipeline_name,
                partition,
                "T3 data stream registered"
            );

            let codec = codec.clone();
            let signing_key = signing_key.clone();
            let process_fn = process_fn.clone();
            let label_pipeline = pipeline_name.clone();

            stream_tasks.spawn(async move {
                if let Err(e) =
                    run_data_stream(send, recv, codec, batch_signing, signing_key, process_fn).await
                {
                    tracing::debug!(
                        pipeline = %label_pipeline,
                        partition,
                        err = %e,
                        "T3 data stream ended"
                    );
                }
            });
        }
    }

    if stream_tasks.is_empty() {
        tracing::warn!(
            "No pipeline assignments in Accepted — WebTransport processor has nothing to do"
        );
    } else {
        // Wait for the first data stream task to finish (error / connection close),
        // then tear down the remaining streams and the heartbeat.
        let _ = stream_tasks.join_next().await;
        tracing::info!("A T3 data stream ended, draining remaining streams");
    }

    stream_tasks.abort_all();
    while stream_tasks.join_next().await.is_some() {}
    heartbeat_handle.abort();
    tracing::info!("WebTransport processor disconnected");
    Ok(())
}

/// Drive a single long-lived WebTransport data stream.
///
/// Reads length-prefixed batch requests from `recv`, runs `process_fn`, and
/// writes length-prefixed batch responses on `send`. Matches the server-side
/// `wt_data_stream_reader` + `call_batch` framing:
/// `[4B len LE][batch_wire bytes]`, with the wire payload built/consumed by
/// `wire::{decode_batch_request, encode_batch_response}`.
async fn run_data_stream(
    mut send: wtransport::SendStream,
    mut recv: wtransport::RecvStream,
    codec: String,
    batch_signing: bool,
    signing_key: ed25519_dalek::SigningKey,
    process_fn: SharedProcessFn,
) -> Result<(), AeonError> {
    loop {
        // Read 4-byte LE length prefix.
        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf)
            .await
            .map_err(|e| AeonError::state(format!("Read frame length: {e}")))?;
        let frame_len = u32::from_le_bytes(len_buf) as usize;
        if frame_len > 16 * 1024 * 1024 {
            return Err(AeonError::state(format!(
                "Batch frame too large: {frame_len} bytes"
            )));
        }

        // Read frame body.
        let mut frame = vec![0u8; frame_len];
        recv.read_exact(&mut frame)
            .await
            .map_err(|e| AeonError::state(format!("Read frame body: {e}")))?;

        // Decode → process → encode.
        let (batch_id, events) = wire::decode_batch_request(&frame, &codec)?;
        let outputs = process_fn(events);
        let response =
            wire::encode_batch_response(batch_id, &outputs, &codec, &signing_key, batch_signing)?;

        // Write length-prefixed response.
        let resp_len = (response.len() as u32).to_le_bytes();
        send.write_all(&resp_len)
            .await
            .map_err(|e| AeonError::state(format!("Write response length: {e}")))?;
        send.write_all(&response)
            .await
            .map_err(|e| AeonError::state(format!("Write response body: {e}")))?;
    }
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
