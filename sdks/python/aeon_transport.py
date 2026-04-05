"""
Aeon Transport SDK for Python — T4 WebSocket processor client.

Connects to the Aeon engine via WebSocket at /api/v1/processors/connect,
runs the AWPP handshake (ED25519 challenge-response authentication),
receives batch requests, calls the user's processor function, and sends
batch responses.

Dependencies:
    pip install websockets msgpack pynacl

Usage:
    from aeon_transport import processor, Event, Output, run

    @processor
    def my_processor(event: Event) -> list[Output]:
        return [Output(destination="out", payload=event.payload)]

    run("ws://localhost:4471/api/v1/processors/connect",
        name="my-processor", version="1.0.0",
        private_key_path="processor.key",
        pipelines=["my-pipeline"])
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import struct
import time
from dataclasses import dataclass, field
from typing import Callable, Optional, Union

logger = logging.getLogger("aeon.transport")

# ── Types ────────────────────────────────────────────────────────────────


@dataclass
class Header:
    """Key-value metadata header."""
    key: str
    value: str


@dataclass
class Event:
    """Event received from the Aeon engine over AWPP.

    Fields mirror the Rust WireEvent struct (transport_codec.rs).
    """
    id: str  # UUID string
    timestamp: int  # Unix epoch nanoseconds
    source: str
    partition: int
    metadata: list[tuple[str, str]]
    payload: bytes
    source_offset: Optional[int] = None


@dataclass
class Output:
    """Processor output to send back to the Aeon engine.

    Fields mirror the Rust WireOutput struct.
    """
    destination: str
    payload: bytes
    key: Optional[bytes] = None
    headers: list[tuple[str, str]] = field(default_factory=list)
    source_event_id: Optional[str] = None
    source_partition: Optional[int] = None
    source_offset: Optional[int] = None

    @staticmethod
    def from_str(destination: str, payload: str) -> Output:
        """Create an output with a UTF-8 string payload."""
        return Output(destination=destination, payload=payload.encode("utf-8"))

    def with_key(self, key: bytes) -> Output:
        self.key = key
        return self

    def with_key_str(self, key: str) -> Output:
        self.key = key.encode("utf-8")
        return self

    def with_header(self, key: str, value: str) -> Output:
        self.headers.append((key, value))
        return self

    def with_event_identity(self, event: Event) -> Output:
        """Propagate source event identity for delivery tracking."""
        self.source_event_id = event.id
        self.source_partition = event.partition
        self.source_offset = event.source_offset
        return self


# ── Codec ────────────────────────────────────────────────────────────────


def _wire_event_from_dict(d: dict) -> Event:
    """Convert a deserialized WireEvent dict to an Event."""
    return Event(
        id=d.get("id", ""),
        timestamp=d.get("timestamp", 0),
        source=d.get("source", ""),
        partition=d.get("partition", 0),
        metadata=[(k, v) for k, v in d.get("metadata", [])],
        payload=_to_bytes(d.get("payload", b"")),
        source_offset=d.get("source_offset"),
    )


def _output_to_dict(out: Output) -> dict:
    """Convert an Output to a WireOutput-compatible dict."""
    d: dict = {
        "destination": out.destination,
        "payload": out.payload,
        "headers": list(out.headers),
    }
    if out.key is not None:
        d["key"] = out.key
    if out.source_event_id is not None:
        d["source_event_id"] = out.source_event_id
    if out.source_partition is not None:
        d["source_partition"] = out.source_partition
    if out.source_offset is not None:
        d["source_offset"] = out.source_offset
    return d


def _to_bytes(val) -> bytes:
    """Coerce a value to bytes (handles msgpack bin type and str)."""
    if isinstance(val, bytes):
        return val
    if isinstance(val, str):
        return val.encode("utf-8")
    return bytes(val)


def _json_sanitize_bytes(obj, encode_fn):
    """Recursively convert bytes values to base64 strings for JSON encoding."""
    if isinstance(obj, dict):
        return {k: _json_sanitize_bytes(v, encode_fn) for k, v in obj.items()}
    if isinstance(obj, list):
        return [_json_sanitize_bytes(v, encode_fn) for v in obj]
    if isinstance(obj, tuple):
        return [_json_sanitize_bytes(v, encode_fn) for v in obj]
    if isinstance(obj, bytes):
        return encode_fn(obj).decode("ascii")
    return obj


def _json_restore_bytes(obj, decode_fn):
    """Restore base64-encoded bytes fields from JSON.

    We restore known binary fields: payload, key.
    """
    if isinstance(obj, dict):
        result = {}
        for k, v in obj.items():
            if k in ("payload", "key") and isinstance(v, str):
                result[k] = decode_fn(v)
            else:
                result[k] = _json_restore_bytes(v, decode_fn)
        return result
    if isinstance(obj, list):
        return [_json_restore_bytes(v, decode_fn) for v in obj]
    return obj


class Codec:
    """Transport codec — MsgPack or JSON."""

    def __init__(self, name: str = "msgpack"):
        self.name = name
        if name == "msgpack":
            import msgpack as _msgpack
            self._msgpack = _msgpack
        elif name == "json":
            pass
        else:
            raise ValueError(f"unknown codec: {name}")

    def encode_event(self, event: Event) -> bytes:
        d = {
            "id": event.id,
            "timestamp": event.timestamp,
            "source": event.source,
            "partition": event.partition,
            "metadata": list(event.metadata),
            "payload": event.payload,
        }
        if event.source_offset is not None:
            d["source_offset"] = event.source_offset
        return self._encode(d)

    def decode_event(self, data: bytes) -> Event:
        d = self._decode(data)
        return _wire_event_from_dict(d)

    def encode_output(self, output: Output) -> bytes:
        return self._encode(_output_to_dict(output))

    def decode_output(self, data: bytes) -> Output:
        d = self._decode(data)
        return Output(
            destination=d.get("destination", ""),
            payload=_to_bytes(d.get("payload", b"")),
            key=d.get("key"),
            headers=[(k, v) for k, v in d.get("headers", [])],
            source_event_id=d.get("source_event_id"),
            source_partition=d.get("source_partition"),
            source_offset=d.get("source_offset"),
        )

    def _encode(self, obj: dict) -> bytes:
        if self.name == "msgpack":
            return self._msgpack.packb(obj, use_bin_type=True)
        # JSON: bytes fields must be base64-encoded
        import base64
        sanitized = _json_sanitize_bytes(obj, base64.b64encode)
        return json.dumps(sanitized, separators=(",", ":")).encode("utf-8")

    def _decode(self, data: bytes) -> dict:
        if self.name == "msgpack":
            return self._msgpack.unpackb(data, raw=False)
        import base64
        parsed = json.loads(data)
        return _json_restore_bytes(parsed, base64.b64decode)


# ── Batch Wire Format ────────────────────────────────────────────────────


def decode_batch_request(data: bytes, codec: Codec) -> tuple[int, list[Event]]:
    """Decode a batch request from wire format.

    Wire: [8B batch_id LE][4B event_count LE][per event: 4B len LE + bytes][4B CRC32]
    """
    if len(data) < 16:
        raise ValueError("batch request too short")

    # Verify CRC32
    payload = data[:-4]
    expected_crc = struct.unpack_from("<I", data, len(data) - 4)[0]
    import zlib
    actual_crc = zlib.crc32(payload) & 0xFFFFFFFF
    if expected_crc != actual_crc:
        raise ValueError(f"CRC32 mismatch: expected {expected_crc:#x}, got {actual_crc:#x}")

    batch_id = struct.unpack_from("<Q", data, 0)[0]
    event_count = struct.unpack_from("<I", data, 8)[0]

    offset = 12
    events = []
    for _ in range(event_count):
        elen = struct.unpack_from("<I", data, offset)[0]
        offset += 4
        event_bytes = data[offset:offset + elen]
        offset += elen
        events.append(codec.decode_event(event_bytes))

    return batch_id, events


def encode_batch_response(
    batch_id: int,
    outputs_per_event: list[list[Output]],
    signature: bytes,
    codec: Codec,
) -> bytes:
    """Encode a batch response into wire format.

    Wire: [8B batch_id LE][4B event_count LE]
          [per event: 4B output_count LE, per output: 4B len LE + bytes]
          [4B CRC32][64B signature]
    """
    parts: list[bytes] = []

    parts.append(struct.pack("<Q", batch_id))
    parts.append(struct.pack("<I", len(outputs_per_event)))

    for outputs in outputs_per_event:
        parts.append(struct.pack("<I", len(outputs)))
        for out in outputs:
            encoded = codec.encode_output(out)
            parts.append(struct.pack("<I", len(encoded)))
            parts.append(encoded)

    body = b"".join(parts)

    # CRC32 over the body (matching Rust crc32fast)
    import zlib
    crc = zlib.crc32(body) & 0xFFFFFFFF

    result = body + struct.pack("<I", crc)

    # Signature (64 bytes)
    if len(signature) != 64:
        raise ValueError(f"signature must be 64 bytes, got {len(signature)}")
    result += signature

    return result


# ── WebSocket Data Frame ─────────────────────────────────────────────────


def build_ws_data_frame(
    pipeline_name: str, partition: int, batch_wire_data: bytes
) -> bytes:
    """Build a T4 binary data frame with routing header.

    Format: [4B name_len LE][name][2B partition LE][batch_wire data]
    """
    name_bytes = pipeline_name.encode("utf-8")
    return (
        struct.pack("<I", len(name_bytes))
        + name_bytes
        + struct.pack("<H", partition)
        + batch_wire_data
    )


def parse_ws_data_frame(data: bytes) -> tuple[str, int, bytes]:
    """Parse a T4 binary data frame routing header.

    Returns: (pipeline_name, partition, batch_wire_data)
    """
    if len(data) < 7:
        raise ValueError("data frame too short")
    name_len = struct.unpack_from("<I", data, 0)[0]
    if len(data) < 4 + name_len + 2:
        raise ValueError("data frame name truncated")
    name = data[4:4 + name_len].decode("utf-8")
    partition = struct.unpack_from("<H", data, 4 + name_len)[0]
    batch_data = data[4 + name_len + 2:]
    return name, partition, batch_data


# ── ED25519 Signing ──────────────────────────────────────────────────────


class Signer:
    """ED25519 key pair for AWPP authentication and batch signing."""

    def __init__(self, private_key_bytes: bytes):
        from nacl.signing import SigningKey
        self._signing_key = SigningKey(private_key_bytes)
        self._verify_key = self._signing_key.verify_key

    @classmethod
    def from_file(cls, path: str) -> Signer:
        """Load a 32-byte ED25519 seed from a file."""
        with open(path, "rb") as f:
            seed = f.read()
        if len(seed) != 32:
            raise ValueError(f"expected 32-byte ED25519 seed, got {len(seed)} bytes")
        return cls(seed)

    @classmethod
    def generate(cls) -> Signer:
        """Generate a new random ED25519 key pair."""
        from nacl.signing import SigningKey
        return cls(SigningKey.generate()._seed)

    @property
    def public_key_hex(self) -> str:
        """Hex-encoded public key (for AWPP registration)."""
        return self._verify_key.encode().hex()

    def sign_challenge(self, nonce: str) -> str:
        """Sign an AWPP challenge nonce, returning hex-encoded signature."""
        signed = self._signing_key.sign(nonce.encode("utf-8"))
        return signed.signature.hex()

    def sign_batch(self, batch_data: bytes) -> bytes:
        """Sign batch response data (64-byte ED25519 signature)."""
        signed = self._signing_key.sign(batch_data)
        return signed.signature

    def compute_fingerprint(self) -> str:
        """SHA-256 fingerprint of the public key (matches Rust identity store)."""
        pubkey_bytes = self._verify_key.encode()
        return hashlib.sha256(pubkey_bytes).hexdigest()


# ── AWPP Protocol ────────────────────────────────────────────────────────


async def awpp_handshake(
    ws,
    signer: Signer,
    name: str,
    version: str,
    pipelines: list[str],
    codec_name: str = "msgpack",
) -> dict:
    """Run the AWPP handshake over a WebSocket connection.

    Returns the Accepted payload dict on success.
    Raises RuntimeError on rejection.

    Steps:
    1. Receive Challenge from host
    2. Send Register with signed nonce + public key
    3. Receive Accepted or Rejected
    """
    # Step 1: Receive Challenge
    raw = await ws.recv()
    if isinstance(raw, bytes):
        raw = raw.decode("utf-8")
    msg = json.loads(raw)

    if msg.get("type") != "challenge":
        raise RuntimeError(f"expected challenge, got: {msg.get('type')}")

    nonce = msg["nonce"]
    logger.info("received AWPP challenge (protocol=%s)", msg.get("protocol", "?"))

    # Step 2: Send Register
    register = {
        "type": "register",
        "protocol": "awpp/1",
        "transport": "websocket",
        "name": name,
        "version": version,
        "public_key": signer.public_key_hex,
        "challenge_signature": signer.sign_challenge(nonce),
        "capabilities": ["batch"],
        "transport_codec": codec_name,
        "requested_pipelines": pipelines,
        "binding": "dedicated",
    }
    await ws.send(json.dumps(register))
    logger.info("sent AWPP register (name=%s, codec=%s)", name, codec_name)

    # Step 3: Receive Accepted or Rejected
    raw = await ws.recv()
    if isinstance(raw, bytes):
        raw = raw.decode("utf-8")
    msg = json.loads(raw)

    if msg.get("type") == "rejected":
        raise RuntimeError(
            f"AWPP handshake rejected: [{msg.get('code')}] {msg.get('message')}"
        )
    if msg.get("type") != "accepted":
        raise RuntimeError(f"unexpected AWPP message: {msg.get('type')}")

    logger.info(
        "AWPP handshake accepted (session=%s, pipelines=%s, codec=%s)",
        msg.get("session_id"),
        [p.get("name") for p in msg.get("pipelines", [])],
        msg.get("transport_codec", "msgpack"),
    )

    return msg


# ── Processor Registration ───────────────────────────────────────────────

ProcessorFn = Callable[[Event], list[Output]]
BatchProcessorFn = Callable[[list[Event]], list[list[Output]]]

_processor_fn: Optional[ProcessorFn] = None
_batch_processor_fn: Optional[BatchProcessorFn] = None


def processor(fn: ProcessorFn) -> ProcessorFn:
    """Decorator to register a per-event processor function.

    Usage:
        @processor
        def my_proc(event: Event) -> list[Output]:
            return [Output(destination="out", payload=event.payload)]
    """
    global _processor_fn
    _processor_fn = fn
    return fn


def batch_processor(fn: BatchProcessorFn) -> BatchProcessorFn:
    """Decorator to register a batch processor function.

    The function receives a list of events and returns a list of output lists
    (one per input event).

    Usage:
        @batch_processor
        def my_proc(events: list[Event]) -> list[list[Output]]:
            return [[Output(destination="out", payload=e.payload)] for e in events]
    """
    global _batch_processor_fn
    _batch_processor_fn = fn
    return fn


def _process_batch(events: list[Event]) -> list[list[Output]]:
    """Dispatch a batch to the registered processor."""
    if _batch_processor_fn is not None:
        return _batch_processor_fn(events)
    if _processor_fn is not None:
        return [_processor_fn(e) for e in events]
    raise RuntimeError("no processor registered — use @processor or @batch_processor")


# ── Main Run Loop ────────────────────────────────────────────────────────


async def _run_async(
    url: str,
    name: str,
    version: str,
    signer: Signer,
    pipelines: list[str],
    codec_name: str = "msgpack",
    heartbeat_interval: float = 10.0,
):
    """Connect to Aeon, handshake, and process batches."""
    import websockets

    logger.info("connecting to %s", url)
    async with websockets.connect(url, max_size=16 * 1024 * 1024) as ws:
        # AWPP handshake
        accepted = await awpp_handshake(
            ws, signer, name, version, pipelines, codec_name
        )

        # Use the codec negotiated by the host
        effective_codec = accepted.get("transport_codec", codec_name)
        codec = Codec(effective_codec)
        session_id = accepted.get("session_id", "unknown")
        hb_interval_ms = accepted.get("heartbeat_interval_ms", 10000)
        batch_signing = accepted.get("batch_signing", True)

        logger.info(
            "session %s active (codec=%s, hb=%dms, signing=%s)",
            session_id, effective_codec, hb_interval_ms, batch_signing,
        )

        # Start heartbeat task
        hb_task = asyncio.create_task(
            _heartbeat_loop(ws, hb_interval_ms / 1000.0)
        )

        try:
            await _message_loop(ws, codec, signer, batch_signing)
        except Exception as e:
            logger.error("session %s error: %s", session_id, e)
            raise
        finally:
            hb_task.cancel()
            try:
                await hb_task
            except asyncio.CancelledError:
                pass


async def _heartbeat_loop(ws, interval_secs: float):
    """Send periodic heartbeat messages."""
    while True:
        await asyncio.sleep(interval_secs)
        hb = {"type": "heartbeat", "timestamp_ms": int(time.time() * 1000)}
        try:
            await ws.send(json.dumps(hb))
        except Exception:
            break


async def _message_loop(ws, codec: Codec, signer: Signer, batch_signing: bool):
    """Main message loop — demux text (control) and binary (data) frames."""
    async for message in ws:
        if isinstance(message, str):
            # Control message
            _handle_control(message)
        elif isinstance(message, bytes):
            # Data frame — batch request
            response_frame = _handle_data_frame(message, codec, signer, batch_signing)
            await ws.send(response_frame)
        else:
            logger.warning("unexpected message type: %s", type(message))


def _handle_control(text: str):
    """Handle a text-frame control message."""
    try:
        msg = json.loads(text)
    except json.JSONDecodeError:
        logger.warning("malformed control message")
        return

    msg_type = msg.get("type")
    if msg_type == "drain":
        reason = msg.get("reason", "unknown")
        deadline = msg.get("deadline_ms", 0)
        logger.info("received drain (reason=%s, deadline=%dms)", reason, deadline)
        # TODO: graceful shutdown — finish in-flight, then exit
    elif msg_type == "error":
        logger.error(
            "host error: [%s] %s", msg.get("code"), msg.get("message")
        )
    elif msg_type == "heartbeat":
        # Host heartbeat — just log
        pass
    else:
        logger.debug("unhandled control message: %s", msg_type)


def _handle_data_frame(
    data: bytes, codec: Codec, signer: Signer, batch_signing: bool
) -> bytes:
    """Process a binary data frame (batch request) and return the response frame."""
    # Parse routing header
    pipeline_name, partition, batch_wire = parse_ws_data_frame(data)

    # Decode batch request
    batch_id, events = decode_batch_request(batch_wire, codec)

    logger.debug(
        "batch %d: %d events (pipeline=%s, partition=%d)",
        batch_id, len(events), pipeline_name, partition,
    )

    # Process
    outputs_per_event = _process_batch(events)

    # Sign if required
    if batch_signing:
        # Sign the response body (before CRC + signature are appended)
        response_body = _build_response_body(batch_id, outputs_per_event, codec)
        signature = signer.sign_batch(response_body)
    else:
        signature = b"\x00" * 64

    # Encode response
    response_wire = encode_batch_response(batch_id, outputs_per_event, signature, codec)

    # Wrap in routing frame
    return build_ws_data_frame(pipeline_name, partition, response_wire)


def _build_response_body(
    batch_id: int, outputs_per_event: list[list[Output]], codec: Codec
) -> bytes:
    """Build the response body (pre-CRC, pre-signature) for signing."""
    parts: list[bytes] = []
    parts.append(struct.pack("<Q", batch_id))
    parts.append(struct.pack("<I", len(outputs_per_event)))
    for outputs in outputs_per_event:
        parts.append(struct.pack("<I", len(outputs)))
        for out in outputs:
            encoded = codec.encode_output(out)
            parts.append(struct.pack("<I", len(encoded)))
            parts.append(encoded)
    return b"".join(parts)


def run(
    url: str,
    *,
    name: str,
    version: str = "1.0.0",
    private_key_path: Optional[str] = None,
    private_key: Optional[bytes] = None,
    pipelines: Optional[list[str]] = None,
    codec: str = "msgpack",
    log_level: str = "INFO",
):
    """Connect to Aeon and run the processor.

    Args:
        url: WebSocket URL (e.g. ws://localhost:4471/api/v1/processors/connect)
        name: Processor name (must match registered identity in Aeon)
        version: Processor version string
        private_key_path: Path to 32-byte ED25519 seed file
        private_key: Raw 32-byte ED25519 seed (alternative to private_key_path)
        pipelines: List of pipeline names to request
        codec: Transport codec ("msgpack" or "json")
        log_level: Logging level
    """
    logging.basicConfig(
        level=getattr(logging, log_level.upper(), logging.INFO),
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
    )

    if private_key_path:
        signer = Signer.from_file(private_key_path)
    elif private_key:
        signer = Signer(private_key)
    else:
        logger.warning("no private key provided — generating ephemeral key pair")
        signer = Signer.generate()

    logger.info(
        "processor %s v%s (fingerprint=%s)",
        name, version, signer.compute_fingerprint()[:16] + "...",
    )

    asyncio.run(_run_async(
        url=url,
        name=name,
        version=version,
        signer=signer,
        pipelines=pipelines or [],
        codec_name=codec,
    ))
