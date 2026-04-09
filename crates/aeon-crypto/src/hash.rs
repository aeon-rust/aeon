//! SHA-512 hashing primitives for PoH chains and Merkle trees.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha512};

/// A 64-byte SHA-512 hash with serde support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash512(pub [u8; 64]);

impl Hash512 {
    /// Zero hash — used as the "no data" sentinel.
    pub const ZERO: Self = Self([0u8; 64]);

    /// Access the raw bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    /// Format as lowercase hex string.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl AsRef<[u8]> for Hash512 {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Serialize for Hash512 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Hash512 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HashVisitor;

        impl<'de> serde::de::Visitor<'de> for HashVisitor {
            type Value = Hash512;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("64 bytes")
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Hash512, E> {
                if v.len() != 64 {
                    return Err(E::invalid_length(v.len(), &self));
                }
                let mut arr = [0u8; 64];
                arr.copy_from_slice(v);
                Ok(Hash512(arr))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Hash512, A::Error> {
                let mut arr = [0u8; 64];
                for (i, byte) in arr.iter_mut().enumerate() {
                    *byte = seq
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                }
                Ok(Hash512(arr))
            }
        }

        deserializer.deserialize_bytes(HashVisitor)
    }
}

/// Compute SHA-512 of a single byte slice.
#[inline]
pub fn sha512(data: &[u8]) -> Hash512 {
    let mut hasher = Sha512::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut hash = [0u8; 64];
    hash.copy_from_slice(&result);
    Hash512(hash)
}

/// Compute SHA-512 of two concatenated byte slices (no allocation).
#[inline]
pub fn sha512_pair(left: &[u8], right: &[u8]) -> Hash512 {
    let mut hasher = Sha512::new();
    hasher.update(left);
    hasher.update(right);
    let result = hasher.finalize();
    let mut hash = [0u8; 64];
    hash.copy_from_slice(&result);
    Hash512(hash)
}

/// Compute SHA-512(previous_hash || merkle_root || timestamp_bytes).
/// This is the PoH chain hash function.
#[inline]
pub fn sha512_poh(previous: &Hash512, merkle_root: &Hash512, timestamp_nanos: i64) -> Hash512 {
    let mut hasher = Sha512::new();
    hasher.update(previous.0);
    hasher.update(merkle_root.0);
    hasher.update(timestamp_nanos.to_le_bytes());
    let result = hasher.finalize();
    let mut hash = [0u8; 64];
    hash.copy_from_slice(&result);
    Hash512(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha512_deterministic() {
        let h1 = sha512(b"hello world");
        let h2 = sha512(b"hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn sha512_different_inputs() {
        let h1 = sha512(b"hello");
        let h2 = sha512(b"world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn sha512_correct_length() {
        let h = sha512(b"test");
        assert_eq!(h.as_bytes().len(), 64);
    }

    #[test]
    fn sha512_pair_matches_concat() {
        let pair_hash = sha512_pair(b"hello", b"world");
        let concat_hash = sha512(b"helloworld");
        assert_eq!(pair_hash, concat_hash);
    }

    #[test]
    fn sha512_poh_deterministic() {
        let prev = sha512(b"genesis");
        let root = sha512(b"merkle-root");
        let h1 = sha512_poh(&prev, &root, 1234567890);
        let h2 = sha512_poh(&prev, &root, 1234567890);
        assert_eq!(h1, h2);
    }

    #[test]
    fn sha512_poh_different_timestamps() {
        let prev = sha512(b"genesis");
        let root = sha512(b"merkle-root");
        let h1 = sha512_poh(&prev, &root, 1000);
        let h2 = sha512_poh(&prev, &root, 2000);
        assert_ne!(h1, h2);
    }

    #[test]
    fn sha512_empty_input() {
        let h = sha512(b"");
        assert_ne!(h, Hash512::ZERO);
    }

    #[test]
    fn hash_to_hex_format() {
        let h = sha512(b"test");
        let hex = h.to_hex();
        assert_eq!(hex.len(), 128);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn zero_hash_is_all_zeros() {
        assert!(Hash512::ZERO.as_bytes().iter().all(|&b| b == 0));
    }

    #[test]
    fn hash512_serde_roundtrip() {
        let h = sha512(b"serde test");
        let bytes = bincode::serialize(&h).unwrap();
        let decoded: Hash512 = bincode::deserialize(&bytes).unwrap();
        assert_eq!(h, decoded);
    }
}
