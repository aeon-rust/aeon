//! AWPP wire format: batch encoding/decoding, control messages, routing headers.
//!
//! Implements the binary batch wire format and JSON control stream messages
//! for the Aeon Wire Processor Protocol (AWPP).

use aeon_types::AeonError;
use serde::{Deserialize, Serialize};

use crate::{ProcessEvent, ProcessOutput};

// --- Control Stream Messages (JSON) ---

/// AWPP Challenge message (server → processor).
#[derive(Debug, Deserialize)]
pub struct Challenge {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub protocol: String,
    pub nonce: String,
    #[serde(default)]
    pub oauth_required: bool,
}

/// AWPP Register message (processor → server).
#[derive(Debug, Serialize)]
pub struct Register {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub protocol: String,
    pub transport: String,
    pub name: String,
    pub version: String,
    pub public_key: String,
    pub challenge_signature: String,
    pub oauth_token: Option<String>,
    pub capabilities: Vec<String>,
    pub max_batch_size: Option<u32>,
    pub transport_codec: String,
    pub requested_pipelines: Vec<String>,
    pub binding: String,
}

/// AWPP Accepted message (server → processor).
#[derive(Debug, Deserialize)]
pub struct Accepted {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub session_id: String,
    pub pipelines: Vec<PipelineAssignment>,
    pub wire_format: String,
    pub transport_codec: String,
    pub heartbeat_interval_ms: u64,
    #[serde(default)]
    pub batch_signing: bool,
}

/// Pipeline assignment within Accepted.
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineAssignment {
    pub name: String,
    pub partitions: Vec<u16>,
    pub batch_size: u32,
}

/// AWPP Rejected message (server → processor).
#[derive(Debug, Deserialize)]
pub struct Rejected {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub code: String,
    pub message: String,
}

/// AWPP Heartbeat message (bidirectional).
#[derive(Debug, Serialize, Deserialize)]
pub struct Heartbeat {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub timestamp_ms: u64,
}

/// AWPP Drain message (server → processor).
#[derive(Debug, Deserialize)]
pub struct Drain {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub reason: String,
    pub deadline_ms: u64,
}

impl Heartbeat {
    /// Create a new heartbeat with current timestamp.
    pub fn now() -> Self {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            msg_type: "heartbeat".into(),
            timestamp_ms: ts,
        }
    }
}

// --- Batch Wire Format ---

// FT-10: Fixed-size LE readers. The caller guarantees length via explicit
// bounds checks before calling. Using `from_le_bytes` directly on a copied
// array avoids the `try_into().unwrap()` pattern flagged by clippy and
// surfaces a clean panic-free signature.
#[inline]
fn read_u64_le(data: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[..8]);
    u64::from_le_bytes(b)
}
#[inline]
fn read_u32_le(data: &[u8]) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&data[..4]);
    u32::from_le_bytes(b)
}
#[inline]
fn read_u16_le(data: &[u8]) -> u16 {
    let mut b = [0u8; 2];
    b.copy_from_slice(&data[..2]);
    u16::from_le_bytes(b)
}

/// Decode a batch request from binary wire format.
///
/// Wire format:
/// `[8B batch_id LE][4B count LE][per event: 4B len LE + bytes][4B CRC32 LE]`
pub fn decode_batch_request(
    data: &[u8],
    codec: &str,
) -> Result<(u64, Vec<ProcessEvent>), AeonError> {
    if data.len() < 16 {
        return Err(AeonError::state("Batch request too short"));
    }

    let batch_id = read_u64_le(&data[0..8]);
    let event_count = read_u32_le(&data[8..12]) as usize;

    // Verify CRC32.
    if data.len() < 16 {
        return Err(AeonError::state("Batch missing CRC32"));
    }
    let crc_offset = data.len() - 4;
    let stored_crc = read_u32_le(&data[crc_offset..]);
    let computed_crc = crc32fast::hash(&data[..crc_offset]);
    if stored_crc != computed_crc {
        return Err(AeonError::state(format!(
            "CRC32 mismatch: stored={stored_crc:#X}, computed={computed_crc:#X}"
        )));
    }

    // Parse events.
    let mut offset = 12;
    let mut events = Vec::with_capacity(event_count);

    for _ in 0..event_count {
        if offset + 4 > crc_offset {
            return Err(AeonError::state("Truncated event in batch"));
        }
        let event_len = read_u32_le(&data[offset..offset + 4]) as usize;
        offset += 4;

        if offset + event_len > crc_offset {
            return Err(AeonError::state("Event data exceeds batch boundary"));
        }
        let event_bytes = &data[offset..offset + event_len];
        offset += event_len;

        let event: ProcessEvent = match codec {
            "json" => serde_json::from_slice(event_bytes)
                .map_err(|e| AeonError::state(format!("JSON decode event: {e}")))?,
            _ => rmp_serde::from_slice(event_bytes)
                .map_err(|e| AeonError::state(format!("MsgPack decode event: {e}")))?,
        };
        events.push(event);
    }

    Ok((batch_id, events))
}

/// Encode a batch response into binary wire format.
///
/// Wire format:
/// `[8B batch_id LE][4B count LE][per event: 4B output_count + outputs][4B CRC32 LE][64B sig]`
pub fn encode_batch_response(
    batch_id: u64,
    outputs: &[Vec<ProcessOutput>],
    codec: &str,
    signing_key: &ed25519_dalek::SigningKey,
    batch_signing: bool,
) -> Result<Vec<u8>, AeonError> {
    let mut buf = Vec::with_capacity(1024);

    // Header.
    buf.extend_from_slice(&batch_id.to_le_bytes());
    buf.extend_from_slice(&(outputs.len() as u32).to_le_bytes());

    // Per-event outputs.
    for event_outputs in outputs {
        buf.extend_from_slice(&(event_outputs.len() as u32).to_le_bytes());
        for output in event_outputs {
            let encoded = match codec {
                "json" => serde_json::to_vec(output)
                    .map_err(|e| AeonError::state(format!("JSON encode output: {e}")))?,
                _ => rmp_serde::to_vec_named(output)
                    .map_err(|e| AeonError::state(format!("MsgPack encode output: {e}")))?,
            };
            buf.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
            buf.extend_from_slice(&encoded);
        }
    }

    // CRC32.
    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());

    // Signature.
    if batch_signing {
        let sig = crate::auth::sign_batch(signing_key, &buf);
        buf.extend_from_slice(&sig);
    } else {
        buf.extend_from_slice(&[0u8; 64]);
    }

    Ok(buf)
}

/// Parse a data frame routing header (T4 WebSocket).
///
/// Format: `[4B name_len LE][name][2B partition LE][batch_data...]`
pub fn parse_data_frame(data: &[u8]) -> Result<(String, u16, &[u8]), AeonError> {
    if data.len() < 6 {
        return Err(AeonError::state("Data frame too short"));
    }

    let name_len = read_u32_le(&data[0..4]) as usize;
    if data.len() < 4 + name_len + 2 {
        return Err(AeonError::state("Data frame truncated"));
    }

    let name = std::str::from_utf8(&data[4..4 + name_len])
        .map_err(|e| AeonError::state(format!("Invalid pipeline name UTF-8: {e}")))?
        .to_string();

    let partition = read_u16_le(&data[4 + name_len..4 + name_len + 2]);

    let batch_data = &data[4 + name_len + 2..];
    Ok((name, partition, batch_data))
}

/// Build a data frame with routing header (T4 WebSocket response).
///
/// Format: `[4B name_len LE][name][2B partition LE][response_data]`
pub fn build_data_frame(pipeline: &str, partition: u16, response_data: &[u8]) -> Vec<u8> {
    let name_bytes = pipeline.as_bytes();
    let mut frame = Vec::with_capacity(4 + name_bytes.len() + 2 + response_data.len());
    frame.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
    frame.extend_from_slice(name_bytes);
    frame.extend_from_slice(&partition.to_le_bytes());
    frame.extend_from_slice(response_data);
    frame
}

/// Parse a control message type from JSON.
///
/// Returns the "type" field value for routing to the appropriate handler.
pub fn parse_control_type(json: &str) -> Result<String, AeonError> {
    #[derive(Deserialize)]
    struct TypeOnly {
        #[serde(rename = "type")]
        msg_type: String,
    }
    let parsed: TypeOnly = serde_json::from_str(json)
        .map_err(|e| AeonError::state(format!("Invalid control message JSON: {e}")))?;
    Ok(parsed.msg_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_now() {
        let hb = Heartbeat::now();
        assert_eq!(hb.msg_type, "heartbeat");
        assert!(hb.timestamp_ms > 0);

        let json = serde_json::to_string(&hb).unwrap();
        assert!(json.contains("\"type\":\"heartbeat\""));
    }

    #[test]
    fn data_frame_roundtrip() {
        let pipeline = "test-pipeline";
        let partition = 42u16;
        let batch_data = b"batch_payload_here";

        let frame = build_data_frame(pipeline, partition, batch_data);
        let (p, part, data) = parse_data_frame(&frame).unwrap();

        assert_eq!(p, "test-pipeline");
        assert_eq!(part, 42);
        assert_eq!(data, batch_data);
    }

    #[test]
    fn data_frame_too_short() {
        assert!(parse_data_frame(&[0, 0, 0]).is_err());
    }

    #[test]
    fn batch_response_encode_decode_roundtrip() {
        let sk = crate::auth::generate_keypair();
        let outputs = vec![
            vec![ProcessOutput {
                destination: "out".into(),
                key: None,
                payload: b"hello".to_vec(),
                headers: vec![],
            }],
            vec![], // event with no outputs
        ];

        let encoded = encode_batch_response(42, &outputs, "msgpack", &sk, true).unwrap();

        // Verify structure: 8 + 4 + events + 4 (CRC) + 64 (sig) >= 80 bytes
        assert!(encoded.len() >= 80);

        // Verify batch_id.
        let batch_id = u64::from_le_bytes(encoded[0..8].try_into().unwrap());
        assert_eq!(batch_id, 42);

        // Verify event count.
        let count = u32::from_le_bytes(encoded[8..12].try_into().unwrap());
        assert_eq!(count, 2);

        // Verify CRC32.
        let crc_offset = encoded.len() - 68; // 64 sig + 4 CRC from end
        let stored_crc =
            u32::from_le_bytes(encoded[crc_offset..crc_offset + 4].try_into().unwrap());
        let computed_crc = crc32fast::hash(&encoded[..crc_offset]);
        assert_eq!(stored_crc, computed_crc);
    }

    #[test]
    fn batch_request_crc_validation() {
        // Build a minimal valid batch request: batch_id=1, count=0, CRC32.
        let mut data = Vec::new();
        data.extend_from_slice(&1u64.to_le_bytes()); // batch_id
        data.extend_from_slice(&0u32.to_le_bytes()); // count = 0
        let crc = crc32fast::hash(&data);
        data.extend_from_slice(&crc.to_le_bytes());

        let (batch_id, events) = decode_batch_request(&data, "msgpack").unwrap();
        assert_eq!(batch_id, 1);
        assert!(events.is_empty());
    }

    #[test]
    fn batch_request_bad_crc() {
        let mut data = Vec::new();
        data.extend_from_slice(&1u64.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0xDEADBEEFu32.to_le_bytes()); // wrong CRC

        assert!(decode_batch_request(&data, "msgpack").is_err());
    }

    #[test]
    fn parse_control_type_works() {
        assert_eq!(
            parse_control_type(r#"{"type":"challenge","nonce":"abc"}"#).unwrap(),
            "challenge"
        );
        assert_eq!(
            parse_control_type(r#"{"type":"heartbeat","timestamp_ms":123}"#).unwrap(),
            "heartbeat"
        );
    }

    #[test]
    fn parse_control_type_invalid_json() {
        assert!(parse_control_type("not json").is_err());
    }

    #[test]
    fn batch_encode_json_codec() {
        let sk = crate::auth::generate_keypair();
        let outputs = vec![vec![ProcessOutput {
            destination: "sink".into(),
            key: Some(b"k".to_vec()),
            payload: b"data".to_vec(),
            headers: vec![("h".into(), "v".into())],
        }]];

        let encoded = encode_batch_response(1, &outputs, "json", &sk, false).unwrap();
        assert!(encoded.len() >= 80);
    }

    #[test]
    fn batch_encode_no_signing() {
        let sk = crate::auth::generate_keypair();
        let outputs = vec![vec![]];

        let encoded = encode_batch_response(1, &outputs, "msgpack", &sk, false).unwrap();

        // Last 64 bytes should be zeros (no signing).
        let sig = &encoded[encoded.len() - 64..];
        assert!(sig.iter().all(|&b| b == 0));
    }
}
