//! Cryptographic integrity and security for Aeon.
//!
//! ## Modules
//!
//! - [`hash`] — SHA-512 hashing primitives
//! - [`merkle`] — Batch Merkle tree with inclusion proofs
//! - [`mmr`] — Merkle Mountain Range (append-only authenticated log)
//! - [`poh`] — Per-partition Proof of History chains
//! - [`signing`] — Ed25519 digital signatures
//! - [`encryption`] — EtM symmetric encryption (AES-256-CTR + HMAC-SHA-512)
//! - [`keys`] — Key management (KeyProvider trait, env/file providers)
//! - [`fips`] — FIPS 140-3 mode guard
//! - [`tls`] — TLS/mTLS certificate management
//! - [`auth`] — REST API authentication (api-key, mTLS)

pub mod auth;
pub mod encryption;
pub mod fips;
pub mod hash;
pub mod keys;
pub mod merkle;
pub mod mmr;
pub mod poh;
pub mod signing;
pub mod tls;
