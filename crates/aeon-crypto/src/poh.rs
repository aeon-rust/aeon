//! Proof of History — per-partition hash chains.
//!
//! Each partition maintains an independent PoH chain:
//!   hash[n] = SHA-512(hash[n-1] || merkle_root || timestamp_nanos)
//!
//! The genesis hash is: SHA-512(b"aeon-poh-genesis" || partition_id_le_bytes)
//!
//! PoH provides:
//! - **Ordering proof**: Event batch N came before batch N+1 (hash chain).
//! - **Integrity**: Any modification to a past batch breaks the chain.
//! - **Transfer continuity**: When a partition moves between nodes, the chain
//!   continues from the last entry — no gaps allowed.

use aeon_types::PartitionId;
use serde::{Deserialize, Serialize};

use crate::hash::{Hash512, sha512, sha512_poh};
use crate::merkle::MerkleTree;
use crate::mmr::MerkleMountainRange;
use crate::signing::{SignedRoot, SigningKey};

/// A single entry in a PoH chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PohEntry {
    /// Sequence number in the chain (0 = genesis, 1 = first batch, ...).
    pub sequence: u64,
    /// The PoH hash: SHA-512(previous_hash || merkle_root || timestamp).
    pub hash: Hash512,
    /// Merkle root of the event batch that produced this entry.
    pub merkle_root: Hash512,
    /// Timestamp (nanos since epoch) of the batch.
    pub timestamp_nanos: i64,
    /// Number of events in the batch.
    pub batch_size: u32,
    /// Ed25519 signature of the Merkle root (signed by the producing node).
    pub signed_root: Option<SignedRoot>,
}

impl PohEntry {
    /// Verify this entry's hash against its predecessor.
    pub fn verify_chain(&self, previous_hash: &Hash512) -> bool {
        let expected = sha512_poh(previous_hash, &self.merkle_root, self.timestamp_nanos);
        self.hash == expected
    }

    /// Verify the Ed25519 signature on the Merkle root (if present).
    pub fn verify_signature(&self) -> Result<bool, aeon_types::AeonError> {
        match &self.signed_root {
            Some(sr) => sr.verify(),
            None => Ok(true), // No signature to verify
        }
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

/// Per-partition Proof of History chain.
///
/// Maintains the current chain head and an MMR of all batch Merkle roots.
pub struct PohChain {
    /// Which partition this chain belongs to.
    partition: PartitionId,
    /// The genesis hash for this partition.
    genesis_hash: Hash512,
    /// Current chain head hash.
    current_hash: Hash512,
    /// Current sequence number (next entry will be this number).
    sequence: u64,
    /// Append-only log of Merkle roots (MMR).
    mmr: MerkleMountainRange,
    /// Recent entries kept in memory for verification (ring buffer style).
    /// Only the last N entries are kept; older ones are in the MMR.
    recent_entries: Vec<PohEntry>,
    /// Max recent entries to keep in memory.
    max_recent: usize,
}

impl PohChain {
    /// Create a new PoH chain for a partition.
    ///
    /// Genesis hash = SHA-512(b"aeon-poh-genesis" || partition_id_le_bytes)
    pub fn new(partition: PartitionId, max_recent: usize) -> Self {
        let genesis_hash = Self::compute_genesis(partition);
        Self {
            partition,
            genesis_hash,
            current_hash: genesis_hash,
            sequence: 0,
            mmr: MerkleMountainRange::new(),
            recent_entries: Vec::new(),
            max_recent,
        }
    }

    /// Compute the genesis hash for a partition.
    pub fn compute_genesis(partition: PartitionId) -> Hash512 {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(b"aeon-poh-genesis");
        data.extend_from_slice(&partition.as_u16().to_le_bytes());
        sha512(&data)
    }

    /// Resume a PoH chain from a known state (e.g., after partition transfer).
    ///
    /// The caller provides the last known hash, sequence, and MMR state.
    pub fn resume(
        partition: PartitionId,
        last_hash: Hash512,
        sequence: u64,
        mmr: MerkleMountainRange,
        max_recent: usize,
    ) -> Self {
        Self {
            partition,
            genesis_hash: Self::compute_genesis(partition),
            current_hash: last_hash,
            sequence,
            mmr,
            recent_entries: Vec::new(),
            max_recent,
        }
    }

    /// Append a batch of event payloads to the PoH chain.
    ///
    /// 1. Builds a Merkle tree from the payloads.
    /// 2. Optionally signs the Merkle root with the node's signing key.
    /// 3. Computes the new PoH hash: SHA-512(previous || merkle_root || timestamp).
    /// 4. Appends the Merkle root to the MMR.
    ///
    /// Returns the new PohEntry, or None if payloads is empty.
    pub fn append_batch(
        &mut self,
        payloads: &[&[u8]],
        timestamp_nanos: i64,
        signing_key: Option<&SigningKey>,
    ) -> Option<PohEntry> {
        if payloads.is_empty() {
            return None;
        }

        // Build Merkle tree from event payloads
        let tree = MerkleTree::from_data(payloads)?;
        let merkle_root = tree.root();

        // Compute PoH hash
        let poh_hash = sha512_poh(&self.current_hash, &merkle_root, timestamp_nanos);

        // Sign the Merkle root
        let signed_root = signing_key.map(|sk| sk.sign_root(&merkle_root, timestamp_nanos));

        let entry = PohEntry {
            sequence: self.sequence,
            hash: poh_hash,
            merkle_root,
            timestamp_nanos,
            batch_size: payloads.len() as u32,
            signed_root,
        };

        // Update chain state
        self.current_hash = poh_hash;
        self.sequence += 1;

        // Append to MMR
        self.mmr.append(merkle_root);

        // Keep in recent entries
        self.recent_entries.push(entry.clone());
        if self.recent_entries.len() > self.max_recent {
            self.recent_entries.remove(0);
        }

        Some(entry)
    }

    /// Append a pre-computed Merkle root (used during chain transfer).
    pub fn append_root(
        &mut self,
        merkle_root: Hash512,
        timestamp_nanos: i64,
        batch_size: u32,
        signing_key: Option<&SigningKey>,
    ) -> PohEntry {
        let poh_hash = sha512_poh(&self.current_hash, &merkle_root, timestamp_nanos);
        let signed_root = signing_key.map(|sk| sk.sign_root(&merkle_root, timestamp_nanos));

        let entry = PohEntry {
            sequence: self.sequence,
            hash: poh_hash,
            merkle_root,
            timestamp_nanos,
            batch_size,
            signed_root,
        };

        self.current_hash = poh_hash;
        self.sequence += 1;
        self.mmr.append(merkle_root);

        self.recent_entries.push(entry.clone());
        if self.recent_entries.len() > self.max_recent {
            self.recent_entries.remove(0);
        }

        entry
    }

    /// Current chain head hash.
    pub fn current_hash(&self) -> &Hash512 {
        &self.current_hash
    }

    /// Genesis hash for this partition.
    pub fn genesis_hash(&self) -> &Hash512 {
        &self.genesis_hash
    }

    /// Current sequence number (number of entries appended so far).
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The partition this chain belongs to.
    pub fn partition(&self) -> PartitionId {
        self.partition
    }

    /// The MMR root covering all batch Merkle roots.
    pub fn mmr_root(&self) -> Hash512 {
        self.mmr.root()
    }

    /// Reference to the internal MMR.
    pub fn mmr(&self) -> &MerkleMountainRange {
        &self.mmr
    }

    /// Recent entries (most recent N).
    pub fn recent_entries(&self) -> &[PohEntry] {
        &self.recent_entries
    }

    /// Verify the entire chain of recent entries.
    ///
    /// Returns the index of the first invalid entry, or None if all are valid.
    pub fn verify_recent(&self) -> Option<usize> {
        if self.recent_entries.is_empty() {
            return None;
        }

        // Determine the hash before the first recent entry
        let mut prev_hash = if self.recent_entries[0].sequence == 0 {
            self.genesis_hash
        } else {
            // We can't verify entries before what we have in memory.
            // Verify internal consistency starting from the first available.
            // Use the first entry's chain link: prev must produce first entry's hash.
            // We can't verify the first entry without its predecessor, so start from entry[1].
            // But we can verify entry[0] by recomputing.
            // Actually, we don't have the previous hash. We'll verify pair consistency.
            return self.verify_recent_pairwise();
        };

        for (i, entry) in self.recent_entries.iter().enumerate() {
            if !entry.verify_chain(&prev_hash) {
                return Some(i);
            }
            prev_hash = entry.hash;
        }

        None
    }

    /// Verify consecutive pairs of recent entries are chain-linked.
    fn verify_recent_pairwise(&self) -> Option<usize> {
        for i in 1..self.recent_entries.len() {
            let prev = &self.recent_entries[i - 1];
            let curr = &self.recent_entries[i];
            if !curr.verify_chain(&prev.hash) {
                return Some(i);
            }
        }
        None
    }

    /// Export the chain state for transfer to another node.
    pub fn export_state(&self) -> PohChainState {
        PohChainState {
            partition: self.partition,
            current_hash: self.current_hash,
            sequence: self.sequence,
            mmr: self.mmr.clone(),
        }
    }

    /// Import chain state from another node (partition transfer).
    pub fn from_state(state: PohChainState, max_recent: usize) -> Self {
        Self {
            partition: state.partition,
            genesis_hash: Self::compute_genesis(state.partition),
            current_hash: state.current_hash,
            sequence: state.sequence,
            mmr: state.mmr,
            recent_entries: Vec::new(),
            max_recent,
        }
    }
}

/// Serializable PoH chain state for transfer between nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PohChainState {
    pub partition: PartitionId,
    pub current_hash: Hash512,
    pub sequence: u64,
    pub mmr: MerkleMountainRange,
}

impl PohChainState {
    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }
}

/// Policy for verifying a `PohChainState` received during partition
/// transfer (CL-6b). Configurable so operators can trade correctness
/// strictness against transfer throughput.
///
/// * `Verify` — default. Structural validation of the incoming state:
///   partition id matches the expected partition, `current_hash` is
///   consistent with `sequence` (genesis iff seq==0), and the MMR's
///   internal leaf/peak accounting is self-consistent. Does **not**
///   verify hash-chain linkage because the current wire format
///   (`PohChainState`) is MMR-only and does not carry recent entries.
///
/// * `VerifyWithKey` — like `Verify`, plus validates every signed
///   `PohEntry` carried in the (future, optional) `recent_entries`
///   field against the supplied `VerifyingKey`. Until the wire format
///   carries recent entries this degrades to `Verify` plus a
///   documented TODO-on-import, so callers can opt in today and get
///   stronger checks for free when the extension lands.
///
/// * `TrustExtend` — skip all verification and accept the state as-is
///   (legacy behaviour of `PohChain::from_state`). Only safe when the
///   transport is already authenticated (mTLS) and the operator fully
///   trusts the source node — e.g. internal cluster with signed certs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PohVerifyMode {
    #[default]
    Verify,
    VerifyWithKey,
    TrustExtend,
}

impl std::str::FromStr for PohVerifyMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "verify" => Ok(PohVerifyMode::Verify),
            "verify_with_key" | "verify-with-key" | "verifywithkey" => {
                Ok(PohVerifyMode::VerifyWithKey)
            }
            "trust_extend" | "trust-extend" | "trustextend" => Ok(PohVerifyMode::TrustExtend),
            other => Err(format!(
                "unknown poh_verify_mode '{other}' (valid: verify, verify_with_key, trust_extend)"
            )),
        }
    }
}

/// Errors returned by [`PohChain::verify_state`].
#[derive(Debug, thiserror::Error)]
pub enum PohVerifyError {
    #[error("poh-verify: partition mismatch: expected {expected:?}, state carries {actual:?}")]
    PartitionMismatch {
        expected: PartitionId,
        actual: PartitionId,
    },
    #[error(
        "poh-verify: sequence/hash inconsistency: seq={sequence} but current_hash {state} genesis"
    )]
    SequenceHashInconsistent {
        sequence: u64,
        /// "== " or "!= " — just a string slot for the message.
        state: &'static str,
    },
    #[error("poh-verify: MMR leaf_count {mmr_leaves} exceeds chain sequence {sequence}")]
    MmrAheadOfSequence { mmr_leaves: u64, sequence: u64 },
    #[error(
        "poh-verify: MMR internal inconsistency: leaf_count={leaf_count} but {peaks} peaks for that count is invalid"
    )]
    MmrPeaksInconsistent { leaf_count: u64, peaks: usize },
}

impl PohChain {
    /// Structurally verify a `PohChainState` before trusting it on import.
    ///
    /// See [`PohVerifyMode`] for the boundaries of what this can and
    /// cannot catch given the current wire format. Callers in
    /// `TrustExtend` mode skip this entirely and hand the state
    /// straight to [`PohChain::from_state`].
    ///
    /// `expected_partition` is the partition the caller believes is
    /// being transferred; we surface a `PartitionMismatch` error rather
    /// than silently accepting a state for the wrong partition.
    pub fn verify_state(
        state: &PohChainState,
        expected_partition: PartitionId,
    ) -> Result<(), PohVerifyError> {
        if state.partition != expected_partition {
            return Err(PohVerifyError::PartitionMismatch {
                expected: expected_partition,
                actual: state.partition,
            });
        }

        let genesis = Self::compute_genesis(state.partition);
        let at_genesis = state.current_hash == genesis;
        match (state.sequence, at_genesis) {
            (0, true) | (1.., false) => {} // consistent
            (0, false) => {
                return Err(PohVerifyError::SequenceHashInconsistent {
                    sequence: 0,
                    state: "!= ",
                });
            }
            (seq, true) => {
                return Err(PohVerifyError::SequenceHashInconsistent {
                    sequence: seq,
                    state: "== ",
                });
            }
        }

        if state.mmr.leaf_count() > state.sequence {
            return Err(PohVerifyError::MmrAheadOfSequence {
                mmr_leaves: state.mmr.leaf_count(),
                sequence: state.sequence,
            });
        }

        let peaks = state.mmr.peaks();
        let expected_peaks = state.mmr.leaf_count().count_ones() as usize;
        if peaks.len() != expected_peaks {
            return Err(PohVerifyError::MmrPeaksInconsistent {
                leaf_count: state.mmr.leaf_count(),
                peaks: peaks.len(),
            });
        }

        Ok(())
    }
}

/// Global PoH checkpoint — aggregates per-partition chain heads.
///
/// Replicated via Raft to give the cluster a consistent view of PoH state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PohCheckpoint {
    /// Per-partition: (partition_id, latest_hash, sequence).
    pub partition_heads: Vec<(PartitionId, Hash512, u64)>,
    /// Aggregate hash: SHA-512 of all partition head hashes concatenated.
    pub aggregate_hash: Hash512,
    /// Timestamp of the checkpoint.
    pub timestamp_nanos: i64,
}

impl PohCheckpoint {
    /// Build a checkpoint from a set of partition chain heads.
    pub fn build(heads: &[(PartitionId, Hash512, u64)], timestamp_nanos: i64) -> Self {
        let mut sorted = heads.to_vec();
        sorted.sort_by_key(|(p, _, _)| *p);

        // Aggregate hash: SHA-512 of all hashes concatenated
        let aggregate_hash = if sorted.is_empty() {
            Hash512::ZERO
        } else {
            let mut data = Vec::with_capacity(sorted.len() * 64);
            for (_, hash, _) in &sorted {
                data.extend_from_slice(hash.as_ref());
            }
            sha512(&data)
        };

        Self {
            partition_heads: sorted,
            aggregate_hash,
            timestamp_nanos,
        }
    }

    /// Verify the aggregate hash matches the partition heads.
    pub fn verify(&self) -> bool {
        let mut sorted = self.partition_heads.clone();
        sorted.sort_by_key(|(p, _, _)| *p);

        let expected = if sorted.is_empty() {
            Hash512::ZERO
        } else {
            let mut data = Vec::with_capacity(sorted.len() * 64);
            for (_, hash, _) in &sorted {
                data.extend_from_slice(hash.as_ref());
            }
            sha512(&data)
        };

        self.aggregate_hash == expected
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_chain(partition: u16) -> PohChain {
        PohChain::new(PartitionId::new(partition), 100)
    }

    #[test]
    fn genesis_hash_deterministic() {
        let h1 = PohChain::compute_genesis(PartitionId::new(0));
        let h2 = PohChain::compute_genesis(PartitionId::new(0));
        assert_eq!(h1, h2);
    }

    #[test]
    fn genesis_hash_differs_per_partition() {
        let h0 = PohChain::compute_genesis(PartitionId::new(0));
        let h1 = PohChain::compute_genesis(PartitionId::new(1));
        assert_ne!(h0, h1);
    }

    #[test]
    fn new_chain_starts_at_genesis() {
        let chain = make_chain(0);
        assert_eq!(*chain.current_hash(), chain.genesis_hash);
        assert_eq!(chain.sequence(), 0);
    }

    #[test]
    fn append_batch_advances_chain() {
        let mut chain = make_chain(0);
        let payloads: Vec<&[u8]> = vec![b"event-1", b"event-2", b"event-3"];

        let entry = chain.append_batch(&payloads, 1000, None).unwrap();

        assert_eq!(entry.sequence, 0);
        assert_eq!(entry.batch_size, 3);
        assert_eq!(entry.timestamp_nanos, 1000);
        assert_eq!(chain.sequence(), 1);
        assert_eq!(*chain.current_hash(), entry.hash);
    }

    #[test]
    fn chain_is_deterministic() {
        let mut c1 = make_chain(5);
        let mut c2 = make_chain(5);

        let payloads: Vec<&[u8]> = vec![b"a", b"b"];
        let e1 = c1.append_batch(&payloads, 1000, None).unwrap();
        let e2 = c2.append_batch(&payloads, 1000, None).unwrap();

        assert_eq!(e1.hash, e2.hash);
        assert_eq!(e1.merkle_root, e2.merkle_root);
    }

    #[test]
    fn different_payloads_different_hashes() {
        let mut c1 = make_chain(0);
        let mut c2 = make_chain(0);

        let e1 = c1.append_batch(&[b"hello"], 1000, None).unwrap();
        let e2 = c2.append_batch(&[b"world"], 1000, None).unwrap();

        assert_ne!(e1.hash, e2.hash);
        assert_ne!(e1.merkle_root, e2.merkle_root);
    }

    #[test]
    fn different_timestamps_different_hashes() {
        let mut c1 = make_chain(0);
        let mut c2 = make_chain(0);

        let e1 = c1.append_batch(&[b"same"], 1000, None).unwrap();
        let e2 = c2.append_batch(&[b"same"], 2000, None).unwrap();

        assert_ne!(e1.hash, e2.hash);
        // Merkle roots should be the same (same payloads)
        assert_eq!(e1.merkle_root, e2.merkle_root);
    }

    #[test]
    fn chain_links_verify() {
        let mut chain = make_chain(0);
        let genesis = *chain.genesis_hash();

        let e0 = chain.append_batch(&[b"batch-0"], 1000, None).unwrap();
        assert!(e0.verify_chain(&genesis));

        let e1 = chain.append_batch(&[b"batch-1"], 2000, None).unwrap();
        assert!(e1.verify_chain(&e0.hash));

        let e2 = chain.append_batch(&[b"batch-2"], 3000, None).unwrap();
        assert!(e2.verify_chain(&e1.hash));

        // Wrong predecessor fails
        assert!(!e2.verify_chain(&e0.hash));
        assert!(!e1.verify_chain(&genesis));
    }

    #[test]
    fn empty_batch_returns_none() {
        let mut chain = make_chain(0);
        let payloads: Vec<&[u8]> = vec![];
        assert!(chain.append_batch(&payloads, 1000, None).is_none());
        assert_eq!(chain.sequence(), 0);
    }

    #[test]
    fn signed_batch() {
        let mut chain = make_chain(0);
        let key = SigningKey::generate();

        let entry = chain
            .append_batch(&[b"signed-event"], 1000, Some(&key))
            .unwrap();

        assert!(entry.signed_root.is_some());
        assert!(entry.verify_signature().unwrap());
    }

    #[test]
    fn unsigned_batch() {
        let mut chain = make_chain(0);
        let entry = chain.append_batch(&[b"unsigned"], 1000, None).unwrap();

        assert!(entry.signed_root.is_none());
        assert!(entry.verify_signature().unwrap()); // No signature = trivially valid
    }

    #[test]
    fn mmr_grows_with_batches() {
        let mut chain = make_chain(0);

        for i in 0..5 {
            chain.append_batch(&[format!("batch-{i}").as_bytes()], (i + 1) * 1000, None);
        }

        assert_eq!(chain.mmr().leaf_count(), 5);
    }

    #[test]
    fn recent_entries_capped() {
        let mut chain = PohChain::new(PartitionId::new(0), 3);

        for i in 0..10 {
            chain.append_batch(&[format!("batch-{i}").as_bytes()], (i + 1) * 1000, None);
        }

        assert_eq!(chain.recent_entries().len(), 3);
        assert_eq!(chain.recent_entries()[0].sequence, 7);
        assert_eq!(chain.recent_entries()[2].sequence, 9);
    }

    #[test]
    fn verify_recent_chain_valid() {
        let mut chain = make_chain(0);

        for i in 0..5 {
            chain.append_batch(&[format!("batch-{i}").as_bytes()], (i + 1) * 1000, None);
        }

        assert!(chain.verify_recent().is_none()); // None = all valid
    }

    #[test]
    fn export_import_state() {
        let mut chain = make_chain(3);
        for i in 0..5 {
            chain.append_batch(&[format!("data-{i}").as_bytes()], (i + 1) * 1000, None);
        }

        let state = chain.export_state();
        let bytes = state.to_bytes().unwrap();
        let decoded_state = PohChainState::from_bytes(&bytes).unwrap();

        let restored = PohChain::from_state(decoded_state, 100);

        assert_eq!(restored.partition(), chain.partition());
        assert_eq!(*restored.current_hash(), *chain.current_hash());
        assert_eq!(restored.sequence(), chain.sequence());
        assert_eq!(restored.mmr_root(), chain.mmr_root());
    }

    #[test]
    fn transfer_continuity() {
        // Simulate partition transfer: node A produces batches, exports state,
        // node B imports and continues the chain.
        let mut node_a = make_chain(7);
        let key_a = SigningKey::generate();

        let e0 = node_a
            .append_batch(&[b"from-a-0"], 1000, Some(&key_a))
            .unwrap();
        let e1 = node_a
            .append_batch(&[b"from-a-1"], 2000, Some(&key_a))
            .unwrap();

        // Transfer
        let state = node_a.export_state();
        let mut node_b = PohChain::from_state(state, 100);
        let key_b = SigningKey::generate();

        // Node B continues the chain
        let e2 = node_b
            .append_batch(&[b"from-b-0"], 3000, Some(&key_b))
            .unwrap();

        // Chain continuity: e2 links to e1
        assert!(e2.verify_chain(&e1.hash));
        assert_eq!(e2.sequence, 2);

        // e2 was signed by node B, not node A
        let sr = e2.signed_root.as_ref().unwrap();
        assert!(sr.verify_with_key(&key_b.verifying_key()));
        assert!(!sr.verify_with_key(&key_a.verifying_key()));

        // Full chain verifiable
        let genesis = PohChain::compute_genesis(PartitionId::new(7));
        assert!(e0.verify_chain(&genesis));
        assert!(e1.verify_chain(&e0.hash));
        assert!(e2.verify_chain(&e1.hash));
    }

    #[test]
    fn checkpoint_build_and_verify() {
        let mut c0 = make_chain(0);
        let mut c1 = make_chain(1);
        let mut c2 = make_chain(2);

        c0.append_batch(&[b"p0"], 1000, None);
        c1.append_batch(&[b"p1"], 1000, None);
        c2.append_batch(&[b"p2"], 1000, None);

        let heads = vec![
            (PartitionId::new(0), *c0.current_hash(), c0.sequence()),
            (PartitionId::new(1), *c1.current_hash(), c1.sequence()),
            (PartitionId::new(2), *c2.current_hash(), c2.sequence()),
        ];

        let checkpoint = PohCheckpoint::build(&heads, 2000);
        assert!(checkpoint.verify());
        assert_eq!(checkpoint.partition_heads.len(), 3);
    }

    #[test]
    fn checkpoint_serde_roundtrip() {
        let mut c0 = make_chain(0);
        c0.append_batch(&[b"data"], 1000, None);

        let heads = vec![(PartitionId::new(0), *c0.current_hash(), c0.sequence())];
        let checkpoint = PohCheckpoint::build(&heads, 2000);

        let bytes = checkpoint.to_bytes().unwrap();
        let decoded = PohCheckpoint::from_bytes(&bytes).unwrap();
        assert!(decoded.verify());
        assert_eq!(decoded.aggregate_hash, checkpoint.aggregate_hash);
    }

    #[test]
    fn checkpoint_tampered_fails() {
        let heads = vec![
            (PartitionId::new(0), sha512(b"h0"), 1),
            (PartitionId::new(1), sha512(b"h1"), 2),
        ];
        let mut checkpoint = PohCheckpoint::build(&heads, 1000);

        // Tamper with one hash
        checkpoint.partition_heads[0].1 = sha512(b"tampered");
        assert!(!checkpoint.verify());
    }

    #[test]
    fn checkpoint_empty_partitions() {
        let checkpoint = PohCheckpoint::build(&[], 1000);
        assert!(checkpoint.verify());
        assert_eq!(checkpoint.aggregate_hash, Hash512::ZERO);
    }

    #[test]
    fn poh_entry_serde_roundtrip() {
        let mut chain = make_chain(0);
        let key = SigningKey::generate();
        let entry = chain.append_batch(&[b"test"], 1000, Some(&key)).unwrap();

        let bytes = entry.to_bytes().unwrap();
        let decoded = PohEntry::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.hash, entry.hash);
        assert_eq!(decoded.merkle_root, entry.merkle_root);
        assert_eq!(decoded.sequence, entry.sequence);
        assert!(decoded.verify_signature().unwrap());
    }

    #[test]
    fn append_root_directly() {
        let mut chain = make_chain(0);
        let root = sha512(b"precomputed-root");

        let entry = chain.append_root(root, 1000, 42, None);
        assert_eq!(entry.merkle_root, root);
        assert_eq!(entry.batch_size, 42);
        assert_eq!(chain.sequence(), 1);
    }

    #[test]
    fn multi_batch_chain_integrity() {
        let mut chain = make_chain(0);
        let key = SigningKey::generate();
        let mut entries = Vec::new();

        for i in 0..20 {
            let payloads: Vec<Vec<u8>> = (0..10)
                .map(|j| format!("batch-{i}-event-{j}").into_bytes())
                .collect();
            let refs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();

            let entry = chain
                .append_batch(&refs, (i + 1) * 1_000_000, Some(&key))
                .unwrap();
            entries.push(entry);
        }

        // Verify full chain
        let genesis = *chain.genesis_hash();
        assert!(entries[0].verify_chain(&genesis));
        for i in 1..entries.len() {
            assert!(
                entries[i].verify_chain(&entries[i - 1].hash),
                "chain break at entry {i}"
            );
        }

        // All signatures valid
        for (i, entry) in entries.iter().enumerate() {
            assert!(
                entry.verify_signature().unwrap(),
                "signature invalid at entry {i}"
            );
        }

        // MMR has all roots
        assert_eq!(chain.mmr().leaf_count(), 20);
    }

    // ── verify_state / PohVerifyMode tests ─────────────────────────────

    #[test]
    fn verify_mode_parses_common_spellings() {
        use std::str::FromStr;
        assert_eq!(
            PohVerifyMode::from_str("verify").unwrap(),
            PohVerifyMode::Verify
        );
        assert_eq!(
            PohVerifyMode::from_str("Verify_With_Key").unwrap(),
            PohVerifyMode::VerifyWithKey
        );
        assert_eq!(
            PohVerifyMode::from_str("trust-extend").unwrap(),
            PohVerifyMode::TrustExtend
        );
        assert!(PohVerifyMode::from_str("yolo").is_err());
    }

    #[test]
    fn verify_state_accepts_freshly_exported_state() {
        let mut chain = make_chain(5);
        for i in 0..4 {
            chain.append_batch(&[format!("b-{i}").as_bytes()], (i + 1) * 1000, None);
        }
        let state = chain.export_state();
        PohChain::verify_state(&state, PartitionId::new(5)).unwrap();
    }

    #[test]
    fn verify_state_accepts_empty_chain() {
        let chain = make_chain(0);
        let state = chain.export_state();
        PohChain::verify_state(&state, PartitionId::new(0)).unwrap();
    }

    #[test]
    fn verify_state_rejects_wrong_partition() {
        let chain = make_chain(9);
        let state = chain.export_state();
        let err = PohChain::verify_state(&state, PartitionId::new(10)).unwrap_err();
        matches!(err, PohVerifyError::PartitionMismatch { .. });
    }

    #[test]
    fn verify_state_rejects_nonzero_seq_with_genesis_hash() {
        let mut state = make_chain(0).export_state();
        state.sequence = 5; // lied: we claim 5 entries but hash is genesis
        let err = PohChain::verify_state(&state, PartitionId::new(0)).unwrap_err();
        matches!(err, PohVerifyError::SequenceHashInconsistent { .. });
    }

    #[test]
    fn verify_state_rejects_zero_seq_with_non_genesis_hash() {
        let mut state = make_chain(0).export_state();
        state.current_hash = sha512(b"not-genesis");
        // sequence is still 0 — inconsistent.
        let err = PohChain::verify_state(&state, PartitionId::new(0)).unwrap_err();
        matches!(err, PohVerifyError::SequenceHashInconsistent { .. });
    }

    #[test]
    fn verify_state_rejects_mmr_ahead_of_sequence() {
        let mut chain = make_chain(2);
        for i in 0..3 {
            chain.append_batch(&[format!("x-{i}").as_bytes()], (i + 1) * 100, None);
        }
        let mut state = chain.export_state();
        state.sequence = 1; // lied downwards
        let err = PohChain::verify_state(&state, PartitionId::new(2)).unwrap_err();
        matches!(err, PohVerifyError::MmrAheadOfSequence { .. });
    }
}
