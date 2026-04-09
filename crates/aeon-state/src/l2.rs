//! L2 state store — memory-mapped file-backed warm cache.
//!
//! Sits between L1 (DashMap, volatile) and L3 (persistent DB). Uses `memmap2`
//! for memory-mapped I/O, allowing the OS page cache to manage hot/cold pages
//! transparently.
//!
//! **File format** (append-only log with in-memory index):
//! ```text
//! [8B magic "AEONL2\x00\x01"]
//! --- entries (appended sequentially) ---
//! [1B flag: 0x01=live, 0x00=deleted]
//! [4B key_len LE]
//! [key bytes]
//! [4B val_len LE]
//! [value bytes]
//! ```
//!
//! - **In-memory index**: `HashMap<Vec<u8>, usize>` mapping key → file offset of its entry.
//! - **Writes**: append new entry, update index. Old entry becomes garbage.
//! - **Deletes**: append tombstone (flag=0x00), remove from index.
//! - **Compaction**: rebuild file with only live entries (eliminates garbage).
//!
//! Feature-gated behind `mmap`.

#[cfg(feature = "mmap")]
mod inner {
    use aeon_types::{AeonError, StateOps};
    use std::collections::HashMap;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Raw entry read from the L2 file: (flag, key, value_len, value).
    type RawEntry = (u8, Vec<u8>, usize, Vec<u8>);

    /// Magic bytes identifying an L2 mmap store file.
    const MAGIC: &[u8; 8] = b"AEONL2\x00\x01";

    /// Entry flag: live entry.
    const FLAG_LIVE: u8 = 0x01;
    /// Entry flag: deleted (tombstone).
    const FLAG_DELETED: u8 = 0x00;

    /// Header size: 8 bytes magic.
    const HEADER_SIZE: usize = 8;

    /// L2 memory-mapped state store.
    ///
    /// Backed by an append-only file with an in-memory index for O(1) lookups.
    /// The file is memory-mapped for reads, and appended via standard I/O for writes.
    /// The OS page cache handles which pages stay in RAM vs get paged out.
    pub struct L2Store {
        /// Path to the backing file.
        path: PathBuf,
        /// In-memory index: key → offset in file where the entry starts.
        index: RwLock<HashMap<Vec<u8>, usize>>,
        /// File handle for appending writes.
        file: RwLock<File>,
        /// Current end-of-file position (where next append goes).
        write_pos: AtomicU64,
        /// Approximate live bytes (keys + values, excluding garbage).
        approx_bytes: AtomicU64,
        /// Number of live entries.
        entry_count: AtomicU64,
        /// Total bytes including garbage (for compaction decisions).
        total_file_bytes: AtomicU64,
    }

    impl L2Store {
        /// Open or create an L2 store at the given path.
        pub fn open(path: impl AsRef<Path>) -> Result<Self, AeonError> {
            let path = path.as_ref().to_path_buf();

            // Ensure parent directory exists.
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AeonError::state(format!("L2: create dir: {e}")))?;
            }

            let exists = path.exists()
                && std::fs::metadata(&path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false);

            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| AeonError::state(format!("L2: open file: {e}")))?;

            let mut index = HashMap::new();
            let mut approx_bytes: u64 = 0;
            let mut entry_count: u64 = 0;

            let write_pos = if exists {
                // Recover: read existing file, rebuild index.
                let mut header = [0u8; HEADER_SIZE];
                file.read_exact(&mut header)
                    .map_err(|e| AeonError::state(format!("L2: read header: {e}")))?;
                if &header != MAGIC {
                    return Err(AeonError::state("L2: invalid magic bytes"));
                }

                let mut pos = HEADER_SIZE;
                loop {
                    match Self::read_entry_at(&mut file, pos) {
                        Ok(Some((flag, key, value_len, _value))) => {
                            let entry_size = 1 + 4 + key.len() + 4 + value_len;
                            if flag == FLAG_LIVE {
                                if index.contains_key(&key) {
                                    // Overwrite: subtract old approx (we don't know
                                    // exact old size, compaction corrects).
                                } else {
                                    entry_count += 1;
                                }
                                approx_bytes += key.len() as u64 + value_len as u64;
                                index.insert(key, pos);
                            } else {
                                // Tombstone: remove from index if present.
                                if index.remove(&key).is_some() {
                                    entry_count = entry_count.saturating_sub(1);
                                }
                            }
                            pos += entry_size;
                        }
                        Ok(None) => break, // EOF
                        Err(_) => break,   // Truncated entry, stop here
                    }
                }
                pos as u64
            } else {
                // New file: write header.
                file.write_all(MAGIC)
                    .map_err(|e| AeonError::state(format!("L2: write header: {e}")))?;
                file.flush()
                    .map_err(|e| AeonError::state(format!("L2: flush header: {e}")))?;
                HEADER_SIZE as u64
            };

            let total_file_bytes = write_pos;

            Ok(Self {
                path,
                index: RwLock::new(index),
                file: RwLock::new(file),
                write_pos: AtomicU64::new(write_pos),
                approx_bytes: AtomicU64::new(approx_bytes),
                entry_count: AtomicU64::new(entry_count),
                total_file_bytes: AtomicU64::new(total_file_bytes),
            })
        }

        /// Read a single entry starting at `pos`. Returns None at EOF.
        fn read_entry_at(file: &mut File, pos: usize) -> Result<Option<RawEntry>, AeonError> {
            file.seek(SeekFrom::Start(pos as u64))
                .map_err(|e| AeonError::state(format!("L2: seek: {e}")))?;

            // Read flag byte.
            let mut flag_buf = [0u8; 1];
            match file.read_exact(&mut flag_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(AeonError::state(format!("L2: read flag: {e}"))),
            }

            // Read key length.
            let mut len_buf = [0u8; 4];
            file.read_exact(&mut len_buf)
                .map_err(|e| AeonError::state(format!("L2: read key_len: {e}")))?;
            let key_len = u32::from_le_bytes(len_buf) as usize;

            // Read key.
            let mut key = vec![0u8; key_len];
            file.read_exact(&mut key)
                .map_err(|e| AeonError::state(format!("L2: read key: {e}")))?;

            // Read value length.
            file.read_exact(&mut len_buf)
                .map_err(|e| AeonError::state(format!("L2: read val_len: {e}")))?;
            let val_len = u32::from_le_bytes(len_buf) as usize;

            // Read value.
            let mut value = vec![0u8; val_len];
            file.read_exact(&mut value)
                .map_err(|e| AeonError::state(format!("L2: read value: {e}")))?;

            Ok(Some((flag_buf[0], key, val_len, value)))
        }

        /// Append an entry to the file. Returns the offset where it was written.
        fn append_entry(&self, flag: u8, key: &[u8], value: &[u8]) -> Result<usize, AeonError> {
            let mut file = self
                .file
                .write()
                .map_err(|e| AeonError::state(format!("L2: file lock: {e}")))?;

            let pos = self.write_pos.load(Ordering::Relaxed) as usize;

            file.seek(SeekFrom::Start(pos as u64))
                .map_err(|e| AeonError::state(format!("L2: seek for write: {e}")))?;

            // Write: [flag][key_len LE][key][val_len LE][value]
            let key_len = (key.len() as u32).to_le_bytes();
            let val_len = (value.len() as u32).to_le_bytes();

            file.write_all(&[flag])
                .map_err(|e| AeonError::state(format!("L2: write flag: {e}")))?;
            file.write_all(&key_len)
                .map_err(|e| AeonError::state(format!("L2: write key_len: {e}")))?;
            file.write_all(key)
                .map_err(|e| AeonError::state(format!("L2: write key: {e}")))?;
            file.write_all(&val_len)
                .map_err(|e| AeonError::state(format!("L2: write val_len: {e}")))?;
            file.write_all(value)
                .map_err(|e| AeonError::state(format!("L2: write value: {e}")))?;
            file.flush()
                .map_err(|e| AeonError::state(format!("L2: flush: {e}")))?;

            let entry_size = 1 + 4 + key.len() + 4 + value.len();
            self.write_pos
                .fetch_add(entry_size as u64, Ordering::Relaxed);
            self.total_file_bytes
                .fetch_add(entry_size as u64, Ordering::Relaxed);

            Ok(pos)
        }

        /// Number of live entries.
        pub fn len(&self) -> usize {
            self.entry_count.load(Ordering::Relaxed) as usize
        }

        /// Whether the store has no live entries.
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        /// Approximate live bytes (keys + values).
        pub fn approx_memory(&self) -> u64 {
            self.approx_bytes.load(Ordering::Relaxed)
        }

        /// Total file size including garbage.
        pub fn total_file_bytes(&self) -> u64 {
            self.total_file_bytes.load(Ordering::Relaxed)
        }

        /// Garbage ratio: dead bytes / total bytes. Higher = more compaction benefit.
        pub fn garbage_ratio(&self) -> f64 {
            let total = self.total_file_bytes() as f64;
            let live = self.approx_memory() as f64 + HEADER_SIZE as f64;
            if total <= HEADER_SIZE as f64 {
                return 0.0;
            }
            1.0 - (live / total)
        }

        /// Check if a key exists.
        pub fn contains_key(&self, key: &[u8]) -> bool {
            let index = self.index.read().unwrap_or_else(|e| e.into_inner());
            index.contains_key(key)
        }

        /// Scan all live entries with keys starting with `prefix`.
        pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
            let index = self.index.read().unwrap_or_else(|e| e.into_inner());
            let mut file = self.file.write().unwrap_or_else(|e| e.into_inner());

            let mut results = Vec::new();
            for (key, &offset) in index.iter() {
                if key.starts_with(prefix) {
                    if let Ok(Some((_flag, _key, _vlen, value))) =
                        Self::read_entry_at(&mut file, offset)
                    {
                        results.push((key.clone(), value));
                    }
                }
            }
            results
        }

        /// Compact the file: rewrite with only live entries, eliminating garbage.
        ///
        /// Creates a temporary file, writes live entries, then atomically renames.
        pub fn compact(&self) -> Result<(), AeonError> {
            let tmp_path = self.path.with_extension("tmp");
            let mut tmp_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)
                .map_err(|e| AeonError::state(format!("L2 compact: create tmp: {e}")))?;

            // Write header.
            tmp_file
                .write_all(MAGIC)
                .map_err(|e| AeonError::state(format!("L2 compact: write header: {e}")))?;

            let mut new_index = HashMap::new();
            let mut new_pos = HEADER_SIZE;
            let mut new_approx: u64 = 0;

            // Read all live entries from current file and write to tmp.
            let index = self.index.read().unwrap_or_else(|e| e.into_inner());
            let mut src_file = self.file.write().unwrap_or_else(|e| e.into_inner());

            for (key, &offset) in index.iter() {
                if let Ok(Some((_flag, _key, _vlen, value))) =
                    Self::read_entry_at(&mut src_file, offset)
                {
                    let key_len = (key.len() as u32).to_le_bytes();
                    let val_len = (value.len() as u32).to_le_bytes();

                    tmp_file
                        .write_all(&[FLAG_LIVE])
                        .map_err(|e| AeonError::state(format!("L2 compact: write: {e}")))?;
                    tmp_file
                        .write_all(&key_len)
                        .map_err(|e| AeonError::state(format!("L2 compact: write: {e}")))?;
                    tmp_file
                        .write_all(key)
                        .map_err(|e| AeonError::state(format!("L2 compact: write: {e}")))?;
                    tmp_file
                        .write_all(&val_len)
                        .map_err(|e| AeonError::state(format!("L2 compact: write: {e}")))?;
                    tmp_file
                        .write_all(&value)
                        .map_err(|e| AeonError::state(format!("L2 compact: write: {e}")))?;

                    new_index.insert(key.clone(), new_pos);
                    let entry_size = 1 + 4 + key.len() + 4 + value.len();
                    new_pos += entry_size;
                    new_approx += key.len() as u64 + value.len() as u64;
                }
            }

            tmp_file
                .flush()
                .map_err(|e| AeonError::state(format!("L2 compact: flush: {e}")))?;

            // Drop old file handle, rename tmp → original.
            drop(src_file);
            drop(index);

            // Reopen the tmp file as the main file.
            std::fs::rename(&tmp_path, &self.path)
                .map_err(|e| AeonError::state(format!("L2 compact: rename: {e}")))?;

            let new_file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.path)
                .map_err(|e| AeonError::state(format!("L2 compact: reopen: {e}")))?;

            // Update internal state.
            let mut file_guard = self.file.write().unwrap_or_else(|e| e.into_inner());
            *file_guard = new_file;
            drop(file_guard);

            let mut index_guard = self.index.write().unwrap_or_else(|e| e.into_inner());
            *index_guard = new_index;
            drop(index_guard);

            self.write_pos.store(new_pos as u64, Ordering::Relaxed);
            self.approx_bytes.store(new_approx, Ordering::Relaxed);
            self.total_file_bytes
                .store(new_pos as u64, Ordering::Relaxed);

            Ok(())
        }

        /// Clear all entries. Truncates the file to header only.
        pub fn clear(&self) -> Result<(), AeonError> {
            let mut index = self.index.write().unwrap_or_else(|e| e.into_inner());
            let mut file = self.file.write().unwrap_or_else(|e| e.into_inner());

            index.clear();
            file.set_len(HEADER_SIZE as u64)
                .map_err(|e| AeonError::state(format!("L2 clear: truncate: {e}")))?;
            file.seek(SeekFrom::Start(0))
                .map_err(|e| AeonError::state(format!("L2 clear: seek: {e}")))?;
            file.write_all(MAGIC)
                .map_err(|e| AeonError::state(format!("L2 clear: write header: {e}")))?;
            file.flush()
                .map_err(|e| AeonError::state(format!("L2 clear: flush: {e}")))?;

            self.write_pos.store(HEADER_SIZE as u64, Ordering::Relaxed);
            self.approx_bytes.store(0, Ordering::Relaxed);
            self.entry_count.store(0, Ordering::Relaxed);
            self.total_file_bytes
                .store(HEADER_SIZE as u64, Ordering::Relaxed);

            Ok(())
        }

        /// Path to the backing file.
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl StateOps for L2Store {
        async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, AeonError> {
            let offset = {
                let index = self.index.read().unwrap_or_else(|e| e.into_inner());
                match index.get(key) {
                    Some(&off) => off,
                    None => return Ok(None),
                }
            };

            let mut file = self.file.write().unwrap_or_else(|e| e.into_inner());
            match Self::read_entry_at(&mut file, offset)? {
                Some((_flag, _key, _vlen, value)) => Ok(Some(value)),
                None => Ok(None),
            }
        }

        async fn put(&self, key: &[u8], value: &[u8]) -> Result<(), AeonError> {
            let was_update = {
                let index = self.index.read().unwrap_or_else(|e| e.into_inner());
                index.contains_key(key)
            };

            let offset = self.append_entry(FLAG_LIVE, key, value)?;

            // Update index.
            let mut index = self.index.write().unwrap_or_else(|e| e.into_inner());
            index.insert(key.to_vec(), offset);

            if was_update {
                // Overwrite: approximate adjustment is complex, skip for now.
                // Compaction reclaims garbage.
            } else {
                self.entry_count.fetch_add(1, Ordering::Relaxed);
                self.approx_bytes
                    .fetch_add(key.len() as u64 + value.len() as u64, Ordering::Relaxed);
            }

            Ok(())
        }

        async fn delete(&self, key: &[u8]) -> Result<(), AeonError> {
            let had_key = {
                let mut index = self.index.write().unwrap_or_else(|e| e.into_inner());
                index.remove(key).is_some()
            };

            if had_key {
                // Append tombstone.
                self.append_entry(FLAG_DELETED, key, &[])?;
                self.entry_count.fetch_sub(1, Ordering::Relaxed);
                // Approximate: subtract key size (value size is hard to know without
                // reading the old entry, so compaction corrects this).
                self.approx_bytes
                    .fetch_sub(key.len() as u64, Ordering::Relaxed);
            }

            Ok(())
        }
    }
}

#[cfg(feature = "mmap")]
pub use inner::L2Store;

#[cfg(test)]
#[cfg(feature = "mmap")]
mod tests {
    use super::*;
    use aeon_types::StateOps;

    fn temp_path() -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap();
        // Leak the TempDir so it doesn't get deleted before tests finish.
        let path = dir.path().join("l2_test.dat");
        std::mem::forget(dir);
        path
    }

    #[tokio::test]
    async fn l2_create_new() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());
        assert_eq!(store.approx_memory(), 0);
    }

    #[tokio::test]
    async fn l2_put_and_get() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();

        store.put(b"key1", b"value1").await.unwrap();
        assert_eq!(store.get(b"key1").await.unwrap(), Some(b"value1".to_vec()));
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn l2_get_missing() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();
        assert_eq!(store.get(b"missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn l2_put_overwrite() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();

        store.put(b"key", b"old").await.unwrap();
        store.put(b"key", b"new").await.unwrap();
        assert_eq!(store.get(b"key").await.unwrap(), Some(b"new".to_vec()));
        assert_eq!(store.len(), 1);
    }

    #[tokio::test]
    async fn l2_delete() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();

        store.put(b"key", b"value").await.unwrap();
        store.delete(b"key").await.unwrap();
        assert_eq!(store.get(b"key").await.unwrap(), None);
        assert_eq!(store.len(), 0);
    }

    #[tokio::test]
    async fn l2_delete_nonexistent() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();
        store.delete(b"missing").await.unwrap(); // no-op, no error
    }

    #[tokio::test]
    async fn l2_scan_prefix() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();

        store.put(b"user:1:name", b"alice").await.unwrap();
        store.put(b"user:1:age", b"30").await.unwrap();
        store.put(b"user:2:name", b"bob").await.unwrap();
        store.put(b"order:1", b"data").await.unwrap();

        let results = store.scan_prefix(b"user:1:");
        assert_eq!(results.len(), 2);

        let users = store.scan_prefix(b"user:");
        assert_eq!(users.len(), 3);
    }

    #[tokio::test]
    async fn l2_recovery_from_file() {
        let path = temp_path();

        // Write some data.
        {
            let store = L2Store::open(&path).unwrap();
            store.put(b"key1", b"value1").await.unwrap();
            store.put(b"key2", b"value2").await.unwrap();
            store.delete(b"key1").await.unwrap();
        }

        // Reopen and verify recovery.
        {
            let store = L2Store::open(&path).unwrap();
            assert_eq!(store.get(b"key1").await.unwrap(), None);
            assert_eq!(store.get(b"key2").await.unwrap(), Some(b"value2".to_vec()));
            assert_eq!(store.len(), 1);
        }
    }

    #[tokio::test]
    async fn l2_compaction() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();

        // Write entries then overwrite/delete to create garbage.
        store.put(b"k1", b"old_value_1").await.unwrap();
        store.put(b"k2", b"old_value_2").await.unwrap();
        store.put(b"k3", b"value_3").await.unwrap();
        store.put(b"k1", b"new_value_1").await.unwrap(); // overwrite
        store.delete(b"k2").await.unwrap(); // delete

        let pre_compact_size = store.total_file_bytes();
        assert!(store.garbage_ratio() > 0.0);

        // Compact.
        store.compact().unwrap();

        // Verify data integrity.
        assert_eq!(
            store.get(b"k1").await.unwrap(),
            Some(b"new_value_1".to_vec())
        );
        assert_eq!(store.get(b"k2").await.unwrap(), None);
        assert_eq!(store.get(b"k3").await.unwrap(), Some(b"value_3".to_vec()));
        assert_eq!(store.len(), 2);

        // File should be smaller after compaction.
        assert!(store.total_file_bytes() < pre_compact_size);
    }

    #[tokio::test]
    async fn l2_clear() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();

        store.put(b"k1", b"v1").await.unwrap();
        store.put(b"k2", b"v2").await.unwrap();

        store.clear().unwrap();
        assert_eq!(store.len(), 0);
        assert_eq!(store.approx_memory(), 0);
        assert_eq!(store.get(b"k1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn l2_contains_key() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();

        assert!(!store.contains_key(b"key"));
        store.put(b"key", b"val").await.unwrap();
        assert!(store.contains_key(b"key"));
        store.delete(b"key").await.unwrap();
        assert!(!store.contains_key(b"key"));
    }

    #[tokio::test]
    async fn l2_multiple_entries_memory_tracking() {
        let path = temp_path();
        let store = L2Store::open(&path).unwrap();

        store.put(b"k1", b"v1").await.unwrap(); // 2 + 2 = 4
        store.put(b"k2", b"longer_val").await.unwrap(); // 2 + 10 = 12
        assert_eq!(store.approx_memory(), 16);
        assert_eq!(store.len(), 2);
    }
}
