//! ED25519 authentication for AWPP protocol.
//!
//! Handles keypair generation, challenge-response signing, and batch signing.

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

/// Generate a new random ED25519 keypair.
pub fn generate_keypair() -> SigningKey {
    let mut rng = rand::thread_rng();
    SigningKey::generate(&mut rng)
}

/// Format a public key as `"ed25519:<base64>"` for AWPP registration.
pub fn format_public_key(verifying_key: &VerifyingKey) -> String {
    let b64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        verifying_key.as_bytes(),
    );
    format!("ed25519:{b64}")
}

/// Sign a hex-encoded nonce for AWPP challenge-response.
///
/// 1. Decode nonce from hex → raw bytes
/// 2. Sign raw bytes with ED25519 private key
/// 3. Return hex-encoded signature (128 chars)
pub fn sign_challenge(
    signing_key: &SigningKey,
    nonce_hex: &str,
) -> Result<String, aeon_types::AeonError> {
    let nonce_bytes = hex::decode(nonce_hex)
        .map_err(|e| aeon_types::AeonError::state(format!("Invalid nonce hex: {e}")))?;
    let signature = signing_key.sign(&nonce_bytes);
    Ok(hex::encode(signature.to_bytes()))
}

/// Sign a batch response payload for non-repudiation.
///
/// Signs the entire batch response bytes (including CRC32) and returns
/// the raw 64-byte signature.
pub fn sign_batch(signing_key: &SigningKey, data: &[u8]) -> [u8; 64] {
    let signature = signing_key.sign(data);
    signature.to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;

    #[test]
    fn generate_and_format_public_key() {
        let sk = generate_keypair();
        let pk = sk.verifying_key();
        let formatted = format_public_key(&pk);

        assert!(formatted.starts_with("ed25519:"));
        let b64 = &formatted["ed25519:".len()..];
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        assert_eq!(decoded.len(), 32);
        assert_eq!(decoded, pk.as_bytes());
    }

    #[test]
    fn sign_and_verify_challenge() {
        let sk = generate_keypair();
        let pk = sk.verifying_key();

        // Simulate server nonce.
        let nonce_bytes: [u8; 32] = rand::random();
        let nonce_hex = hex::encode(nonce_bytes);

        // Processor signs.
        let sig_hex = sign_challenge(&sk, &nonce_hex).unwrap();
        assert_eq!(sig_hex.len(), 128); // 64 bytes * 2

        // Server verifies.
        let sig_bytes = hex::decode(&sig_hex).unwrap();
        let signature = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();
        assert!(pk.verify(&nonce_bytes, &signature).is_ok());
    }

    #[test]
    fn sign_batch_produces_valid_signature() {
        let sk = generate_keypair();
        let pk = sk.verifying_key();

        let data = b"batch payload with CRC32 at the end";
        let sig = sign_batch(&sk, data);

        let signature = ed25519_dalek::Signature::from_slice(&sig).unwrap();
        assert!(pk.verify(data, &signature).is_ok());
    }

    #[test]
    fn invalid_nonce_hex_returns_error() {
        let sk = generate_keypair();
        assert!(sign_challenge(&sk, "not-valid-hex!").is_err());
    }
}
