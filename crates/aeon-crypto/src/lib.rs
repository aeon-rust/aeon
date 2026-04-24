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
//! - [`at_rest`] — S3 AES-256-GCM payload sealing for L2 segments + L3 values
//! - [`keys`] — Key management (KeyProvider trait, env/file providers)
//! - [`kek`] — S1.2 dual-domain KEK + envelope-encrypted DEKs
//! - [`kek_provider`] — async [`kek_provider::KekProvider`] trait for
//!   adapter-pattern KMS backends (Vault Transit, AWS KMS, GCP KMS,
//!   Azure Key Vault, PKCS#11); local AES-GCM [`kek::KekHandle`]
//!   implements it too.
//! - [`hsm`] — S1.4 HSM / PKCS#11 provider trait stub
//! - [`fips`] — FIPS 140-3 mode guard
//! - [`tls`] — TLS/mTLS certificate management
//! - [`auth`] — REST API authentication (api-key, mTLS)

// FT-10: no-panic policy. Production code in this crate must not use
// `.unwrap()` or `.expect()` except for explicitly-documented startup-time
// invariants, which must carry an `#[allow(...)]` attribute with rationale.
// Test modules and benches are exempt (`cfg(not(test))`).
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod at_rest;
pub mod auth;
pub mod encryption;
pub mod fips;
pub mod hash;
pub mod hsm;
pub mod kek;
pub mod kek_provider;
pub mod keys;
pub mod merkle;
pub mod mmr;
pub mod null_receipt;
pub mod poh;
pub mod signing;
pub mod tls;
