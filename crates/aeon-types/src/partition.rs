//! Partition identifier — newtype wrapping a `u16`.

use std::fmt;

/// Identifies a partition within the Aeon pipeline.
///
/// Partitions are the unit of parallelism and ownership.
/// Partition count is immutable after cluster creation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct PartitionId(u16);

impl PartitionId {
    /// Create a new partition ID.
    #[inline]
    pub const fn new(id: u16) -> Self {
        Self(id)
    }

    /// Get the raw partition number.
    #[inline]
    pub const fn as_u16(self) -> u16 {
        self.0
    }
}

impl fmt::Display for PartitionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "P{}", self.0)
    }
}

impl From<u16> for PartitionId {
    #[inline]
    fn from(id: u16) -> Self {
        Self(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_id_roundtrip() {
        let p = PartitionId::new(42);
        assert_eq!(p.as_u16(), 42);
    }

    #[test]
    fn partition_id_display() {
        let p = PartitionId::new(7);
        assert_eq!(format!("{p}"), "P7");
    }

    #[test]
    fn partition_id_ordering() {
        let a = PartitionId::new(1);
        let b = PartitionId::new(2);
        assert!(a < b);
    }
}
