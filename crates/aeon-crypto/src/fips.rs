//! FIPS 140-3 mode guard.
//!
//! When the `fips` feature is enabled, only approved cryptographic algorithms
//! are permitted. Calling a non-approved algorithm returns an error.
//!
//! Approved algorithms (NIST SP 800-140C / aws-lc-rs):
//! - AES-128/256 (CTR, GCM, CBC)
//! - SHA-256, SHA-384, SHA-512
//! - HMAC-SHA-256, HMAC-SHA-512
//! - Ed25519 (signing/verification)
//! - ECDSA P-256, P-384
//! - RSA 2048+ (signing only)
//! - HKDF, PBKDF2

use std::sync::atomic::{AtomicBool, Ordering};

/// Global FIPS mode flag. Once activated, it cannot be deactivated.
static FIPS_MODE: AtomicBool = AtomicBool::new(cfg!(feature = "fips"));

/// Algorithms approved under FIPS 140-3 mode.
const APPROVED_ALGORITHMS: &[&str] = &[
    "AES-256-CTR",
    "AES-256-CTR + HMAC-SHA-512",
    "AES-256-GCM",
    "AES-128-GCM",
    "SHA-256",
    "SHA-384",
    "SHA-512",
    "HMAC-SHA-256",
    "HMAC-SHA-512",
    "Ed25519",
    "ECDSA-P256",
    "ECDSA-P384",
    "HKDF-SHA-256",
    "HKDF-SHA-512",
];

/// Returns true if FIPS mode is currently active.
pub fn is_fips_mode() -> bool {
    FIPS_MODE.load(Ordering::Relaxed)
}

/// Enable FIPS mode. Once enabled, it cannot be disabled.
pub fn enable_fips_mode() {
    FIPS_MODE.store(true, Ordering::SeqCst);
}

/// Check if the named algorithm is approved. Returns an error if FIPS mode
/// is active and the algorithm is not on the approved list.
pub fn assert_fips_approved(algorithm: &str) -> Result<(), aeon_types::AeonError> {
    if !FIPS_MODE.load(Ordering::Relaxed) {
        return Ok(());
    }
    if APPROVED_ALGORITHMS.contains(&algorithm) {
        return Ok(());
    }
    Err(aeon_types::AeonError::Crypto {
        message: format!("algorithm '{algorithm}' is not approved in FIPS 140-3 mode"),
        source: None,
    })
}

/// Check if a specific algorithm is on the approved list (regardless of FIPS mode state).
pub fn is_approved(algorithm: &str) -> bool {
    APPROVED_ALGORITHMS.contains(&algorithm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_algorithms_pass() {
        // Reset to non-FIPS for isolated testing
        // Note: in real FIPS mode (feature = "fips"), this is always on.
        // Here we test the logic directly.
        assert!(is_approved("AES-256-CTR + HMAC-SHA-512"));
        assert!(is_approved("SHA-512"));
        assert!(is_approved("Ed25519"));
        assert!(is_approved("HMAC-SHA-512"));
    }

    #[test]
    fn non_approved_algorithms_detected() {
        assert!(!is_approved("ChaCha20-Poly1305"));
        assert!(!is_approved("MD5"));
        assert!(!is_approved("RC4"));
        assert!(!is_approved("Blowfish"));
    }

    #[test]
    fn assert_passes_when_fips_disabled() {
        // When FIPS mode is off (default without feature), any algorithm passes
        if !is_fips_mode() {
            assert!(assert_fips_approved("ChaCha20-Poly1305").is_ok());
        }
    }

    #[test]
    fn fips_mode_default_matches_feature() {
        // Without the `fips` feature, default is off
        #[cfg(not(feature = "fips"))]
        assert!(!is_fips_mode());
        #[cfg(feature = "fips")]
        assert!(is_fips_mode());
    }
}
