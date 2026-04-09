//! File system connector — Source and Sink implementations.
//!
//! `FileSource` reads newline-delimited records from a file (or set of files).
//! `FileSink` writes outputs as newline-delimited records to a file.
//!
//! Both are batch-first:
//! - `FileSource::next_batch()` reads up to `batch_size` lines per call
//! - `FileSink::write_batch()` appends all outputs in a single buffered write

mod sink;
mod source;

pub use sink::{FileSink, FileSinkConfig};
pub use source::{FileSource, FileSourceConfig};
