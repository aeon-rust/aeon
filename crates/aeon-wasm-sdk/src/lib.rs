//! # Aeon Wasm Processor SDK
//!
//! Idiomatic Rust SDK for building Wasm processors that run inside the Aeon
//! engine. Replaces the manual `no_std` boilerplate, bump allocator, wire
//! format parsing, and ABI exports with a single macro invocation.
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! #![no_std]
//! extern crate alloc;
//!
//! use aeon_wasm_sdk::prelude::*;
//!
//! fn my_processor(event: Event) -> Vec<Output> {
//!     vec![Output::new("output-topic", event.payload.clone())]
//! }
//!
//! aeon_processor!(my_processor);
//! ```
//!
//! The `aeon_processor!` macro generates:
//! - A bump allocator + `#[global_allocator]`
//! - `alloc` / `dealloc` Wasm exports
//! - `process(ptr, len) -> ptr` Wasm export that deserializes the event,
//!   calls your function, and serializes the outputs
//! - A `#[panic_handler]` (only when not testing)
//!
//! ## Host Imports
//!
//! The Aeon engine provides these host functions to guest processors:
//! - **State**: [`state::get`], [`state::put`], [`state::delete`]
//! - **Logging**: [`log::info`], [`log::warn`], [`log::error`]
//! - **Metrics**: [`metrics::counter_inc`], [`metrics::gauge_set`]
//! - **Clock**: [`clock::now_ms`]

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

pub mod clock;
pub mod host;
pub mod log;
pub mod metrics;
pub mod state;
pub mod wire;

// ── Event type (guest-side, no_std) ────────────────────────────────────

/// An event received from the Aeon pipeline.
///
/// This is the guest-side representation, deserialized from the binary wire
/// format. All fields are owned (no lifetimes) for ergonomic use.
pub struct Event {
    /// Event ID as raw UUID bytes (16 bytes, UUIDv7).
    pub id: [u8; 16],
    /// Unix epoch nanoseconds.
    pub timestamp: i64,
    /// Source identifier.
    pub source: String,
    /// Partition number.
    pub partition: u16,
    /// Key-value metadata headers.
    pub metadata: Vec<(String, String)>,
    /// Event payload bytes.
    pub payload: Vec<u8>,
}

impl Event {
    /// Get the payload as a UTF-8 string slice, if valid.
    pub fn payload_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.payload).ok()
    }
}

// ── Output type (guest-side, no_std) ───────────────────────────────────

/// An output to emit from the processor.
///
/// Construct via [`Output::new`] or [`Output::from_str`].
pub struct Output {
    /// Destination sink/topic name.
    pub destination: String,
    /// Optional partition key.
    pub key: Option<Vec<u8>>,
    /// Output payload bytes.
    pub payload: Vec<u8>,
    /// Key-value headers.
    pub headers: Vec<(String, String)>,
}

impl Output {
    /// Create a new output with a destination and payload. No key, no headers.
    pub fn new(destination: &str, payload: Vec<u8>) -> Self {
        Self {
            destination: String::from(destination),
            key: None,
            payload,
            headers: Vec::new(),
        }
    }

    /// Create a new output from a string payload.
    pub fn from_str(destination: &str, payload: &str) -> Self {
        Self::new(destination, payload.as_bytes().to_vec())
    }

    /// Set the partition key.
    pub fn with_key(mut self, key: Vec<u8>) -> Self {
        self.key = Some(key);
        self
    }

    /// Set the partition key from a string.
    pub fn with_key_str(mut self, key: &str) -> Self {
        self.key = Some(key.as_bytes().to_vec());
        self
    }

    /// Add a header key-value pair.
    pub fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.push((String::from(key), String::from(value)));
        self
    }
}

// ── Prelude ────────────────────────────────────────────────────────────

/// Re-exports for convenient `use aeon_wasm_sdk::prelude::*;`
pub mod prelude {
    pub use alloc::string::String;
    pub use alloc::vec;
    pub use alloc::vec::Vec;

    pub use crate::{Event, Output, aeon_processor};
    pub use crate::{clock, log, metrics, state};
}

// ── aeon_processor! macro ──────────────────────────────────────────────

/// Generate all Wasm ABI exports for an Aeon processor.
///
/// Takes a function `fn(Event) -> Vec<Output>` and generates:
/// - Bump allocator with `#[global_allocator]`
/// - `alloc(size: i32) -> i32` export
/// - `dealloc(ptr: i32, size: i32)` export
/// - `process(ptr: i32, len: i32) -> i32` export
/// - `#[panic_handler]` (in non-test builds)
///
/// # Example
///
/// ```rust,ignore
/// fn handle(event: Event) -> Vec<Output> {
///     vec![Output::new("output", event.payload.clone())]
/// }
/// aeon_processor!(handle);
/// ```
#[macro_export]
macro_rules! aeon_processor {
    ($process_fn:ident) => {
        // ── Bump allocator ─────────────────────────────────────────────

        const _AEON_HEAP_SIZE: usize = 256 * 1024;
        static _AEON_HEAP_POS: core::sync::atomic::AtomicUsize =
            core::sync::atomic::AtomicUsize::new(0);
        // Base address in linear memory for the bump heap (128 KB offset,
        // leaving room for data/stack).
        const _AEON_HEAP_BASE: usize = 131072;

        #[unsafe(no_mangle)]
        pub extern "C" fn alloc(size: i32) -> i32 {
            let aligned = ((size as usize) + 7) & !7;
            let pos = _AEON_HEAP_POS.fetch_add(aligned, core::sync::atomic::Ordering::Relaxed);
            if pos + aligned > _AEON_HEAP_SIZE {
                return 0; // OOM
            }
            (_AEON_HEAP_BASE + pos) as i32
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn dealloc(_ptr: i32, _size: i32) {
            // Bump allocator — no-op dealloc
        }

        // GlobalAlloc implementation so `alloc::vec::Vec` etc. work
        struct _AeonWasmAllocator;

        unsafe impl core::alloc::GlobalAlloc for _AeonWasmAllocator {
            unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
                let size = layout.size().max(layout.align());
                alloc(size as i32) as *mut u8
            }

            unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {
                // bump — no-op
            }
        }

        #[global_allocator]
        static _AEON_ALLOCATOR: _AeonWasmAllocator = _AeonWasmAllocator;

        // ── process export ─────────────────────────────────────────────

        #[unsafe(no_mangle)]
        pub extern "C" fn process(ptr: i32, len: i32) -> i32 {
            // Reset bump allocator per call
            _AEON_HEAP_POS.store(0, core::sync::atomic::Ordering::Relaxed);

            let data = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };

            // Deserialize event from wire format
            let event = match $crate::wire::deserialize_event(data) {
                Some(e) => e,
                None => {
                    // Return empty output list on parse failure
                    return _aeon_write_empty_result();
                }
            };

            // Call user's processor function
            let outputs = $process_fn(event);

            // Serialize outputs to wire format
            let serialized = $crate::wire::serialize_outputs(&outputs);

            // Write length-prefixed result to guest memory
            let total_len = 4 + serialized.len();
            let result_ptr = alloc(total_len as i32);
            if result_ptr == 0 {
                return _aeon_write_empty_result();
            }

            let out = unsafe { core::slice::from_raw_parts_mut(result_ptr as *mut u8, total_len) };
            out[0..4].copy_from_slice(&(serialized.len() as u32).to_le_bytes());
            out[4..].copy_from_slice(&serialized);

            result_ptr
        }

        fn _aeon_write_empty_result() -> i32 {
            let ptr = alloc(8);
            if ptr == 0 {
                return 0;
            }
            let out = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, 8) };
            // length prefix = 4 (just the count field)
            out[0..4].copy_from_slice(&4u32.to_le_bytes());
            // count = 0
            out[4..8].copy_from_slice(&0u32.to_le_bytes());
            ptr
        }

        // ── panic handler ──────────────────────────────────────────────

        #[cfg(not(test))]
        #[panic_handler]
        fn _aeon_panic(_info: &core::panic::PanicInfo) -> ! {
            loop {}
        }
    };
}
