//! EO-2 L2 event-body store — per-(pipeline, partition) segmented append-only
//! log of event bodies.
//!
//! Distinct from `aeon_state::l2` (KV warm cache). This store exists to satisfy
//! EO-2 durability for push and poll sources that have no durable upstream
//! replay position: the body must live in Aeon-owned storage until every sink
//! has acked the corresponding delivery sequence number.
//!
//! # Layout
//!
//! ```text
//! {root}/{pipeline}/p{partition:05}/{start_seq:020}.l2b
//! ```
//!
//! Each segment file:
//!
//! ```text
//! [ magic: 8B = "AEON-L2B" ][ version: u16 LE ][ reserved: u16 ]   // 12B header
//! records...
//! ```
//!
//! Record format (appended sequentially):
//!
//! ```text
//! [ seq : u64 LE ][ crc32 : u32 LE ][ len : u32 LE ][ bincode(WireEvent) : len B ]
//! ```
//!
//! `crc32` is computed over `len || payload`. Truncated or corrupt trailing
//! records are detected on read and the iterator stops cleanly (truncation-
//! safe, matches `checkpoint::read_wal_records` semantics).
//!
//! # Rollover + GC
//!
//! A new segment is opened when the current segment exceeds
//! `segment_bytes` (default 256 MiB). The store is keyed by the
//! monotonically increasing delivery sequence `seq` — GC drops whole segments
//! whose max_seq < `min(per_sink_ack_seq)` (see
//! `docs/EO-2-DURABILITY-DESIGN.md` §5).
//!
//! # Scope in P3
//!
//! This module provides the segment primitive + multi-segment partition store
//! + GC cursor + tests. Wiring into the pipeline runner (push/poll ingest
//!   path, sink ack sequence tracking, fsync cadence) lands in P4.

use aeon_types::{AeonError, Event, TransportCodec};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Magic bytes identifying an EO-2 L2 body segment file.
const SEGMENT_MAGIC: &[u8; 8] = b"AEON-L2B";

/// Current segment format version.
const SEGMENT_VERSION: u16 = 1;

/// Header size: 8 (magic) + 2 (version) + 2 (reserved) = 12 B.
const HEADER_SIZE: u64 = 12;

/// Per-record fixed overhead: 8 (seq) + 4 (crc32) + 4 (len) = 16 B.
const RECORD_HEADER_SIZE: usize = 16;

/// Default segment rollover threshold: 256 MiB.
pub const DEFAULT_SEGMENT_BYTES: u64 = 256 * 1024 * 1024;

/// A single segment file — append-only, bounded by `segment_bytes`.
pub struct L2BodySegment {
    path: PathBuf,
    file: File,
    /// Sequence of the first record in this segment (from filename).
    start_seq: u64,
    /// Highest seq appended, or `None` if segment is empty.
    max_seq: Option<u64>,
    /// Current byte length of the file on disk.
    byte_len: u64,
}

impl L2BodySegment {
    /// Create a new segment starting at `start_seq`. Writes the header.
    pub fn create(path: impl Into<PathBuf>, start_seq: u64) -> Result<Self, AeonError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AeonError::state(format!("l2-body: mkdir: {e}")))?;
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&path)
            .map_err(|e| AeonError::state(format!("l2-body: create {path:?}: {e}")))?;

        file.write_all(SEGMENT_MAGIC)
            .and_then(|_| file.write_all(&SEGMENT_VERSION.to_le_bytes()))
            .and_then(|_| file.write_all(&0u16.to_le_bytes()))
            .and_then(|_| file.flush())
            .map_err(|e| AeonError::state(format!("l2-body: write header: {e}")))?;

        Ok(Self {
            path,
            file,
            start_seq,
            max_seq: None,
            byte_len: HEADER_SIZE,
        })
    }

    /// Open an existing segment — validates header, scans to derive `max_seq`.
    pub fn open(path: impl Into<PathBuf>, start_seq: u64) -> Result<Self, AeonError> {
        let path = path.into();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| AeonError::state(format!("l2-body: open {path:?}: {e}")))?;

        let meta = file
            .metadata()
            .map_err(|e| AeonError::state(format!("l2-body: stat: {e}")))?;
        let byte_len = meta.len();
        if byte_len < HEADER_SIZE {
            return Err(AeonError::state("l2-body: file shorter than header"));
        }

        let mut header = [0u8; HEADER_SIZE as usize];
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.read_exact(&mut header))
            .map_err(|e| AeonError::state(format!("l2-body: read header: {e}")))?;
        if &header[0..8] != SEGMENT_MAGIC {
            return Err(AeonError::state("l2-body: bad magic"));
        }
        let version = u16::from_le_bytes([header[8], header[9]]);
        if version != SEGMENT_VERSION {
            return Err(AeonError::state(format!(
                "l2-body: unsupported version {version}"
            )));
        }

        // Scan records to recover max_seq. Stop on corruption / truncation.
        let mut max_seq: Option<u64> = None;
        let records = Self::iter_records(&path)?;
        for r in records {
            let (seq, _) = r?;
            max_seq = Some(seq);
        }

        file.seek(SeekFrom::End(0))
            .map_err(|e| AeonError::state(format!("l2-body: seek end: {e}")))?;

        Ok(Self {
            path,
            file,
            start_seq,
            max_seq,
            byte_len,
        })
    }

    /// Append one event. Assumes `seq > max_seq` (the caller is responsible
    /// for monotonic sequencing — typically the partition store).
    pub fn append(&mut self, seq: u64, event: &Event) -> Result<(), AeonError> {
        if let Some(m) = self.max_seq {
            if seq <= m {
                return Err(AeonError::state(format!(
                    "l2-body: non-monotonic seq {seq} ≤ max {m}"
                )));
            }
        }

        let payload = TransportCodec::MsgPack
            .encode_event(event)
            .map_err(|e| AeonError::state(format!("l2-body: encode event: {e}")))?;
        let len: u32 = payload
            .len()
            .try_into()
            .map_err(|_| AeonError::state("l2-body: event exceeds u32 len"))?;

        // CRC covers len || payload (not seq — mirrors checkpoint WAL).
        let mut crc_hasher = crc32fast::Hasher::new();
        crc_hasher.update(&len.to_le_bytes());
        crc_hasher.update(&payload);
        let crc = crc_hasher.finalize();

        let mut header = [0u8; RECORD_HEADER_SIZE];
        header[0..8].copy_from_slice(&seq.to_le_bytes());
        header[8..12].copy_from_slice(&crc.to_le_bytes());
        header[12..16].copy_from_slice(&len.to_le_bytes());

        self.file
            .write_all(&header)
            .and_then(|_| self.file.write_all(&payload))
            .map_err(|e| AeonError::state(format!("l2-body: write record: {e}")))?;

        self.max_seq = Some(seq);
        self.byte_len += RECORD_HEADER_SIZE as u64 + payload.len() as u64;
        Ok(())
    }

    /// Force outstanding writes to disk.
    pub fn fsync(&mut self) -> Result<(), AeonError> {
        self.file
            .sync_data()
            .map_err(|e| AeonError::state(format!("l2-body: fsync: {e}")))
    }

    pub fn start_seq(&self) -> u64 {
        self.start_seq
    }

    pub fn max_seq(&self) -> Option<u64> {
        self.max_seq
    }

    pub fn byte_len(&self) -> u64 {
        self.byte_len
    }

    pub fn is_empty(&self) -> bool {
        self.max_seq.is_none()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Iterate valid records from a segment path. Stops cleanly on corruption
    /// or truncation of the trailing record.
    pub fn iter_records(
        path: &Path,
    ) -> Result<impl Iterator<Item = Result<(u64, Event), AeonError>>, AeonError> {
        // Read the whole file — segments are bounded (256 MiB default) and
        // recovery is a cold path.
        let data = std::fs::read(path)
            .map_err(|e| AeonError::state(format!("l2-body: read {path:?}: {e}")))?;
        if data.len() < HEADER_SIZE as usize
            || &data[0..8] != SEGMENT_MAGIC
        {
            return Err(AeonError::state("l2-body: bad header on read"));
        }
        Ok(SegmentRecordIter {
            data,
            offset: HEADER_SIZE as usize,
            done: false,
        })
    }
}

struct SegmentRecordIter {
    data: Vec<u8>,
    offset: usize,
    done: bool,
}

impl Iterator for SegmentRecordIter {
    type Item = Result<(u64, Event), AeonError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.offset + RECORD_HEADER_SIZE > self.data.len() {
            self.done = true;
            return None;
        }
        let seq = u64::from_le_bytes(
            self.data[self.offset..self.offset + 8]
                .try_into()
                .ok()?,
        );
        let crc = u32::from_le_bytes(
            self.data[self.offset + 8..self.offset + 12]
                .try_into()
                .ok()?,
        );
        let len = u32::from_le_bytes(
            self.data[self.offset + 12..self.offset + 16]
                .try_into()
                .ok()?,
        ) as usize;

        let body_start = self.offset + RECORD_HEADER_SIZE;
        if body_start + len > self.data.len() {
            self.done = true;
            return None;
        }
        let payload = &self.data[body_start..body_start + len];

        let mut h = crc32fast::Hasher::new();
        h.update(&(len as u32).to_le_bytes());
        h.update(payload);
        if h.finalize() != crc {
            self.done = true;
            return None;
        }

        let event = match TransportCodec::MsgPack.decode_event(payload) {
            Ok(e) => e,
            Err(e) => {
                self.done = true;
                return Some(Err(AeonError::state(format!(
                    "l2-body: decode event: {e}"
                ))));
            }
        };

        self.offset = body_start + len;
        Some(Ok((seq, event)))
    }
}

/// Configuration for a partition body store.
#[derive(Debug, Clone)]
pub struct L2BodyConfig {
    /// Rollover threshold in bytes (default 256 MiB).
    pub segment_bytes: u64,
}

impl Default for L2BodyConfig {
    fn default() -> Self {
        Self {
            segment_bytes: DEFAULT_SEGMENT_BYTES,
        }
    }
}

/// Per-partition append-only body store. Manages rolling segments and GC.
pub struct L2BodyStore {
    dir: PathBuf,
    config: L2BodyConfig,
    /// Open segments, ordered by `start_seq` ascending. Last one is writable.
    segments: Vec<L2BodySegment>,
    /// Next seq to hand out via `append`.
    next_seq: u64,
}

impl L2BodyStore {
    /// Open / create a partition store at `dir`. Scans existing `*.l2b` files
    /// to recover state (next_seq = max_seq + 1 across segments, or 0 fresh).
    pub fn open(dir: impl Into<PathBuf>, config: L2BodyConfig) -> Result<Self, AeonError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .map_err(|e| AeonError::state(format!("l2-body: mkdir {dir:?}: {e}")))?;

        let mut starts: Vec<u64> = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| AeonError::state(format!("l2-body: readdir: {e}")))?
        {
            let entry = entry.map_err(|e| AeonError::state(format!("l2-body: readdir: {e}")))?;
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("l2b") {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                if let Ok(n) = stem.parse::<u64>() {
                    starts.push(n);
                }
            }
        }
        starts.sort_unstable();

        let mut segments = Vec::with_capacity(starts.len());
        let mut next_seq: u64 = 0;
        for s in starts {
            let path = dir.join(format!("{s:020}.l2b"));
            let seg = L2BodySegment::open(path, s)?;
            if let Some(m) = seg.max_seq {
                next_seq = (m + 1).max(next_seq);
            }
            segments.push(seg);
        }

        Ok(Self {
            dir,
            config,
            segments,
            next_seq,
        })
    }

    /// Append an event, returning the assigned seq.
    pub fn append(&mut self, event: &Event) -> Result<u64, AeonError> {
        let seq = self.next_seq;
        // Rollover if needed.
        let need_new = match self.segments.last() {
            None => true,
            Some(s) => s.byte_len >= self.config.segment_bytes,
        };
        if need_new {
            let path = self.dir.join(format!("{seq:020}.l2b"));
            let seg = L2BodySegment::create(path, seq)?;
            self.segments.push(seg);
        }

        // Unwrap-safe: we just pushed above if the vec was empty.
        #[allow(clippy::unwrap_used)]
        let writable = self.segments.last_mut().unwrap();
        writable.append(seq, event)?;
        self.next_seq = seq + 1;
        Ok(seq)
    }

    /// fsync the active (writable) segment.
    pub fn fsync(&mut self) -> Result<(), AeonError> {
        if let Some(s) = self.segments.last_mut() {
            s.fsync()?;
        }
        Ok(())
    }

    /// Drop all segments whose `max_seq < min_ack_seq`. Returns the number of
    /// segments reclaimed. Called periodically by the pipeline's GC sweep.
    pub fn gc_up_to(&mut self, min_ack_seq: u64) -> Result<usize, AeonError> {
        let mut reclaimed = 0;
        // Never drop the last segment if it's currently writable (it may be
        // the active segment); only drop fully-superseded historical segments.
        while self.segments.len() > 1 {
            let seg = &self.segments[0];
            match seg.max_seq {
                Some(m) if m < min_ack_seq => {
                    let path = seg.path.clone();
                    std::fs::remove_file(&path)
                        .map_err(|e| AeonError::state(format!("l2-body: gc remove: {e}")))?;
                    self.segments.remove(0);
                    reclaimed += 1;
                }
                _ => break,
            }
        }
        Ok(reclaimed)
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Total bytes-on-disk across all segments (live + header overhead).
    pub fn disk_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.byte_len).sum()
    }

    /// Iterate events from `from_seq` onward. Used on recovery to replay the
    /// body store from the last committed sink ack seq.
    pub fn iter_from(
        &self,
        from_seq: u64,
    ) -> Result<Vec<(u64, Event)>, AeonError> {
        let mut out = Vec::new();
        for seg in &self.segments {
            if let Some(m) = seg.max_seq {
                if m < from_seq {
                    continue;
                }
            } else {
                continue;
            }
            for rec in L2BodySegment::iter_records(&seg.path)? {
                let (seq, ev) = rec?;
                if seq >= from_seq {
                    out.push((seq, ev));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::PartitionId;
    use bytes::Bytes;
    use std::sync::Arc;

    fn tmp_dir() -> PathBuf {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().to_path_buf();
        std::mem::forget(d);
        p
    }

    fn ev(n: u64) -> Event {
        Event::new(
            uuid::Uuid::now_v7(),
            n as i64,
            Arc::from("src"),
            PartitionId::new(0),
            Bytes::from(format!("payload-{n}")),
        )
    }

    #[test]
    fn segment_append_and_iterate() {
        let dir = tmp_dir();
        let path = dir.join("00000000000000000000.l2b");
        let mut seg = L2BodySegment::create(&path, 0).unwrap();
        for i in 0..5 {
            seg.append(i, &ev(i)).unwrap();
        }
        assert_eq!(seg.max_seq(), Some(4));
        assert!(!seg.is_empty());

        let records: Vec<_> = L2BodySegment::iter_records(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(records.len(), 5);
        for (i, (seq, e)) in records.into_iter().enumerate() {
            assert_eq!(seq, i as u64);
            assert_eq!(e.payload.as_ref(), format!("payload-{i}").as_bytes());
        }
    }

    #[test]
    fn segment_reopen_recovers_max_seq() {
        let dir = tmp_dir();
        let path = dir.join("00000000000000000000.l2b");
        {
            let mut seg = L2BodySegment::create(&path, 0).unwrap();
            seg.append(0, &ev(0)).unwrap();
            seg.append(1, &ev(1)).unwrap();
            seg.fsync().unwrap();
        }
        let seg = L2BodySegment::open(&path, 0).unwrap();
        assert_eq!(seg.max_seq(), Some(1));
    }

    #[test]
    fn segment_rejects_non_monotonic() {
        let dir = tmp_dir();
        let path = dir.join("00000000000000000000.l2b");
        let mut seg = L2BodySegment::create(&path, 0).unwrap();
        seg.append(0, &ev(0)).unwrap();
        assert!(seg.append(0, &ev(1)).is_err());
    }

    #[test]
    fn store_rolls_over_on_segment_bytes() {
        let dir = tmp_dir();
        // Tiny threshold to force rollover after a few events.
        let cfg = L2BodyConfig { segment_bytes: 64 };
        let mut store = L2BodyStore::open(&dir, cfg).unwrap();
        for i in 0..8u64 {
            store.append(&ev(i)).unwrap();
        }
        assert!(store.segment_count() >= 2, "expected rollover");
        assert_eq!(store.next_seq(), 8);
    }

    #[test]
    fn store_reopen_resumes_next_seq() {
        let dir = tmp_dir();
        {
            let mut store = L2BodyStore::open(&dir, L2BodyConfig::default()).unwrap();
            for i in 0..3u64 {
                store.append(&ev(i)).unwrap();
            }
            store.fsync().unwrap();
        }
        let store = L2BodyStore::open(&dir, L2BodyConfig::default()).unwrap();
        assert_eq!(store.next_seq(), 3);
    }

    #[test]
    fn store_iter_from() {
        let dir = tmp_dir();
        let mut store = L2BodyStore::open(&dir, L2BodyConfig { segment_bytes: 64 }).unwrap();
        for i in 0..6u64 {
            store.append(&ev(i)).unwrap();
        }
        store.fsync().unwrap();

        let all = store.iter_from(0).unwrap();
        assert_eq!(all.len(), 6);

        let tail = store.iter_from(4).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].0, 4);
        assert_eq!(tail[1].0, 5);
    }

    #[test]
    fn store_gc_drops_fully_acked_segments() {
        let dir = tmp_dir();
        let mut store = L2BodyStore::open(&dir, L2BodyConfig { segment_bytes: 64 }).unwrap();
        for i in 0..10u64 {
            store.append(&ev(i)).unwrap();
        }
        store.fsync().unwrap();
        let before = store.segment_count();
        assert!(before >= 2);

        // GC everything except the latest — min_ack_seq well ahead.
        let reclaimed = store.gc_up_to(1_000_000).unwrap();
        assert!(reclaimed >= 1);
        // Writable segment is preserved.
        assert!(store.segment_count() >= 1);
    }

    #[test]
    fn store_gc_respects_ack_cursor() {
        let dir = tmp_dir();
        let mut store = L2BodyStore::open(&dir, L2BodyConfig { segment_bytes: 64 }).unwrap();
        for i in 0..10u64 {
            store.append(&ev(i)).unwrap();
        }
        store.fsync().unwrap();

        // No acks yet — nothing should be reclaimed.
        assert_eq!(store.gc_up_to(0).unwrap(), 0);
    }

    #[test]
    fn segment_truncation_is_recoverable() {
        let dir = tmp_dir();
        let path = dir.join("00000000000000000000.l2b");
        {
            let mut seg = L2BodySegment::create(&path, 0).unwrap();
            for i in 0..3 {
                seg.append(i, &ev(i)).unwrap();
            }
            seg.fsync().unwrap();
        }
        // Corrupt the last 5 bytes (simulating a crash mid-write).
        let mut bytes = std::fs::read(&path).unwrap();
        let cut = bytes.len() - 5;
        bytes.truncate(cut);
        std::fs::write(&path, &bytes).unwrap();

        // Iterator must stop cleanly on the truncated trailing record.
        let recs: Vec<_> = L2BodySegment::iter_records(&path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(recs.len() < 3, "truncated tail must be skipped");
    }
}
