//! Binary wire format for native `.so` processor communication.
//!
//! This format is intentionally simple and matches the Wasm ABI wire format
//! used in `aeon-wasm`. All multi-byte integers are little-endian.
//!
//! ## Event wire format
//!
//! ```text
//! [16 bytes]  UUID
//! [8 bytes]   timestamp (i64 LE)
//! [4 bytes]   source_len (u32 LE)
//! [N bytes]   source (UTF-8)
//! [2 bytes]   partition (u16 LE)
//! [4 bytes]   metadata_count (u32 LE)
//! for each metadata:
//!   [4 bytes]  key_len (u32 LE)
//!   [N bytes]  key (UTF-8)
//!   [4 bytes]  val_len (u32 LE)
//!   [N bytes]  val (UTF-8)
//! [4 bytes]   payload_len (u32 LE)
//! [N bytes]   payload
//! ```
//!
//! ## Output wire format
//!
//! ```text
//! [4 bytes]   output_count (u32 LE)
//! for each output:
//!   [4 bytes]  dest_len (u32 LE)
//!   [N bytes]  destination (UTF-8)
//!   [1 byte]   has_key (0 or 1)
//!   if has_key:
//!     [4 bytes] key_len (u32 LE)
//!     [N bytes] key
//!   [4 bytes]  payload_len (u32 LE)
//!   [N bytes]  payload
//!   [4 bytes]  header_count (u32 LE)
//!   for each header:
//!     [4 bytes] key_len (u32 LE)
//!     [N bytes] key (UTF-8)
//!     [4 bytes] val_len (u32 LE)
//!     [N bytes] val (UTF-8)
//! ```
//!
//! ## Events batch wire format
//!
//! ```text
//! [4 bytes]   event_count (u32 LE)
//! for each event:
//!   [4 bytes]  event_len (u32 LE)
//!   [N bytes]  event (above format)
//! ```

use aeon_types::AeonError;
use aeon_types::event::{Event, Output};
use aeon_types::partition::PartitionId;
use bytes::Bytes;
use smallvec::SmallVec;
use std::sync::Arc;

// ─── Helpers ─────────────────────────────────────────────────────

fn read_u16(data: &[u8], offset: &mut usize) -> Result<u16, AeonError> {
    if *offset + 2 > data.len() {
        return Err(wire_error("unexpected end of data reading u16"));
    }
    let val = u16::from_le_bytes([data[*offset], data[*offset + 1]]);
    *offset += 2;
    Ok(val)
}

fn read_u32(data: &[u8], offset: &mut usize) -> Result<u32, AeonError> {
    if *offset + 4 > data.len() {
        return Err(wire_error("unexpected end of data reading u32"));
    }
    let val = u32::from_le_bytes([
        data[*offset],
        data[*offset + 1],
        data[*offset + 2],
        data[*offset + 3],
    ]);
    *offset += 4;
    Ok(val)
}

fn read_i64(data: &[u8], offset: &mut usize) -> Result<i64, AeonError> {
    if *offset + 8 > data.len() {
        return Err(wire_error("unexpected end of data reading i64"));
    }
    let val = i64::from_le_bytes([
        data[*offset],
        data[*offset + 1],
        data[*offset + 2],
        data[*offset + 3],
        data[*offset + 4],
        data[*offset + 5],
        data[*offset + 6],
        data[*offset + 7],
    ]);
    *offset += 8;
    Ok(val)
}

fn read_bytes<'a>(data: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], AeonError> {
    if *offset + len > data.len() {
        return Err(wire_error("unexpected end of data reading bytes"));
    }
    let slice = &data[*offset..*offset + len];
    *offset += len;
    Ok(slice)
}

fn read_string(data: &[u8], offset: &mut usize) -> Result<Arc<str>, AeonError> {
    let len = read_u32(data, offset)? as usize;
    let bytes = read_bytes(data, offset, len)?;
    let s = std::str::from_utf8(bytes).map_err(|e| wire_error(&format!("invalid UTF-8: {e}")))?;
    Ok(Arc::from(s))
}

fn wire_error(msg: &str) -> AeonError {
    AeonError::Serialization {
        message: format!("native wire format: {msg}"),
        source: None,
    }
}

// ─── Event serialization ─────────────────────────────────────────

/// Serialize a single event to wire format bytes.
pub fn serialize_event(event: &Event) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);

    // UUID (16 bytes)
    buf.extend_from_slice(event.id.as_bytes());
    // Timestamp (8 bytes, i64 LE)
    buf.extend_from_slice(&event.timestamp.to_le_bytes());
    // Source
    let src = event.source.as_bytes();
    buf.extend_from_slice(&(src.len() as u32).to_le_bytes());
    buf.extend_from_slice(src);
    // Partition
    buf.extend_from_slice(&event.partition.as_u16().to_le_bytes());
    // Metadata
    buf.extend_from_slice(&(event.metadata.len() as u32).to_le_bytes());
    for (k, v) in event.metadata.iter() {
        let kb = k.as_bytes();
        let vb = v.as_bytes();
        buf.extend_from_slice(&(kb.len() as u32).to_le_bytes());
        buf.extend_from_slice(kb);
        buf.extend_from_slice(&(vb.len() as u32).to_le_bytes());
        buf.extend_from_slice(vb);
    }
    // Payload
    buf.extend_from_slice(&(event.payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&event.payload);

    buf
}

/// Deserialize a single event from wire format bytes.
pub fn deserialize_event(data: &[u8]) -> Result<Event, AeonError> {
    let mut offset = 0;

    // UUID
    let uuid_bytes = read_bytes(data, &mut offset, 16)?;
    let id = uuid::Uuid::from_bytes(
        uuid_bytes
            .try_into()
            .map_err(|_| wire_error("invalid UUID bytes"))?,
    );

    // Timestamp
    let timestamp = read_i64(data, &mut offset)?;

    // Source
    let source = read_string(data, &mut offset)?;

    // Partition
    let partition = PartitionId::new(read_u16(data, &mut offset)?);

    // Metadata
    let meta_count = read_u32(data, &mut offset)? as usize;
    let mut metadata = SmallVec::with_capacity(meta_count);
    for _ in 0..meta_count {
        let key = read_string(data, &mut offset)?;
        let val = read_string(data, &mut offset)?;
        metadata.push((key, val));
    }

    // Payload
    let payload_len = read_u32(data, &mut offset)? as usize;
    let payload_bytes = read_bytes(data, &mut offset, payload_len)?;
    let payload = Bytes::copy_from_slice(payload_bytes);

    let mut event = Event::new(id, timestamp, source, partition, payload);
    event.metadata = metadata;
    Ok(event)
}

/// Serialize a batch of events: [4-byte count] + [4-byte len + event bytes]...
pub fn serialize_events(events: &[Event]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(events.len() * 128);
    buf.extend_from_slice(&(events.len() as u32).to_le_bytes());
    for event in events {
        let event_bytes = serialize_event(event);
        buf.extend_from_slice(&(event_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&event_bytes);
    }
    buf
}

/// Deserialize a batch of events.
pub fn deserialize_events(data: &[u8]) -> Result<Vec<Event>, AeonError> {
    let mut offset = 0;
    let count = read_u32(data, &mut offset)? as usize;
    let mut events = Vec::with_capacity(count);
    for _ in 0..count {
        let event_len = read_u32(data, &mut offset)? as usize;
        let event_bytes = read_bytes(data, &mut offset, event_len)?;
        events.push(deserialize_event(event_bytes)?);
    }
    Ok(events)
}

// ─── Output serialization ────────────────────────────────────────

/// Serialize outputs to wire format.
pub fn serialize_outputs(outputs: &[Output]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(outputs.len() * 128);
    buf.extend_from_slice(&(outputs.len() as u32).to_le_bytes());

    for output in outputs {
        // Destination
        let dest = output.destination.as_bytes();
        buf.extend_from_slice(&(dest.len() as u32).to_le_bytes());
        buf.extend_from_slice(dest);

        // Key (optional)
        match &output.key {
            Some(key) => {
                buf.push(1);
                buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
                buf.extend_from_slice(key);
            }
            None => {
                buf.push(0);
            }
        }

        // Payload
        buf.extend_from_slice(&(output.payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&output.payload);

        // Headers
        buf.extend_from_slice(&(output.headers.len() as u32).to_le_bytes());
        for (k, v) in output.headers.iter() {
            let kb = k.as_bytes();
            let vb = v.as_bytes();
            buf.extend_from_slice(&(kb.len() as u32).to_le_bytes());
            buf.extend_from_slice(kb);
            buf.extend_from_slice(&(vb.len() as u32).to_le_bytes());
            buf.extend_from_slice(vb);
        }
    }

    buf
}

/// Deserialize outputs from wire format.
pub fn deserialize_outputs(data: &[u8]) -> Result<Vec<Output>, AeonError> {
    let mut offset = 0;
    let count = read_u32(data, &mut offset)? as usize;
    let mut outputs = Vec::with_capacity(count);

    for _ in 0..count {
        // Destination
        let destination = read_string(data, &mut offset)?;

        // Key
        let has_key = read_bytes(data, &mut offset, 1)?[0];
        let key = if has_key == 1 {
            let key_len = read_u32(data, &mut offset)? as usize;
            let key_bytes = read_bytes(data, &mut offset, key_len)?;
            Some(Bytes::copy_from_slice(key_bytes))
        } else {
            None
        };

        // Payload
        let payload_len = read_u32(data, &mut offset)? as usize;
        let payload_bytes = read_bytes(data, &mut offset, payload_len)?;
        let payload = Bytes::copy_from_slice(payload_bytes);

        // Headers
        let header_count = read_u32(data, &mut offset)? as usize;
        let mut headers = SmallVec::with_capacity(header_count);
        for _ in 0..header_count {
            let key = read_string(data, &mut offset)?;
            let val = read_string(data, &mut offset)?;
            headers.push((key, val));
        }

        let mut output = Output::new(destination, payload);
        if let Some(k) = key {
            output = output.with_key(k);
        }
        output.headers = headers;
        outputs.push(output);
    }

    Ok(outputs)
}
