//! CL-6a — L2 segment streaming primitive for partition transfer.
//!
//! When a partition migrates between nodes, its L2 body segments (the
//! durable replay store for push/poll sources under
//! [`DurabilityMode::OrderedBatch`]) must follow it — otherwise the target
//! node cannot recover un-acked events on crash. This module is the
//! file-level transport: enumerate segments at the source, stream them in
//! chunks, install them at the target, and verify integrity on completion.
//!
//! It is deliberately transport-agnostic. A QUIC wire binding that calls
//! into `SegmentReader` / `SegmentWriter` lives in `aeon-cluster` (CL-6a.2).
//!
//! # Layout assumptions
//!
//! Segment files are `{partition_dir}/{start_seq:020}.l2b`, matching the
//! `L2BodyStore` layout (see `l2_body.rs`). Each file is self-describing
//! (magic + version header, then records) so byte-for-byte transfer
//! preserves correctness without re-encoding events.
//!
//! # Chunking model
//!
//! `SegmentManifest` enumerates all segments up front with sizes + CRC32
//! over the full file. `SegmentReader::next_chunk` then yields
//! `SegmentChunk { start_seq, offset, data, is_last }` tuples. The
//! receiver writes each chunk at `offset` into the matching filename and
//! verifies CRC32 when `is_last` arrives. `SegmentWriter::finish` checks
//! that every manifested segment was fully delivered + CRCs match.
//!
//! # Not in scope for CL-6a.1
//!
//! - QUIC framing, transfer coordinator, `TransferTracker` wiring — those
//!   are later sub-tasks.
//! - In-place cutover semantics (pause source, flush final delta) — lives
//!   at the cluster level where `TransferTracker::begin_cutover` runs.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use aeon_types::AeonError;
use bytes::Bytes;

// Wire types live in aeon-types so the cluster transport can share them.
pub use aeon_types::l2_transfer::{
    DEFAULT_CHUNK_BYTES, SegmentChunk, SegmentEntry, SegmentManifest,
};

/// Scan `partition_dir` and return a sorted manifest of its `.l2b` files.
/// Returns an empty manifest (not an error) if the directory is empty or
/// does not yet exist — lets a new node receive its first segments without
/// pre-creating directories.
pub fn read_manifest(partition_dir: &Path) -> Result<SegmentManifest, AeonError> {
    if !partition_dir.exists() {
        return Ok(SegmentManifest { entries: vec![] });
    }

    let mut entries: BTreeMap<u64, SegmentEntry> = BTreeMap::new();
    for entry in std::fs::read_dir(partition_dir)
        .map_err(|e| AeonError::state(format!("l2-transfer: readdir {partition_dir:?}: {e}")))?
    {
        let entry = entry
            .map_err(|e| AeonError::state(format!("l2-transfer: readdir: {e}")))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("l2b") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(start_seq) = stem.parse::<u64>() else {
            continue;
        };

        // Fresh stat via `fs::metadata` — `DirEntry::metadata` on Windows
        // can return a stale size for a file still held open by us.
        let size_bytes = std::fs::metadata(&path)
            .map_err(|e| AeonError::state(format!("l2-transfer: stat {path:?}: {e}")))?
            .len();
        let crc32 = file_crc32(&path)?;

        entries.insert(
            start_seq,
            SegmentEntry {
                start_seq,
                size_bytes,
                crc32,
            },
        );
    }

    Ok(SegmentManifest {
        entries: entries.into_values().collect(),
    })
}

fn file_crc32(path: &Path) -> Result<u32, AeonError> {
    let mut f = File::open(path)
        .map_err(|e| AeonError::state(format!("l2-transfer: open {path:?}: {e}")))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| AeonError::state(format!("l2-transfer: read {path:?}: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// Sender-side chunked reader over one segment file. Call `next_chunk`
/// repeatedly until it returns `None`.
pub struct SegmentReader {
    start_seq: u64,
    file: File,
    file_len: u64,
    offset: u64,
    chunk_bytes: usize,
}

impl SegmentReader {
    /// Open a segment in `partition_dir` for streaming. `chunk_bytes`
    /// bounds per-chunk payload size (default via [`DEFAULT_CHUNK_BYTES`]).
    pub fn open(
        partition_dir: &Path,
        start_seq: u64,
        chunk_bytes: usize,
    ) -> Result<Self, AeonError> {
        let path = partition_dir.join(format!("{start_seq:020}.l2b"));
        let file = File::open(&path)
            .map_err(|e| AeonError::state(format!("l2-transfer: open {path:?}: {e}")))?;
        let file_len = file
            .metadata()
            .map_err(|e| AeonError::state(format!("l2-transfer: stat: {e}")))?
            .len();
        Ok(Self {
            start_seq,
            file,
            file_len,
            offset: 0,
            chunk_bytes: chunk_bytes.max(1),
        })
    }

    /// Read + yield the next chunk, or `Ok(None)` when exhausted.
    pub fn next_chunk(&mut self) -> Result<Option<SegmentChunk>, AeonError> {
        if self.offset >= self.file_len {
            return Ok(None);
        }

        let remaining = self.file_len - self.offset;
        let take = (self.chunk_bytes as u64).min(remaining) as usize;

        let mut buf = vec![0u8; take];
        self.file
            .read_exact(&mut buf)
            .map_err(|e| AeonError::state(format!("l2-transfer: read chunk: {e}")))?;

        let chunk = SegmentChunk {
            start_seq: self.start_seq,
            offset: self.offset,
            data: Bytes::from(buf),
            is_last: self.offset + take as u64 >= self.file_len,
        };
        self.offset += take as u64;
        Ok(Some(chunk))
    }

    pub fn start_seq(&self) -> u64 {
        self.start_seq
    }

    pub fn total_bytes(&self) -> u64 {
        self.file_len
    }
}

/// Receiver-side incremental installer for one partition. Call
/// `apply_chunk` for each received chunk then `finish` to validate.
pub struct SegmentWriter {
    partition_dir: PathBuf,
    manifest: SegmentManifest,
    /// Open file handles + per-segment bytes-received counters, keyed by
    /// `start_seq`. A segment is "finalized" when bytes_received ==
    /// size_bytes AND CRC passed.
    open_segments: BTreeMap<u64, OpenSegment>,
    /// Seen set — start_seqs for which CRC check passed.
    finalized: BTreeMap<u64, u32>,
}

struct OpenSegment {
    file: File,
    expected_bytes: u64,
    bytes_received: u64,
    path: PathBuf,
}

impl SegmentWriter {
    /// Create a new writer targeting `partition_dir`. The directory is
    /// created if absent. The manifest tells us which segments to expect.
    pub fn open(
        partition_dir: impl Into<PathBuf>,
        manifest: SegmentManifest,
    ) -> Result<Self, AeonError> {
        let partition_dir = partition_dir.into();
        std::fs::create_dir_all(&partition_dir).map_err(|e| {
            AeonError::state(format!(
                "l2-transfer: mkdir {:?}: {}",
                partition_dir, e
            ))
        })?;
        Ok(Self {
            partition_dir,
            manifest,
            open_segments: BTreeMap::new(),
            finalized: BTreeMap::new(),
        })
    }

    /// Apply one received chunk. Opens the segment lazily on first chunk,
    /// writes at the provided offset, and CRC-validates when `is_last`.
    pub fn apply_chunk(&mut self, chunk: SegmentChunk) -> Result<(), AeonError> {
        let entry = self
            .manifest
            .entries
            .iter()
            .find(|e| e.start_seq == chunk.start_seq)
            .ok_or_else(|| {
                AeonError::state(format!(
                    "l2-transfer: chunk for unknown segment start_seq={}",
                    chunk.start_seq
                ))
            })?
            .clone();

        if self.finalized.contains_key(&chunk.start_seq) {
            return Err(AeonError::state(format!(
                "l2-transfer: chunk after finalize for start_seq={}",
                chunk.start_seq
            )));
        }

        let path = self
            .partition_dir
            .join(format!("{:020}.l2b", chunk.start_seq));
        let seg = match self.open_segments.entry(chunk.start_seq) {
            std::collections::btree_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::btree_map::Entry::Vacant(v) => {
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .read(true)
                    .open(&path)
                    .map_err(|e| {
                        AeonError::state(format!(
                            "l2-transfer: create {path:?}: {e}"
                        ))
                    })?;
                v.insert(OpenSegment {
                    file,
                    expected_bytes: entry.size_bytes,
                    bytes_received: 0,
                    path: path.clone(),
                })
            }
        };

        if chunk.offset + chunk.data.len() as u64 > seg.expected_bytes {
            return Err(AeonError::state(format!(
                "l2-transfer: chunk overflows segment: offset={}, len={}, expected={}",
                chunk.offset,
                chunk.data.len(),
                seg.expected_bytes,
            )));
        }

        seg.file
            .seek(SeekFrom::Start(chunk.offset))
            .map_err(|e| AeonError::state(format!("l2-transfer: seek: {e}")))?;
        seg.file
            .write_all(&chunk.data)
            .map_err(|e| AeonError::state(format!("l2-transfer: write: {e}")))?;
        seg.bytes_received = seg.bytes_received.max(chunk.offset + chunk.data.len() as u64);

        if chunk.is_last {
            if seg.bytes_received != seg.expected_bytes {
                return Err(AeonError::state(format!(
                    "l2-transfer: is_last but received {} of {} bytes for start_seq={}",
                    seg.bytes_received, seg.expected_bytes, chunk.start_seq
                )));
            }
            seg.file.sync_data().map_err(|e| {
                AeonError::state(format!("l2-transfer: fsync: {e}"))
            })?;
            let actual_crc = file_crc32(&seg.path)?;
            if actual_crc != entry.crc32 {
                return Err(AeonError::state(format!(
                    "l2-transfer: CRC mismatch for start_seq={}: expected {:08x}, got {:08x}",
                    chunk.start_seq, entry.crc32, actual_crc
                )));
            }
            self.finalized.insert(chunk.start_seq, actual_crc);
            self.open_segments.remove(&chunk.start_seq);
        }

        Ok(())
    }

    /// Assert every segment in the manifest was fully received + validated.
    /// Consumes self — after `finish` the target directory is ready for
    /// `L2BodyStore::open`.
    pub fn finish(self) -> Result<(), AeonError> {
        for entry in &self.manifest.entries {
            if !self.finalized.contains_key(&entry.start_seq) {
                return Err(AeonError::state(format!(
                    "l2-transfer: missing segment start_seq={} at finish",
                    entry.start_seq
                )));
            }
        }
        Ok(())
    }

    /// Number of segments fully received + CRC-validated so far.
    pub fn finalized_count(&self) -> usize {
        self.finalized.len()
    }

    /// Total bytes received + validated (sum of finalized segment sizes).
    pub fn bytes_received(&self) -> u64 {
        self.finalized
            .keys()
            .filter_map(|k| {
                self.manifest
                    .entries
                    .iter()
                    .find(|e| e.start_seq == *k)
                    .map(|e| e.size_bytes)
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::l2_body::{L2BodyConfig, L2BodyStore};
    use aeon_types::{Event, PartitionId};
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
    fn empty_dir_yields_empty_manifest() {
        let dir = tmp_dir();
        let m = read_manifest(&dir).unwrap();
        assert!(m.is_empty());
        assert_eq!(m.total_bytes(), 0);
    }

    #[test]
    fn nonexistent_dir_yields_empty_manifest() {
        let dir = tmp_dir();
        let m = read_manifest(&dir.join("does/not/exist")).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn manifest_lists_segments_sorted_by_start_seq() {
        let dir = tmp_dir();
        // Force multi-segment rollover with a tiny threshold.
        let mut store = L2BodyStore::open(&dir, L2BodyConfig { segment_bytes: 64 }).unwrap();
        for i in 0..8u64 {
            store.append(&ev(i)).unwrap();
        }
        store.fsync().unwrap();

        let m = read_manifest(&dir).unwrap();
        assert!(m.entries.len() >= 2);
        // Sorted ascending.
        for w in m.entries.windows(2) {
            assert!(w[0].start_seq < w[1].start_seq);
        }
        // Sizes non-zero.
        for e in &m.entries {
            assert!(e.size_bytes > 0);
        }
    }

    #[test]
    fn skips_non_l2b_files() {
        let dir = tmp_dir();
        let mut store = L2BodyStore::open(&dir, L2BodyConfig::default()).unwrap();
        store.append(&ev(0)).unwrap();
        store.fsync().unwrap();
        // Drop a stray file that must be ignored.
        std::fs::write(dir.join("notes.txt"), b"hi").unwrap();
        std::fs::write(dir.join("00000000000000000000.bak"), b"hi").unwrap();

        let m = read_manifest(&dir).unwrap();
        assert_eq!(m.entries.len(), 1);
    }

    #[test]
    fn reader_chunks_cover_whole_file() {
        let dir = tmp_dir();
        let mut store = L2BodyStore::open(&dir, L2BodyConfig::default()).unwrap();
        for i in 0..3u64 {
            store.append(&ev(i)).unwrap();
        }
        store.fsync().unwrap();

        let manifest = read_manifest(&dir).unwrap();
        let entry = manifest.entries.first().cloned().unwrap();

        // Small chunks to exercise ragged tail.
        let mut reader = SegmentReader::open(&dir, entry.start_seq, 17).unwrap();
        let mut total = 0u64;
        let mut seen_last = false;
        while let Some(chunk) = reader.next_chunk().unwrap() {
            assert_eq!(chunk.start_seq, entry.start_seq);
            assert_eq!(chunk.offset, total);
            total += chunk.data.len() as u64;
            if chunk.is_last {
                seen_last = true;
                assert_eq!(total, entry.size_bytes);
            }
        }
        assert!(seen_last, "expected one chunk with is_last=true");
        assert_eq!(total, entry.size_bytes);
    }

    #[test]
    fn roundtrip_via_l2_body_store() {
        // Source: write a multi-segment store.
        let src = tmp_dir();
        {
            let mut store = L2BodyStore::open(&src, L2BodyConfig { segment_bytes: 64 }).unwrap();
            for i in 0..10u64 {
                store.append(&ev(i)).unwrap();
            }
            store.fsync().unwrap();
        }

        let manifest = read_manifest(&src).unwrap();
        assert!(manifest.entries.len() >= 2);

        // Target: empty dir, install via SegmentWriter.
        let dst = tmp_dir();
        let mut writer = SegmentWriter::open(&dst, manifest.clone()).unwrap();
        for entry in &manifest.entries {
            let mut reader = SegmentReader::open(&src, entry.start_seq, 64).unwrap();
            while let Some(chunk) = reader.next_chunk().unwrap() {
                writer.apply_chunk(chunk).unwrap();
            }
        }
        assert_eq!(writer.finalized_count(), manifest.entries.len());
        writer.finish().unwrap();

        // Re-open with L2BodyStore — must replay exactly the same events.
        let store = L2BodyStore::open(&dst, L2BodyConfig::default()).unwrap();
        let events = store.iter_from(0).unwrap();
        assert_eq!(events.len(), 10);
        for (i, (seq, e)) in events.into_iter().enumerate() {
            assert_eq!(seq, i as u64);
            assert_eq!(e.payload.as_ref(), format!("payload-{i}").as_bytes());
        }
    }

    #[test]
    fn finish_fails_when_segment_missing() {
        let src = tmp_dir();
        {
            let mut store = L2BodyStore::open(&src, L2BodyConfig { segment_bytes: 64 }).unwrap();
            for i in 0..6u64 {
                store.append(&ev(i)).unwrap();
            }
            store.fsync().unwrap();
        }
        let manifest = read_manifest(&src).unwrap();
        assert!(manifest.entries.len() >= 2);

        let dst = tmp_dir();
        let mut writer = SegmentWriter::open(&dst, manifest.clone()).unwrap();
        // Apply only the first segment.
        let first = &manifest.entries[0];
        let mut reader = SegmentReader::open(&src, first.start_seq, 64).unwrap();
        while let Some(chunk) = reader.next_chunk().unwrap() {
            writer.apply_chunk(chunk).unwrap();
        }
        // finish must reject incomplete transfer.
        assert!(writer.finish().is_err());
    }

    #[test]
    fn apply_rejects_chunk_for_unknown_segment() {
        let src = tmp_dir();
        {
            let mut s = L2BodyStore::open(&src, L2BodyConfig::default()).unwrap();
            s.append(&ev(0)).unwrap();
            s.fsync().unwrap();
        }
        let manifest = read_manifest(&src).unwrap();
        let dst = tmp_dir();
        let mut writer = SegmentWriter::open(&dst, manifest).unwrap();

        let bogus = SegmentChunk {
            start_seq: 99_999,
            offset: 0,
            data: Bytes::from_static(b"hi"),
            is_last: true,
        };
        assert!(writer.apply_chunk(bogus).is_err());
    }

    #[test]
    fn apply_rejects_overflow_chunk() {
        let src = tmp_dir();
        {
            let mut s = L2BodyStore::open(&src, L2BodyConfig::default()).unwrap();
            s.append(&ev(0)).unwrap();
            s.fsync().unwrap();
        }
        let manifest = read_manifest(&src).unwrap();
        let entry = manifest.entries.first().cloned().unwrap();

        let dst = tmp_dir();
        let mut writer = SegmentWriter::open(&dst, manifest).unwrap();

        // Offset beyond expected length — must be rejected before any IO
        // corrupts the target file.
        let bad = SegmentChunk {
            start_seq: entry.start_seq,
            offset: entry.size_bytes, // one past end → any payload overflows
            data: Bytes::from_static(b"x"),
            is_last: true,
        };
        assert!(writer.apply_chunk(bad).is_err());
    }

    #[test]
    fn crc_mismatch_fails_on_last_chunk() {
        let src = tmp_dir();
        {
            let mut s = L2BodyStore::open(&src, L2BodyConfig::default()).unwrap();
            for i in 0..3u64 {
                s.append(&ev(i)).unwrap();
            }
            s.fsync().unwrap();
        }
        // Capture a *correct* manifest from the real file.
        let true_manifest = read_manifest(&src).unwrap();
        let mut poisoned_manifest = true_manifest.clone();
        // Flip a bit in the expected CRC — receiver must reject on finalise.
        poisoned_manifest.entries[0].crc32 ^= 0xDEAD_BEEF;

        let dst = tmp_dir();
        let mut writer = SegmentWriter::open(&dst, poisoned_manifest).unwrap();

        let entry = &true_manifest.entries[0];
        let mut reader = SegmentReader::open(&src, entry.start_seq, 1024).unwrap();
        let mut last_err: Option<AeonError> = None;
        while let Some(chunk) = reader.next_chunk().unwrap() {
            if let Err(e) = writer.apply_chunk(chunk) {
                last_err = Some(e);
                break;
            }
        }
        let msg = last_err.expect("expected CRC failure").to_string();
        assert!(msg.contains("CRC mismatch"), "got error: {msg}");
    }

    #[test]
    fn second_finalize_is_rejected() {
        // A chunk that arrives with is_last=true a second time would
        // overwrite an already-validated segment — reject it.
        let src = tmp_dir();
        {
            let mut s = L2BodyStore::open(&src, L2BodyConfig::default()).unwrap();
            s.append(&ev(0)).unwrap();
            s.fsync().unwrap();
        }
        let manifest = read_manifest(&src).unwrap();
        let entry = manifest.entries.first().cloned().unwrap();

        let dst = tmp_dir();
        let mut writer = SegmentWriter::open(&dst, manifest).unwrap();

        // First transfer: apply every chunk normally.
        let mut reader = SegmentReader::open(&src, entry.start_seq, 64).unwrap();
        while let Some(chunk) = reader.next_chunk().unwrap() {
            writer.apply_chunk(chunk).unwrap();
        }
        assert_eq!(writer.finalized_count(), 1);

        // Any further chunk for the same start_seq must be rejected.
        let replay = SegmentChunk {
            start_seq: entry.start_seq,
            offset: 0,
            data: Bytes::from_static(b"ignore-me"),
            is_last: true,
        };
        assert!(writer.apply_chunk(replay).is_err());
    }

    #[test]
    fn bytes_received_tracks_validated_segments() {
        let src = tmp_dir();
        {
            let mut s = L2BodyStore::open(&src, L2BodyConfig { segment_bytes: 64 }).unwrap();
            for i in 0..8u64 {
                s.append(&ev(i)).unwrap();
            }
            s.fsync().unwrap();
        }
        let manifest = read_manifest(&src).unwrap();
        let total = manifest.total_bytes();
        assert!(manifest.entries.len() >= 2);

        let dst = tmp_dir();
        let mut writer = SegmentWriter::open(&dst, manifest.clone()).unwrap();
        assert_eq!(writer.bytes_received(), 0);

        for (i, entry) in manifest.entries.iter().enumerate() {
            let mut reader = SegmentReader::open(&src, entry.start_seq, 32).unwrap();
            while let Some(chunk) = reader.next_chunk().unwrap() {
                writer.apply_chunk(chunk).unwrap();
            }
            assert_eq!(writer.finalized_count(), i + 1);
        }
        assert_eq!(writer.bytes_received(), total);
    }
}
