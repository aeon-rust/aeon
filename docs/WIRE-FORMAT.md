# Wasm Processor Wire Format Specification

This document defines the binary ABI between the Aeon host runtime and Wasm guest processors.

## Overview

Communication between host and guest uses a simple length-prefixed binary format. No external serialization library is required — the format can be implemented in any language with basic byte manipulation.

```
Host                              Guest (.wasm)
  |                                  |
  |-- alloc(event_size) ------------>|  returns ptr
  |-- write event to ptr ----------->|
  |-- process(ptr, len) ------------>|  returns result_ptr
  |<-- read result from result_ptr --|
  |-- dealloc(ptr, size) ----------->|
  |-- dealloc(result_ptr, size) ---->|
```

## Guest Exports (Required)

Every Wasm processor must export these three functions and a `memory`:

| Export | Signature | Description |
|--------|-----------|-------------|
| `alloc` | `(size: i32) -> i32` | Allocate `size` bytes in guest memory. Return pointer. Return 0 on failure. |
| `dealloc` | `(ptr: i32, size: i32)` | Free previously allocated memory. May be a no-op for bump allocators. |
| `process` | `(ptr: i32, len: i32) -> i32` | Process the serialized event at `[ptr, ptr+len)`. Return pointer to serialized output list. |
| `memory` | `Memory` | The guest's linear memory, exported for host read/write access. |

## Host Imports (Optional)

The guest may import any of these functions from the `"env"` module:

### State Operations

```
state_get(key_ptr: i32, key_len: i32, out_ptr: i32, out_max_len: i32) -> i32
```
Read a value by key. The host writes the value to `[out_ptr, out_ptr+min(value_len, out_max_len))`. Returns the actual value length, or **-1** if the key is not found. State is automatically namespaced per processor.

```
state_put(key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32)
```
Store a key-value pair.

```
state_delete(key_ptr: i32, key_len: i32)
```
Delete a key.

### Logging

```
log_info(ptr: i32, len: i32)
log_warn(ptr: i32, len: i32)
log_error(ptr: i32, len: i32)
```
Log a UTF-8 message at the given level. The host reads `[ptr, ptr+len)` from guest memory.

### Time

```
clock_now_ms() -> i64
```
Returns the current time as milliseconds since Unix epoch.

### Metrics

```
metrics_counter_inc(name_ptr: i32, name_len: i32, delta: i64)
```
Increment a named counter by `delta`.

```
metrics_gauge_set(name_ptr: i32, name_len: i32, value: f64)
```
Set a named gauge to `value`.

### AssemblyScript Compatibility

```
abort(msg_ptr: i32, file_ptr: i32, line: i32, col: i32)
```
Called by AssemblyScript on assertion failures. The host logs the error.

---

## Event Format (Host -> Guest)

The host serializes an Event into this binary format and writes it into guest memory via `alloc` + memory write.

All integers are **little-endian**.

```
Offset  Size    Field
──────  ──────  ─────────────────────────
0       16      Event ID (UUID, 16 raw bytes)
16      8       Timestamp (i64 LE, Unix epoch nanoseconds)
24      4       Source string length (u32 LE)
28      N       Source string (UTF-8 bytes, N = source length)
28+N    2       Partition (u16 LE)
30+N    4       Metadata count (u32 LE, number of key-value pairs)
34+N    var     Metadata entries (see below)
var     4       Payload length (u32 LE)
var+4   M       Payload bytes (M = payload length)
```

### Metadata Entry Format

Each metadata entry:
```
4 bytes     Key length (u32 LE)
K bytes     Key (UTF-8)
4 bytes     Value length (u32 LE)
V bytes     Value (UTF-8)
```

### Parsing Example (pseudocode)

```
pos = 0
uuid = data[pos..pos+16];      pos += 16
timestamp = read_i64_le(pos);   pos += 8
src_len = read_u32_le(pos);     pos += 4
source = data[pos..pos+src_len]; pos += src_len
partition = read_u16_le(pos);   pos += 2
meta_count = read_u32_le(pos);  pos += 4
for i in 0..meta_count:
    key_len = read_u32_le(pos); pos += 4
    key = data[pos..pos+key_len]; pos += key_len
    val_len = read_u32_le(pos); pos += 4
    val = data[pos..pos+val_len]; pos += val_len
payload_len = read_u32_le(pos); pos += 4
payload = data[pos..pos+payload_len]
```

---

## Output List Format (Guest -> Host)

The guest returns a pointer to this structure. The **first 4 bytes** are the length of the remaining content. The host reads this length, then reads the content.

```
Offset  Size    Field
──────  ──────  ─────────────────────────
0       4       Content length (u32 LE, bytes after this field)
4       4       Output count (u32 LE)
8       var     Output entries (see below)
```

### Output Entry Format

Each output entry:

```
4 bytes     Destination length (u32 LE)
D bytes     Destination string (UTF-8)
1 byte      Has key? (0 = no, 1 = yes)
  if has_key:
    4 bytes   Key length (u32 LE)
    K bytes   Key bytes
4 bytes     Payload length (u32 LE)
P bytes     Payload bytes
4 bytes     Header count (u32 LE)
  for each header:
    4 bytes   Header key length (u32 LE)
    HK bytes  Header key (UTF-8)
    4 bytes   Header value length (u32 LE)
    HV bytes  Header value (UTF-8)
```

### Example: Single Output, No Key, No Headers

For destination `"output"` (6 bytes) and payload `"hello"` (5 bytes):

```
Content length: 4 + 4 + 6 + 1 + 4 + 5 + 4 = 28

Hex dump:
  1C 00 00 00   # content length = 28
  01 00 00 00   # output count = 1
  06 00 00 00   # destination length = 6
  6F 75 74 70 75 74  # "output"
  00            # no key
  05 00 00 00   # payload length = 5
  68 65 6C 6C 6F     # "hello"
  00 00 00 00   # header count = 0
```

### Example: Empty Output List (Filter/Drop)

```
Hex dump:
  04 00 00 00   # content length = 4
  00 00 00 00   # output count = 0
```

### Example: Output With Key and One Header

For destination `"topic"`, key `"k1"`, payload `"data"`, header `("h","v")`:

```
Content length: 4 + 4 + 5 + 1 + 4 + 2 + 4 + 4 + 4 + 4 + 1 + 4 + 1 = 42

  2A 00 00 00   # content length = 42
  01 00 00 00   # output count = 1
  05 00 00 00   # destination length = 5
  74 6F 70 69 63  # "topic"
  01            # has key = yes
  02 00 00 00   # key length = 2
  6B 31        # "k1"
  04 00 00 00   # payload length = 4
  64 61 74 61  # "data"
  01 00 00 00   # header count = 1
  01 00 00 00   # header key length = 1
  68           # "h"
  01 00 00 00   # header value length = 1
  76           # "v"
```

---

## Memory Model

### Bump Allocator Pattern

The recommended pattern for guest allocators is a **bump allocator** that resets at the start of each `process()` call:

```
[0 .. 64KB)          Data segment, stack (page 0)
[64KB .. 128KB)      Reserved / data (page 1)
[128KB .. 384KB)     Heap (bump allocator region)
```

- `alloc()` returns the current bump pointer and advances it
- `dealloc()` is a no-op
- At the start of `process()`, reset the bump pointer to the heap base
- The host calls `alloc()` before `process()` to place the event data, then calls `process()`

This pattern is simple, fast, and avoids fragmentation.

### Memory Requirements

| Scenario | Minimum Pages |
|----------|---------------|
| Simple passthrough | 2 (128KB) |
| JSON enrichment | 4 (256KB) |
| Stateful with large state | 8+ (512KB+) |

Configure via `WasmConfig::max_memory_bytes`.

---

## Fuel Metering

Each `process()` call is given a fuel budget. Every Wasm instruction consumes fuel. When fuel runs out, the call traps with an error.

Default: **1,000,000 fuel units** per call (roughly 1M instructions).

Configure via `WasmConfig::max_fuel`:
- `Some(1_000_000)` — Default, good for simple transforms
- `Some(10_000_000)` — For complex processors
- `None` — No limit (not recommended for untrusted code)

---

## Namespace Isolation

Each `WasmProcessor` has a namespace (from `WasmConfig::namespace`). All state keys are automatically prefixed with `{namespace}:` by the host. This means:

- Processor A (namespace `"orders"`) stores key `"count"` -> actually stored as `"orders:count"`
- Processor B (namespace `"users"`) stores key `"count"` -> actually stored as `"users:count"`
- No cross-processor state leakage is possible

---

## Implementing in Other Languages

The wire format is simple enough to implement in any language that compiles to Wasm:

- **Go**: Use TinyGo (`tinygo build -target wasm`)
- **C/C++**: Use Clang with `--target=wasm32` or Emscripten
- **Zig**: Native Wasm target support
- **Grain**: Compiles to Wasm natively

The key requirements:
1. Export `alloc`, `dealloc`, `process`, and `memory`
2. Read the event wire format (little-endian integers + length-prefixed bytes)
3. Write the output wire format
4. Optionally import host functions from the `"env"` module
