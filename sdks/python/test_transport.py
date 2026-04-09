"""Tests for the Aeon Python Transport SDK (T4 WebSocket)."""

import json
import struct
import zlib

from aeon_transport import (
    Codec,
    Event,
    Header,
    Output,
    Signer,
    build_ws_data_frame,
    decode_batch_request,
    encode_batch_response,
    parse_ws_data_frame,
    processor,
    batch_processor,
    _process_batch,
    _output_to_dict,
    _wire_event_from_dict,
)


# ── Codec Tests ──────────────────────────────────────────────────────────


def test_msgpack_codec_event_roundtrip():
    codec = Codec("msgpack")
    event = Event(
        id="550e8400-e29b-41d4-a716-446655440000",
        timestamp=1000000,
        source="test-source",
        partition=3,
        metadata=[("key1", "val1")],
        payload=b"hello world",
    )
    encoded = codec.encode_event(event)
    decoded = codec.decode_event(encoded)
    assert decoded.id == event.id
    assert decoded.timestamp == event.timestamp
    assert decoded.source == event.source
    assert decoded.partition == event.partition
    assert decoded.metadata == event.metadata
    assert decoded.payload == event.payload


def test_json_codec_event_roundtrip():
    codec = Codec("json")
    event = Event(
        id="550e8400-e29b-41d4-a716-446655440000",
        timestamp=42,
        source="json-src",
        partition=0,
        metadata=[],
        payload=b"data",
    )
    encoded = codec.encode_event(event)
    # JSON codec should produce valid JSON
    parsed = json.loads(encoded)
    assert parsed["source"] == "json-src"

    decoded = codec.decode_event(encoded)
    assert decoded.id == event.id
    assert decoded.source == "json-src"


def test_msgpack_codec_output_roundtrip():
    codec = Codec("msgpack")
    out = Output(
        destination="sink-topic",
        payload=b"output-data",
        key=b"my-key",
        headers=[("h1", "v1"), ("h2", "v2")],
        source_event_id="some-uuid",
        source_partition=5,
    )
    encoded = codec.encode_output(out)
    decoded = codec.decode_output(encoded)
    assert decoded.destination == "sink-topic"
    assert decoded.payload == b"output-data"
    assert decoded.key == b"my-key"
    assert decoded.headers == [("h1", "v1"), ("h2", "v2")]
    assert decoded.source_event_id == "some-uuid"
    assert decoded.source_partition == 5


def test_json_codec_output_no_key():
    codec = Codec("json")
    out = Output(destination="dest", payload=b"p")
    encoded = codec.encode_output(out)
    decoded = codec.decode_output(encoded)
    assert decoded.destination == "dest"
    assert decoded.key is None


def test_msgpack_more_compact_than_json():
    codec_mp = Codec("msgpack")
    codec_json = Codec("json")
    event = Event(
        id="550e8400-e29b-41d4-a716-446655440000",
        timestamp=1234567890,
        source="a-source",
        partition=1,
        metadata=[("content-type", "application/json")],
        payload=b'{"key": "value", "number": 42}',
    )
    mp_bytes = codec_mp.encode_event(event)
    json_bytes = codec_json.encode_event(event)
    assert len(mp_bytes) < len(json_bytes), (
        f"msgpack ({len(mp_bytes)}B) should be smaller than json ({len(json_bytes)}B)"
    )


# ── Data Frame Tests ─────────────────────────────────────────────────────


def test_ws_data_frame_roundtrip():
    pipeline = "my-pipeline"
    partition = 7
    batch_data = b"some-batch-wire-data"

    frame = build_ws_data_frame(pipeline, partition, batch_data)
    name, part, data = parse_ws_data_frame(frame)
    assert name == pipeline
    assert part == partition
    assert data == batch_data


def test_ws_data_frame_empty_name():
    frame = build_ws_data_frame("", 0, b"data")
    name, part, data = parse_ws_data_frame(frame)
    assert name == ""
    assert part == 0
    assert data == b"data"


def test_ws_data_frame_too_short():
    try:
        parse_ws_data_frame(b"\x00" * 5)
        assert False, "should have raised"
    except ValueError:
        pass


def test_ws_data_frame_truncated_name():
    # name_len says 100 but only 2 bytes of name present
    frame = struct.pack("<I", 100) + b"ab" + struct.pack("<H", 0)
    try:
        parse_ws_data_frame(frame)
        assert False, "should have raised"
    except ValueError:
        pass


# ── Batch Wire Format Tests ──────────────────────────────────────────────


def _build_batch_request(batch_id: int, events_bytes: list[bytes]) -> bytes:
    """Build a batch request in wire format (mimics Rust serialize_batch_request)."""
    parts = []
    parts.append(struct.pack("<Q", batch_id))
    parts.append(struct.pack("<I", len(events_bytes)))
    for eb in events_bytes:
        parts.append(struct.pack("<I", len(eb)))
        parts.append(eb)
    body = b"".join(parts)
    crc = zlib.crc32(body) & 0xFFFFFFFF
    return body + struct.pack("<I", crc)


def test_decode_batch_request_msgpack():
    codec = Codec("msgpack")
    event = Event(
        id="test-uuid", timestamp=999, source="src", partition=0,
        metadata=[], payload=b"hello",
    )
    event_bytes = codec.encode_event(event)
    wire = _build_batch_request(42, [event_bytes])

    batch_id, events = decode_batch_request(wire, codec)
    assert batch_id == 42
    assert len(events) == 1
    assert events[0].id == "test-uuid"
    assert events[0].payload == b"hello"


def test_decode_batch_request_json():
    codec = Codec("json")
    event = Event(
        id="json-uuid", timestamp=100, source="s", partition=1,
        metadata=[("k", "v")], payload=b"data",
    )
    event_bytes = codec.encode_event(event)
    wire = _build_batch_request(99, [event_bytes])

    batch_id, events = decode_batch_request(wire, codec)
    assert batch_id == 99
    assert len(events) == 1
    assert events[0].metadata == [("k", "v")]


def test_decode_batch_request_crc_mismatch():
    codec = Codec("msgpack")
    event_bytes = codec.encode_event(
        Event(id="x", timestamp=0, source="s", partition=0, metadata=[], payload=b"")
    )
    wire = _build_batch_request(1, [event_bytes])
    # Corrupt one byte
    corrupted = bytearray(wire)
    corrupted[0] ^= 0xFF
    try:
        decode_batch_request(bytes(corrupted), codec)
        assert False, "should have raised"
    except ValueError as e:
        assert "CRC32" in str(e)


def test_encode_batch_response_structure():
    codec = Codec("msgpack")
    out = Output(destination="dest", payload=b"result")
    sig = b"\x00" * 64

    wire = encode_batch_response(7, [[out]], sig, codec)

    # Verify structure: [8B batch_id][4B count=1][4B out_count=1][4B len][data][4B CRC][64B sig]
    assert len(wire) > 80  # minimum: 12 + data + 4 + 64
    batch_id = struct.unpack_from("<Q", wire, 0)[0]
    assert batch_id == 7
    event_count = struct.unpack_from("<I", wire, 8)[0]
    assert event_count == 1

    # Last 64 bytes = signature
    assert wire[-64:] == sig


def test_batch_request_multiple_events():
    codec = Codec("msgpack")
    events = [
        Event(id=f"evt-{i}", timestamp=i, source="s", partition=0,
              metadata=[], payload=f"payload-{i}".encode())
        for i in range(5)
    ]
    events_bytes = [codec.encode_event(e) for e in events]
    wire = _build_batch_request(123, events_bytes)

    batch_id, decoded = decode_batch_request(wire, codec)
    assert batch_id == 123
    assert len(decoded) == 5
    for i, e in enumerate(decoded):
        assert e.id == f"evt-{i}"
        assert e.payload == f"payload-{i}".encode()


# ── ED25519 Signer Tests ────────────────────────────────────────────────


def test_signer_generate():
    signer = Signer.generate()
    assert len(signer.public_key_hex) == 64  # 32 bytes hex-encoded
    fp = signer.compute_fingerprint()
    assert len(fp) == 64  # SHA-256 hex


def test_signer_challenge_response():
    signer = Signer.generate()
    nonce = "test-nonce-abc123"
    sig_hex = signer.sign_challenge(nonce)
    assert len(sig_hex) == 128  # 64 bytes hex-encoded

    # Verify the signature
    from nacl.signing import VerifyKey
    vk = VerifyKey(bytes.fromhex(signer.public_key_hex))
    sig_bytes = bytes.fromhex(sig_hex)
    vk.verify(nonce.encode("utf-8"), sig_bytes)  # raises if invalid


def test_signer_batch_signing():
    signer = Signer.generate()
    data = b"batch-data-to-sign"
    signature = signer.sign_batch(data)
    assert len(signature) == 64

    # Verify
    from nacl.signing import VerifyKey
    vk = VerifyKey(bytes.fromhex(signer.public_key_hex))
    vk.verify(data, signature)


def test_signer_fingerprint_deterministic():
    signer = Signer.generate()
    fp1 = signer.compute_fingerprint()
    fp2 = signer.compute_fingerprint()
    assert fp1 == fp2


# ── Processor Registration Tests ─────────────────────────────────────────


def test_processor_decorator():
    @processor
    def my_proc(event: Event) -> list[Output]:
        return [Output(destination="out", payload=event.payload.upper())]

    events = [Event(id="1", timestamp=0, source="s", partition=0,
                    metadata=[], payload=b"hello")]
    results = _process_batch(events)
    assert len(results) == 1
    assert len(results[0]) == 1
    assert results[0][0].payload == b"HELLO"


def test_batch_processor_decorator():
    @batch_processor
    def my_batch(events: list[Event]) -> list[list[Output]]:
        return [[Output(destination="out", payload=e.payload)] for e in events]

    events = [
        Event(id=str(i), timestamp=0, source="s", partition=0,
              metadata=[], payload=f"p{i}".encode())
        for i in range(3)
    ]
    results = _process_batch(events)
    assert len(results) == 3
    assert results[2][0].payload == b"p2"


# ── Output Builder Tests ────────────────────────────────────────────────


def test_output_with_event_identity():
    event = Event(
        id="evt-123", timestamp=0, source="s", partition=5,
        metadata=[], payload=b"", source_offset=42,
    )
    out = Output(destination="d", payload=b"").with_event_identity(event)
    assert out.source_event_id == "evt-123"
    assert out.source_partition == 5
    assert out.source_offset == 42


def test_output_to_dict_optional_fields():
    out = Output(destination="d", payload=b"p")
    d = _output_to_dict(out)
    assert "key" not in d
    assert "source_event_id" not in d

    out2 = Output(destination="d", payload=b"p", key=b"k", source_event_id="id")
    d2 = _output_to_dict(out2)
    assert d2["key"] == b"k"
    assert d2["source_event_id"] == "id"


# ── Wire Event/Output Dict Conversion Tests ─────────────────────────────


def test_wire_event_from_dict():
    d = {
        "id": "test-id",
        "timestamp": 12345,
        "source": "my-source",
        "partition": 3,
        "metadata": [["k1", "v1"]],
        "payload": b"raw-bytes",
        "source_offset": 99,
    }
    event = _wire_event_from_dict(d)
    assert event.id == "test-id"
    assert event.timestamp == 12345
    assert event.source == "my-source"
    assert event.partition == 3
    assert event.metadata == [("k1", "v1")]
    assert event.payload == b"raw-bytes"
    assert event.source_offset == 99


def test_wire_event_from_dict_minimal():
    d = {}
    event = _wire_event_from_dict(d)
    assert event.id == ""
    assert event.timestamp == 0
    assert event.payload == b""


if __name__ == "__main__":
    test_msgpack_codec_event_roundtrip()
    test_json_codec_event_roundtrip()
    test_msgpack_codec_output_roundtrip()
    test_json_codec_output_no_key()
    test_msgpack_more_compact_than_json()
    test_ws_data_frame_roundtrip()
    test_ws_data_frame_empty_name()
    test_ws_data_frame_too_short()
    test_ws_data_frame_truncated_name()
    test_decode_batch_request_msgpack()
    test_decode_batch_request_json()
    test_decode_batch_request_crc_mismatch()
    test_encode_batch_response_structure()
    test_batch_request_multiple_events()
    test_signer_generate()
    test_signer_challenge_response()
    test_signer_batch_signing()
    test_signer_fingerprint_deterministic()
    test_processor_decorator()
    test_batch_processor_decorator()
    test_output_with_event_identity()
    test_output_to_dict_optional_fields()
    test_wire_event_from_dict()
    test_wire_event_from_dict_minimal()
    print("All Python Transport SDK tests passed!")
