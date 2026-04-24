//! # aeon-secrets — adapter hub for secret and KEK backends.
//!
//! Aeon separates secret material into two trait-shaped seams and this
//! crate hosts the concrete backends behind feature flags.
//!
//! | Trait | Shape | Home | Covers |
//! |---|---|---|---|
//! | [`aeon_types::SecretProvider`] | sync `resolve(path) -> SecretBytes` | `aeon-types` | env vars, `.env`, literal, Vault / OpenBao KV-v2, AWS Secrets Manager, GCP Secret Manager, Azure Key Vault reads |
//! | [`aeon_crypto::kek_provider::KekProvider`] | async `wrap` / `unwrap` without exposing KEK bytes | `aeon-crypto` | AWS KMS, GCP KMS, Azure Key Vault crypto, Vault Transit, OpenBao Transit, PKCS#11; local AES-GCM ([`aeon_crypto::kek::KekHandle`]) also implements it |
//!
//! The traits live in their respective upstream crates so downstream
//! callers can depend on them without pulling in backend dependencies.
//! This crate's job is to:
//!
//! 1. Declare the YAML / serde-deserializable config enums
//!    ([`config::SecretProviderConfig`], [`config::KekProviderConfig`])
//!    that name every backend Aeon is ever expected to support, whether
//!    or not its backend feature is currently compiled. This keeps
//!    operator-facing config schemas stable across backend landings.
//! 2. Provide [`kek_registry::KekRegistry`], the per-domain dispatcher
//!    that mirrors [`aeon_types::SecretRegistry`] for the KEK side.
//! 3. Provide [`builder::SecretRegistryBuilder`] and
//!    [`builder::KekRegistryBuilder`], which turn config enums into
//!    registered providers at pipeline start and fail fast with a clear
//!    error when a config names a backend whose feature flag is off.
//!
//! ## Feature-flag contract
//!
//! Every backend is feature-gated. A config value naming a backend
//! whose feature is not compiled returns
//! [`SecretsAdapterError::BackendFeatureDisabled`] from the builder —
//! never a panic, never a silent downgrade.
//!
//! ## What this crate does NOT do
//!
//! - No backend code ships in `main` yet. The `vault` feature carries
//!   the dep declarations today; the adapter implementation lands as a
//!   follow-up commit on task #35. Other backend features are placeholders.
//! - Trait definitions live upstream (`aeon-types`, `aeon-crypto`) and
//!   are not duplicated here.

#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod backends;
pub mod builder;
pub mod config;
pub mod error;
pub mod kek_registry;

pub use builder::{KekRegistryBuilder, SecretRegistryBuilder};
pub use config::{KekProviderConfig, SecretProviderConfig};
pub use error::SecretsAdapterError;
pub use kek_registry::KekRegistry;

// Convenience re-exports so downstream callers don't have to know which
// upstream crate owns each trait.
pub use aeon_crypto::kek_provider::{KekFuture, KekProvider};
pub use aeon_types::{SecretBytes, SecretProvider, SecretRef, SecretRegistry, SecretScheme};
