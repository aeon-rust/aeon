//! Wasmtime processor runtime with WIT contracts and fuel metering.

// FT-10: no-panic policy. Production code in this crate must not use
// `.unwrap()` or `.expect()` except for explicitly-documented startup-time
// invariants, which must carry an `#[allow(...)]` attribute with rationale.
// Test modules and benches are exempt (`cfg(not(test))`).
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod processor;
pub mod runtime;

pub use processor::WasmProcessor;
pub use runtime::{HostState, WasmConfig, WasmInstance, WasmModule};
