//! Processor authentication: challenge-response and batch signature verification.
//!
//! Implements the AWPP security layers:
//! - Layer 2 (ED25519): Challenge-response + per-batch signing
//! - Layer 3 (Authorization): Pipeline scope + instance limit checks
//!
//! Gated behind the `processor-auth` feature flag.

#[cfg(feature = "processor-auth")]
use aeon_crypto::signing::{Signature, VerifyingKey};
#[cfg(feature = "processor-auth")]
use aeon_types::awpp::{Challenge, RejectCode, Rejected};
#[cfg(feature = "processor-auth")]
use aeon_types::error::AeonError;
#[cfg(feature = "processor-auth")]
use aeon_types::processor_identity::ProcessorIdentity;

/// Generate a cryptographically random 32-byte nonce (hex-encoded).
#[cfg(feature = "processor-auth")]
pub fn generate_nonce() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut nonce = [0u8; 32];
    rng.fill(&mut nonce);
    hex::encode(nonce)
}

/// Create a challenge message with a fresh nonce.
#[cfg(feature = "processor-auth")]
pub fn create_challenge(oauth_required: bool) -> Challenge {
    Challenge::new(generate_nonce(), oauth_required)
}

/// Decode an ED25519 public key from the "ed25519:<base64>" format.
#[cfg(feature = "processor-auth")]
pub fn decode_public_key(public_key_str: &str) -> Result<[u8; 32], AeonError> {
    let base64_part = public_key_str
        .strip_prefix("ed25519:")
        .ok_or_else(|| AeonError::serialization("public key must start with 'ed25519:'"))?;

    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_part)
        .map_err(|e| AeonError::serialization(format!("invalid base64 in public key: {e}")))?;

    if bytes.len() != 32 {
        return Err(AeonError::serialization(format!(
            "ED25519 public key must be 32 bytes, got {}",
            bytes.len()
        )));
    }

    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

/// Compute the fingerprint (SHA-256 hex) for a public key string.
#[cfg(feature = "processor-auth")]
pub fn compute_fingerprint(public_key_str: &str) -> Result<String, AeonError> {
    let pk_bytes = decode_public_key(public_key_str)?;
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(pk_bytes);
    Ok(format!("SHA256:{}", hex::encode(hash)))
}

/// Verify a challenge-response signature.
///
/// The processor signs the raw nonce bytes (decoded from hex) with its
/// ED25519 private key. This verifies the signature against the registered
/// public key.
#[cfg(feature = "processor-auth")]
pub fn verify_challenge(
    identity: &ProcessorIdentity,
    nonce_hex: &str,
    signature_hex: &str,
) -> Result<bool, AeonError> {
    let public_key_bytes = decode_public_key(&identity.public_key)?;
    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)?;

    let nonce_bytes = hex::decode(nonce_hex)
        .map_err(|e| AeonError::serialization(format!("invalid nonce hex: {e}")))?;

    let sig_bytes = hex::decode(signature_hex)
        .map_err(|e| AeonError::serialization(format!("invalid signature hex: {e}")))?;

    if sig_bytes.len() != 64 {
        return Err(AeonError::serialization(format!(
            "signature must be 64 bytes, got {}",
            sig_bytes.len()
        )));
    }

    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(sig_arr);

    Ok(verifying_key.verify(&nonce_bytes, &signature))
}

/// Verify a per-batch ED25519 signature.
///
/// `batch_payload` is the batch response bytes from batch_id through CRC32
/// (inclusive). `signature_bytes` is the 64-byte ED25519 signature.
#[cfg(feature = "processor-auth")]
pub fn verify_batch_signature(
    public_key: &str,
    batch_payload: &[u8],
    signature_bytes: &[u8],
) -> Result<bool, AeonError> {
    let pk_bytes = decode_public_key(public_key)?;
    let verifying_key = VerifyingKey::from_bytes(&pk_bytes)?;

    if signature_bytes.len() != 64 {
        return Err(AeonError::serialization(format!(
            "batch signature must be 64 bytes, got {}",
            signature_bytes.len()
        )));
    }

    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(signature_bytes);
    let signature = Signature::from_bytes(sig_arr);

    Ok(verifying_key.verify(batch_payload, &signature))
}

/// Validate authorization: check pipeline scope and instance limits.
///
/// Returns `Ok(())` if the identity is authorized, or `Err(Rejected)` with
/// the appropriate AWPP rejection code.
#[cfg(feature = "processor-auth")]
pub fn check_authorization(
    identity: &ProcessorIdentity,
    requested_pipelines: &[String],
    current_connections: u32,
) -> Result<(), Rejected> {
    if !identity.is_active() {
        return Err(Rejected::new(
            RejectCode::KeyRevoked,
            format!("identity {} is revoked", identity.fingerprint),
        ));
    }

    for pipeline in requested_pipelines {
        if !identity.allowed_pipelines.allows(pipeline) {
            return Err(Rejected::new(
                RejectCode::PipelineNotAuthorized,
                format!(
                    "pipeline '{}' not authorized for identity {}",
                    pipeline, identity.fingerprint
                ),
            ));
        }
    }

    if current_connections >= identity.max_instances {
        return Err(Rejected::new(
            RejectCode::MaxInstancesReached,
            format!(
                "max instances ({}) reached for identity {}",
                identity.max_instances, identity.fingerprint
            ),
        ));
    }

    Ok(())
}

#[cfg(all(test, feature = "processor-auth"))]
mod tests {
    use super::*;
    use aeon_crypto::signing::SigningKey;
    use aeon_types::processor_identity::PipelineScope;
    use base64::Engine;

    fn make_keypair_and_identity() -> (SigningKey, ProcessorIdentity) {
        let signing_key = SigningKey::generate();
        let vk = signing_key.verifying_key();
        let pk_base64 = base64::engine::general_purpose::STANDARD.encode(vk.to_bytes());
        let public_key = format!("ed25519:{pk_base64}");
        let fingerprint = compute_fingerprint(&public_key).unwrap();

        let identity = ProcessorIdentity {
            public_key,
            fingerprint,
            processor_name: "test-proc".into(),
            allowed_pipelines: PipelineScope::Named(vec!["orders".into(), "payments".into()]),
            max_instances: 2,
            registered_at: 1000,
            registered_by: "test".into(),
            revoked_at: None,
        };

        (signing_key, identity)
    }

    #[test]
    fn generate_nonce_is_64_hex_chars() {
        let nonce = generate_nonce();
        assert_eq!(nonce.len(), 64);
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn create_challenge_has_correct_fields() {
        let challenge = create_challenge(true);
        assert_eq!(challenge.msg_type, "challenge");
        assert_eq!(challenge.protocol, "awpp/1");
        assert!(challenge.oauth_required);
        assert_eq!(challenge.nonce.len(), 64);
    }

    #[test]
    fn decode_public_key_valid() {
        let (_, identity) = make_keypair_and_identity();
        let bytes = decode_public_key(&identity.public_key).unwrap();
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn decode_public_key_missing_prefix() {
        let result = decode_public_key("MCowBQYDK2VwAyEA");
        assert!(result.is_err());
    }

    #[test]
    fn compute_fingerprint_deterministic() {
        let (_, identity) = make_keypair_and_identity();
        let fp1 = compute_fingerprint(&identity.public_key).unwrap();
        let fp2 = compute_fingerprint(&identity.public_key).unwrap();
        assert_eq!(fp1, fp2);
        assert!(fp1.starts_with("SHA256:"));
    }

    #[test]
    fn verify_challenge_valid_signature() {
        let (signing_key, identity) = make_keypair_and_identity();

        let nonce = generate_nonce();
        let nonce_bytes = hex::decode(&nonce).unwrap();
        let sig = signing_key.sign(&nonce_bytes);
        let sig_hex = hex::encode(sig.as_bytes());

        assert!(verify_challenge(&identity, &nonce, &sig_hex).unwrap());
    }

    #[test]
    fn verify_challenge_wrong_signature() {
        let (_, identity) = make_keypair_and_identity();
        let other_key = SigningKey::generate();

        let nonce = generate_nonce();
        let nonce_bytes = hex::decode(&nonce).unwrap();
        let sig = other_key.sign(&nonce_bytes);
        let sig_hex = hex::encode(sig.as_bytes());

        assert!(!verify_challenge(&identity, &nonce, &sig_hex).unwrap());
    }

    #[test]
    fn verify_batch_signature_valid() {
        let (signing_key, identity) = make_keypair_and_identity();

        let batch_payload = b"batch-payload-with-crc";
        let sig = signing_key.sign(batch_payload);

        assert!(
            verify_batch_signature(&identity.public_key, batch_payload, sig.as_bytes()).unwrap()
        );
    }

    #[test]
    fn verify_batch_signature_tampered() {
        let (signing_key, identity) = make_keypair_and_identity();

        let batch_payload = b"original-payload";
        let sig = signing_key.sign(batch_payload);

        assert!(
            !verify_batch_signature(&identity.public_key, b"tampered-payload", sig.as_bytes())
                .unwrap()
        );
    }

    #[test]
    fn check_authorization_ok() {
        let (_, identity) = make_keypair_and_identity();
        let result = check_authorization(&identity, &["orders".into(), "payments".into()], 0);
        assert!(result.is_ok());
    }

    #[test]
    fn check_authorization_revoked() {
        let (_, mut identity) = make_keypair_and_identity();
        identity.revoked_at = Some(5000);

        let result = check_authorization(&identity, &["orders".into()], 0);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, RejectCode::KeyRevoked);
    }

    #[test]
    fn check_authorization_pipeline_not_allowed() {
        let (_, identity) = make_keypair_and_identity();
        let result = check_authorization(&identity, &["unauthorized-pipeline".into()], 0);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, RejectCode::PipelineNotAuthorized);
    }

    #[test]
    fn check_authorization_max_instances() {
        let (_, identity) = make_keypair_and_identity();
        let result = check_authorization(&identity, &["orders".into()], 2);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, RejectCode::MaxInstancesReached);
    }
}
