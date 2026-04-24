//! HashiCorp Vault / OpenBao adapters.
//!
//! Two engines share auth + HTTP plumbing:
//!
//! - [`kv::VaultKvProvider`] — KV-v2 read-side `SecretProvider`.
//! - [`transit::VaultTransitKekProvider`] — Transit engine `KekProvider`
//!   (wrap / unwrap remote, KEK bytes never enter the Aeon process).
//!
//! Token lifecycle, AppRole login, namespace headers, and 403-driven
//! re-auth all live in [`auth::VaultAuthClient`] so the two engine
//! adapters stay focused on their respective request shapes.

pub mod auth;
pub mod cache;
pub mod kv;
pub mod retry;
pub mod transit;

pub use kv::VaultKvProvider;
pub use retry::RetryPolicy;
pub use transit::VaultTransitKekProvider;
