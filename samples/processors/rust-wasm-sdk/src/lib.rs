//! Sample Aeon Wasm processor using the SDK.
//!
//! Compare with `samples/processors/rust-wasm/src/lib.rs` which uses raw
//! no_std, manual wire format parsing, and a hand-written bump allocator.
//!
//! This processor extracts `user_id` from JSON payloads and emits an
//! enriched output — same logic, ~10x less boilerplate.
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

#![no_std]
extern crate alloc;

use aeon_wasm_sdk::prelude::*;

fn enrich(event: Event) -> Vec<Output> {
    // Try to extract user_id from JSON payload
    let payload = &event.payload;
    let user_id = extract_json_field(payload, b"user_id");

    let output_payload = match user_id {
        Some(uid) => {
            let mut buf = Vec::with_capacity(payload.len() + 64);
            buf.extend_from_slice(b"{\"original\":");
            buf.extend_from_slice(payload);
            buf.extend_from_slice(b",\"extracted_user_id\":\"");
            buf.extend_from_slice(uid);
            buf.extend_from_slice(b"\"}");
            buf
        }
        None => payload.clone(),
    };

    vec![Output::new("enriched", output_payload)]
}

aeon_processor!(enrich);

// ── Simple JSON field extraction (same logic as raw sample) ────────────

fn extract_json_field<'a>(payload: &'a [u8], field: &[u8]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i + field.len() + 3 < payload.len() {
        if payload[i] == b'"' && payload[i + 1..].starts_with(field) {
            let after_field = i + 1 + field.len();
            if after_field < payload.len() && payload[after_field] == b'"' {
                let after_colon = after_field + 1;
                if after_colon < payload.len() && payload[after_colon] == b':' {
                    let mut val_start = after_colon + 1;
                    while val_start < payload.len()
                        && (payload[val_start] == b' ' || payload[val_start] == b'\t')
                    {
                        val_start += 1;
                    }
                    if val_start < payload.len() && payload[val_start] == b'"' {
                        val_start += 1;
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
