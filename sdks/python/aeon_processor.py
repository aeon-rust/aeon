"""
Aeon Processor SDK for Python (Wasm target via componentize-py).

Provides Event/Output types, binary wire format serialization, and
host function bindings matching the Aeon engine's Wasm ABI.

Build: componentize-py -d processor.wit -w processor componentize app -o processor.wasm
"""

from __future__ import annotations

import struct
from dataclasses import dataclass, field
from typing import Callable, Optional

# ── Types ──────────────────────────────────────────────────────────────


@dataclass
class Header:
    """Key-value metadata header."""

    key: str
    value: str


@dataclass
class Event:
    """Aeon event received from the host engine.

    Wire format (host -> guest):
      [16B: UUID][8B: timestamp i64 LE][4B: source_len][source]
      [2B: partition u16 LE][4B: meta_count]
        for each: [4B: key_len][key][4B: val_len][val]
      [4B: payload_len][payload]
    """

    id: bytes  # 16 bytes UUID
    timestamp: int  # Unix epoch nanoseconds
    source: str
    partition: int
    metadata: list[Header]
    payload: bytes

    def payload_str(self) -> str:
        """Decode payload as UTF-8 string."""
        return self.payload.decode("utf-8", errors="replace")


@dataclass
class Output:
    """Processor output to be sent to a sink.

    Wire format (guest -> host):
      [4B: count]
      per output:
        [4B: dest_len][dest][1B: has_key]
          if has_key: [4B: key_len][key]
        [4B: payload_len][payload][4B: header_count]
          for each: [4B: key_len][key][4B: val_len][val]
    """

    destination: str
    payload: bytes
    key: Optional[bytes] = None
    headers: list[Header] = field(default_factory=list)

    @staticmethod
    def from_str(destination: str, payload: str) -> Output:
        """Create an output with a UTF-8 string payload."""
        return Output(destination=destination, payload=payload.encode("utf-8"))

    def with_key(self, key: bytes) -> Output:
        """Set the partition key."""
        self.key = key
        return self

    def with_key_str(self, key: str) -> Output:
        """Set the partition key from a string."""
        self.key = key.encode("utf-8")
        return self

    def with_header(self, key: str, value: str) -> Output:
        """Add a header."""
        self.headers.append(Header(key=key, value=value))
        return self


# ── Wire Format ────────────────────────────────────────────────────────


def deserialize_event(data: bytes) -> Event:
    """Deserialize an Event from the Aeon binary wire format."""
    pos = 0

    # UUID (16 bytes)
    event_id = data[pos : pos + 16]
    pos += 16

    # Timestamp (i64 LE)
    (timestamp,) = struct.unpack_from("<q", data, pos)
    pos += 8

    # Source (length-prefixed)
    (src_len,) = struct.unpack_from("<I", data, pos)
    pos += 4
    source = data[pos : pos + src_len].decode("utf-8", errors="replace")
    pos += src_len

    # Partition (u16 LE)
    (partition,) = struct.unpack_from("<H", data, pos)
    pos += 2

    # Metadata
    (meta_count,) = struct.unpack_from("<I", data, pos)
    pos += 4
    metadata: list[Header] = []
    for _ in range(meta_count):
        (kl,) = struct.unpack_from("<I", data, pos)
        pos += 4
        k = data[pos : pos + kl].decode("utf-8", errors="replace")
        pos += kl
        (vl,) = struct.unpack_from("<I", data, pos)
        pos += 4
        v = data[pos : pos + vl].decode("utf-8", errors="replace")
        pos += vl
        metadata.append(Header(key=k, value=v))

    # Payload (length-prefixed)
    (payload_len,) = struct.unpack_from("<I", data, pos)
    pos += 4
    payload = data[pos : pos + payload_len]

    return Event(
        id=event_id,
        timestamp=timestamp,
        source=source,
        partition=partition,
        metadata=metadata,
        payload=payload,
    )


def serialize_outputs(outputs: list[Output]) -> bytes:
    """Serialize a list of Outputs to the Aeon binary wire format.

    Returns the serialized bytes WITHOUT the 4-byte length prefix
    (the caller/ABI layer prepends that).
    """
    parts: list[bytes] = []

    # Output count
    parts.append(struct.pack("<I", len(outputs)))

    for out in outputs:
        # Destination
        dest_bytes = out.destination.encode("utf-8")
        parts.append(struct.pack("<I", len(dest_bytes)))
        parts.append(dest_bytes)

        # Key
        if out.key is not None:
            parts.append(b"\x01")
            parts.append(struct.pack("<I", len(out.key)))
            parts.append(out.key)
        else:
            parts.append(b"\x00")

        # Payload
        parts.append(struct.pack("<I", len(out.payload)))
        parts.append(out.payload)

        # Headers
        parts.append(struct.pack("<I", len(out.headers)))
        for h in out.headers:
            kb = h.key.encode("utf-8")
            vb = h.value.encode("utf-8")
            parts.append(struct.pack("<I", len(kb)))
            parts.append(kb)
            parts.append(struct.pack("<I", len(vb)))
            parts.append(vb)

    return b"".join(parts)


# ── Host Bindings ──────────────────────────────────────────────────────
#
# These are stubs that get replaced by the actual Wasm host imports at
# runtime. When running under componentize-py or any Wasm host that
# provides the "env" module, these will be linked to the real functions.
#
# For native testing, they are no-ops.


def _not_in_wasm(*_args, **_kwargs):
    """Stub for host functions when running outside Wasm."""
    return None


try:
    # When running inside Aeon Wasm runtime, these are provided by the host
    from env import (  # type: ignore[import-not-found]
        state_get as _state_get,
        state_put as _state_put,
        state_delete as _state_delete,
        log_info as _log_info,
        log_warn as _log_warn,
        log_error as _log_error,
        clock_now_ms as _clock_now_ms,
        metrics_counter_inc as _metrics_counter_inc,
        metrics_gauge_set as _metrics_gauge_set,
    )
except ImportError:
    _state_get = _not_in_wasm
    _state_put = _not_in_wasm
    _state_delete = _not_in_wasm
    _log_info = _not_in_wasm
    _log_warn = _not_in_wasm
    _log_error = _not_in_wasm
    _clock_now_ms = _not_in_wasm
    _metrics_counter_inc = _not_in_wasm
    _metrics_gauge_set = _not_in_wasm


# ── Safe Host API Wrappers ─────────────────────────────────────────────


def state_get(key: bytes) -> Optional[bytes]:
    """Get a value from the host state store. Returns None if not found."""
    result = _state_get(key)
    return result if result is not None else None


def state_put(key: bytes, value: bytes) -> None:
    """Put a key-value pair into the host state store."""
    _state_put(key, value)


def state_delete(key: bytes) -> None:
    """Delete a key from the host state store."""
    _state_delete(key)


def log_info(msg: str) -> None:
    """Log at info level."""
    _log_info(msg.encode("utf-8"))


def log_warn(msg: str) -> None:
    """Log at warn level."""
    _log_warn(msg.encode("utf-8"))


def log_error(msg: str) -> None:
    """Log at error level."""
    _log_error(msg.encode("utf-8"))


def clock_now_ms() -> int:
    """Get current time in milliseconds since Unix epoch."""
    result = _clock_now_ms()
    return result if result is not None else 0


def counter_inc(name: str, delta: int = 1) -> None:
    """Increment a counter metric."""
    _metrics_counter_inc(name.encode("utf-8"), delta)


def gauge_set(name: str, value: float) -> None:
    """Set a gauge metric value."""
    _metrics_gauge_set(name.encode("utf-8"), value)


# ── Processor Registration ─────────────────────────────────────────────

ProcessorFn = Callable[[Event], list[Output]]

_processor_fn: Optional[ProcessorFn] = None


def register_processor(fn: ProcessorFn) -> None:
    """Register the processor function.

    The function receives an Event and returns a list of Outputs.
    Must be called exactly once at module load time.
    """
    global _processor_fn
    _processor_fn = fn


def process(event_bytes: bytes) -> bytes:
    """Entry point called by the Aeon Wasm host.

    Deserializes the event, calls the registered processor, and
    returns serialized outputs with a 4-byte length prefix.
    """
    if _processor_fn is None:
        # No processor registered — return empty output list
        return struct.pack("<II", 4, 0)

    event = deserialize_event(event_bytes)
    outputs = _processor_fn(event)
    serialized = serialize_outputs(outputs)

    # Prepend 4-byte length prefix
    return struct.pack("<I", len(serialized)) + serialized
