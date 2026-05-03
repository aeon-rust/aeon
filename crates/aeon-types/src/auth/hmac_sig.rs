//! Shared HMAC-SHA256 request signing used by S9 (inbound verification)
//! and S10 (outbound signing).
//!
//! ## Canonical string
//!
//! The request-signing scheme concatenates a well-known fixed-order set of
//! fields into a single byte string, then takes HMAC-SHA256 over it:
//!
//! ```text
//! method || "\n" || path || "\n" || timestamp_unix_secs || "\n" || body
//! ```
//!
//! The timestamp is a decimal ASCII Unix-epoch-seconds integer. The body is
//! the raw request body bytes. The signature is transmitted as lowercase
//! hex in a configurable header (default `X-Aeon-Signature`) alongside the
//! timestamp (default `X-Aeon-Timestamp`).
//!
//! ## Clock-skew window
//!
//! The verifier rejects any request whose timestamp is older or newer than
//! the caller-supplied skew window (typically 300 s). This caps replay
//! attacks at that window even without an explicit replay cache.
//!
//! ## Algorithm
//!
//! HMAC-SHA256 is the only algorithm defined today; the `HmacAlgorithm`
//! enum reserves space for SHA-512 if a compliance regime requires it
//! later. Using anything weaker (MD5, SHA-1) is deliberately impossible.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Sha256, Sha512};

/// HMAC digest algorithm. Only SHA-family allowed — no MD5, no SHA-1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HmacAlgorithm {
    #[default]
    HmacSha256,
    HmacSha512,
}

impl HmacAlgorithm {
    /// Returns the hex-encoded digest length (2 * byte length).
    pub const fn hex_len(self) -> usize {
        match self {
            Self::HmacSha256 => 64,
            Self::HmacSha512 => 128,
        }
    }
}

/// Signer error. Construction and signing only fail when the secret is
/// empty — every other path is infallible (HMAC itself doesn't reject
/// any key length).
#[derive(Debug, thiserror::Error)]
pub enum HmacSignError {
    #[error("HMAC sign: secret is empty")]
    EmptySecret,
}

/// Verifier error. The caller maps these into an [`AuthRejection`].
#[derive(Debug, thiserror::Error)]
pub enum HmacVerifyError {
    #[error("HMAC verify: signature header contains non-hex characters")]
    SignatureNotHex,
    #[error("HMAC verify: signature length {got} does not match expected {expected}")]
    SignatureWrongLength { got: usize, expected: usize },
    #[error("HMAC verify: signature does not match")]
    SignatureMismatch,
    #[error("HMAC verify: secret is empty")]
    EmptySecret,
}

/// Sign a request with the given secret. Returns the lowercase-hex
/// signature suitable for placing in the `X-Aeon-Signature` header.
///
/// `timestamp_unix_secs` is the decimal ASCII representation of the
/// request's unix timestamp.
pub fn sign_request(
    algo: HmacAlgorithm,
    secret: &[u8],
    method: &[u8],
    path: &[u8],
    timestamp_unix_secs: &[u8],
    body: &[u8],
) -> Result<String, HmacSignError> {
    if secret.is_empty() {
        return Err(HmacSignError::EmptySecret);
    }
    let digest = compute_digest(algo, secret, method, path, timestamp_unix_secs, body);
    Ok(hex::encode(digest))
}

/// Verify a signature. The comparison is constant-time (uses `hmac`'s
/// `CtOutput` equality under the hood). Returns `Ok(())` when valid.
///
/// `candidates` holds the caller's accepted secrets in preferred order
/// — callers pass `[active, previous]` to support hot rotation. The
/// function returns the first match; if no candidate matches the final
/// `SignatureMismatch` error propagates.
pub fn verify_request(
    algo: HmacAlgorithm,
    candidates: &[&[u8]],
    method: &[u8],
    path: &[u8],
    timestamp_unix_secs: &[u8],
    body: &[u8],
    signature_hex: &str,
) -> Result<(), HmacVerifyError> {
    if candidates.is_empty() || candidates.iter().all(|s| s.is_empty()) {
        return Err(HmacVerifyError::EmptySecret);
    }
    if signature_hex.len() != algo.hex_len() {
        return Err(HmacVerifyError::SignatureWrongLength {
            got: signature_hex.len(),
            expected: algo.hex_len(),
        });
    }
    let expected = hex::decode(signature_hex).map_err(|_| HmacVerifyError::SignatureNotHex)?;

    for secret in candidates {
        if secret.is_empty() {
            continue;
        }
        if verify_one(
            algo,
            secret,
            method,
            path,
            timestamp_unix_secs,
            body,
            &expected,
        ) {
            return Ok(());
        }
    }
    Err(HmacVerifyError::SignatureMismatch)
}

fn verify_one(
    algo: HmacAlgorithm,
    secret: &[u8],
    method: &[u8],
    path: &[u8],
    ts: &[u8],
    body: &[u8],
    expected: &[u8],
) -> bool {
    match algo {
        HmacAlgorithm::HmacSha256 => {
            let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
                return false;
            };
            feed(&mut mac, method, path, ts, body);
            mac.verify_slice(expected).is_ok()
        }
        HmacAlgorithm::HmacSha512 => {
            let Ok(mut mac) = Hmac::<Sha512>::new_from_slice(secret) else {
                return false;
            };
            feed(&mut mac, method, path, ts, body);
            mac.verify_slice(expected).is_ok()
        }
    }
}

fn feed<M: Mac>(mac: &mut M, method: &[u8], path: &[u8], ts: &[u8], body: &[u8]) {
    mac.update(method);
    mac.update(b"\n");
    mac.update(path);
    mac.update(b"\n");
    mac.update(ts);
    mac.update(b"\n");
    mac.update(body);
}

fn compute_digest(
    algo: HmacAlgorithm,
    secret: &[u8],
    method: &[u8],
    path: &[u8],
    ts: &[u8],
    body: &[u8],
) -> Vec<u8> {
    match algo {
        HmacAlgorithm::HmacSha256 => {
            // new_from_slice accepts any key length for HMAC — infallible in practice.
            #[allow(clippy::expect_used)]
            let mut mac =
                Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
            feed(&mut mac, method, path, ts, body);
            mac.finalize().into_bytes().to_vec()
        }
        HmacAlgorithm::HmacSha512 => {
            #[allow(clippy::expect_used)]
            let mut mac =
                Hmac::<Sha512>::new_from_slice(secret).expect("HMAC accepts any key length");
            feed(&mut mac, method, path, ts, body);
            mac.finalize().into_bytes().to_vec()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify_roundtrip_sha256() {
        let sig = sign_request(
            HmacAlgorithm::HmacSha256,
            b"s3cret",
            b"POST",
            b"/ingest",
            b"1700000000",
            b"hello",
        )
        .unwrap();
        assert_eq!(sig.len(), 64);
        verify_request(
            HmacAlgorithm::HmacSha256,
            &[b"s3cret"],
            b"POST",
            b"/ingest",
            b"1700000000",
            b"hello",
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn sign_then_verify_roundtrip_sha512() {
        let sig = sign_request(
            HmacAlgorithm::HmacSha512,
            b"s3cret",
            b"POST",
            b"/ingest",
            b"1700000000",
            b"hello",
        )
        .unwrap();
        assert_eq!(sig.len(), 128);
        verify_request(
            HmacAlgorithm::HmacSha512,
            &[b"s3cret"],
            b"POST",
            b"/ingest",
            b"1700000000",
            b"hello",
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let sig = sign_request(
            HmacAlgorithm::HmacSha256,
            b"correct",
            b"POST",
            b"/x",
            b"1",
            b"",
        )
        .unwrap();
        let err = verify_request(
            HmacAlgorithm::HmacSha256,
            &[b"wrong"],
            b"POST",
            b"/x",
            b"1",
            b"",
            &sig,
        )
        .unwrap_err();
        assert!(matches!(err, HmacVerifyError::SignatureMismatch));
    }

    #[test]
    fn verify_rejects_wrong_body() {
        let sig = sign_request(
            HmacAlgorithm::HmacSha256,
            b"k",
            b"POST",
            b"/x",
            b"1",
            b"body-a",
        )
        .unwrap();
        let err = verify_request(
            HmacAlgorithm::HmacSha256,
            &[b"k"],
            b"POST",
            b"/x",
            b"1",
            b"body-b",
            &sig,
        )
        .unwrap_err();
        assert!(matches!(err, HmacVerifyError::SignatureMismatch));
    }

    #[test]
    fn verify_rejects_wrong_method() {
        let sig = sign_request(HmacAlgorithm::HmacSha256, b"k", b"POST", b"/x", b"1", b"").unwrap();
        let err = verify_request(
            HmacAlgorithm::HmacSha256,
            &[b"k"],
            b"GET",
            b"/x",
            b"1",
            b"",
            &sig,
        )
        .unwrap_err();
        assert!(matches!(err, HmacVerifyError::SignatureMismatch));
    }

    #[test]
    fn verify_rejects_wrong_timestamp() {
        let sig = sign_request(HmacAlgorithm::HmacSha256, b"k", b"POST", b"/x", b"1", b"").unwrap();
        let err = verify_request(
            HmacAlgorithm::HmacSha256,
            &[b"k"],
            b"POST",
            b"/x",
            b"2",
            b"",
            &sig,
        )
        .unwrap_err();
        assert!(matches!(err, HmacVerifyError::SignatureMismatch));
    }

    #[test]
    fn verify_accepts_previous_key_for_rotation() {
        let sig = sign_request(
            HmacAlgorithm::HmacSha256,
            b"old-key",
            b"POST",
            b"/x",
            b"1",
            b"",
        )
        .unwrap();
        // active is "new-key", previous is "old-key" — rotation case.
        verify_request(
            HmacAlgorithm::HmacSha256,
            &[b"new-key", b"old-key"],
            b"POST",
            b"/x",
            b"1",
            b"",
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn verify_rejects_malformed_hex() {
        let err = verify_request(
            HmacAlgorithm::HmacSha256,
            &[b"k"],
            b"POST",
            b"/x",
            b"1",
            b"",
            &"z".repeat(64),
        )
        .unwrap_err();
        assert!(matches!(err, HmacVerifyError::SignatureNotHex));
    }

    #[test]
    fn verify_rejects_wrong_length() {
        let err = verify_request(
            HmacAlgorithm::HmacSha256,
            &[b"k"],
            b"POST",
            b"/x",
            b"1",
            b"",
            "abcd",
        )
        .unwrap_err();
        assert!(matches!(
            err,
            HmacVerifyError::SignatureWrongLength {
                got: 4,
                expected: 64
            }
        ));
    }

    #[test]
    fn sign_rejects_empty_secret() {
        let err =
            sign_request(HmacAlgorithm::HmacSha256, b"", b"POST", b"/x", b"1", b"").unwrap_err();
        assert!(matches!(err, HmacSignError::EmptySecret));
    }

    #[test]
    fn verify_rejects_empty_secrets_list() {
        let err = verify_request(
            HmacAlgorithm::HmacSha256,
            &[],
            b"POST",
            b"/x",
            b"1",
            b"",
            &"a".repeat(64),
        )
        .unwrap_err();
        assert!(matches!(err, HmacVerifyError::EmptySecret));
    }

    #[test]
    fn verify_rejects_all_empty_candidates() {
        let err = verify_request(
            HmacAlgorithm::HmacSha256,
            &[b"", b""],
            b"POST",
            b"/x",
            b"1",
            b"",
            &"a".repeat(64),
        )
        .unwrap_err();
        assert!(matches!(err, HmacVerifyError::EmptySecret));
    }

    #[test]
    fn sha256_hex_len_is_64() {
        assert_eq!(HmacAlgorithm::HmacSha256.hex_len(), 64);
    }

    #[test]
    fn sha512_hex_len_is_128() {
        assert_eq!(HmacAlgorithm::HmacSha512.hex_len(), 128);
    }

    #[test]
    fn algorithm_serde_snake_case() {
        let j = serde_json::to_string(&HmacAlgorithm::HmacSha256).unwrap();
        assert_eq!(j, "\"hmac-sha256\"");
        let back: HmacAlgorithm = serde_json::from_str("\"hmac-sha512\"").unwrap();
        assert_eq!(back, HmacAlgorithm::HmacSha512);
    }
}
