//! MySQL CDC source — binlog-based change data capture.
//!
//! Uses mysql_async to query the binlog for row changes.
//! Each change is emitted as an Event with a JSON payload.
//!
//! For a production CDC implementation, this would use the MySQL replication
//! protocol directly. This implementation uses polling of the binlog via
//! `SHOW BINLOG EVENTS` as a simpler starting point.

use aeon_types::{AeonError, BackoffPolicy, CoreLocalUuidGenerator, Event, PartitionId, Source};
use bytes::Bytes;
use mysql_async::prelude::*;
use std::sync::{Arc, Mutex};
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
    /// Reconnect backoff policy (TR-3). Applied when the pool fails to hand
    /// out a connection or a query fails. Exponential + jitter prevents
    /// reconnect storms against a flaky MySQL primary.
    pub backoff: BackoffPolicy,
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
            backoff: BackoffPolicy::default(),
        }
    }

    /// Override the reconnect backoff policy.
    pub fn with_backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
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
    /// TR-3 reconnect backoff. Advanced on pool/query errors; reset on
    /// every successful poll (even when no events arrive).
    backoff: aeon_types::Backoff,
    uuid_gen: Mutex<CoreLocalUuidGenerator>,
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
        let rows: Vec<(String, u64, String, String, String)> = conn
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

        let backoff = config.backoff.iter();
        Ok(Self {
            pool,
            config,
            current_file: file,
            current_position: position,
            backoff,
            uuid_gen: Mutex::new(CoreLocalUuidGenerator::new(0)),
        })
    }
}

impl Source for MysqlCdcSource {
    async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
        // TR-3: on connection/query failure, back off and return Ok(empty)
        // rather than propagating Err — keeps the pipeline alive through
        // transient MySQL outages. Reset backoff on every successful poll.
        let mut conn = match self.pool.get_conn().await {
            Ok(c) => c,
            Err(e) => {
                let delay = self.backoff.next_delay();
                tracing::warn!(
                    error = %e,
                    delay_ms = delay.as_millis() as u64,
                    "mysql get_conn failed; backing off before retry"
                );
                tokio::time::sleep(delay).await;
                return Ok(Vec::new());
            }
        };

        // Query binlog events from current position
        let query = format!(
            "SHOW BINLOG EVENTS IN '{}' FROM {} LIMIT {}",
            self.current_file, self.current_position, self.config.batch_size
        );

        let rows: Vec<(String, u64, String, u32, u64, String)> = match conn.query(&query).await {
            Ok(r) => r,
            Err(e) => {
                let delay = self.backoff.next_delay();
                tracing::warn!(
                    error = %e,
                    delay_ms = delay.as_millis() as u64,
                    "SHOW BINLOG EVENTS failed; backing off before retry"
                );
                drop(conn);
                tokio::time::sleep(delay).await;
                return Ok(Vec::new());
            }
        };

        self.backoff.reset();

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

            let event_id = self
                .uuid_gen
                .lock()
                .map_err(|_| AeonError::connection("UUID generator mutex poisoned"))?
                .next_uuid();
            let mut event = Event::new(
                event_id,
                0,
                Arc::clone(&self.config.source_name),
                PartitionId::new(0),
                payload_bytes,
            );
            event = event.with_source_ts(now);
            // B2: end_pos is the binlog offset after this event, a monotonic
            // u64 within a single binlog file. Cast to i64 (sign bit cleared)
            // for checkpoint ordering. The binlog file name is carried in
            // metadata — callers that need full resume identity should
            // combine both.
            event = event.with_source_offset((end_pos & 0x7FFF_FFFF_FFFF_FFFF) as i64);
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
