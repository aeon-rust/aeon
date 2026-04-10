"""
Aeon Transport SDK for Python — T3 WebTransport + T4 WebSocket processor client.

Connects to the Aeon engine via either WebSocket (T4, default) or
WebTransport (T3, HTTP/3 over QUIC), runs the AWPP handshake (ED25519
challenge-response authentication), receives batch requests, calls the
user's processor function, and sends batch responses.

Dependencies:
    pip install websockets msgpack pynacl    # T4 (default)
    pip install 'aeon-processor[webtransport]'  # T3 (adds aioquic)

Usage (T4 WebSocket, default):
    from aeon_transport import processor, Event, Output, run

    @processor
    def my_processor(event: Event) -> list[Output]:
        return [Output(destination="out", payload=event.payload)]

    run("ws://localhost:4471/api/v1/processors/connect",
        name="my-processor", version="1.0.0",
        private_key_path="processor.key",
        pipelines=["my-pipeline"])

Usage (T3 WebTransport):
    from aeon_transport import processor, Event, Output, run_webtransport

    @processor
    def my_processor(event: Event) -> list[Output]:
        return [Output(destination="out", payload=event.payload)]

    run_webtransport("https://localhost:4472/",
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
        """Hex-encoded 32-byte public key (raw, no framing).

        Primarily exposed for tests and crypto verification. For the
        ``public_key`` field of an AWPP Register message, use
        :attr:`awpp_public_key` instead — Aeon's identity store keys
        identities by the ``ed25519:<base64>`` string, not raw hex.
        """
        return self._verify_key.encode().hex()

    @property
    def awpp_public_key(self) -> str:
        """Canonical AWPP public-key string: ``ed25519:<base64>``.

        Matches the format used by Aeon's processor identity store when
        registering an ED25519 identity (see
        ``crates/aeon-engine/src/identity_store.rs``). This is what the
        ``public_key`` field of an AWPP Register message must contain.
        """
        import base64
        b64 = base64.b64encode(self._verify_key.encode()).decode("ascii")
        return f"ed25519:{b64}"

    def sign_challenge(self, nonce_hex: str) -> str:
        """Sign an AWPP challenge nonce, returning hex-encoded signature.

        The AWPP Challenge nonce is a hex-encoded random byte string. The
        processor must sign the **raw bytes** (hex-decoded), NOT the
        UTF-8 encoding of the hex string — the server verifies against
        ``hex::decode(nonce)`` on its side
        (``processor_auth.rs::verify_challenge``).
        """
        nonce_bytes = bytes.fromhex(nonce_hex)
        signed = self._signing_key.sign(nonce_bytes)
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
        "public_key": signer.awpp_public_key,
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


# ── T3 WebTransport Transport ────────────────────────────────────────────
#
# The WT client is a faithful port of aeon-processor-client/src/webtransport.rs.
# It implements the 6-step AWPP-over-WT adapter contract documented in
# docs/WT-SDK-INTEGRATION-PLAN.md §4:
#   1. QUIC connect + H3 CONNECT :protocol: webtransport
#   2. ONE long-lived bidirectional control stream (length-prefixed JSON)
#   3. Heartbeat task on the control stream
#   4. ONE long-lived bidirectional data stream per (pipeline, partition)
#      with routing header written once at open
#   5. Length-prefixed batch wire frames bidirectionally on each data stream
#   6. Graceful shutdown when any stream ends
#
# The aioquic dependency is imported lazily so the base SDK stays importable
# without it — install with `pip install 'aeon-processor[webtransport]'`.


def build_wt_routing_header(pipeline_name: str, partition: int) -> bytes:
    """Build the T3 routing header written once at the start of a data stream.

    Format (matches the engine host and Rust client):
        [4B name_len LE][name utf-8][2B partition LE]
    """
    name_bytes = pipeline_name.encode("utf-8")
    return (
        struct.pack("<I", len(name_bytes))
        + name_bytes
        + struct.pack("<H", partition)
    )


def build_awpp_register_json(
    signer: Signer,
    challenge_nonce: str,
    name: str,
    version: str,
    pipelines: list[str],
    codec_name: str,
    transport: str = "webtransport",
) -> str:
    """Build the AWPP Register message as a JSON string.

    Transport-agnostic — used by both WS and WT handshakes. The `transport`
    field is what tells the host which channel is being negotiated
    ("websocket" or "webtransport").
    """
    reg = {
        "type": "register",
        "protocol": "awpp/1",
        "transport": transport,
        "name": name,
        "version": version,
        "public_key": signer.awpp_public_key,
        "challenge_signature": signer.sign_challenge(challenge_nonce),
        "capabilities": ["batch"],
        "transport_codec": codec_name,
        "requested_pipelines": pipelines,
        "binding": "dedicated",
    }
    return json.dumps(reg)


def _pack_len_prefixed(data: bytes) -> bytes:
    """Prefix `data` with a 4-byte little-endian length header."""
    return struct.pack("<I", len(data)) + data


def _import_aioquic():
    """Lazy import of the aioquic pieces needed by the WT client.

    Raises ImportError with a pip-install hint if aioquic is not installed.
    """
    try:
        from aioquic.asyncio.client import connect
        from aioquic.asyncio.protocol import QuicConnectionProtocol
        from aioquic.h3.connection import FrameType, H3Connection, H3Stream
        from aioquic.h3.events import (
            DataReceived,
            HeadersReceived,
            WebTransportStreamDataReceived,
        )
        from aioquic.quic.configuration import QuicConfiguration
    except ImportError as exc:
        raise ImportError(
            "WebTransport transport requires aioquic — install with "
            "`pip install aioquic` or `pip install 'aeon-processor[webtransport]'`"
        ) from exc
    return {
        "connect": connect,
        "QuicConnectionProtocol": QuicConnectionProtocol,
        "H3Connection": H3Connection,
        "H3Stream": H3Stream,
        "FrameType": FrameType,
        "HeadersReceived": HeadersReceived,
        "DataReceived": DataReceived,
        "WebTransportStreamDataReceived": WebTransportStreamDataReceived,
        "QuicConfiguration": QuicConfiguration,
    }


class _WtStreamBuffer:
    """Async byte buffer for a single WebTransport stream.

    The QUIC event handler calls `feed()` with incoming stream data; the
    per-stream worker task awaits `read_exact()` to consume it. EOF is
    signalled via `ended=True` on the final feed.
    """

    def __init__(self) -> None:
        self._buffer = bytearray()
        self._closed = False
        self._event = asyncio.Event()

    def feed(self, data: bytes, ended: bool) -> None:
        if data:
            self._buffer.extend(data)
        if ended:
            self._closed = True
        self._event.set()

    async def read_exact(self, n: int) -> bytes:
        while len(self._buffer) < n:
            if self._closed:
                raise EOFError(
                    f"stream closed before {n} bytes available "
                    f"(have {len(self._buffer)})"
                )
            self._event.clear()
            await self._event.wait()
        out = bytes(self._buffer[:n])
        del self._buffer[:n]
        return out


async def _read_len_prefixed(
    buf: _WtStreamBuffer, max_size: int = 16 * 1024 * 1024
) -> bytes:
    """Read a [4B LE length][body] frame from a stream buffer."""
    len_bytes = await buf.read_exact(4)
    length = struct.unpack("<I", len_bytes)[0]
    if length > max_size:
        raise ValueError(f"WT frame too large: {length} bytes")
    return await buf.read_exact(length)


def _make_wt_protocol_cls():
    """Build the QuicConnectionProtocol subclass used by the WT client.

    We construct this lazily so aioquic types aren't imported at module load.
    """
    mods = _import_aioquic()
    QuicConnectionProtocol = mods["QuicConnectionProtocol"]
    H3Connection = mods["H3Connection"]
    H3Stream = mods["H3Stream"]
    FrameType = mods["FrameType"]
    HeadersReceived = mods["HeadersReceived"]
    WebTransportStreamDataReceived = mods["WebTransportStreamDataReceived"]

    class _AeonWtProtocol(QuicConnectionProtocol):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            self._h3 = H3Connection(self._quic, enable_webtransport=True)
            self._stream_buffers: dict[int, _WtStreamBuffer] = {}
            self._connect_stream_id: Optional[int] = None
            self._connect_response_headers: Optional[list] = None
            self._connect_ready: asyncio.Event = asyncio.Event()
            self._session_id: Optional[int] = None

        # -- Stream buffer registration --

        def register_stream_buffer(self, stream_id: int) -> _WtStreamBuffer:
            buf = _WtStreamBuffer()
            self._stream_buffers[stream_id] = buf
            return buf

        # -- QUIC event dispatch --

        def quic_event_received(self, event) -> None:  # type: ignore[override]
            for h3_event in self._h3.handle_event(event):
                if isinstance(h3_event, HeadersReceived):
                    if h3_event.stream_id == self._connect_stream_id:
                        self._connect_response_headers = h3_event.headers
                        self._connect_ready.set()
                elif isinstance(h3_event, WebTransportStreamDataReceived):
                    buf = self._stream_buffers.get(h3_event.stream_id)
                    if buf is not None:
                        buf.feed(h3_event.data, h3_event.stream_ended)
                # DataReceived on CONNECT stream is ignored — AWPP rides on
                # user-opened WT streams, not on the CONNECT stream itself.

        # -- WT session setup --

        async def send_connect(self, authority: str, path: str = "/") -> int:
            """Send a WebTransport CONNECT request; return the session id."""
            stream_id = self._quic.get_next_available_stream_id()
            self._connect_stream_id = stream_id
            self._h3.send_headers(
                stream_id=stream_id,
                headers=[
                    (b":method", b"CONNECT"),
                    (b":protocol", b"webtransport"),
                    (b":scheme", b"https"),
                    (b":authority", authority.encode("ascii")),
                    (b":path", path.encode("ascii")),
                ],
                end_stream=False,
            )
            self.transmit()

            await self._connect_ready.wait()
            status: Optional[str] = None
            assert self._connect_response_headers is not None
            for k, v in self._connect_response_headers:
                if k == b":status":
                    status = v.decode("ascii")
                    break
            if status != "200":
                raise RuntimeError(
                    f"WebTransport CONNECT rejected: status={status}"
                )
            self._session_id = stream_id
            return stream_id

        # -- User-stream helpers --

        def open_wt_bi_stream(self) -> int:
            """Open a new bidirectional WebTransport stream.

            Workaround for an aioquic gap: `create_webtransport_stream`
            sends the `[0x41][session_id]` prologue on the wire but does
            not register the new stream's type in the H3 connection's
            internal `_stream` dict. When the server writes back on that
            stream, `_receive_request_or_push_data` misparses the raw
            WT bytes as HTTP/3 frames (looking for HEADERS/DATA/SETTINGS)
            and emits nothing — so the session hangs waiting for the
            AWPP Challenge. We patch the stream state here with the
            same fields `_receive_stream_data_uni` would set for an
            incoming WT stream, so the "shortcut for WEBTRANSPORT_STREAM
            frame fragments" branch in `_receive_request_or_push_data`
            fires correctly on the client side too.
            """
            if self._session_id is None:
                raise RuntimeError("CONNECT has not completed")
            stream_id = self._h3.create_webtransport_stream(
                session_id=self._session_id, is_unidirectional=False
            )
            # Register the stream in H3's state dict as a WT bi stream so
            # incoming server data is dispatched as WebTransportStreamDataReceived.
            if stream_id not in self._h3._stream:
                self._h3._stream[stream_id] = H3Stream(stream_id)
            h3_stream = self._h3._stream[stream_id]
            h3_stream.frame_type = FrameType.WEBTRANSPORT_STREAM
            h3_stream.session_id = self._session_id
            self.transmit()
            return stream_id

        def write_wt_stream(self, stream_id: int, data: bytes) -> None:
            """Write raw bytes to a WT stream (framing left to the caller)."""
            self._quic.send_stream_data(stream_id, data, end_stream=False)
            self.transmit()

    return _AeonWtProtocol


def _parse_https_url(url: str) -> tuple[str, int, str]:
    """Parse an https://host:port/path URL into (host, port, path)."""
    from urllib.parse import urlparse

    parsed = urlparse(url)
    if parsed.scheme != "https":
        raise ValueError(
            f"WebTransport URL must be https://, got scheme={parsed.scheme!r}"
        )
    host = parsed.hostname
    if host is None:
        raise ValueError(f"WebTransport URL missing host: {url}")
    port = parsed.port or 443
    path = parsed.path or "/"
    return host, port, path


async def _wt_heartbeat_loop(
    protocol, ctrl_stream_id: int, interval_secs: float
) -> None:
    """Send periodic AWPP heartbeats on the control stream as length-prefixed JSON."""
    while True:
        try:
            await asyncio.sleep(interval_secs)
        except asyncio.CancelledError:
            break
        hb = {"type": "heartbeat", "timestamp_ms": int(time.time() * 1000)}
        try:
            protocol.write_wt_stream(
                ctrl_stream_id,
                _pack_len_prefixed(json.dumps(hb).encode("utf-8")),
            )
        except Exception:
            break


async def _wt_data_stream_loop(
    protocol,
    stream_id: int,
    buf: _WtStreamBuffer,
    codec: Codec,
    signer: Signer,
    batch_signing: bool,
    pipeline_name: str,
    partition: int,
) -> None:
    """Run the per-data-stream read→process→write loop.

    Wire (both directions):
        [4B len LE][batch_wire bytes]

    The routing header has already been written by the caller; this loop
    only handles batch frames.
    """
    while True:
        try:
            frame = await _read_len_prefixed(buf)
        except EOFError:
            logger.debug(
                "WT data stream closed by peer (pipeline=%s, partition=%d, stream=%d)",
                pipeline_name,
                partition,
                stream_id,
            )
            return

        batch_id, events = decode_batch_request(frame, codec)
        logger.debug(
            "WT batch %d: %d events (pipeline=%s, partition=%d)",
            batch_id,
            len(events),
            pipeline_name,
            partition,
        )

        outputs_per_event = _process_batch(events)

        if batch_signing:
            body = _build_response_body(batch_id, outputs_per_event, codec)
            signature = signer.sign_batch(body)
        else:
            signature = b"\x00" * 64

        response_wire = encode_batch_response(
            batch_id, outputs_per_event, signature, codec
        )
        protocol.write_wt_stream(stream_id, _pack_len_prefixed(response_wire))


async def _run_webtransport_async(
    url: str,
    name: str,
    version: str,
    signer: Signer,
    pipelines: list[str],
    codec_name: str = "msgpack",
    insecure: bool = False,
    server_name: Optional[str] = None,
) -> None:
    """Connect to Aeon via T3 WebTransport and run the registered processor."""
    mods = _import_aioquic()
    connect = mods["connect"]
    QuicConfiguration = mods["QuicConfiguration"]

    host, port, path = _parse_https_url(url)
    effective_server_name = server_name or host
    authority = f"{effective_server_name}:{port}"

    config = QuicConfiguration(
        is_client=True,
        alpn_protocols=["h3"],
        server_name=effective_server_name,
        max_datagram_frame_size=65536,
    )
    if insecure:
        import ssl
        config.verify_mode = ssl.CERT_NONE

    protocol_cls = _make_wt_protocol_cls()

    logger.info(
        "connecting to WebTransport %s (authority=%s, insecure=%s)",
        url,
        authority,
        insecure,
    )
    async with connect(
        host,
        port,
        configuration=config,
        create_protocol=protocol_cls,
    ) as protocol:
        # 1. CONNECT :protocol: webtransport → establishes the WT session.
        session_id = await protocol.send_connect(authority, path)
        logger.info("WebTransport session established (session_id=%d)", session_id)

        # 2. Open the AWPP control stream (first user-opened WT bi stream).
        ctrl_stream_id = protocol.open_wt_bi_stream()
        ctrl_buf = protocol.register_stream_buffer(ctrl_stream_id)

        # 3. AWPP handshake: Challenge → Register → Accepted.
        challenge_json = (await _read_len_prefixed(ctrl_buf)).decode("utf-8")
        challenge = json.loads(challenge_json)
        if challenge.get("type") != "challenge":
            raise RuntimeError(
                f"expected challenge, got: {challenge.get('type')}"
            )
        nonce = challenge["nonce"]
        logger.info("received WT AWPP challenge (protocol=%s)", challenge.get("protocol", "?"))

        register_json = build_awpp_register_json(
            signer, nonce, name, version, pipelines, codec_name, "webtransport"
        )
        protocol.write_wt_stream(
            ctrl_stream_id, _pack_len_prefixed(register_json.encode("utf-8"))
        )
        logger.info("sent WT AWPP register (name=%s, codec=%s)", name, codec_name)

        response_json = (await _read_len_prefixed(ctrl_buf)).decode("utf-8")
        accepted = json.loads(response_json)
        if accepted.get("type") == "rejected":
            raise RuntimeError(
                f"AWPP WT handshake rejected: "
                f"[{accepted.get('code')}] {accepted.get('message')}"
            )
        if accepted.get("type") != "accepted":
            raise RuntimeError(
                f"unexpected AWPP message: {accepted.get('type')}"
            )

        awpp_session_id = accepted.get("session_id", "unknown")
        effective_codec = accepted.get("transport_codec", codec_name)
        codec = Codec(effective_codec)
        hb_interval_ms = accepted.get("heartbeat_interval_ms", 10000)
        batch_signing = accepted.get("batch_signing", True)
        pipeline_assignments = accepted.get("pipelines", [])

        logger.info(
            "WT session %s active (codec=%s, hb=%dms, signing=%s, pipelines=%s)",
            awpp_session_id,
            effective_codec,
            hb_interval_ms,
            batch_signing,
            [p.get("name") for p in pipeline_assignments],
        )

        # 4. Heartbeat task on the control stream.
        hb_task = asyncio.create_task(
            _wt_heartbeat_loop(
                protocol, ctrl_stream_id, hb_interval_ms / 1000.0
            )
        )

        # 5. Open one long-lived data stream per (pipeline, partition).
        stream_tasks: list[asyncio.Task] = []
        for assignment in pipeline_assignments:
            pipeline_name = assignment.get("name", "")
            partitions = assignment.get("partitions", [])
            for partition in partitions:
                data_stream_id = protocol.open_wt_bi_stream()
                data_buf = protocol.register_stream_buffer(data_stream_id)
                protocol.write_wt_stream(
                    data_stream_id,
                    build_wt_routing_header(pipeline_name, int(partition)),
                )
                logger.debug(
                    "WT data stream registered (pipeline=%s, partition=%d, stream=%d)",
                    pipeline_name,
                    partition,
                    data_stream_id,
                )
                stream_tasks.append(
                    asyncio.create_task(
                        _wt_data_stream_loop(
                            protocol,
                            data_stream_id,
                            data_buf,
                            codec,
                            signer,
                            batch_signing,
                            pipeline_name,
                            int(partition),
                        )
                    )
                )

        if not stream_tasks:
            logger.warning(
                "no pipeline assignments — WebTransport processor has nothing to do"
            )

        try:
            # 6. Wait for the first data stream task to end; then tear down.
            if stream_tasks:
                done, _pending = await asyncio.wait(
                    stream_tasks, return_when=asyncio.FIRST_COMPLETED
                )
                for t in done:
                    exc = t.exception()
                    if exc is not None and not isinstance(
                        exc, (asyncio.CancelledError, EOFError)
                    ):
                        logger.warning("WT data stream error: %s", exc)
                logger.info("a WT data stream ended, draining remaining streams")
        finally:
            hb_task.cancel()
            try:
                await hb_task
            except asyncio.CancelledError:
                pass
            for t in stream_tasks:
                if not t.done():
                    t.cancel()
            if stream_tasks:
                await asyncio.gather(*stream_tasks, return_exceptions=True)

    logger.info("WebTransport processor disconnected")


def run_webtransport(
    url: str,
    *,
    name: str,
    version: str = "1.0.0",
    private_key_path: Optional[str] = None,
    private_key: Optional[bytes] = None,
    pipelines: Optional[list[str]] = None,
    codec: str = "msgpack",
    insecure: bool = False,
    server_name: Optional[str] = None,
    log_level: str = "INFO",
) -> None:
    """Connect to Aeon over T3 WebTransport and run the registered processor.

    Args:
        url: HTTPS URL of the Aeon WebTransport endpoint
            (e.g. ``https://127.0.0.1:4472/``).
        name: Processor name — must match a registered identity in Aeon.
        version: Processor version string.
        private_key_path: Path to a 32-byte ED25519 seed file.
        private_key: Raw 32-byte ED25519 seed (alternative to
            ``private_key_path``).
        pipelines: List of pipeline names to request.
        codec: Transport codec — ``"msgpack"`` (default) or ``"json"``.
        insecure: If True, disable TLS certificate verification. **Test
            use only** — mirrors the ``webtransport-insecure`` Cargo
            feature in the Rust SDK. Required for Tier D D1 against a
            ``wtransport::Identity::self_signed`` server cert.
        server_name: Override the TLS SNI / :authority header. Useful
            when connecting to a server by IP but the cert's SAN is a
            hostname (e.g. ``server_name="localhost"`` when the URL is
            ``https://127.0.0.1:<port>/``).
        log_level: Logging level.

    Requires the ``aioquic`` package. Install with
    ``pip install 'aeon-processor[webtransport]'``.
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
        "WT processor %s v%s (fingerprint=%s...)",
        name,
        version,
        signer.compute_fingerprint()[:16],
    )

    asyncio.run(
        _run_webtransport_async(
            url=url,
            name=name,
            version=version,
            signer=signer,
            pipelines=pipelines or [],
            codec_name=codec,
            insecure=insecure,
            server_name=server_name,
        )
    )
