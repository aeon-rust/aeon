//! Aeon error types — `Result<T, AeonError>` everywhere.
//!
//! `thiserror` for typed, matchable errors. No panics on hot path.

use std::fmt;

/// All Aeon errors. Every library function returns `Result<T, AeonError>`.
#[derive(Debug, thiserror::Error)]
pub enum AeonError {
    #[error("connection error: {message}")]
    Connection {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
        retryable: bool,
    },

    #[error("serialization error: {message}")]
    Serialization {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("state error: {message}")]
    State {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("processor error: {message}")]
    Processor {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("config error: {message}")]
    Config { message: String },

    #[error("cluster error: {message}")]
    Cluster {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("crypto error: {message}")]
    Crypto {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("resource error: {message}")]
    Resource { message: String },

    #[error("timeout: {message}")]
    Timeout { message: String },

    #[error("not found: {0}")]
    NotFound(String),
}

impl AeonError {
    /// Whether this error is retryable with backoff.
    pub fn is_retryable(&self) -> bool {
        match self {
            AeonError::Connection { retryable, .. } => *retryable,
            AeonError::Timeout { .. } => true,
            AeonError::Cluster { .. } => true,
            AeonError::Serialization { .. }
            | AeonError::Processor { .. }
            | AeonError::Config { .. }
            | AeonError::Crypto { .. }
            | AeonError::Resource { .. }
            | AeonError::NotFound(_)
            | AeonError::State { .. } => false,
        }
    }

    pub fn connection(message: impl Into<String>) -> Self {
        AeonError::Connection {
            message: message.into(),
            source: None,
            retryable: true,
        }
    }

    pub fn serialization(message: impl Into<String>) -> Self {
        AeonError::Serialization {
            message: message.into(),
            source: None,
        }
    }

    pub fn state(message: impl Into<String>) -> Self {
        AeonError::State {
            message: message.into(),
            source: None,
        }
    }

    pub fn processor(message: impl Into<String>) -> Self {
        AeonError::Processor {
            message: message.into(),
            source: None,
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        AeonError::Config {
            message: message.into(),
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        AeonError::Timeout {
            message: message.into(),
        }
    }

    pub fn not_found(message: impl fmt::Display) -> Self {
        AeonError::NotFound(message.to_string())
    }
}

/// Convenience type alias used throughout Aeon.
pub type Result<T> = std::result::Result<T, AeonError>;

impl From<crate::ssrf::SsrfError> for AeonError {
    fn from(err: crate::ssrf::SsrfError) -> Self {
        use crate::ssrf::SsrfError;
        match err {
            // Policy denied the address: operator intervention required
            // (change SsrfPolicy or use an allowed target). Non-retryable.
            SsrfError::AddressDenied { .. } | SsrfError::UrlParseFailed { .. } => {
                AeonError::Config {
                    message: err.to_string(),
                }
            }
            // DNS hiccup: retryable connection-class failure.
            SsrfError::ResolutionFailed { .. } | SsrfError::NoAddresses { .. } => {
                AeonError::Connection {
                    message: err.to_string(),
                    source: None,
                    retryable: true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_error_is_retryable() {
        let err = AeonError::connection("broker unreachable");
        assert!(err.is_retryable());
    }

    #[test]
    fn serialization_error_is_not_retryable() {
        let err = AeonError::serialization("malformed event");
        assert!(!err.is_retryable());
    }

    #[test]
    fn timeout_error_is_retryable() {
        let err = AeonError::timeout("poll timeout");
        assert!(err.is_retryable());
    }

    #[test]
    fn config_error_is_not_retryable() {
        let err = AeonError::config("missing field");
        assert!(!err.is_retryable());
    }

    #[test]
    fn error_display() {
        let err = AeonError::connection("broker down");
        assert_eq!(format!("{err}"), "connection error: broker down");
    }
}
