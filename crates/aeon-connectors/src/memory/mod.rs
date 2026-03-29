//! In-memory source and sink for testing.
//!
//! `MemorySource` serves pre-loaded events from a `Vec`.
//! `MemorySink` collects outputs into a `Vec` for assertion.

mod sink;
mod source;

pub use sink::MemorySink;
pub use source::MemorySource;
