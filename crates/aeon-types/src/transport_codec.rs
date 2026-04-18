//! Transport codec for AWPP Event/Output envelope serialization.
//!
//! Serializes `Event` and `Output` envelopes for T3/T4 network transport.
//! Two codecs are supported: MessagePack (default, compact) and JSON (fallback,
//! debuggable). The codec only applies to the envelope — `Event.payload` passes
//! through as opaque bytes regardless of codec.
//!
//! **Scope**: T3 (WebTransport) and T4 (WebSocket) data streams only.
//! T1/T2 are in-process and never use this. Control stream always uses JSON.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AeonError;
use crate::event::{Event, Output};
use crate::partition::PartitionId;

/// Which codec to use for Event/Output envelope serialization on data streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportCodec {
    /// MessagePack — compact binary, ~200ns encode, mature cross-language support.
    #[default]
    MsgPack,
    /// JSON — human-readable text, ~800ns encode, universal language support.
    Json,
}

impl std::fmt::Display for TransportCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MsgPack => write!(f, "msgpack"),
            Self::Json => write!(f, "json"),
        }
    }
}

// ── Wire-format structs (serde-friendly mirrors of Event/Output) ────────────

/// Serde-friendly representation of `Event` for wire transport.
///
/// Converts `Arc<str>` → `String`, `Bytes` → `Vec<u8>`, `SmallVec` → `Vec`,
/// and drops `source_ts` (Instant is not meaningful over the network).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEvent {
    /// UUIDv7 event identifier.
    pub id: uuid::Uuid,
    /// Unix epoch nanoseconds.
    pub timestamp: i64,
    /// Source identifier.
    pub source: String,
    /// Partition ID.
    pub partition: PartitionId,
    /// Key-value metadata headers.
    pub metadata: Vec<(String, String)>,
    /// Payload bytes (opaque user data).
    ///
    /// FT-11: `bytes::Bytes` with `serde` feature — encoder serializes
    /// via `&[u8]` (zero-copy refcount borrow); decoder allocates a new
    /// `Bytes` from the byte stream. Replaces prior `Vec<u8>` which forced
    /// a full per-event copy on both encode and decode paths.
    pub payload: Bytes,
    /// Source-system offset for checkpoint resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_offset: Option<i64>,
    /// EO-2: L2 body-store sequence (push/poll sources under durability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l2_seq: Option<u64>,
}

/// Serde-friendly representation of `Output` for wire transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireOutput {
    /// Destination sink/topic name.
    pub destination: String,
    /// Optional partition key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Bytes>,
    /// Output payload bytes (FT-11: zero-copy on encode path).
    pub payload: Bytes,
    /// Key-value headers.
    pub headers: Vec<(String, String)>,
    /// Source event ID for delivery tracking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<uuid::Uuid>,
    /// Source partition for checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_partition: Option<PartitionId>,
    /// Source offset for checkpoint resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_offset: Option<i64>,
    /// EO-2: L2 body-store sequence propagated from the originating Event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l2_seq: Option<u64>,
}

// ── Conversions ─────────────────────────────────────────────────────────────

impl From<&Event> for WireEvent {
    fn from(event: &Event) -> Self {
        Self {
            id: event.id,
            timestamp: event.timestamp,
            source: event.source.to_string(),
            partition: event.partition,
            metadata: event
                .metadata
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            // FT-11: Bytes clone = refcount bump, not data copy.
            payload: event.payload.clone(),
            source_offset: event.source_offset,
            l2_seq: event.l2_seq,
        }
    }
}

impl From<WireEvent> for Event {
    fn from(wire: WireEvent) -> Self {
        use smallvec::SmallVec;

        let metadata: SmallVec<[(Arc<str>, Arc<str>); 4]> = wire
            .metadata
            .into_iter()
            .map(|(k, v)| (Arc::from(k.as_str()), Arc::from(v.as_str())))
            .collect();

        Event {
            id: wire.id,
            timestamp: wire.timestamp,
            source: Arc::from(wire.source.as_str()),
            partition: wire.partition,
            metadata,
            payload: wire.payload,
            source_ts: None, // Not meaningful over the wire
            source_offset: wire.source_offset,
            l2_seq: wire.l2_seq,
        }
    }
}

impl From<&Output> for WireOutput {
    fn from(output: &Output) -> Self {
        Self {
            destination: output.destination.to_string(),
            // FT-11: Bytes clone = refcount bump.
            key: output.key.clone(),
            payload: output.payload.clone(),
            headers: output
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            source_event_id: output.source_event_id,
            source_partition: output.source_partition,
            source_offset: output.source_offset,
            l2_seq: output.l2_seq,
        }
    }
}

impl From<WireOutput> for Output {
    fn from(wire: WireOutput) -> Self {
        use smallvec::SmallVec;

        let headers: SmallVec<[(Arc<str>, Arc<str>); 4]> = wire
            .headers
            .into_iter()
            .map(|(k, v)| (Arc::from(k.as_str()), Arc::from(v.as_str())))
            .collect();

        Output {
            destination: Arc::from(wire.destination.as_str()),
            key: wire.key,
            payload: wire.payload,
            headers,
            source_ts: None,
            source_event_id: wire.source_event_id,
            source_partition: wire.source_partition,
            source_offset: wire.source_offset,
            l2_seq: wire.l2_seq,
        }
    }
}

// ── Codec encode/decode ─────────────────────────────────────────────────────

impl TransportCodec {
    /// Encode a single Event envelope to bytes.
    pub fn encode_event(&self, event: &Event) -> Result<Vec<u8>, AeonError> {
        let wire = WireEvent::from(event);
        match self {
            Self::MsgPack => {
                rmp_serde::to_vec_named(&wire).map_err(|e| AeonError::serialization(e.to_string()))
            }
            Self::Json => {
                serde_json::to_vec(&wire).map_err(|e| AeonError::serialization(e.to_string()))
            }
        }
    }

    /// Decode a single Event envelope from bytes.
    pub fn decode_event(&self, data: &[u8]) -> Result<Event, AeonError> {
        let wire: WireEvent =
            match self {
                Self::MsgPack => rmp_serde::from_slice(data)
                    .map_err(|e| AeonError::serialization(e.to_string()))?,
                Self::Json => serde_json::from_slice(data)
                    .map_err(|e| AeonError::serialization(e.to_string()))?,
            };
        Ok(Event::from(wire))
    }

    /// Encode a batch of Events. Returns one Vec<u8> per event.
    pub fn encode_events(&self, events: &[Event]) -> Result<Vec<Vec<u8>>, AeonError> {
        events.iter().map(|e| self.encode_event(e)).collect()
    }

    /// Decode a batch of Events from per-event byte slices.
    pub fn decode_events(&self, data: &[&[u8]]) -> Result<Vec<Event>, AeonError> {
        data.iter().map(|d| self.decode_event(d)).collect()
    }

    /// Encode a single Output envelope to bytes.
    pub fn encode_output(&self, output: &Output) -> Result<Vec<u8>, AeonError> {
        let wire = WireOutput::from(output);
        match self {
            Self::MsgPack => {
                rmp_serde::to_vec_named(&wire).map_err(|e| AeonError::serialization(e.to_string()))
            }
            Self::Json => {
                serde_json::to_vec(&wire).map_err(|e| AeonError::serialization(e.to_string()))
            }
        }
    }

    /// Decode a single Output envelope from bytes.
    pub fn decode_output(&self, data: &[u8]) -> Result<Output, AeonError> {
        let wire: WireOutput =
            match self {
                Self::MsgPack => rmp_serde::from_slice(data)
                    .map_err(|e| AeonError::serialization(e.to_string()))?,
                Self::Json => serde_json::from_slice(data)
                    .map_err(|e| AeonError::serialization(e.to_string()))?,
            };
        Ok(Output::from(wire))
    }

    /// Encode a batch of Outputs. Returns one Vec<u8> per output.
    pub fn encode_outputs(&self, outputs: &[Output]) -> Result<Vec<Vec<u8>>, AeonError> {
        outputs.iter().map(|o| self.encode_output(o)).collect()
    }

    /// Decode a batch of Outputs from per-output byte slices.
    pub fn decode_outputs(&self, data: &[&[u8]]) -> Result<Vec<Output>, AeonError> {
        data.iter().map(|d| self.decode_output(d)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::Arc;

    fn make_event() -> Event {
        Event::new(
            uuid::Uuid::nil(),
            1712345678000000000,
            Arc::from("orders-topic"),
            PartitionId::new(3),
            Bytes::from_static(b"{\"order_id\": 42}"),
        )
        .with_metadata(Arc::from("trace-id"), Arc::from("abc123"))
        .with_source_offset(1000)
    }

    fn make_output() -> Output {
        Output::new(
            Arc::from("enriched-orders"),
            Bytes::from_static(b"{\"order_id\": 42, \"status\": \"enriched\"}"),
        )
        .with_key(Bytes::from_static(b"user-123"))
        .with_header(Arc::from("content-type"), Arc::from("application/json"))
        .with_source_event_id(uuid::Uuid::nil())
        .with_source_partition(PartitionId::new(3))
        .with_source_offset(1000)
    }

    #[test]
    fn transport_codec_default_is_msgpack() {
        assert_eq!(TransportCodec::default(), TransportCodec::MsgPack);
    }

    #[test]
    fn transport_codec_display() {
        assert_eq!(TransportCodec::MsgPack.to_string(), "msgpack");
        assert_eq!(TransportCodec::Json.to_string(), "json");
    }

    #[test]
    fn transport_codec_serde_roundtrip() {
        let json = serde_json::to_string(&TransportCodec::MsgPack).unwrap();
        assert_eq!(json, "\"msgpack\"");
        let back: TransportCodec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, TransportCodec::MsgPack);

        let json = serde_json::to_string(&TransportCodec::Json).unwrap();
        assert_eq!(json, "\"json\"");
    }

    #[test]
    fn wire_event_roundtrip_from_event() {
        let event = make_event();
        let wire = WireEvent::from(&event);
        assert_eq!(wire.id, event.id);
        assert_eq!(wire.timestamp, event.timestamp);
        assert_eq!(wire.source, "orders-topic");
        assert_eq!(wire.partition, PartitionId::new(3));
        assert_eq!(wire.metadata.len(), 1);
        assert_eq!(wire.payload.as_ref(), b"{\"order_id\": 42}");
        assert_eq!(wire.source_offset, Some(1000));

        let back = Event::from(wire);
        assert_eq!(back.id, event.id);
        assert_eq!(back.timestamp, event.timestamp);
        assert_eq!(&*back.source, "orders-topic");
        assert_eq!(back.partition, PartitionId::new(3));
        assert_eq!(back.metadata.len(), 1);
        assert_eq!(back.payload.as_ref(), b"{\"order_id\": 42}");
        assert_eq!(back.source_offset, Some(1000));
        assert!(back.source_ts.is_none()); // Instant doesn't survive wire
    }

    #[test]
    fn wire_output_roundtrip_from_output() {
        let output = make_output();
        let wire = WireOutput::from(&output);
        assert_eq!(wire.destination, "enriched-orders");
        assert_eq!(wire.key.as_deref(), Some(b"user-123".as_slice()));
        assert_eq!(wire.headers.len(), 1);
        assert_eq!(wire.source_event_id, Some(uuid::Uuid::nil()));
        assert_eq!(wire.source_partition, Some(PartitionId::new(3)));
        assert_eq!(wire.source_offset, Some(1000));

        let back = Output::from(wire);
        assert_eq!(&*back.destination, "enriched-orders");
        assert_eq!(
            back.key.as_ref().map(|k| k.as_ref()),
            Some(b"user-123".as_ref())
        );
        assert_eq!(back.headers.len(), 1);
        assert_eq!(back.source_event_id, Some(uuid::Uuid::nil()));
    }

    #[test]
    fn msgpack_event_encode_decode() {
        let codec = TransportCodec::MsgPack;
        let event = make_event();
        let encoded = codec.encode_event(&event).unwrap();
        let decoded = codec.decode_event(&encoded).unwrap();

        assert_eq!(decoded.id, event.id);
        assert_eq!(decoded.timestamp, event.timestamp);
        assert_eq!(&*decoded.source, &*event.source);
        assert_eq!(decoded.partition, event.partition);
        assert_eq!(decoded.payload, event.payload);
        assert_eq!(decoded.metadata.len(), event.metadata.len());
        assert_eq!(decoded.source_offset, event.source_offset);
    }

    #[test]
    fn json_event_encode_decode() {
        let codec = TransportCodec::Json;
        let event = make_event();
        let encoded = codec.encode_event(&event).unwrap();

        // JSON should be human-readable
        let text = std::str::from_utf8(&encoded).unwrap();
        assert!(text.contains("orders-topic"));

        let decoded = codec.decode_event(&encoded).unwrap();
        assert_eq!(decoded.id, event.id);
        assert_eq!(decoded.timestamp, event.timestamp);
        assert_eq!(&*decoded.source, &*event.source);
        assert_eq!(decoded.payload, event.payload);
    }

    #[test]
    fn msgpack_output_encode_decode() {
        let codec = TransportCodec::MsgPack;
        let output = make_output();
        let encoded = codec.encode_output(&output).unwrap();
        let decoded = codec.decode_output(&encoded).unwrap();

        assert_eq!(&*decoded.destination, &*output.destination);
        assert_eq!(decoded.payload, output.payload);
        assert_eq!(decoded.key, output.key);
        assert_eq!(decoded.headers.len(), output.headers.len());
        assert_eq!(decoded.source_event_id, output.source_event_id);
        assert_eq!(decoded.source_partition, output.source_partition);
        assert_eq!(decoded.source_offset, output.source_offset);
    }

    #[test]
    fn json_output_encode_decode() {
        let codec = TransportCodec::Json;
        let output = make_output();
        let encoded = codec.encode_output(&output).unwrap();
        let decoded = codec.decode_output(&encoded).unwrap();

        assert_eq!(&*decoded.destination, &*output.destination);
        assert_eq!(decoded.payload, output.payload);
    }

    #[test]
    fn msgpack_batch_encode_decode() {
        let codec = TransportCodec::MsgPack;
        let events = vec![make_event(), make_event()];
        let encoded = codec.encode_events(&events).unwrap();
        assert_eq!(encoded.len(), 2);

        let slices: Vec<&[u8]> = encoded.iter().map(|e| e.as_slice()).collect();
        let decoded = codec.decode_events(&slices).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].id, events[0].id);
        assert_eq!(decoded[1].id, events[1].id);
    }

    #[test]
    fn msgpack_is_more_compact_than_json() {
        let event = make_event();
        let msgpack = TransportCodec::MsgPack.encode_event(&event).unwrap();
        let json = TransportCodec::Json.encode_event(&event).unwrap();
        assert!(
            msgpack.len() < json.len(),
            "MsgPack ({} bytes) should be smaller than JSON ({} bytes)",
            msgpack.len(),
            json.len()
        );
    }

    #[test]
    fn cross_codec_decode_fails_gracefully() {
        let event = make_event();
        // Encode as MsgPack, try to decode as JSON
        let encoded = TransportCodec::MsgPack.encode_event(&event).unwrap();
        let result = TransportCodec::Json.decode_event(&encoded);
        assert!(result.is_err());

        // Encode as JSON, try to decode as MsgPack
        let encoded = TransportCodec::Json.encode_event(&event).unwrap();
        let result = TransportCodec::MsgPack.decode_event(&encoded);
        assert!(result.is_err());
    }

    #[test]
    fn empty_payload_roundtrip() {
        let event = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("src"),
            PartitionId::new(0),
            Bytes::new(),
        );
        for codec in [TransportCodec::MsgPack, TransportCodec::Json] {
            let encoded = codec.encode_event(&event).unwrap();
            let decoded = codec.decode_event(&encoded).unwrap();
            assert!(decoded.payload.is_empty());
        }
    }

    #[test]
    fn empty_batch_roundtrip() {
        let events: Vec<Event> = vec![];
        for codec in [TransportCodec::MsgPack, TransportCodec::Json] {
            let encoded = codec.encode_events(&events).unwrap();
            assert!(encoded.is_empty());
            let slices: Vec<&[u8]> = vec![];
            let decoded = codec.decode_events(&slices).unwrap();
            assert!(decoded.is_empty());
        }
    }
}
