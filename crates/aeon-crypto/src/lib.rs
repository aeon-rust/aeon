//! Cryptographic integrity for Aeon: PoH hash chains, Merkle trees, Ed25519 signing.
//!
//! ## Modules
//!
//! - [`hash`] — SHA-512 hashing primitives
//! - [`merkle`] — Batch Merkle tree with inclusion proofs
//! - [`mmr`] — Merkle Mountain Range (append-only authenticated log)
//! - [`poh`] — Per-partition Proof of History chains
//! - [`signing`] — Ed25519 digital signatures

pub mod hash;
pub mod merkle;
pub mod mmr;
pub mod poh;
pub mod signing;
