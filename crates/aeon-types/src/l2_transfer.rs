//! CL-6a wire types for L2 segment streaming.
//!
//! Split out of `aeon_engine::l2_transfer` so both the engine (which
//! produces + consumes L2 segments on disk) and the cluster transport
//! (which ships them between nodes over QUIC) can share the exact same
//! `Serialize`/`Deserialize` layout without a dependency cycle.
//!
//! File-I/O primitives (`SegmentReader`, `SegmentWriter`, `read_manifest`)
//! stay in `aeon-engine` since they know about the `L2BodyStore` layout.
//! These types are the pure data-plane contracts.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Default chunk size for a segment reader — 1 MiB. Balances per-chunk
/// framing overhead against memory footprint + QUIC stream fairness.
pub const DEFAULT_CHUNK_BYTES: usize = 1024 * 1024;

/// File-level description of a single L2 segment: its `start_seq` (which
/// is the filename stem in the `L2BodyStore` layout), total byte length,
/// and a CRC32 over the whole file (header + records). The receiver
/// uses `crc32` to validate the bytes it reassembled before declaring the
/// segment finalised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEntry {
    pub start_seq: u64,
    pub size_bytes: u64,
    pub crc32: u32,
}

/// Enumeration of every `.l2b` segment in a partition directory, ordered
/// by `start_seq` ascending. Sent by the source before any chunks so the
/// target knows what to expect + can validate completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentManifest {
    pub entries: Vec<SegmentEntry>,
}

impl SegmentManifest {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size_bytes).sum()
    }
}

/// One unit of segment data over the wire.
///
/// `data` uses `bytes::Bytes` so forwarding through layers (reader →
/// framing → QUIC → writer) can stay zero-copy where possible. `serde`
/// round-trips `Bytes` via `Vec<u8>`, which is acceptable for the
/// control-plane transfer path (not a hot-path concern).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentChunk {
    /// Identifies which segment this chunk belongs to (matches
    /// [`SegmentEntry::start_seq`]).
    pub start_seq: u64,
    /// Byte offset within the segment file where `data` begins.
    pub offset: u64,
    /// Raw segment bytes.
    #[serde(with = "bytes_serde")]
    pub data: Bytes,
    /// `true` iff this is the final chunk for `start_seq` — the receiver
    /// should CRC-verify on seeing this.
    pub is_last: bool,
}

mod bytes_serde {
    use bytes::Bytes;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(b: &Bytes, ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serde_bytes::Bytes::new(b.as_ref()).serialize(ser)
    }

    pub fn deserialize<'de, D>(de: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v = <serde_bytes::ByteBuf as Deserialize>::deserialize(de)?;
        Ok(Bytes::from(v.into_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_total_bytes_sums_entries() {
        let m = SegmentManifest {
            entries: vec![
                SegmentEntry {
                    start_seq: 0,
                    size_bytes: 100,
                    crc32: 1,
                },
                SegmentEntry {
                    start_seq: 100,
                    size_bytes: 250,
                    crc32: 2,
                },
            ],
        };
        assert_eq!(m.total_bytes(), 350);
        assert!(!m.is_empty());
    }

    #[test]
    fn manifest_empty() {
        let m = SegmentManifest { entries: vec![] };
        assert!(m.is_empty());
        assert_eq!(m.total_bytes(), 0);
    }

    #[test]
    fn chunk_roundtrips_through_bincode() {
        let c = SegmentChunk {
            start_seq: 42,
            offset: 1024,
            data: Bytes::from_static(&[1, 2, 3, 4, 5]),
            is_last: true,
        };
        let bytes = bincode::serialize(&c).unwrap();
        let back: SegmentChunk = bincode::deserialize(&bytes).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn manifest_roundtrips_through_bincode() {
        let m = SegmentManifest {
            entries: vec![SegmentEntry {
                start_seq: 7,
                size_bytes: 99,
                crc32: 0xDEAD_BEEF,
            }],
        };
        let bytes = bincode::serialize(&m).unwrap();
        let back: SegmentManifest = bincode::deserialize(&bytes).unwrap();
        assert_eq!(m, back);
    }
}
