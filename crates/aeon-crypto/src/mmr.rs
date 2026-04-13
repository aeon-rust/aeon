//! Merkle Mountain Range — append-only authenticated log.
//!
//! An MMR is a forest of perfect binary Merkle trees. Each append may merge
//! trees of equal height (like binary addition carries). The "peaks" of the
//! MMR are the roots of these trees; the MMR root is the hash of all peaks.
//!
//! Used to maintain an append-only log of batch Merkle roots, allowing proof
//! that any historical batch is part of the log.

use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::hash::sha512;
use crate::hash::{Hash512, sha512_pair};

/// A node position in the MMR (0-indexed).
type MmrPos = usize;

/// Merkle Mountain Range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MerkleMountainRange {
    /// All nodes stored flat. Leaves and internal nodes interleaved per MMR layout.
    nodes: Vec<Hash512>,
    /// Number of leaves appended.
    leaf_count: u64,
}

impl MerkleMountainRange {
    /// Create an empty MMR.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            leaf_count: 0,
        }
    }

    /// Number of leaves in the MMR.
    pub fn leaf_count(&self) -> u64 {
        self.leaf_count
    }

    /// Total number of nodes (leaves + internal).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Append a leaf hash and return its position.
    ///
    /// After appending, merges are performed based on the binary representation
    /// of `leaf_count`. The number of trailing zeros in the new leaf count
    /// determines how many merges to do.
    pub fn append(&mut self, leaf: Hash512) -> MmrPos {
        let leaf_pos = self.nodes.len();
        self.nodes.push(leaf);
        self.leaf_count += 1;

        // Number of merges = number of trailing zeros in leaf_count after increment.
        // This is equivalent to the number of carry operations in binary addition.
        let merges = self.leaf_count.trailing_zeros();

        let mut right_pos = leaf_pos;
        for h in 0..merges {
            // The subtree rooted at the right child has (2^(h+1) - 1) nodes.
            let subtree_size = (1usize << (h + 1)) - 1;
            let left_pos = right_pos - subtree_size;
            let parent = sha512_pair(
                self.nodes[left_pos].as_ref(),
                self.nodes[right_pos].as_ref(),
            );
            self.nodes.push(parent);
            right_pos = self.nodes.len() - 1;
        }

        leaf_pos
    }

    /// Get the peak positions in the MMR.
    ///
    /// Each set bit in `leaf_count` (from MSB to LSB) represents a perfect
    /// binary tree. The peak of each tree is at a predictable position.
    pub fn peaks(&self) -> Vec<MmrPos> {
        if self.leaf_count == 0 {
            return Vec::new();
        }

        let mut peaks = Vec::new();
        let mut remaining = self.leaf_count;
        let mut offset = 0usize;
        let mut height = 63 - remaining.leading_zeros() as usize;

        loop {
            let tree_leaves = 1u64 << height;
            if remaining >= tree_leaves {
                // A perfect binary tree with `tree_leaves` leaves has (2*tree_leaves - 1) nodes.
                let tree_nodes = (2 * tree_leaves - 1) as usize;
                let peak_pos = offset + tree_nodes - 1;
                peaks.push(peak_pos);
                offset += tree_nodes;
                remaining -= tree_leaves;
            }
            if height == 0 {
                break;
            }
            height -= 1;
        }

        peaks
    }

    /// Compute the MMR root: "bag" the peaks right-to-left.
    pub fn root(&self) -> Hash512 {
        let peaks = self.peaks();
        if peaks.is_empty() {
            return Hash512::ZERO;
        }
        if peaks.len() == 1 {
            return self.nodes[peaks[0]];
        }

        // Bag the peaks: start from the rightmost, fold left.
        // FT-10: guarded above by `peaks.len() == 1` / empty early-return,
        // so at this point peaks.len() >= 2 and last() is Some.
        #[allow(clippy::expect_used)]
        let mut hash = self.nodes[*peaks.last().expect("invariant: peaks.len() >= 2 here")];
        for &peak_pos in peaks.iter().rev().skip(1) {
            hash = sha512_pair(self.nodes[peak_pos].as_ref(), hash.as_ref());
        }
        hash
    }

    /// Serialize to bincode bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    /// Deserialize from bincode bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }
}

impl Default for MerkleMountainRange {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_mmr() {
        let mmr = MerkleMountainRange::new();
        assert_eq!(mmr.leaf_count(), 0);
        assert_eq!(mmr.node_count(), 0);
        assert_eq!(mmr.root(), Hash512::ZERO);
        assert!(mmr.peaks().is_empty());
    }

    #[test]
    fn single_leaf() {
        let mut mmr = MerkleMountainRange::new();
        let h = sha512(b"leaf-0");
        mmr.append(h);

        assert_eq!(mmr.leaf_count(), 1);
        assert_eq!(mmr.node_count(), 1); // just the leaf
        assert_eq!(mmr.peaks(), vec![0]);
        assert_eq!(mmr.root(), h);
    }

    #[test]
    fn two_leaves_merge() {
        let mut mmr = MerkleMountainRange::new();
        let h0 = sha512(b"leaf-0");
        let h1 = sha512(b"leaf-1");
        mmr.append(h0);
        mmr.append(h1);

        // [h0, h1, parent] = 3 nodes
        assert_eq!(mmr.leaf_count(), 2);
        assert_eq!(mmr.node_count(), 3);
        assert_eq!(mmr.peaks(), vec![2]);

        let expected_root = sha512_pair(h0.as_ref(), h1.as_ref());
        assert_eq!(mmr.root(), expected_root);
    }

    #[test]
    fn three_leaves_two_peaks() {
        let mut mmr = MerkleMountainRange::new();
        let h0 = sha512(b"leaf-0");
        let h1 = sha512(b"leaf-1");
        let h2 = sha512(b"leaf-2");
        mmr.append(h0);
        mmr.append(h1);
        mmr.append(h2);

        // [h0, h1, h01, h2] = 4 nodes
        assert_eq!(mmr.leaf_count(), 3);
        assert_eq!(mmr.node_count(), 4);

        let peaks = mmr.peaks();
        assert_eq!(peaks.len(), 2);
        assert_eq!(peaks, vec![2, 3]);
    }

    #[test]
    fn four_leaves_single_peak() {
        let mut mmr = MerkleMountainRange::new();
        let hashes: Vec<Hash512> = (0..4)
            .map(|i| sha512(format!("leaf-{i}").as_bytes()))
            .collect();
        for h in &hashes {
            mmr.append(*h);
        }

        // 4 leaves → 7 nodes (full binary tree)
        assert_eq!(mmr.leaf_count(), 4);
        assert_eq!(mmr.node_count(), 7);
        assert_eq!(mmr.peaks(), vec![6]);
    }

    #[test]
    fn five_leaves_two_peaks() {
        let mut mmr = MerkleMountainRange::new();
        let hashes: Vec<Hash512> = (0..5)
            .map(|i| sha512(format!("leaf-{i}").as_bytes()))
            .collect();
        for h in &hashes {
            mmr.append(*h);
        }

        assert_eq!(mmr.leaf_count(), 5);
        // Tree of 4 (7 nodes) + standalone leaf (1 node) = 8 nodes
        assert_eq!(mmr.node_count(), 8);
        assert_eq!(mmr.peaks(), vec![6, 7]);
    }

    #[test]
    fn seven_leaves_three_peaks() {
        let mut mmr = MerkleMountainRange::new();
        for i in 0..7 {
            mmr.append(sha512(format!("leaf-{i}").as_bytes()));
        }

        assert_eq!(mmr.leaf_count(), 7);
        // Tree of 4 (7 nodes) + tree of 2 (3 nodes) + tree of 1 (1 node) = 11
        assert_eq!(mmr.node_count(), 11);
        let peaks = mmr.peaks();
        assert_eq!(peaks.len(), 3);
        assert_eq!(peaks, vec![6, 9, 10]);
    }

    #[test]
    fn root_changes_with_each_append() {
        let mut mmr = MerkleMountainRange::new();
        let mut roots = Vec::new();

        for i in 0..8 {
            mmr.append(sha512(format!("leaf-{i}").as_bytes()));
            roots.push(mmr.root());
        }

        // All roots should be unique
        for i in 0..roots.len() {
            for j in (i + 1)..roots.len() {
                assert_ne!(roots[i], roots[j], "roots at {i} and {j} collided");
            }
        }
    }

    #[test]
    fn mmr_serde_roundtrip() {
        let mut mmr = MerkleMountainRange::new();
        for i in 0..10 {
            mmr.append(sha512(format!("leaf-{i}").as_bytes()));
        }

        let bytes = mmr.to_bytes().unwrap();
        let decoded = MerkleMountainRange::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.leaf_count(), mmr.leaf_count());
        assert_eq!(decoded.node_count(), mmr.node_count());
        assert_eq!(decoded.root(), mmr.root());
        assert_eq!(decoded.peaks(), mmr.peaks());
    }

    #[test]
    fn append_returns_leaf_position() {
        let mut mmr = MerkleMountainRange::new();
        let pos0 = mmr.append(sha512(b"a"));
        assert_eq!(pos0, 0);

        let pos1 = mmr.append(sha512(b"b"));
        assert_eq!(pos1, 1); // leaf position, parent is at 2

        let pos2 = mmr.append(sha512(b"c"));
        assert_eq!(pos2, 3); // after [a, b, ab, c]
    }

    #[test]
    fn large_mmr_peaks_follow_binary() {
        let mut mmr = MerkleMountainRange::new();
        for i in 0..100 {
            mmr.append(sha512(format!("leaf-{i}").as_bytes()));
        }

        // 100 = 64 + 32 + 4 = binary 1100100 → 3 set bits → 3 peaks
        let peaks = mmr.peaks();
        assert_eq!(peaks.len(), 3);
        assert_eq!(mmr.leaf_count(), 100);
    }

    #[test]
    fn power_of_two_leaves_single_peak() {
        for &n in &[1u64, 2, 4, 8, 16, 32] {
            let mut mmr = MerkleMountainRange::new();
            for i in 0..n {
                mmr.append(sha512(format!("leaf-{i}").as_bytes()));
            }
            assert_eq!(
                mmr.peaks().len(),
                1,
                "expected 1 peak for {n} leaves, got {}",
                mmr.peaks().len()
            );
        }
    }

    #[test]
    fn node_count_formula() {
        // For N leaves, node_count = N + (N - popcount(N))
        // popcount is number of set bits
        for n in 1u64..=64 {
            let mut mmr = MerkleMountainRange::new();
            for i in 0..n {
                mmr.append(sha512(format!("leaf-{i}").as_bytes()));
            }
            let expected = n + n - n.count_ones() as u64;
            assert_eq!(
                mmr.node_count() as u64,
                expected,
                "node_count wrong for {n} leaves"
            );
        }
    }
}
