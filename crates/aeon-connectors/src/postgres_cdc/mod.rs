//! PostgreSQL CDC connector — logical replication source.
//!
//! `PostgresCdcSource`: Connects to PostgreSQL using logical replication
//!   protocol (pgoutput plugin). Reads WAL changes as events.
//!   Each change (INSERT, UPDATE, DELETE) becomes an Event with a JSON payload.
//!
//! Supports:
//! - Automatic replication slot creation
//! - Publication filtering (specific tables or all)
//! - Schema change tracking (new columns, type changes)
//! - LSN-based resume for exactly-once semantics

mod source;

pub use source::{PostgresCdcSource, PostgresCdcSourceConfig};
