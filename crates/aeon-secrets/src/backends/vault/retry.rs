//! Transient-failure classification + retry policy shared by the
//! KV-v2 (blocking) and Transit (async) Vault adapters.
//!
//! The two adapters run against the same server and see the same
//! failure shapes — 401/403 (expired token), 429 (rate limited),
//! 5xx (Vault replica demotion, seal/unseal transitions), TCP
//! timeouts during a leader election. A stable classifier keeps the
//! retry decision local to one place; the adapters just drive it.
//!
//! Backoff itself is delegated to [`aeon_types::backoff::BackoffPolicy`]
//! so Aeon uses one set of defaults (initial 100 ms, cap 30 s,
//! ±20 % jitter) across every retryable call-site.

pub use crate::config::RetryPolicy;
use reqwest::StatusCode;

/// Per-response classification that drives the retry decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Classification {
    /// 2xx — stop retrying, return the body.
    Success,
    /// 401 / 403 — AppRole tokens expire. Invalidate and retry once.
    Auth,
    /// 408 / 425 / 429 / 5xx — back off and retry, up to `max_attempts`.
    Transient,
    /// 400 / 404 / other 4xx — caller fault; no retry would help.
    Permanent,
}

pub(crate) fn classify_status(status: StatusCode) -> Classification {
    if status.is_success() {
        return Classification::Success;
    }
    match status.as_u16() {
        401 | 403 => Classification::Auth,
        408 | 425 | 429 => Classification::Transient,
        s if (500..=599).contains(&s) => Classification::Transient,
        _ => Classification::Permanent,
    }
}

/// A connection-level failure is transient if it's a timeout, a TCP
/// connect failure, or a mid-flight request interruption. Parse
/// errors, TLS handshake failures, and URL errors are not retried.
pub(crate) fn is_transient_network_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

pub(crate) trait RetryPolicyExt {
    /// Returns `true` if another attempt is permitted after the given
    /// zero-based attempt index.
    fn can_retry(&self, attempt_idx: u32) -> bool;
}

impl RetryPolicyExt for RetryPolicy {
    fn can_retry(&self, attempt_idx: u32) -> bool {
        attempt_idx + 1 < self.max_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::backoff::BackoffPolicy;

    #[test]
    fn status_classifies_into_four_buckets() {
        assert_eq!(
            classify_status(StatusCode::OK),
            Classification::Success
        );
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED),
            Classification::Auth
        );
        assert_eq!(
            classify_status(StatusCode::FORBIDDEN),
            Classification::Auth
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS),
            Classification::Transient
        );
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE),
            Classification::Transient
        );
        assert_eq!(
            classify_status(StatusCode::GATEWAY_TIMEOUT),
            Classification::Transient
        );
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST),
            Classification::Permanent
        );
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND),
            Classification::Permanent
        );
    }

    #[test]
    fn can_retry_respects_max_attempts() {
        let p = RetryPolicy {
            max_attempts: 3,
            backoff: BackoffPolicy::default(),
        };
        assert!(p.can_retry(0));
        assert!(p.can_retry(1));
        assert!(!p.can_retry(2));
    }

    #[test]
    fn max_attempts_one_means_no_retry() {
        let p = RetryPolicy {
            max_attempts: 1,
            backoff: BackoffPolicy::default(),
        };
        assert!(!p.can_retry(0));
    }

    #[test]
    fn default_is_four_attempts() {
        assert_eq!(RetryPolicy::default().max_attempts, 4);
    }
}
