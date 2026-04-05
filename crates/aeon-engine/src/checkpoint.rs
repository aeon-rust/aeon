//! Checkpoint WAL — append-only write-ahead log for delivery checkpoint persistence.
//!
//! Format:
//! ```text
//! [Magic: "AEON-CKP" 8B][Version: u16 LE]
//! [Record length: u32 LE][CRC32: u32 LE][CheckpointRecord (bincode)]
//! [Record length: u32 LE][CRC32: u32 LE][CheckpointRecord (bincode)]
//! ...
//! ```
//!
//! Each checkpoint is ~100-200 bytes. At 1/sec, 24h ≈ 8-17 MB.
//! CRC32 integrity check on read — corrupted trailing records are skipped.

use aeon_types::{AeonError, PartitionId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Magic bytes identifying an Aeon checkpoint WAL file.
const WAL_MAGIC: &[u8; 8] = b"AEON-CKP";

/// Current WAL format version.
const WAL_VERSION: u16 = 1;

/// Header size: 8 (magic) + 2 (version) = 10 bytes.
const HEADER_SIZE: usize = 10;

/// Per-record header: 4 (length) + 4 (CRC32) = 8 bytes.
const RECORD_HEADER_SIZE: usize = 8;

/// A single checkpoint record persisted to the WAL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRecord {
    /// Monotonically increasing checkpoint ID.
    pub checkpoint_id: u64,
    /// Unix epoch nanoseconds when this checkpoint was created.
    pub timestamp_nanos: i64,
    /// Per-partition source offsets at checkpoint time.
    /// The safe replay point for crash recovery.
    pub source_offsets: HashMap<u16, i64>,
    /// Event IDs still pending at checkpoint time (typically empty = clean checkpoint).
    pub pending_event_ids: Vec<[u8; 16]>,
    /// Number of events delivered since last checkpoint.
    pub delivered_count: u64,
    /// Number of events failed since last checkpoint.
    pub failed_count: u64,
}

impl CheckpointRecord {
    /// Create a new checkpoint record.
    pub fn new(
        checkpoint_id: u64,
        source_offsets: HashMap<PartitionId, i64>,
        pending_event_ids: Vec<uuid::Uuid>,
        delivered_count: u64,
        failed_count: u64,
    ) -> Self {
        Self {
            checkpoint_id,
            timestamp_nanos: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as i64,
            source_offsets: source_offsets
                .into_iter()
                .map(|(p, o)| (p.as_u16(), o))
                .collect(),
            pending_event_ids: pending_event_ids
                .into_iter()
                .map(|id| *id.as_bytes())
                .collect(),
            delivered_count,
            failed_count,
        }
    }

    /// Get source offsets as PartitionId keys.
    pub fn partition_offsets(&self) -> HashMap<PartitionId, i64> {
        self.source_offsets
            .iter()
            .map(|(p, o)| (PartitionId::new(*p), *o))
            .collect()
    }

    /// Get pending event IDs as UUIDs.
    pub fn pending_uuids(&self) -> Vec<uuid::Uuid> {
        self.pending_event_ids
            .iter()
            .map(|bytes| uuid::Uuid::from_bytes(*bytes))
            .collect()
    }
}

/// Checkpoint WAL writer — appends checkpoint records to a file.
pub struct CheckpointWriter {
    path: PathBuf,
    next_checkpoint_id: u64,
}

impl CheckpointWriter {
    /// Create a new WAL writer. Creates the file with header if it doesn't exist.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, AeonError> {
        let path = path.into();

        // If file exists, read header and find the last checkpoint ID.
        let next_checkpoint_id = if path.exists() {
            let records = read_wal_records(&path)?;
            records.last().map_or(0, |r| r.checkpoint_id + 1)
        } else {
            // Create new file with header.
            write_wal_header(&path)?;
            0
        };

        Ok(Self {
            path,
            next_checkpoint_id,
        })
    }

    /// Append a checkpoint record to the WAL.
    pub fn append(&mut self, record: &mut CheckpointRecord) -> Result<(), AeonError> {
        record.checkpoint_id = self.next_checkpoint_id;

        let data = bincode::serialize(record)
            .map_err(|e| AeonError::state(format!("bincode serialize: {e}")))?;

        let crc = crc32fast::hash(&data);
        let len = data.len() as u32;

        let mut buf = Vec::with_capacity(RECORD_HEADER_SIZE + data.len());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&data);

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|e| AeonError::state(format!("wal open for append: {e}")))?;
        file.write_all(&buf)
            .map_err(|e| AeonError::state(format!("wal write: {e}")))?;
        file.flush()
            .map_err(|e| AeonError::state(format!("wal flush: {e}")))?;

        self.next_checkpoint_id += 1;
        Ok(())
    }

    /// Path to the WAL file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Next checkpoint ID that will be assigned.
    pub fn next_checkpoint_id(&self) -> u64 {
        self.next_checkpoint_id
    }
}

/// Checkpoint WAL reader — reads the last valid checkpoint for crash recovery.
pub struct CheckpointReader;

impl CheckpointReader {
    /// Read the last valid checkpoint from a WAL file.
    /// Returns None if the file doesn't exist or contains no valid records.
    pub fn read_last(path: impl AsRef<Path>) -> Result<Option<CheckpointRecord>, AeonError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(None);
        }

        let records = read_wal_records(path)?;
        Ok(records.into_iter().last())
    }

    /// Read all valid checkpoint records from a WAL file.
    pub fn read_all(path: impl AsRef<Path>) -> Result<Vec<CheckpointRecord>, AeonError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_wal_records(path)
    }
}

/// Write the WAL file header (magic + version).
fn write_wal_header(path: &Path) -> Result<(), AeonError> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AeonError::state(format!("wal dir create: {e}")))?;
    }
    let mut file =
        std::fs::File::create(path).map_err(|e| AeonError::state(format!("wal create: {e}")))?;
    file.write_all(WAL_MAGIC)
        .map_err(|e| AeonError::state(format!("wal write magic: {e}")))?;
    file.write_all(&WAL_VERSION.to_le_bytes())
        .map_err(|e| AeonError::state(format!("wal write version: {e}")))?;
    file.flush()
        .map_err(|e| AeonError::state(format!("wal flush: {e}")))?;
    Ok(())
}

/// Read and validate all records from a WAL file.
/// Stops at first corrupted record (truncation-safe).
fn read_wal_records(path: &Path) -> Result<Vec<CheckpointRecord>, AeonError> {
    let data = std::fs::read(path).map_err(|e| AeonError::state(format!("wal read: {e}")))?;

    if data.len() < HEADER_SIZE {
        return Err(AeonError::state("wal file too short for header"));
    }

    // Validate magic
    if &data[0..8] != WAL_MAGIC {
        return Err(AeonError::state("wal invalid magic bytes"));
    }

    // Validate version
    let version = u16::from_le_bytes([data[8], data[9]]);
    if version != WAL_VERSION {
        return Err(AeonError::state(format!(
            "wal unsupported version: {version}"
        )));
    }

    let mut offset = HEADER_SIZE;
    let mut records = Vec::new();

    while offset + RECORD_HEADER_SIZE <= data.len() {
        let len = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        let expected_crc = u32::from_le_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]);

        offset += RECORD_HEADER_SIZE;

        if offset + len > data.len() {
            // Truncated record — stop here (safe, just incomplete last write)
            break;
        }

        let record_data = &data[offset..offset + len];
        let actual_crc = crc32fast::hash(record_data);

        if actual_crc != expected_crc {
            // Corrupted record — stop here
            tracing::warn!(
                "checkpoint WAL CRC mismatch at offset {}, stopping read",
                offset - RECORD_HEADER_SIZE
            );
            break;
        }

        match bincode::deserialize::<CheckpointRecord>(record_data) {
            Ok(record) => {
                records.push(record);
            }
            Err(e) => {
                tracing::warn!("checkpoint WAL deserialize error at offset {offset}: {e}");
                break;
            }
        }

        offset += len;
    }

    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::PartitionId;
    use std::collections::HashMap;

    fn temp_wal_path() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        // Leak the dir so it persists for the test duration
        let path = dir.path().join("test.wal");
        std::mem::forget(dir);
        path
    }

    #[test]
    fn write_and_read_single_checkpoint() {
        let path = temp_wal_path();
        let mut writer = CheckpointWriter::new(&path).unwrap();

        let mut offsets = HashMap::new();
        offsets.insert(PartitionId::new(0), 100i64);
        offsets.insert(PartitionId::new(1), 200i64);

        let mut record = CheckpointRecord::new(0, offsets, vec![], 500, 2);
        writer.append(&mut record).unwrap();

        let last = CheckpointReader::read_last(&path).unwrap().unwrap();
        assert_eq!(last.checkpoint_id, 0);
        assert_eq!(last.delivered_count, 500);
        assert_eq!(last.failed_count, 2);
        let part_offsets = last.partition_offsets();
        assert_eq!(part_offsets[&PartitionId::new(0)], 100);
        assert_eq!(part_offsets[&PartitionId::new(1)], 200);
    }

    #[test]
    fn write_multiple_checkpoints_read_last() {
        let path = temp_wal_path();
        let mut writer = CheckpointWriter::new(&path).unwrap();

        for i in 0..5 {
            let mut offsets = HashMap::new();
            offsets.insert(PartitionId::new(0), (i * 100) as i64);
            let mut record = CheckpointRecord::new(0, offsets, vec![], i * 10, 0);
            writer.append(&mut record).unwrap();
        }

        let last = CheckpointReader::read_last(&path).unwrap().unwrap();
        assert_eq!(last.checkpoint_id, 4);
        assert_eq!(last.delivered_count, 40);
        assert_eq!(last.partition_offsets()[&PartitionId::new(0)], 400);
    }

    #[test]
    fn read_all_checkpoints() {
        let path = temp_wal_path();
        let mut writer = CheckpointWriter::new(&path).unwrap();

        for i in 0..3 {
            let mut offsets = HashMap::new();
            offsets.insert(PartitionId::new(0), i as i64);
            let mut record = CheckpointRecord::new(0, offsets, vec![], 0, 0);
            writer.append(&mut record).unwrap();
        }

        let all = CheckpointReader::read_all(&path).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].checkpoint_id, 0);
        assert_eq!(all[1].checkpoint_id, 1);
        assert_eq!(all[2].checkpoint_id, 2);
    }

    #[test]
    fn empty_wal_returns_none() {
        let path = temp_wal_path();
        let _writer = CheckpointWriter::new(&path).unwrap();
        let last = CheckpointReader::read_last(&path).unwrap();
        assert!(last.is_none());
    }

    #[test]
    fn nonexistent_file_returns_none() {
        let path = PathBuf::from("/tmp/aeon_test_nonexistent.wal");
        let last = CheckpointReader::read_last(&path).unwrap();
        assert!(last.is_none());
    }

    #[test]
    fn pending_event_ids_roundtrip() {
        let path = temp_wal_path();
        let mut writer = CheckpointWriter::new(&path).unwrap();

        let ids = vec![uuid::Uuid::now_v7(), uuid::Uuid::now_v7()];
        let mut record = CheckpointRecord::new(0, HashMap::new(), ids.clone(), 0, 0);
        writer.append(&mut record).unwrap();

        let last = CheckpointReader::read_last(&path).unwrap().unwrap();
        let recovered = last.pending_uuids();
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0], ids[0]);
        assert_eq!(recovered[1], ids[1]);
    }

    #[test]
    fn corrupted_trailing_record_skipped() {
        let path = temp_wal_path();
        let mut writer = CheckpointWriter::new(&path).unwrap();

        // Write 2 valid records
        for i in 0..2 {
            let mut offsets = HashMap::new();
            offsets.insert(PartitionId::new(0), i as i64);
            let mut record = CheckpointRecord::new(0, offsets, vec![], 0, 0);
            writer.append(&mut record).unwrap();
        }

        // Append garbage to corrupt trailing record
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&[0xFF; 20]).unwrap();

        // Should read 2 valid records, skip garbage
        let all = CheckpointReader::read_all(&path).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn writer_resumes_from_existing_wal() {
        let path = temp_wal_path();

        // Write 3 records
        {
            let mut writer = CheckpointWriter::new(&path).unwrap();
            for _ in 0..3 {
                let mut record = CheckpointRecord::new(0, HashMap::new(), vec![], 0, 0);
                writer.append(&mut record).unwrap();
            }
        }

        // Reopen and continue
        {
            let mut writer = CheckpointWriter::new(&path).unwrap();
            assert_eq!(writer.next_checkpoint_id(), 3);
            let mut record = CheckpointRecord::new(0, HashMap::new(), vec![], 999, 0);
            writer.append(&mut record).unwrap();
        }

        let all = CheckpointReader::read_all(&path).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[3].checkpoint_id, 3);
        assert_eq!(all[3].delivered_count, 999);
    }

    #[test]
    fn checkpoint_record_timestamp_populated() {
        let record = CheckpointRecord::new(0, HashMap::new(), vec![], 0, 0);
        assert!(record.timestamp_nanos > 0);
    }

    #[test]
    fn wal_header_validation() {
        let path = temp_wal_path();

        // Write invalid magic
        std::fs::write(&path, b"BADMAGIC\x01\x00").unwrap();
        let result = CheckpointReader::read_all(&path);
        assert!(result.is_err());

        // Write valid magic but wrong version
        let mut header = Vec::new();
        header.extend_from_slice(WAL_MAGIC);
        header.extend_from_slice(&99u16.to_le_bytes());
        std::fs::write(&path, &header).unwrap();
        let result = CheckpointReader::read_all(&path);
        assert!(result.is_err());
    }
}
