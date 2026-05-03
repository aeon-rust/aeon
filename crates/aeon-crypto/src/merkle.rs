//! Batch Merkle tree — SHA-512 binary tree over event payloads.
//!
//! Produces a Merkle root for a batch of events and supports inclusion proofs.
//! Proofs allow verifying that a specific event was part of a batch without
//! needing the full batch data.

use serde::{Deserialize, Serialize};

use crate::hash::{Hash512, sha512, sha512_pair};

/// Direction of a sibling in a Merkle proof path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProofSide {
    Left,
    Right,
}

/// A single step in a Merkle inclusion proof.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofStep {
    pub hash: Hash512,
    pub side: ProofSide,
}

/// Merkle inclusion proof: proves a leaf is part of the tree with the given root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MerkleProof {
    /// The leaf hash being proven.
    pub leaf_hash: Hash512,
    /// Path from leaf to root (bottom-up).
    pub path: Vec<ProofStep>,
    /// The Merkle root this proof targets.
    pub root: Hash512,
    /// Index of the leaf in the original batch.
    pub leaf_index: usize,
}

impl MerkleProof {
    /// Verify that this proof is valid: recomputing from leaf to root matches.
    pub fn verify(&self) -> bool {
        let mut current = self.leaf_hash;
        for step in &self.path {
            current = match step.side {
                ProofSide::Left => sha512_pair(step.hash.as_ref(), current.as_ref()),
                ProofSide::Right => sha512_pair(current.as_ref(), step.hash.as_ref()),
            };
        }
        current == self.root
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

/// A Merkle tree built from a batch of data items.
///
/// The tree is built bottom-up. If the number of leaves is odd, the last leaf
/// is duplicated to make the level even (standard Merkle tree padding).
pub struct MerkleTree {
    /// All nodes in the tree, stored level by level (leaves first, root last).
    levels: Vec<Vec<Hash512>>,
}

impl MerkleTree {
    /// Build a Merkle tree from raw data items (e.g., event payloads).
    ///
    /// Each item is hashed with SHA-512 to form a leaf.
    /// Returns `None` if the input is empty.
    pub fn from_data(items: &[&[u8]]) -> Option<Self> {
        if items.is_empty() {
            return None;
        }

        let leaves: Vec<Hash512> = items.iter().map(|item| sha512(item)).collect();
        Self::from_leaves(leaves)
    }

    /// Build a Merkle tree from pre-computed leaf hashes.
    pub fn from_leaves(leaves: Vec<Hash512>) -> Option<Self> {
        if leaves.is_empty() {
            return None;
        }

        let mut levels = Vec::new();
        let mut current_level = leaves;

        loop {
            if current_level.len() == 1 {
                levels.push(current_level);
                break;
            }

            // Duplicate last element if odd count
            if current_level.len() % 2 != 0 {
                // FT-10: odd count implies len >= 1, so last() is Some.
                #[allow(clippy::expect_used)]
                let last = *current_level
                    .last()
                    .expect("invariant: odd-length vec is non-empty");
                current_level.push(last);
            }

            let mut next_level = Vec::with_capacity(current_level.len() / 2);
            for pair in current_level.chunks_exact(2) {
                next_level.push(sha512_pair(pair[0].as_ref(), pair[1].as_ref()));
            }

            levels.push(current_level);
            current_level = next_level;
        }

        Some(Self { levels })
    }

    /// The Merkle root hash.
    pub fn root(&self) -> Hash512 {
        *self
            .levels
            .last()
            .and_then(|level| level.first())
            .unwrap_or(&Hash512::ZERO)
    }

    /// Number of leaves in the tree.
    pub fn leaf_count(&self) -> usize {
        self.levels.first().map(|l| l.len()).unwrap_or(0)
    }

    /// Number of levels (height) in the tree.
    pub fn height(&self) -> usize {
        self.levels.len()
    }

    /// Generate an inclusion proof for the leaf at `index`.
    ///
    /// Returns `None` if the index is out of bounds.
    pub fn proof(&self, index: usize) -> Option<MerkleProof> {
        let leaves = self.levels.first()?;
        if index >= leaves.len() {
            return None;
        }

        let leaf_hash = leaves[index];
        let mut path = Vec::new();
        let mut idx = index;

        for level in &self.levels[..self.levels.len() - 1] {
            let sibling_idx = if idx % 2 == 0 { idx + 1 } else { idx - 1 };

            if sibling_idx < level.len() {
                let side = if idx % 2 == 0 {
                    ProofSide::Right
                } else {
                    ProofSide::Left
                };
                path.push(ProofStep {
                    hash: level[sibling_idx],
                    side,
                });
            }

            idx /= 2;
        }

        Some(MerkleProof {
            leaf_hash,
            path,
            root: self.root(),
            leaf_index: index,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_leaf_tree() {
        let tree = MerkleTree::from_data(&[b"only-leaf"]).unwrap();
        assert_eq!(tree.leaf_count(), 1);
        assert_eq!(tree.height(), 1);
        assert_eq!(tree.root(), sha512(b"only-leaf"));
    }

    #[test]
    fn two_leaf_tree() {
        let tree = MerkleTree::from_data(&[b"left", b"right"]).unwrap();
        assert_eq!(tree.leaf_count(), 2);
        assert_eq!(tree.height(), 2);

        let expected_root = sha512_pair(sha512(b"left").as_ref(), sha512(b"right").as_ref());
        assert_eq!(tree.root(), expected_root);
    }

    #[test]
    fn four_leaf_tree() {
        let items: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let tree = MerkleTree::from_data(&items).unwrap();
        assert_eq!(tree.leaf_count(), 4);
        assert_eq!(tree.height(), 3);

        let h_ab = sha512_pair(sha512(b"a").as_ref(), sha512(b"b").as_ref());
        let h_cd = sha512_pair(sha512(b"c").as_ref(), sha512(b"d").as_ref());
        let expected_root = sha512_pair(h_ab.as_ref(), h_cd.as_ref());
        assert_eq!(tree.root(), expected_root);
    }

    #[test]
    fn odd_leaf_count_pads() {
        let items: Vec<&[u8]> = vec![b"a", b"b", b"c"];
        let tree = MerkleTree::from_data(&items).unwrap();
        // 3 leaves → padded to 4 at leaf level (c duplicated)
        assert_eq!(tree.leaf_count(), 4);
        assert_eq!(tree.height(), 3);
    }

    #[test]
    fn empty_input_returns_none() {
        let items: Vec<&[u8]> = vec![];
        assert!(MerkleTree::from_data(&items).is_none());
    }

    #[test]
    fn proof_for_first_leaf() {
        let items: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let tree = MerkleTree::from_data(&items).unwrap();

        let proof = tree.proof(0).unwrap();
        assert_eq!(proof.leaf_hash, sha512(b"a"));
        assert_eq!(proof.leaf_index, 0);
        assert!(proof.verify());
    }

    #[test]
    fn proof_for_last_leaf() {
        let items: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let tree = MerkleTree::from_data(&items).unwrap();

        let proof = tree.proof(3).unwrap();
        assert_eq!(proof.leaf_hash, sha512(b"d"));
        assert!(proof.verify());
    }

    #[test]
    fn proof_for_middle_leaf() {
        let items: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let tree = MerkleTree::from_data(&items).unwrap();

        let proof = tree.proof(2).unwrap();
        assert_eq!(proof.leaf_hash, sha512(b"c"));
        assert!(proof.verify());
    }

    #[test]
    fn proof_for_all_leaves_of_8() {
        let items: Vec<&[u8]> = vec![b"0", b"1", b"2", b"3", b"4", b"5", b"6", b"7"];
        let tree = MerkleTree::from_data(&items).unwrap();

        for i in 0..8 {
            let proof = tree.proof(i).unwrap();
            assert!(proof.verify(), "proof failed for leaf {i}");
            assert_eq!(proof.leaf_index, i);
        }
    }

    #[test]
    fn tampered_proof_fails() {
        let items: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let tree = MerkleTree::from_data(&items).unwrap();

        let mut proof = tree.proof(0).unwrap();
        // Tamper with the leaf hash
        proof.leaf_hash = sha512(b"tampered");
        assert!(!proof.verify());
    }

    #[test]
    fn tampered_root_fails() {
        let items: Vec<&[u8]> = vec![b"a", b"b"];
        let tree = MerkleTree::from_data(&items).unwrap();

        let mut proof = tree.proof(0).unwrap();
        proof.root = sha512(b"fake-root");
        assert!(!proof.verify());
    }

    #[test]
    fn proof_out_of_bounds() {
        let items: Vec<&[u8]> = vec![b"a", b"b"];
        let tree = MerkleTree::from_data(&items).unwrap();
        assert!(tree.proof(5).is_none());
    }

    #[test]
    fn proof_serde_roundtrip() {
        let items: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d"];
        let tree = MerkleTree::from_data(&items).unwrap();
        let proof = tree.proof(1).unwrap();

        let bytes = proof.to_bytes().unwrap();
        let decoded = MerkleProof::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.leaf_hash, proof.leaf_hash);
        assert_eq!(decoded.root, proof.root);
        assert_eq!(decoded.leaf_index, proof.leaf_index);
        assert!(decoded.verify());
    }

    #[test]
    fn single_leaf_proof() {
        let tree = MerkleTree::from_data(&[b"only"]).unwrap();
        let proof = tree.proof(0).unwrap();
        assert!(proof.path.is_empty());
        assert!(proof.verify());
    }

    #[test]
    fn from_leaves_matches_from_data() {
        let items: Vec<&[u8]> = vec![b"x", b"y", b"z"];
        let tree1 = MerkleTree::from_data(&items).unwrap();

        let leaves: Vec<Hash512> = items.iter().map(|i| sha512(i)).collect();
        let tree2 = MerkleTree::from_leaves(leaves).unwrap();

        assert_eq!(tree1.root(), tree2.root());
    }
}
