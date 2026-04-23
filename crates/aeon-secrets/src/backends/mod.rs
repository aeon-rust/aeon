//! Concrete backend implementations for the adapter hub.
//!
//! Each submodule is feature-gated. The crate compiles cleanly with
//! none, some, or all backends enabled — the builder layer turns
//! "asked for an uncompiled backend" into a typed error rather than a
//! link failure.

#[cfg(feature = "vault")]
pub mod vault;
