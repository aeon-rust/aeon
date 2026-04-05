//! Binary wire format serialization/deserialization.
//!
//! Matches the exact format used by `aeon-wasm/src/processor.rs` on the host
//! side. This ensures zero-copy-compatible data exchange between the Aeon
//! engine and Wasm guest processors.
//!
//! ## Event format (host → guest)
//!
//! ```text
//! [16 bytes: UUID]
//! [8 bytes: timestamp i64 LE]
//! [4 bytes: source len][source bytes]
//! [2 bytes: partition u16 LE]
//! [4 bytes: metadata count]
//!   for each: [4 bytes: key len][key][4 bytes: val len][val]
//! [4 bytes: payload len][payload bytes]
//! ```
//!
//! ## Output list format (guest → host)
//!
//! ```text
//! [4 bytes: output count]
//! for each:
//!   [4 bytes: dest len][dest]
//!   [1 byte: has_key (0 or 1)]
//!     if has_key: [4 bytes: key len][key]
//!   [4 bytes: payload len][payload]
//!   [4 bytes: header count]
//!     for each: [4 bytes: key len][key][4 bytes: val len][val]
//! ```

use alloc::string::String;
use alloc::vec::Vec;

use crate::{Event, Output};

// ── Helpers ────────────────────────────────────────────────────────────

fn read_u16_le(data: &[u8], pos: usize) -> Option<u16> {
    if pos + 2 > data.len() {
        return None;
    }
    Some(u16::from_le_bytes([data[pos], data[pos + 1]]))
}

fn read_u32_le(data: &[u8], pos: usize) -> Option<u32> {
    if pos + 4 > data.len() {
        return None;
    }
    Some(u32::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
    ]))
}

fn read_i64_le(data: &[u8], pos: usize) -> Option<i64> {
    if pos + 8 > data.len() {
        return None;
    }
    Some(i64::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
        data[pos + 4],
        data[pos + 5],
        data[pos + 6],
        data[pos + 7],
    ]))
}

fn read_bytes(data: &[u8], pos: usize, len: usize) -> Option<&[u8]> {
    if pos + len > data.len() {
        return None;
    }
    Some(&data[pos..pos + len])
}

fn write_u32_le(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_le_bytes());
}

// ── Deserialize Event ──────────────────────────────────────────────────

/// Deserialize an Event from the binary wire format.
///
/// Returns `None` if the data is truncated or malformed.
pub fn deserialize_event(data: &[u8]) -> Option<Event> {
    let mut pos: usize = 0;

    // UUID (16 bytes)
    if pos + 16 > data.len() {
        return None;
    }
    let mut id = [0u8; 16];
    id.copy_from_slice(&data[pos..pos + 16]);
    pos += 16;

    // Timestamp (8 bytes LE)
    let timestamp = read_i64_le(data, pos)?;
    pos += 8;

    // Source string (length-prefixed)
    let src_len = read_u32_le(data, pos)? as usize;
    pos += 4;
    let src_bytes = read_bytes(data, pos, src_len)?;
    let source = String::from(core::str::from_utf8(src_bytes).unwrap_or(""));
    pos += src_len;

    // Partition (2 bytes LE)
    let partition = read_u16_le(data, pos)?;
    pos += 2;

    // Metadata
    let meta_count = read_u32_le(data, pos)? as usize;
    pos += 4;
    let mut metadata = Vec::with_capacity(meta_count);
    for _ in 0..meta_count {
        let kl = read_u32_le(data, pos)? as usize;
        pos += 4;
        let kb = read_bytes(data, pos, kl)?;
        pos += kl;
        let vl = read_u32_le(data, pos)? as usize;
        pos += 4;
        let vb = read_bytes(data, pos, vl)?;
        pos += vl;
        metadata.push((
            String::from(core::str::from_utf8(kb).unwrap_or("")),
            String::from(core::str::from_utf8(vb).unwrap_or("")),
        ));
    }

    // Payload (length-prefixed)
    let payload_len = read_u32_le(data, pos)? as usize;
    pos += 4;
    let payload_bytes = read_bytes(data, pos, payload_len)?;
    let payload = payload_bytes.to_vec();

    Some(Event {
        id,
        timestamp,
        source,
        partition,
        metadata,
        payload,
    })
}

// ── Serialize Outputs ──────────────────────────────────────────────────

/// Serialize a list of outputs to the binary wire format.
///
/// The result does NOT include the 4-byte length prefix — the caller
/// (the `aeon_processor!` macro) prepends that.
pub fn serialize_outputs(outputs: &[Output]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);

    // Output count
    write_u32_le(&mut buf, outputs.len() as u32);

    for out in outputs {
        // Destination (length-prefixed)
        let dest = out.destination.as_bytes();
        write_u32_le(&mut buf, dest.len() as u32);
        buf.extend_from_slice(dest);

        // Key (optional)
        match &out.key {
            Some(key) => {
                buf.push(1);
                write_u32_le(&mut buf, key.len() as u32);
                buf.extend_from_slice(key);
            }
            None => {
                buf.push(0);
            }
        }

        // Payload (length-prefixed)
        write_u32_le(&mut buf, out.payload.len() as u32);
        buf.extend_from_slice(&out.payload);

        // Headers
        write_u32_le(&mut buf, out.headers.len() as u32);
        for (k, v) in &out.headers {
            let kb = k.as_bytes();
            let vb = v.as_bytes();
            write_u32_le(&mut buf, kb.len() as u32);
            buf.extend_from_slice(kb);
            write_u32_le(&mut buf, vb.len() as u32);
            buf.extend_from_slice(vb);
        }
    }

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Build a minimal serialized event matching the host wire format.
    fn make_wire_event(
        id: [u8; 16],
        timestamp: i64,
        source: &str,
        partition: u16,
        metadata: &[(&str, &str)],
        payload: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::new();

        // UUID
        buf.extend_from_slice(&id);
        // Timestamp
        buf.extend_from_slice(&timestamp.to_le_bytes());
        // Source
        let sb = source.as_bytes();
        buf.extend_from_slice(&(sb.len() as u32).to_le_bytes());
        buf.extend_from_slice(sb);
        // Partition
        buf.extend_from_slice(&partition.to_le_bytes());
        // Metadata
        buf.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        for (k, v) in metadata {
            let kb = k.as_bytes();
            let vb = v.as_bytes();
            buf.extend_from_slice(&(kb.len() as u32).to_le_bytes());
            buf.extend_from_slice(kb);
            buf.extend_from_slice(&(vb.len() as u32).to_le_bytes());
            buf.extend_from_slice(vb);
        }
        // Payload
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(payload);

        buf
    }

    #[test]
    fn deserialize_minimal_event() {
        let wire = make_wire_event([1u8; 16], 999, "src", 7, &[], b"hello");
        let event = deserialize_event(&wire).unwrap();

        assert_eq!(event.id, [1u8; 16]);
        assert_eq!(event.timestamp, 999);
        assert_eq!(event.source, "src");
        assert_eq!(event.partition, 7);
        assert!(event.metadata.is_empty());
        assert_eq!(event.payload, b"hello");
    }

    #[test]
    fn deserialize_event_with_metadata() {
        let wire = make_wire_event(
            [0u8; 16],
            42,
            "test-source",
            3,
            &[("key1", "val1"), ("key2", "val2")],
            b"payload-data",
        );
        let event = deserialize_event(&wire).unwrap();

        assert_eq!(event.source, "test-source");
        assert_eq!(event.partition, 3);
        assert_eq!(event.metadata.len(), 2);
        assert_eq!(event.metadata[0].0, "key1");
        assert_eq!(event.metadata[0].1, "val1");
        assert_eq!(event.metadata[1].0, "key2");
        assert_eq!(event.metadata[1].1, "val2");
        assert_eq!(event.payload, b"payload-data");
    }

    #[test]
    fn deserialize_truncated_returns_none() {
        // Too short for UUID
        assert!(deserialize_event(&[0u8; 10]).is_none());
        // Has UUID but no timestamp
        assert!(deserialize_event(&[0u8; 20]).is_none());
        // Has UUID+timestamp but no source length
        assert!(deserialize_event(&[0u8; 24]).is_none());
    }

    #[test]
    fn serialize_empty_outputs() {
        let result = serialize_outputs(&[]);
        assert_eq!(result, &0u32.to_le_bytes());
    }

    #[test]
    fn serialize_single_output_no_key() {
        let output = Output::new("dest", b"data".to_vec());
        let result = serialize_outputs(&[output]);

        // Parse it back manually
        let mut pos = 0;
        let count = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap());
        pos += 4;
        assert_eq!(count, 1);

        // Destination
        let dest_len = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        assert_eq!(&result[pos..pos + dest_len], b"dest");
        pos += dest_len;

        // No key
        assert_eq!(result[pos], 0);
        pos += 1;

        // Payload
        let payload_len = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        assert_eq!(&result[pos..pos + payload_len], b"data");
        pos += payload_len;

        // Headers count
        let header_count = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap());
        assert_eq!(header_count, 0);
    }

    #[test]
    fn serialize_output_with_key_and_headers() {
        let output = Output::new("topic-out", b"payload".to_vec())
            .with_key_str("my-key")
            .with_header("h1", "v1");

        let result = serialize_outputs(&[output]);

        let mut pos = 0;
        // Count
        let count = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap());
        pos += 4;
        assert_eq!(count, 1);

        // Destination
        let dest_len = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        assert_eq!(&result[pos..pos + dest_len], b"topic-out");
        pos += dest_len;

        // Has key
        assert_eq!(result[pos], 1);
        pos += 1;
        let key_len = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        assert_eq!(&result[pos..pos + key_len], b"my-key");
        pos += key_len;

        // Payload
        let payload_len = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        assert_eq!(&result[pos..pos + payload_len], b"payload");
        pos += payload_len;

        // Headers
        let header_count = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap());
        pos += 4;
        assert_eq!(header_count, 1);
        let hk_len = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        assert_eq!(&result[pos..pos + hk_len], b"h1");
        pos += hk_len;
        let hv_len = u32::from_le_bytes(result[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        assert_eq!(&result[pos..pos + hv_len], b"v1");
    }

    #[test]
    fn serialize_multiple_outputs() {
        let outputs = vec![
            Output::new("a", b"1".to_vec()),
            Output::new("b", b"2".to_vec()),
            Output::new("c", b"3".to_vec()),
        ];
        let result = serialize_outputs(&outputs);

        let count = u32::from_le_bytes(result[0..4].try_into().unwrap());
        assert_eq!(count, 3);
    }

    #[test]
    fn roundtrip_event_through_host_format() {
        // Simulate what the host does: serialize an event, then the guest
        // deserializes it. We use make_wire_event to mimic host serialization.
        let id = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ];
        let wire = make_wire_event(
            id,
            1_700_000_000_000,
            "pipeline-1",
            42,
            &[("env", "prod")],
            b"{\"key\":\"value\"}",
        );
        let event = deserialize_event(&wire).unwrap();

        assert_eq!(event.id, id);
        assert_eq!(event.timestamp, 1_700_000_000_000);
        assert_eq!(event.source, "pipeline-1");
        assert_eq!(event.partition, 42);
        assert_eq!(event.metadata.len(), 1);
        assert_eq!(event.metadata[0].0, "env");
        assert_eq!(event.metadata[0].1, "prod");
        assert_eq!(event.payload_str(), Some("{\"key\":\"value\"}"));
    }

    #[test]
    fn output_builder_api() {
        let out = Output::from_str("dest", "hello world")
            .with_key(vec![1, 2, 3])
            .with_header("content-type", "text/plain")
            .with_header("x-source", "test");

        assert_eq!(out.destination, "dest");
        assert_eq!(out.key.as_ref().unwrap(), &[1, 2, 3]);
        assert_eq!(out.payload, b"hello world");
        assert_eq!(out.headers.len(), 2);
        assert_eq!(out.headers[0].0, "content-type");
        assert_eq!(out.headers[1].0, "x-source");
    }

    #[test]
    fn empty_payload_event() {
        let wire = make_wire_event([0u8; 16], 0, "", 0, &[], b"");
        let event = deserialize_event(&wire).unwrap();
        assert!(event.payload.is_empty());
        assert!(event.source.is_empty());
    }
}
