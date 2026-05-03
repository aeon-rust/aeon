//! cdylib export shell for the JsonEnrichProcessor.
//!
//! This crate's only job is to surface `aeon_processor_*` C-ABI symbols
//! that aeon-engine's `NativeProcessor::load()` can dlopen at runtime.
//! All processing logic lives in the sibling `aeon-sample-rust-native`
//! crate, so dev tests and the loaded `.so` exercise the same code path.

// The `export_processor!` macro emits C-ABI exports that take raw
// pointers; that's the contract aeon-engine's NativeProcessor::load
// expects. Suppressing the lint at the use site (rather than the
// macro definition) keeps the macro source clean for callers who
// care about lint-clean status.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use aeon_native_sdk::prelude::*;
use aeon_sample_rust_native::JsonEnrichProcessor;

/// Build a fresh `JsonEnrichProcessor` from runtime config bytes. The
/// loader passes the manifest's `processor.runtime` block here as raw
/// bytes; for now we ignore it and use the same defaults the in-process
/// sample uses, so dev parity is preserved.
fn create(_config: &[u8]) -> Box<dyn Processor> {
    Box::new(JsonEnrichProcessor::new(
        "user_id",
        "enriched",
        "rust-native-v1",
    ))
}

export_processor!(create);
