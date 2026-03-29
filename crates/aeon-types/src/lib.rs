//! Core types, traits, and error definitions for Aeon.
//!
//! This crate is the foundation of the Aeon workspace. All other crates
//! depend on `aeon-types` for the canonical Event/Output envelopes,
//! error types, and trait definitions.

pub mod error;
pub mod event;
pub mod interner;
pub mod partition;
pub mod scanner;
pub mod traits;
pub mod uuid;

// Re-export primary types at crate root for convenience.
pub use error::{AeonError, Result};
pub use event::{Event, Output};
pub use interner::StringInterner;
pub use partition::PartitionId;
pub use scanner::{
    BytesFinder, contains_byte, contains_bytes, find_byte, find_bytes, json_field_value,
};
pub use traits::{IdempotentSink, Processor, Seekable, Sink, Source, StateOps};
pub use uuid::CoreLocalUuidGenerator;
