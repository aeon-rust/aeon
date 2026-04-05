//! Processor transport implementations.
//!
//! This module provides concrete `ProcessorTransport` implementations for
//! each processor tier (T1–T4). Phase 12b-1 provides `InProcessTransport`
//! which wraps any sync `Processor` into the async `ProcessorTransport` trait.

pub mod in_process;

pub use in_process::InProcessTransport;
