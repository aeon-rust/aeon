"""Tests for the Aeon Python SDK wire format."""

import struct

from aeon_processor import (
    Event,
    Header,
    Output,
    deserialize_event,
    serialize_outputs,
    register_processor,
    process,
)


def make_wire_event(
    event_id: bytes,
    timestamp: int,
    source: str,
    partition: int,
    metadata: list[tuple[str, str]],
    payload: bytes,
) -> bytes:
    """Build a binary wire-format event (mimics host serialization)."""
    parts: list[bytes] = []

    # UUID
    parts.append(event_id)
    # Timestamp
    parts.append(struct.pack("<q", timestamp))
    # Source
    sb = source.encode("utf-8")
    parts.append(struct.pack("<I", len(sb)))
    parts.append(sb)
    # Partition
    parts.append(struct.pack("<H", partition))
    # Metadata
    parts.append(struct.pack("<I", len(metadata)))
    for k, v in metadata:
        kb = k.encode("utf-8")
        vb = v.encode("utf-8")
        parts.append(struct.pack("<I", len(kb)))
        parts.append(kb)
        parts.append(struct.pack("<I", len(vb)))
        parts.append(vb)
    # Payload
    parts.append(struct.pack("<I", len(payload)))
    parts.append(payload)

    return b"".join(parts)


def test_deserialize_minimal_event():
    wire = make_wire_event(b"\x01" * 16, 999, "src", 7, [], b"hello")
    event = deserialize_event(wire)

    assert event.id == b"\x01" * 16
    assert event.timestamp == 999
    assert event.source == "src"
    assert event.partition == 7
    assert event.metadata == []
    assert event.payload == b"hello"


def test_deserialize_event_with_metadata():
    wire = make_wire_event(
        b"\x00" * 16,
        42,
        "test-source",
        3,
        [("key1", "val1"), ("key2", "val2")],
        b"payload-data",
    )
    event = deserialize_event(wire)

    assert event.source == "test-source"
    assert event.partition == 3
    assert len(event.metadata) == 2
    assert event.metadata[0].key == "key1"
    assert event.metadata[0].value == "val1"
    assert event.metadata[1].key == "key2"
    assert event.metadata[1].value == "val2"
    assert event.payload == b"payload-data"


def test_serialize_empty_outputs():
    result = serialize_outputs([])
    assert result == struct.pack("<I", 0)


def test_serialize_single_output_no_key():
    output = Output(destination="dest", payload=b"data")
    result = serialize_outputs([output])

    pos = 0
    (count,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert count == 1

    # Destination
    (dest_len,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert result[pos : pos + dest_len] == b"dest"
    pos += dest_len

    # No key
    assert result[pos] == 0
    pos += 1

    # Payload
    (payload_len,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert result[pos : pos + payload_len] == b"data"
    pos += payload_len

    # Headers
    (header_count,) = struct.unpack_from("<I", result, pos)
    assert header_count == 0


def test_serialize_output_with_key_and_headers():
    output = (
        Output(destination="topic-out", payload=b"payload")
        .with_key_str("my-key")
        .with_header("h1", "v1")
    )
    result = serialize_outputs([output])

    pos = 0
    (count,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert count == 1

    # Destination
    (dest_len,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert result[pos : pos + dest_len] == b"topic-out"
    pos += dest_len

    # Has key
    assert result[pos] == 1
    pos += 1
    (key_len,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert result[pos : pos + key_len] == b"my-key"
    pos += key_len

    # Payload
    (payload_len,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert result[pos : pos + payload_len] == b"payload"
    pos += payload_len

    # Headers
    (header_count,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert header_count == 1
    (hk_len,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert result[pos : pos + hk_len] == b"h1"
    pos += hk_len
    (hv_len,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert result[pos : pos + hv_len] == b"v1"


def test_roundtrip_process():
    register_processor(lambda event: [Output("output", event.payload)])

    wire = make_wire_event(b"\x00" * 16, 1000, "test", 0, [], b"hello")
    result = process(wire)

    # First 4 bytes: length prefix
    (length,) = struct.unpack_from("<I", result, 0)
    # Next 4 bytes: output count
    (count,) = struct.unpack_from("<I", result, 4)
    assert count == 1

    # Destination
    pos = 8
    (dest_len,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert result[pos : pos + dest_len] == b"output"
    pos += dest_len

    # No key
    assert result[pos] == 0
    pos += 1

    # Payload
    (payload_len,) = struct.unpack_from("<I", result, pos)
    pos += 4
    assert result[pos : pos + payload_len] == b"hello"


def test_payload_str():
    event = Event(
        id=b"\x00" * 16,
        timestamp=0,
        source="test",
        partition=0,
        metadata=[],
        payload=b'{"key":"value"}',
    )
    assert event.payload_str() == '{"key":"value"}'


def test_output_builder():
    out = (
        Output.from_str("dest", "hello world")
        .with_key(b"\x01\x02\x03")
        .with_header("content-type", "text/plain")
        .with_header("x-source", "test")
    )
    assert out.destination == "dest"
    assert out.key == b"\x01\x02\x03"
    assert out.payload == b"hello world"
    assert len(out.headers) == 2
    assert out.headers[0].key == "content-type"
    assert out.headers[1].key == "x-source"


if __name__ == "__main__":
    test_deserialize_minimal_event()
    test_deserialize_event_with_metadata()
    test_serialize_empty_outputs()
    test_serialize_single_output_no_key()
    test_serialize_output_with_key_and_headers()
    test_roundtrip_process()
    test_payload_str()
    test_output_builder()
    print("All Python SDK tests passed!")
