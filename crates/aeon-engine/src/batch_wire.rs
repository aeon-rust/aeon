//! Batch wire framing for T3/T4 network processor transports.
//!
//! Defines the binary framing for batch requests and responses exchanged
//! between the Aeon engine and out-of-process processors over WebTransport
//! (T3) or WebSocket (T4).
//!
//! Wire format (request):
//!   `[8B batch_id][4B event_count][per event: 4B len + bytes][4B CRC32]`
//!
//! Wire format (response):
//!   `[8B batch_id][4B event_count][per event: 4B output_count, per output: 4B len + bytes][4B CRC32][64B signature]`

use aeon_types::error::AeonError;
use aeon_types::event::{Event, Output};
use aeon_types::transport_codec::TransportCodec;

// ── Request ──────────────────────────────────────────────────────────────

/// Serialize a batch request into the wire format.
///
/// Each event is passed as its pre-serialized bytes (the caller is
/// responsible for serializing `Event` → `&[u8]` via bincode or similar).
pub fn serialize_batch_request(batch_id: u64, events: &[&[u8]]) -> Vec<u8> {
    // Header: 8 (batch_id) + 4 (event_count) = 12
    // Per event: 4 (len) + data
    // Footer: 4 (CRC32)
    let data_size: usize = events.iter().map(|e| 4 + e.len()).sum();
    let total = 12 + data_size + 4;
    let mut buf = Vec::with_capacity(total);

    buf.extend_from_slice(&batch_id.to_le_bytes());
    buf.extend_from_slice(&(events.len() as u32).to_le_bytes());

    for event in events {
        buf.extend_from_slice(&(event.len() as u32).to_le_bytes());
        buf.extend_from_slice(event);
    }

    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());

    buf
}

/// Parsed batch request.
#[derive(Debug)]
pub struct BatchRequest<'a> {
    /// Batch identifier (monotonic per pipeline).
    pub batch_id: u64,
    /// Serialized event payloads (zero-copy slices into the input buffer).
    pub events: Vec<&'a [u8]>,
}

/// Deserialize a batch request from wire bytes.
///
/// Returns zero-copy slices into the input buffer for each event.
pub fn deserialize_batch_request(data: &[u8]) -> Result<BatchRequest<'_>, AeonError> {
    if data.len() < 16 {
        return Err(AeonError::serialization("batch request too short"));
    }

    // Verify CRC32 (last 4 bytes)
    let payload = &data[..data.len() - 4];
    let expected_crc = u32::from_le_bytes(
        data[data.len() - 4..]
            .try_into()
            .map_err(|_| AeonError::serialization("invalid CRC32 bytes"))?,
    );
    let actual_crc = crc32fast::hash(payload);
    if expected_crc != actual_crc {
        return Err(AeonError::serialization(format!(
            "CRC32 mismatch: expected {expected_crc:#x}, got {actual_crc:#x}"
        )));
    }

    let batch_id = u64::from_le_bytes(
        data[0..8]
            .try_into()
            .map_err(|_| AeonError::serialization("invalid batch_id"))?,
    );
    let event_count = u32::from_le_bytes(
        data[8..12]
            .try_into()
            .map_err(|_| AeonError::serialization("invalid event_count"))?,
    ) as usize;

    let mut offset = 12;
    let mut events = Vec::with_capacity(event_count);

    for _ in 0..event_count {
        if offset + 4 > payload.len() {
            return Err(AeonError::serialization("unexpected end of batch request"));
        }
        let len = u32::from_le_bytes(
            data[offset..offset + 4]
                .try_into()
                .map_err(|_| AeonError::serialization("invalid event length"))?,
        ) as usize;
        offset += 4;

        if offset + len > payload.len() {
            return Err(AeonError::serialization("event data exceeds buffer"));
        }
        events.push(&data[offset..offset + len]);
        offset += len;
    }

    Ok(BatchRequest { batch_id, events })
}

// ── Response ─────────────────────────────────────────────────────────────

/// Serialize a batch response into the wire format.
///
/// `outputs_per_event` is a nested slice: for each input event, a list of
/// output payloads (pre-serialized). `signature` is the ED25519 signature
/// (64 bytes); pass `&[0u8; 64]` if unsigned.
pub fn serialize_batch_response(
    batch_id: u64,
    outputs_per_event: &[Vec<&[u8]>],
    signature: &[u8; 64],
) -> Vec<u8> {
    // Estimate size: header(12) + per-event(4 + per-output(4 + data)) + CRC(4) + sig(64)
    let data_size: usize = outputs_per_event
        .iter()
        .map(|outs| 4 + outs.iter().map(|o| 4 + o.len()).sum::<usize>())
        .sum();
    let total = 12 + data_size + 4 + 64;
    let mut buf = Vec::with_capacity(total);

    buf.extend_from_slice(&batch_id.to_le_bytes());
    buf.extend_from_slice(&(outputs_per_event.len() as u32).to_le_bytes());

    for outputs in outputs_per_event {
        buf.extend_from_slice(&(outputs.len() as u32).to_le_bytes());
        for output in outputs {
            buf.extend_from_slice(&(output.len() as u32).to_le_bytes());
            buf.extend_from_slice(output);
        }
    }

    let crc = crc32fast::hash(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(signature);

    buf
}

/// Parsed batch response.
#[derive(Debug)]
pub struct BatchResponse<'a> {
    /// Batch identifier (matches the request).
    pub batch_id: u64,
    /// Per-input-event output payloads (zero-copy slices).
    pub outputs_per_event: Vec<Vec<&'a [u8]>>,
    /// ED25519 signature (64 bytes).
    pub signature: &'a [u8],
}

/// Deserialize a batch response from wire bytes.
pub fn deserialize_batch_response(data: &[u8]) -> Result<BatchResponse<'_>, AeonError> {
    // Minimum: header(12) + CRC(4) + signature(64) = 80
    if data.len() < 80 {
        return Err(AeonError::serialization("batch response too short"));
    }

    let signature = &data[data.len() - 64..];
    let crc_offset = data.len() - 64 - 4;
    let payload = &data[..crc_offset];

    let expected_crc = u32::from_le_bytes(
        data[crc_offset..crc_offset + 4]
            .try_into()
            .map_err(|_| AeonError::serialization("invalid CRC32 bytes"))?,
    );
    let actual_crc = crc32fast::hash(payload);
    if expected_crc != actual_crc {
        return Err(AeonError::serialization(format!(
            "CRC32 mismatch: expected {expected_crc:#x}, got {actual_crc:#x}"
        )));
    }

    let batch_id = u64::from_le_bytes(
        data[0..8]
            .try_into()
            .map_err(|_| AeonError::serialization("invalid batch_id"))?,
    );
    let event_count = u32::from_le_bytes(
        data[8..12]
            .try_into()
            .map_err(|_| AeonError::serialization("invalid event_count"))?,
    ) as usize;

    let mut offset = 12;
    let mut outputs_per_event = Vec::with_capacity(event_count);

    for _ in 0..event_count {
        if offset + 4 > payload.len() {
            return Err(AeonError::serialization("unexpected end of batch response"));
        }
        let output_count = u32::from_le_bytes(
            data[offset..offset + 4]
                .try_into()
                .map_err(|_| AeonError::serialization("invalid output_count"))?,
        ) as usize;
        offset += 4;

        let mut outputs = Vec::with_capacity(output_count);
        for _ in 0..output_count {
            if offset + 4 > payload.len() {
                return Err(AeonError::serialization("unexpected end of output data"));
            }
            let len = u32::from_le_bytes(
                data[offset..offset + 4]
                    .try_into()
                    .map_err(|_| AeonError::serialization("invalid output length"))?,
            ) as usize;
            offset += 4;

            if offset + len > payload.len() {
                return Err(AeonError::serialization("output data exceeds buffer"));
            }
            outputs.push(&data[offset..offset + len]);
            offset += len;
        }
        outputs_per_event.push(outputs);
    }

    Ok(BatchResponse {
        batch_id,
        outputs_per_event,
        signature,
    })
}

// ── Codec-aware helpers ─────────────────────────────────────────────────

/// Serialize a batch of Events into a wire-format request using the given codec.
///
/// Encodes each event via the codec, then wraps in the batch frame with CRC32.
pub fn encode_batch_request(
    batch_id: u64,
    events: &[Event],
    codec: TransportCodec,
) -> Result<Vec<u8>, AeonError> {
    let encoded = codec.encode_events(events)?;
    let slices: Vec<&[u8]> = encoded.iter().map(|e| e.as_slice()).collect();
    Ok(serialize_batch_request(batch_id, &slices))
}

/// Deserialize a wire-format batch request into Events using the given codec.
pub fn decode_batch_request(
    data: &[u8],
    codec: TransportCodec,
) -> Result<(u64, Vec<Event>), AeonError> {
    let parsed = deserialize_batch_request(data)?;
    let events = codec.decode_events(&parsed.events)?;
    Ok((parsed.batch_id, events))
}

/// Serialize a batch of Outputs (grouped per input event) into a wire-format
/// response using the given codec.
pub fn encode_batch_response(
    batch_id: u64,
    outputs_per_event: &[Vec<Output>],
    signature: &[u8; 64],
    codec: TransportCodec,
) -> Result<Vec<u8>, AeonError> {
    let mut encoded_groups: Vec<Vec<Vec<u8>>> = Vec::with_capacity(outputs_per_event.len());
    for group in outputs_per_event {
        encoded_groups.push(codec.encode_outputs(group)?);
    }
    let wire_groups: Vec<Vec<&[u8]>> = encoded_groups
        .iter()
        .map(|g| g.iter().map(|o| o.as_slice()).collect())
        .collect();
    Ok(serialize_batch_response(batch_id, &wire_groups, signature))
}

/// Decoded batch response with typed Outputs.
pub struct DecodedBatchResponse {
    /// Batch identifier.
    pub batch_id: u64,
    /// Per-input-event decoded outputs.
    pub outputs_per_event: Vec<Vec<Output>>,
    /// ED25519 signature bytes.
    pub signature: Vec<u8>,
}

/// Deserialize a wire-format batch response into Outputs using the given codec.
pub fn decode_batch_response(
    data: &[u8],
    codec: TransportCodec,
) -> Result<DecodedBatchResponse, AeonError> {
    let parsed = deserialize_batch_response(data)?;
    let mut all_outputs = Vec::with_capacity(parsed.outputs_per_event.len());
    for group in &parsed.outputs_per_event {
        all_outputs.push(codec.decode_outputs(group)?);
    }
    Ok(DecodedBatchResponse {
        batch_id: parsed.batch_id,
        outputs_per_event: all_outputs,
        signature: parsed.signature.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let events: Vec<&[u8]> = vec![b"event-0", b"event-1", b"event-2"];
        let wire = serialize_batch_request(42, &events);
        let parsed = deserialize_batch_request(&wire).unwrap();

        assert_eq!(parsed.batch_id, 42);
        assert_eq!(parsed.events.len(), 3);
        assert_eq!(parsed.events[0], b"event-0");
        assert_eq!(parsed.events[1], b"event-1");
        assert_eq!(parsed.events[2], b"event-2");
    }

    #[test]
    fn request_empty_batch() {
        let wire = serialize_batch_request(0, &[]);
        let parsed = deserialize_batch_request(&wire).unwrap();
        assert_eq!(parsed.batch_id, 0);
        assert!(parsed.events.is_empty());
    }

    #[test]
    fn request_crc_corruption() {
        let events: Vec<&[u8]> = vec![b"data"];
        let mut wire = serialize_batch_request(1, &events);
        // Corrupt the last byte (CRC)
        let last = wire.len() - 1;
        wire[last] ^= 0xFF;
        let result = deserialize_batch_request(&wire);
        assert!(result.is_err());
    }

    #[test]
    fn request_too_short() {
        let result = deserialize_batch_request(&[0u8; 10]);
        assert!(result.is_err());
    }

    #[test]
    fn response_roundtrip() {
        let outputs: Vec<Vec<&[u8]>> = vec![
            vec![b"out-0a", b"out-0b"],
            vec![b"out-1a"],
            vec![], // event produced no outputs
        ];
        let sig = [0xABu8; 64];
        let wire = serialize_batch_response(99, &outputs, &sig);
        let parsed = deserialize_batch_response(&wire).unwrap();

        assert_eq!(parsed.batch_id, 99);
        assert_eq!(parsed.outputs_per_event.len(), 3);
        assert_eq!(parsed.outputs_per_event[0].len(), 2);
        assert_eq!(parsed.outputs_per_event[0][0], b"out-0a");
        assert_eq!(parsed.outputs_per_event[0][1], b"out-0b");
        assert_eq!(parsed.outputs_per_event[1].len(), 1);
        assert_eq!(parsed.outputs_per_event[1][0], b"out-1a");
        assert!(parsed.outputs_per_event[2].is_empty());
        assert_eq!(parsed.signature, &[0xABu8; 64]);
    }

    #[test]
    fn response_empty_batch() {
        let sig = [0u8; 64];
        let wire = serialize_batch_response(0, &[], &sig);
        let parsed = deserialize_batch_response(&wire).unwrap();
        assert_eq!(parsed.batch_id, 0);
        assert!(parsed.outputs_per_event.is_empty());
        assert_eq!(parsed.signature, &[0u8; 64]);
    }

    #[test]
    fn response_crc_corruption() {
        let outputs: Vec<Vec<&[u8]>> = vec![vec![b"x"]];
        let sig = [0u8; 64];
        let mut wire = serialize_batch_response(1, &outputs, &sig);
        // Corrupt a CRC byte (at len - 64 - 4 + 1)
        let crc_pos = wire.len() - 64 - 4;
        wire[crc_pos] ^= 0xFF;
        let result = deserialize_batch_response(&wire);
        assert!(result.is_err());
    }

    #[test]
    fn response_too_short() {
        let result = deserialize_batch_response(&[0u8; 50]);
        assert!(result.is_err());
    }

    // ── Codec-aware helper tests ────────────────────────────────────────

    use aeon_types::partition::PartitionId;
    use bytes::Bytes;
    use std::sync::Arc;

    fn make_test_event(id: u8) -> Event {
        Event::new(
            uuid::Uuid::from_bytes([id; 16]),
            1712345678000000000,
            Arc::from("test-source"),
            PartitionId::new(0),
            Bytes::from(format!("payload-{id}")),
        )
    }

    fn make_test_output(id: u8) -> Output {
        Output::new(Arc::from("test-dest"), Bytes::from(format!("output-{id}")))
            .with_source_event_id(uuid::Uuid::from_bytes([id; 16]))
    }

    #[test]
    fn encode_decode_batch_request_msgpack() {
        let events = vec![make_test_event(1), make_test_event(2)];
        let wire = encode_batch_request(42, &events, TransportCodec::MsgPack).unwrap();
        let (batch_id, decoded) = decode_batch_request(&wire, TransportCodec::MsgPack).unwrap();

        assert_eq!(batch_id, 42);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].id, events[0].id);
        assert_eq!(decoded[1].id, events[1].id);
        assert_eq!(decoded[0].payload, events[0].payload);
    }

    #[test]
    fn encode_decode_batch_request_json() {
        let events = vec![make_test_event(3)];
        let wire = encode_batch_request(7, &events, TransportCodec::Json).unwrap();
        let (batch_id, decoded) = decode_batch_request(&wire, TransportCodec::Json).unwrap();

        assert_eq!(batch_id, 7);
        assert_eq!(decoded.len(), 1);
        assert_eq!(&*decoded[0].source, "test-source");
    }

    #[test]
    fn encode_decode_batch_response_msgpack() {
        let outputs = vec![
            vec![make_test_output(1), make_test_output(2)],
            vec![make_test_output(3)],
        ];
        let sig = [0xAAu8; 64];
        let wire = encode_batch_response(99, &outputs, &sig, TransportCodec::MsgPack).unwrap();
        let resp = decode_batch_response(&wire, TransportCodec::MsgPack).unwrap();

        assert_eq!(resp.batch_id, 99);
        assert_eq!(resp.outputs_per_event.len(), 2);
        assert_eq!(resp.outputs_per_event[0].len(), 2);
        assert_eq!(resp.outputs_per_event[1].len(), 1);
        assert_eq!(&*resp.outputs_per_event[0][0].destination, "test-dest");
        assert_eq!(resp.signature, sig.to_vec());
    }

    #[test]
    fn encode_decode_empty_batch_with_codec() {
        let events: Vec<Event> = vec![];
        let wire = encode_batch_request(0, &events, TransportCodec::MsgPack).unwrap();
        let (batch_id, decoded) = decode_batch_request(&wire, TransportCodec::MsgPack).unwrap();
        assert_eq!(batch_id, 0);
        assert!(decoded.is_empty());
    }

    #[test]
    fn cross_codec_request_fails() {
        let events = vec![make_test_event(1)];
        let wire = encode_batch_request(1, &events, TransportCodec::MsgPack).unwrap();
        // Try decoding MsgPack-encoded events with JSON codec
        let result = decode_batch_request(&wire, TransportCodec::Json);
        assert!(result.is_err());
    }
}
