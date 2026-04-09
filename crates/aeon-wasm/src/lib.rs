//! Wasmtime processor runtime with WIT contracts and fuel metering.

pub mod processor;
pub mod runtime;

pub use processor::WasmProcessor;
pub use runtime::{HostState, WasmConfig, WasmInstance, WasmModule};
