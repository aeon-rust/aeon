# Processor Development Guide

This guide covers how to build event processors for Aeon using **T1 (Native)** and
**T2 (Wasm)** in-process runtimes. For T3 (WebTransport) and T4 (WebSocket) network
processors — which support Python, Go, Node.js, Java, PHP, C#, and more — see
[FOUR-TIER-PROCESSORS.md](FOUR-TIER-PROCESSORS.md) and the SDKs in `sdks/`.

| Runtime | Tier | Latency | Throughput | Best For |
|---------|------|---------|------------|----------|
| **Rust-native** | T1 | ~240ns | ~4.2M/sec | Maximum performance, trusted code |
| **C/C++ native** | T1 | ~240ns | ~4.2M/sec | C-ABI, cross-language native |
| **.NET NativeAOT** | T1 | ~240ns | ~4.2M/sec | C# teams, AOT-compiled to native |
| **Rust-Wasm (SDK)** | T2 | ~1.2us | ~820K/sec | Sandboxed Rust, minimal boilerplate |
| **Rust-Wasm (raw)** | T2 | ~1.2us | ~820K/sec | Sandboxed Rust, full ABI control |
| **AssemblyScript-Wasm** | T2 | ~1.1us | ~940K/sec | TypeScript developers, rapid prototyping |

All produce the same result. Choose based on your team's language preference and whether you need Wasm sandboxing (fuel limits, memory isolation, namespace separation). For Rust-Wasm, the SDK (Option 2) is recommended over raw ABI (Option 3) unless you need low-level control.

---

## Concepts

### Event Flow

```
Source -> [Event] -> Processor -> [Output] -> Sink
```

- **Event**: The input. Contains an ID (UUIDv7), timestamp, source name, partition, metadata headers, and a payload (bytes).
- **Output**: The result. Contains a destination name, optional partition key, payload (bytes), and headers.
- A processor receives one Event and returns zero or more Outputs.
- Returning zero outputs = filtering (dropping the event).
- Returning multiple outputs = fan-out.

### Host Functions (Wasm only)

Wasm processors can call these host-provided functions:

| Function | Signature | Description |
|----------|-----------|-------------|
| `state_get` | `(key_ptr, key_len, out_ptr, out_max_len) -> i32` | Get value by key. Returns length, or -1 if not found |
| `state_put` | `(key_ptr, key_len, val_ptr, val_len)` | Store a key-value pair |
| `state_delete` | `(key_ptr, key_len)` | Delete a key |
| `log_info` | `(ptr, len)` | Log at info level |
| `log_warn` | `(ptr, len)` | Log at warn level |
| `log_error` | `(ptr, len)` | Log at error level |
| `clock_now_ms` | `() -> i64` | Current time in epoch milliseconds |
| `metrics_counter_inc` | `(name_ptr, name_len, delta: i64)` | Increment a counter |
| `metrics_gauge_set` | `(name_ptr, name_len, value: f64)` | Set a gauge value |

All host functions are imported from the `"env"` module.

---

## Option 1: Rust-Native Processor

This is the fastest option. You implement the `Processor` trait directly.

### Step 1: Create a Crate

```bash
mkdir -p my-processor/src
cd my-processor
```

`Cargo.toml`:
```toml
[package]
name = "my-processor"
version = "0.1.0"
edition = "2024"

[dependencies]
aeon-types = { path = "../crates/aeon-types" }
bytes = "1"
```

### Step 2: Implement the Processor Trait

`src/lib.rs`:
```rust
use std::sync::Arc;
use aeon_types::event::{Event, Output};
use aeon_types::{AeonError, Processor};
use bytes::Bytes;

pub struct MyProcessor;

impl Processor for MyProcessor {
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
        // Your logic here. This example uppercases the payload.
        let payload = event.payload.to_vec();
        let upper: Vec<u8> = payload.iter().map(|b| b.to_ascii_uppercase()).collect();

        Ok(vec![
            Output::new(Arc::from("output-topic"), Bytes::from(upper))
                .with_source_ts(event.source_ts),
        ])
    }
}
```

### Step 3: Test

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::partition::PartitionId;

    #[test]
    fn uppercases_payload() {
        let proc = MyProcessor;
        let event = Event::new(
            uuid::Uuid::nil(),
            0,
            Arc::from("test"),
            PartitionId::new(0),
            Bytes::from_static(b"hello"),
        );
        let outputs = proc.process(event).unwrap();
        assert_eq!(outputs[0].payload.as_ref(), b"HELLO");
    }
}
```

### Key APIs

```rust
// Event fields you can read:
event.id          // uuid::Uuid — UUIDv7 event ID
event.timestamp   // i64 — Unix epoch nanoseconds
event.source      // Arc<str> — Source identifier
event.partition   // PartitionId — Partition number
event.metadata    // SmallVec<[(Arc<str>, Arc<str>); 4]> — Key-value headers
event.payload     // Bytes — Zero-copy payload
event.source_ts   // Option<Instant> — For latency tracking

// Output construction:
Output::new(destination: Arc<str>, payload: Bytes)
    .with_key(key: Bytes)                        // Optional partition key
    .with_header(key: Arc<str>, value: Arc<str>) // Add a header
    .with_source_ts(ts: Option<Instant>)         // Propagate latency tracking

// Filtering (drop an event):
Ok(vec![])

// Fan-out (emit multiple outputs):
Ok(vec![output1, output2, output3])

// SIMD-accelerated JSON field extraction (no parsing!):
use aeon_types::json_field_value;
let value: Option<&[u8]> = json_field_value("user_id", &event.payload);
```

### Reference

See `samples/processors/rust-native/src/lib.rs` for a complete JSON enrichment example.

---

## Option 2: Rust-Wasm Processor (SDK — Recommended)

The `aeon-wasm-sdk` crate eliminates all manual boilerplate (bump allocator, wire format parsing, ABI exports). You write a plain function; the `aeon_processor!` macro generates everything else.

### Step 1: Install the Target

```bash
rustup target add wasm32-unknown-unknown
```

### Step 2: Create a Crate

```bash
mkdir -p my-wasm-processor/src
cd my-wasm-processor
```

`Cargo.toml`:
```toml
[package]
name = "my-wasm-processor"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
aeon-wasm-sdk = { path = "../crates/aeon-wasm-sdk" }

[profile.release]
opt-level = "s"
lto = true
strip = true
```

### Step 3: Implement Your Processor

`src/lib.rs`:
```rust
#![no_std]
extern crate alloc;

use aeon_wasm_sdk::prelude::*;

fn my_processor(event: Event) -> Vec<Output> {
    // Access event fields
    let payload = &event.payload;

    // Your logic here — this example uppercases the payload
    let upper: Vec<u8> = payload.iter().map(|b| b.to_ascii_uppercase()).collect();

    // Return one or more outputs (or empty vec to filter/drop)
    vec![Output::new("output-topic", upper)]
}

// This macro generates: allocator, ABI exports, panic handler, global allocator
aeon_processor!(my_processor);
```

That's it. Compare this with Option 3 (raw Wasm) which requires ~100 lines of manual wire format code.

### SDK Event and Output API

```rust
// Event fields:
event.id          // [u8; 16] — UUIDv7 bytes
event.timestamp   // i64 — Unix epoch nanoseconds
event.source      // String — Source identifier
event.partition   // u16 — Partition number
event.metadata    // Vec<(String, String)> — Key-value headers
event.payload     // Vec<u8> — Payload bytes
event.payload_str() // Option<&str> — Payload as UTF-8 (if valid)

// Output construction:
Output::new("destination", payload_bytes)      // Basic output
Output::from_str("destination", "text payload") // From string
    .with_key(key_bytes)                        // Optional partition key
    .with_key_str("key")                        // Key from string
    .with_header("key", "value")                // Add a header

// Filtering (drop an event):
vec![]

// Fan-out (emit multiple outputs):
vec![output1, output2, output3]
```

### SDK Host Imports

The SDK provides ergonomic wrappers for all host functions:

```rust
use aeon_wasm_sdk::{state, log, metrics, clock};

// State operations (namespaced per processor)
state::put(b"my-key", b"my-value");
let value: Option<Vec<u8>> = state::get(b"my-key");
state::delete(b"my-key");

// Logging
log::info("processing event");
log::warn("unusual payload detected");
log::error("failed to parse JSON");

// Metrics
metrics::counter_inc("events_processed", 1);
metrics::gauge_set("queue_depth", 42.0);

// Clock
let now_ms: i64 = clock::now_ms();
```

### Step 4: Build

```bash
cargo build --target wasm32-unknown-unknown --release
# Output: target/wasm32-unknown-unknown/release/my_wasm_processor.wasm
ls -lh target/wasm32-unknown-unknown/release/*.wasm  # Typically 2-5KB
```

### Step 5: Load and Run

Same as any Wasm processor — load with `WasmModule::from_bytes()` or deploy via CLI:

```bash
aeon processor register my-processor --version 1.0.0 \
  --artifact target/wasm32-unknown-unknown/release/my_wasm_processor.wasm \
  --runtime wasm
```

### Reference

See `samples/processors/rust-wasm-sdk/src/lib.rs` for a complete JSON enrichment example.

---

## Option 3: Rust-Wasm Processor (Raw ABI)

Compile Rust to WebAssembly with full control over the ABI. Use this only if you need to optimize beyond what the SDK provides or need custom memory management.

### Step 1: Install the Target

```bash
rustup target add wasm32-unknown-unknown
```

### Step 2: Create a Crate

```bash
mkdir -p my-wasm-processor/src
cd my-wasm-processor
```

`Cargo.toml`:
```toml
[package]
name = "my-wasm-processor"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[profile.release]
opt-level = "s"
lto = true
strip = true
```

### Step 3: Implement the ABI

Your Wasm module must export three functions:

| Export | Signature | Description |
|--------|-----------|-------------|
| `alloc` | `(size: i32) -> i32` | Allocate `size` bytes, return pointer |
| `dealloc` | `(ptr: i32, size: i32)` | Free memory (can be no-op for bump allocator) |
| `process` | `(ptr: i32, len: i32) -> i32` | Process event at `ptr`, return pointer to output |

`src/lib.rs`:
```rust
#![no_std]
extern crate alloc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

// ── Bump allocator ──────────────────────────────────────────────────
const HEAP_SIZE: usize = 256 * 1024;
static HEAP_POS: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
pub extern "C" fn alloc(size: i32) -> i32 {
    let aligned = ((size as usize) + 7) & !7;
    let pos = HEAP_POS.fetch_add(aligned, Ordering::Relaxed);
    if pos + aligned > HEAP_SIZE { return 0; }
    (131072 + pos) as i32  // Base at 128KB
}

#[unsafe(no_mangle)]
pub extern "C" fn dealloc(_ptr: i32, _size: i32) {}

// ── Host imports (optional) ─────────────────────────────────────────
unsafe extern "C" {
    #[link_name = "log_info"]
    fn host_log_info(ptr: i32, len: i32);

    #[link_name = "state_get"]
    fn host_state_get(key_ptr: i32, key_len: i32, out_ptr: i32, out_max: i32) -> i32;

    #[link_name = "state_put"]
    fn host_state_put(key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32);
}

// ── Wire format helpers ─────────────────────────────────────────────
fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off+1], data[off+2], data[off+3]])
}

fn write_u32(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_le_bytes());
}

// ── Process function ────────────────────────────────────────────────
#[unsafe(no_mangle)]
pub extern "C" fn process(ptr: i32, len: i32) -> i32 {
    HEAP_POS.store(0, Ordering::Relaxed); // Reset allocator each call

    let data = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };

    // Skip to payload (see docs/WIRE-FORMAT.md for full layout)
    let mut pos: usize = 24; // UUID(16) + timestamp(8)
    let src_len = read_u32(data, pos) as usize;
    pos += 4 + src_len;         // source string
    pos += 2;                    // partition
    let meta_count = read_u32(data, pos) as usize;
    pos += 4;
    for _ in 0..meta_count {
        let kl = read_u32(data, pos) as usize; pos += 4 + kl;
        let vl = read_u32(data, pos) as usize; pos += 4 + vl;
    }
    let payload_len = read_u32(data, pos) as usize;
    pos += 4;
    let payload = &data[pos..pos + payload_len];

    // ── YOUR LOGIC HERE ────────────────────────────────────────
    // This example: uppercase the payload
    let mut upper = Vec::with_capacity(payload_len);
    for &b in payload {
        upper.push(b.to_ascii_uppercase());
    }
    // ── END YOUR LOGIC ─────────────────────────────────────────

    // Build output (see docs/WIRE-FORMAT.md for output format)
    let dest = b"output-topic";
    let content_len = 4 + 4 + dest.len() + 1 + 4 + upper.len() + 4;
    let mut result = Vec::with_capacity(4 + content_len);
    write_u32(&mut result, content_len as u32); // length prefix
    write_u32(&mut result, 1);                   // output count
    write_u32(&mut result, dest.len() as u32);   // destination length
    result.extend_from_slice(dest);              // destination
    result.push(0);                              // no key
    write_u32(&mut result, upper.len() as u32);  // payload length
    result.extend_from_slice(&upper);            // payload
    write_u32(&mut result, 0);                   // header count

    // Write to guest memory
    let out_ptr = alloc(result.len() as i32);
    let out = unsafe { core::slice::from_raw_parts_mut(out_ptr as *mut u8, result.len()) };
    out.copy_from_slice(&result);
    out_ptr
}

// ── Required for no_std ─────────────────────────────────────────────
#[cfg(not(test))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

use core::alloc::{GlobalAlloc, Layout};
struct A;
unsafe impl GlobalAlloc for A {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 { alloc(l.size() as i32) as *mut u8 }
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}
#[global_allocator]
static ALLOC: A = A;
```

### Step 4: Build

```bash
cargo build --target wasm32-unknown-unknown --release
# Output: target/wasm32-unknown-unknown/release/my_wasm_processor.wasm
ls -lh target/wasm32-unknown-unknown/release/*.wasm  # Typically 2-5KB
```

### Step 5: Load and Run

```rust
use std::sync::Arc;
use aeon_wasm::{WasmConfig, WasmModule, WasmProcessor};

let wasm_bytes = std::fs::read("path/to/my_wasm_processor.wasm")?;
let config = WasmConfig {
    max_fuel: Some(10_000_000),    // CPU budget per process() call
    max_memory_bytes: 64 * 1024 * 1024, // 64MB memory limit
    namespace: "my-processor".into(), // State isolation namespace
    ..Default::default()
};
let module = WasmModule::from_bytes(&wasm_bytes, config)?;
let processor = WasmProcessor::new(Arc::new(module))?;

// Use it like any Processor:
use aeon_types::Processor;
let outputs = processor.process(event)?;
```

### Reference

See `samples/processors/rust-wasm/src/lib.rs` for a complete JSON enrichment example.

---

## Option 4: AssemblyScript-Wasm Processor

AssemblyScript is a TypeScript-like language that compiles to WebAssembly. Great for teams familiar with TypeScript.

### Step 1: Set Up Project

```bash
mkdir my-as-processor && cd my-as-processor
npm init -y
# Change "type" to "module" in package.json
npm install --save-dev assemblyscript
npx asinit . --yes
```

### Step 2: Implement the Processor

`assembly/index.ts`:
```typescript
// Bump allocator — starts at 128KB (page 2)
let heapOffset: i32 = 131072;

export function alloc(size: i32): i32 {
  const ptr = heapOffset;
  heapOffset += (size + 7) & ~7;
  return ptr;
}

export function dealloc(ptr: i32, size: i32): void {}

// Host imports (optional)
@external("env", "log_info")
declare function host_log_info(ptr: i32, len: i32): void;

@external("env", "state_get")
declare function host_state_get(kp: i32, kl: i32, op: i32, om: i32): i32;

@external("env", "state_put")
declare function host_state_put(kp: i32, kl: i32, vp: i32, vl: i32): void;

// Wire format helpers
function readU32LE(ptr: i32): u32 {
  return load<u8>(ptr)
    | (load<u8>(ptr + 1) << 8)
    | (load<u8>(ptr + 2) << 16)
    | (load<u8>(ptr + 3) << 24);
}

function writeU32LE(ptr: i32, val: u32): void {
  store<u8>(ptr, val & 0xFF);
  store<u8>(ptr + 1, (val >> 8) & 0xFF);
  store<u8>(ptr + 2, (val >> 16) & 0xFF);
  store<u8>(ptr + 3, (val >> 24) & 0xFF);
}

export function process(ptr: i32, len: i32): i32 {
  heapOffset = 131072; // Reset allocator

  // Skip to payload (see docs/WIRE-FORMAT.md)
  let pos = ptr + 24; // UUID(16) + timestamp(8)
  const srcLen = readU32LE(pos) as i32;
  pos += 4 + srcLen;
  pos += 2; // partition
  const metaCount = readU32LE(pos) as i32;
  pos += 4;
  for (let m: i32 = 0; m < metaCount; m++) {
    const kl = readU32LE(pos) as i32; pos += 4 + kl;
    const vl = readU32LE(pos) as i32; pos += 4 + vl;
  }
  const payloadLen = readU32LE(pos) as i32;
  pos += 4;
  const payloadPtr = pos;

  // ── YOUR LOGIC HERE ────────────────────────────────────────
  // This example: uppercase the payload
  const outPayloadPtr = alloc(payloadLen);
  for (let i: i32 = 0; i < payloadLen; i++) {
    let b = load<u8>(payloadPtr + i);
    if (b >= 0x61 && b <= 0x7A) b -= 0x20; // lowercase -> uppercase
    store<u8>(outPayloadPtr + i, b);
  }
  const outPayloadLen = payloadLen;
  // ── END YOUR LOGIC ─────────────────────────────────────────

  // Build output (see docs/WIRE-FORMAT.md)
  const destLen: i32 = 12; // "output-topic"
  const contentLen: i32 = 4 + 4 + destLen + 1 + 4 + outPayloadLen + 4;
  const resultPtr = alloc(4 + contentLen);
  let wp = resultPtr;

  writeU32LE(wp, contentLen as u32); wp += 4;  // length prefix
  writeU32LE(wp, 1); wp += 4;                   // output count = 1
  writeU32LE(wp, destLen as u32); wp += 4;       // dest length

  // "output-topic" in ASCII
  store<u8>(wp, 0x6F); store<u8>(wp+1, 0x75); store<u8>(wp+2, 0x74);
  store<u8>(wp+3, 0x70); store<u8>(wp+4, 0x75); store<u8>(wp+5, 0x74);
  store<u8>(wp+6, 0x2D); store<u8>(wp+7, 0x74); store<u8>(wp+8, 0x6F);
  store<u8>(wp+9, 0x70); store<u8>(wp+10, 0x69); store<u8>(wp+11, 0x63);
  wp += destLen;

  store<u8>(wp, 0); wp += 1;                    // no key
  writeU32LE(wp, outPayloadLen as u32); wp += 4; // payload length
  memory.copy(wp, outPayloadPtr, outPayloadLen); wp += outPayloadLen;
  writeU32LE(wp, 0);                             // 0 headers

  return resultPtr;
}
```

### Step 3: Build

```bash
npx asc assembly/index.ts \
  --outFile build/processor.wasm \
  --optimize \
  --exportStart '' \
  --runtime stub \
  --noAssert \
  --initialMemory 4 \
  --maximumMemory 8

ls -lh build/processor.wasm  # Typically 1-3KB
```

### Step 4: Load and Run

Same as Rust-Wasm — load the `.wasm` file with `WasmModule::from_bytes()`.

### Reference

See `samples/processors/assemblyscript-wasm/assembly/index.ts` for a complete JSON enrichment example.

---

## Using State (Wasm Processors)

Wasm processors can read/write persistent state via host functions. State is namespaced per processor to prevent leaks between processors.

```rust
// Rust-Wasm example: counting events per user

// Store counter in state
let key = b"count:user-42";
let key_ptr = /* write key to memory */;

// Read current count
let buf_ptr = alloc(8);
let len = unsafe { host_state_get(key_ptr, key.len() as i32, buf_ptr, 8) };

let count: u64 = if len > 0 {
    // Parse existing count
    let bytes = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len as usize) };
    u64::from_le_bytes(bytes.try_into().unwrap_or([0;8]))
} else {
    0
};

// Increment and store
let new_count = count + 1;
let val = new_count.to_le_bytes();
let val_ptr = alloc(8);
unsafe {
    core::slice::from_raw_parts_mut(val_ptr as *mut u8, 8).copy_from_slice(&val);
    host_state_put(key_ptr, key.len() as i32, val_ptr, 8);
}
```

---

## Filtering and Routing

### Drop an event (filter)

Return an empty output list:

```rust
// Rust-native
fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
    if should_drop(&event) {
        return Ok(vec![]); // Event is filtered out
    }
    // ... normal processing
}
```

For Wasm: set output count to 0 in the serialized response.

### Route to different destinations

Set different destination names on each output:

```rust
fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
    let dest = if event.payload.starts_with(b"{\"type\":\"order\"") {
        "orders-topic"
    } else {
        "other-topic"
    };
    Ok(vec![Output::new(Arc::from(dest), event.payload.clone())])
}
```

### Fan-out (emit multiple outputs)

Return multiple outputs:

```rust
fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {
    Ok(vec![
        Output::new(Arc::from("archive"), event.payload.clone()),
        Output::new(Arc::from("analytics"), event.payload.clone()),
        Output::new(Arc::from("alerts"), event.payload.clone()),
    ])
}
```

---

## Benchmarking Your Processor

### Rust-native

Add criterion to your crate and benchmark directly:

```rust
use criterion::{criterion_group, criterion_main, Criterion, black_box};

fn bench_my_processor(c: &mut Criterion) {
    let proc = MyProcessor;
    c.bench_function("my_processor", |b| {
        b.iter(|| {
            let event = make_test_event();
            proc.process(black_box(event)).unwrap();
        });
    });
}
```

### Wasm

Load your `.wasm` and benchmark through `WasmProcessor`:

```rust
let wasm = std::fs::read("path/to/processor.wasm").unwrap();
let module = WasmModule::from_bytes(&wasm, WasmConfig::default()).unwrap();
let proc = WasmProcessor::new(Arc::new(module)).unwrap();

c.bench_function("my_wasm_processor", |b| {
    b.iter(|| {
        let event = make_test_event();
        Processor::process(&proc, black_box(event)).unwrap();
    });
});
```

---

## Tips

1. **Start with Rust-native** for development, switch to Wasm when you need sandboxing
2. **Use the Wasm SDK** (Option 2) over raw ABI (Option 3) unless you need low-level control
3. **Use `json_field_value()`** for JSON routing — it's SIMD-accelerated and avoids full JSON parsing
4. **Keep payloads as bytes** — don't deserialize to String unless necessary
5. **Propagate `source_ts`** via `.with_source_ts(event.source_ts)` for latency tracking
6. **Reset your bump allocator** at the start of each `process()` call to avoid OOM (raw ABI only; the SDK handles this automatically)
7. **Wasm binaries are tiny** (1-5KB) — deployment is essentially zero-overhead
8. **Fuel metering** protects against infinite loops — set `max_fuel` appropriately
9. **Use `aeon validate`** CLI command to verify your `.wasm` or `.so`/`.dll` artifact before deploying
