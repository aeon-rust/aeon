//! Error taxonomy for the adapter hub. Backend-specific errors wrap
//! [`aeon_types::AeonError`] underneath, but the adapter boundary emits
//! its own typed errors so operators can distinguish "your YAML names a
//! backend that wasn't compiled into this binary" from "the backend
//! itself failed to connect".

use aeon_types::AeonError;

/// Errors surfaced by the adapter builder + registry layer.
#[derive(thiserror::Error, Debug)]
pub enum SecretsAdapterError {
    /// The config names a backend whose feature flag is not compiled
    /// into this binary. Operators see this at pipeline-start time,
    /// never mid-flight.
    #[error(
        "secrets backend '{backend}' is not available in this binary — \
         recompile aeon-secrets with the '{feature}' feature"
    )]
    BackendFeatureDisabled {
        backend: &'static str,
        feature: &'static str,
    },

    /// A backend is declared by a feature flag, but its implementation
    /// has not landed yet. Distinct from `BackendFeatureDisabled`: the
    /// feature IS compiled, but the code behind it is still a stub.
    #[error(
        "secrets backend '{backend}' is declared but not yet implemented \
         (tracked as a follow-up on task #35)"
    )]
    BackendNotImplemented { backend: &'static str },

    /// Duplicate registration — caller added two providers for the
    /// same scheme (SecretProvider) or same domain (KekProvider).
    #[error("duplicate secrets backend registration: {what}")]
    DuplicateRegistration { what: String },

    /// Backend-internal failure (network, auth, malformed response).
    /// Wraps AeonError so callers can bubble up unchanged.
    #[error(transparent)]
    Backend(#[from] AeonError),
}

impl From<SecretsAdapterError> for AeonError {
    fn from(err: SecretsAdapterError) -> Self {
        match err {
            SecretsAdapterError::Backend(e) => e,
            other => AeonError::Config {
                message: other.to_string(),
            },
        }
    }
}
