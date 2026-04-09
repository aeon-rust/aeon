//! Rust→Wasm JSON enrichment processor.
//!
//! Compiles to wasm32-unknown-unknown. Implements the Aeon processor ABI:
//! - `alloc(size) -> ptr`
//! - `dealloc(ptr, size)`
//! - `process(ptr, len) -> ptr`
//!
//! The processor reads a serialized Event, extracts "user_id" from the JSON
//! payload, and emits an enriched output.
//!
//! Build: cargo build --target wasm32-unknown-unknown --release

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

// ── Bump allocator ──────────────────────────────────────────────────────

const HEAP_SIZE: usize = 256 * 1024; // 256KB
static HEAP_POS: AtomicUsize = AtomicUsize::new(0);

// Heap lives in Wasm linear memory, allocated via memory growth
// We use a fixed region starting after the data/stack

#[unsafe(no_mangle)]
pub extern "C" fn alloc(size: i32) -> i32 {
    let aligned_size = ((size as usize) + 7) & !7;
    let pos = HEAP_POS.fetch_add(aligned_size, Ordering::Relaxed);
    if pos + aligned_size > HEAP_SIZE {
        return 0; // OOM
    }
    // Use a fixed base address in linear memory for our heap (at 128KB offset)
    (131072 + pos) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn dealloc(_ptr: i32, _size: i32) {
    // No-op for bump allocator
}

// ── Wire format helpers ─────────────────────────────────────────────────

fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn write_u32_le(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_le_bytes());
}

// ── JSON field extraction (no_std, no SIMD) ─────────────────────────────

/// Simple JSON field extractor — finds "field_name":"value" and returns the
/// value bytes (without quotes). Works for simple string values only.
fn extract_json_field<'a>(payload: &'a [u8], field: &[u8]) -> Option<&'a [u8]> {
    // Search for "field":
    let mut i = 0;
    while i + field.len() + 3 < payload.len() {
        if payload[i] == b'"' && payload[i + 1..].starts_with(field) {
            let after_field = i + 1 + field.len();
            if after_field < payload.len() && payload[after_field] == b'"' {
                let after_colon = after_field + 1;
                if after_colon < payload.len() && payload[after_colon] == b':' {
                    // Skip colon and optional whitespace
                    let mut val_start = after_colon + 1;
                    while val_start < payload.len()
                        && (payload[val_start] == b' ' || payload[val_start] == b'\t')
                    {
                        val_start += 1;
                    }
                    // Expect opening quote
                    if val_start < payload.len() && payload[val_start] == b'"' {
                        val_start += 1;
                        // Find closing quote
                        let mut val_end = val_start;
                        while val_end < payload.len() && payload[val_end] != b'"' {
                            val_end += 1;
                        }
                        return Some(&payload[val_start..val_end]);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

// ── Process function ────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn process(ptr: i32, len: i32) -> i32 {
    // Reset bump allocator for each call
    HEAP_POS.store(0, Ordering::Relaxed);

    let data = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };

    // Parse serialized event — skip to payload
    let mut pos: usize = 24; // skip UUID (16) + timestamp (8)

    // Skip source string
    let src_len = read_u32_le(data, pos) as usize;
    pos += 4 + src_len;

    // Skip partition
    pos += 2;

    // Skip metadata
    let meta_count = read_u32_le(data, pos) as usize;
    pos += 4;
    for _ in 0..meta_count {
        let kl = read_u32_le(data, pos) as usize;
        pos += 4 + kl;
        let vl = read_u32_le(data, pos) as usize;
        pos += 4 + vl;
    }

    // Read payload
    let payload_len = read_u32_le(data, pos) as usize;
    pos += 4;
    let payload = &data[pos..pos + payload_len];

    // Extract user_id field
    let extracted = extract_json_field(payload, b"user_id");

    // Build output payload
    let output_payload = match extracted {
        Some(value) => {
            let mut buf = Vec::with_capacity(payload_len + 64);
            buf.extend_from_slice(b"{\"original\":");
            buf.extend_from_slice(payload);
            buf.extend_from_slice(b",\"extracted_user_id\":\"");
            buf.extend_from_slice(value);
            buf.extend_from_slice(b"\"}");
            buf
        }
        None => {
            payload.to_vec()
        }
    };

    // Serialize output list
    let dest = b"enriched";
    // Content: count(4) + dest_len(4) + dest + no_key(1) + payload_len(4) + payload + headers(4)
    let content_len = 4 + 4 + dest.len() + 1 + 4 + output_payload.len() + 4;
    let mut result = Vec::with_capacity(4 + content_len);

    // Length prefix
    write_u32_le(&mut result, content_len as u32);
    // Output count = 1
    write_u32_le(&mut result, 1);
    // Destination
    write_u32_le(&mut result, dest.len() as u32);
    result.extend_from_slice(dest);
    // No key
    result.push(0);
    // Payload
    write_u32_le(&mut result, output_payload.len() as u32);
    result.extend_from_slice(&output_payload);
    // Header count = 0
    write_u32_le(&mut result, 0);

    // Write result to guest memory
    let result_ptr = alloc(result.len() as i32);
    if result_ptr == 0 {
        // OOM — return empty list
        let empty_ptr = alloc(8);
        let empty_slice =
            unsafe { core::slice::from_raw_parts_mut(empty_ptr as *mut u8, 8) };
        empty_slice[0..4].copy_from_slice(&4u32.to_le_bytes());
        empty_slice[4..8].copy_from_slice(&0u32.to_le_bytes());
        return empty_ptr;
    }

    let out_slice =
        unsafe { core::slice::from_raw_parts_mut(result_ptr as *mut u8, result.len()) };
    out_slice.copy_from_slice(&result);

    result_ptr
}

// ── Panic handler (required for no_std) ─────────────────────────────────

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

// ── Global allocator for alloc crate ────────────────────────────────────

use core::alloc::{GlobalAlloc, Layout};

struct WasmAllocator;

unsafe impl GlobalAlloc for WasmAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(layout.align());
        let ptr = alloc(size as i32);
        ptr as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // bump allocator — no dealloc
    }
}

#[global_allocator]
static ALLOCATOR: WasmAllocator = WasmAllocator;
