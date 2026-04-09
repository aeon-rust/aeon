//! MySQL CDC source — binlog-based change data capture.
//!
//! Uses mysql_async to query the binlog for row changes.
//! Each change is emitted as an Event with a JSON payload.
//!
//! For a production CDC implementation, this would use the MySQL replication
//! protocol directly. This implementation uses polling of the binlog via
//! `SHOW BINLOG EVENTS` as a simpler starting point.

use aeon_types::{AeonError, Event, PartitionId, Source};
use bytes::Bytes;
use mysql_async::prelude::*;
use std::sync::Arc;
use std::time::Duration;

/// Configuration for `MysqlCdcSource`.
pub struct MysqlCdcSourceConfig {
    /// MySQL connection URL (e.g., "mysql://root:password@localhost:3306/mydb").
    pub url: String,
    /// Server ID for the replication slave (must be unique).
    pub server_id: u32,
    /// Tables to watch (empty = all tables in the database).
    pub tables: Vec<String>,
    /// Maximum changes per `next_batch()`.
    pub batch_size: usize,
    /// Poll interval for checking new binlog data.
    pub poll_interval: Duration,
    /// Source identifier for events (interned).
    pub source_name: Arc<str>,
    /// Starting binlog file (None = current).
    pub binlog_file: Option<String>,
    /// Starting binlog position (None = current).
    pub binlog_position: Option<u64>,
}

impl MysqlCdcSourceConfig {
    /// Create a config for MySQL CDC.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            server_id: 1000,
            tables: Vec::new(),
            batch_size: 1024,
            poll_interval: Duration::from_millis(500),
            source_name: Arc::from("mysql-cdc"),
            binlog_file: None,
            binlog_position: None,
        }
    }

    /// Set server ID.
    pub fn with_server_id(mut self, id: u32) -> Self {
        self.server_id = id;
        self
    }

    /// Add a table to watch.
    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.tables.push(table.into());
        self
    }

    /// Set batch size.
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    /// Set the source name.
    pub fn with_source_name(mut self, name: impl Into<Arc<str>>) -> Self {
        self.source_name = name.into();
        self
    }

    /// Set starting binlog position for resume.
    pub fn with_binlog_position(mut self, file: impl Into<String>, position: u64) -> Self {
        self.binlog_file = Some(file.into());
        self.binlog_position = Some(position);
        self
    }
}

/// MySQL CDC event source.
///
/// Polls the MySQL binlog for row-level changes. Each change becomes
/// an Event with a JSON payload describing the operation, table, and data.
///
/// Tracks binlog file + position for resume capability.
pub struct MysqlCdcSource {
    pool: mysql_async::Pool,
    config: MysqlCdcSourceConfig,
    current_file: String,
    current_position: u64,
}

impl MysqlCdcSource {
    /// Connect to MySQL and determine the current binlog position.
    pub async fn new(config: MysqlCdcSourceConfig) -> Result<Self, AeonError> {
        let pool = mysql_async::Pool::new(config.url.as_str());

        let mut conn = pool
            .get_conn()
            .await
            .map_err(|e| AeonError::connection(format!("mysql connect failed: {e}")))?;

        // Verify binlog is enabled
        let rows: Vec<(String, u64, String, String)> = conn
            .query("SHOW MASTER STATUS")
            .await
            .map_err(|e| AeonError::connection(format!("SHOW MASTER STATUS failed: {e}")))?;

        let (file, position) = if let Some(config_file) = &config.binlog_file {
            (config_file.clone(), config.binlog_position.unwrap_or(4))
        } else if let Some(row) = rows.first() {
            (row.0.clone(), row.1)
        } else {
            return Err(AeonError::config(
                "MySQL binlog is not enabled. Set log_bin=ON in my.cnf".to_string(),
            ));
        };

        tracing::info!(
            binlog_file = %file,
            binlog_position = position,
            tables = ?config.tables,
            "MysqlCdcSource connected"
        );

        drop(conn);

        Ok(Self {
            pool,
            config,
            current_file: file,
            current_position: position,
        })
    }
}

impl Source for MysqlCdcSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| AeonError::connection(format!("mysql connect failed: {e}")))?;

        // Query binlog events from current position
        let query = format!(
            "SHOW BINLOG EVENTS IN '{}' FROM {} LIMIT {}",
            self.current_file, self.current_position, self.config.batch_size
        );

        let rows: Vec<(String, u64, String, u32, u64, String)> = conn
            .query(&query)
            .await
            .map_err(|e| AeonError::connection(format!("SHOW BINLOG EVENTS failed: {e}")))?;

        let mut events = Vec::new();
        let now = std::time::Instant::now();

        for (log_name, pos, event_type, _server_id, end_pos, info) in &rows {
            // Skip non-data events
            if *event_type == "Format_desc" || *event_type == "Previous_gtids" {
                self.current_position = *end_pos;
                continue;
            }

            // Filter by table if configured
            if !self.config.tables.is_empty() {
                let matches = self.config.tables.iter().any(|t| info.contains(t.as_str()));
                if !matches {
                    self.current_position = *end_pos;
                    continue;
                }
            }

            let payload = serde_json::json!({
                "binlog_file": log_name,
                "position": pos,
                "end_position": end_pos,
                "event_type": event_type,
                "info": info,
                "source": "mysql-cdc",
            });

            let payload_bytes = Bytes::from(serde_json::to_vec(&payload).unwrap_or_default());

            let mut event = Event::new(
                uuid::Uuid::nil(),
                0,
                Arc::clone(&self.config.source_name),
                PartitionId::new(0),
                payload_bytes,
            );
            event = event.with_source_ts(now);
            event
                .metadata
                .push((Arc::from("mysql.binlog_file"), Arc::from(log_name.as_str())));
            event.metadata.push((
                Arc::from("mysql.position"),
                Arc::from(end_pos.to_string().as_str()),
            ));

            events.push(event);
            self.current_file.clone_from(log_name);
            self.current_position = *end_pos;
        }

        // If no events, wait before polling again
        if events.is_empty() {
            tokio::time::sleep(self.config.poll_interval).await;
        }

        drop(conn);

        Ok(events)
    }
}
