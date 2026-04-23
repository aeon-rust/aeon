//! MySQL CDC connector — binlog replication source.
//!
//! `MysqlCdcSource`: Connects to MySQL as a replication slave and reads
//!   binlog events. Each row change becomes an Event with a JSON payload.
//!
//! Supports:
//! - GTID-based positioning for resume
//! - Row-based replication events (INSERT, UPDATE, DELETE)
//! - Schema change detection (DDL events)
//! - Configurable server ID to avoid conflicts

mod auth;
mod source;

pub use source::{MysqlCdcSource, MysqlCdcSourceConfig};
