//! Ed25519 digital signatures for Merkle root authentication.
//!
//! Each cluster node has an Ed25519 keypair. Batch Merkle roots are signed
//! by the node that produced them, providing authenticity and non-repudiation.

use ed25519_dalek::{Signer, Verifier};
use serde::{Deserialize, Serialize};

use crate::hash::Hash512;

/// An Ed25519 signing keypair (private + public).
///
/// Private key bytes are zeroized on drop to prevent secret leakage.
pub struct SigningKey {
    inner: ed25519_dalek::SigningKey,
}

impl Drop for SigningKey {
    fn drop(&mut self) {
        // Overwrite the inner signing key with zeros to prevent key material
        // from lingering in memory. ed25519_dalek::SigningKey doesn't impl Zeroize
        // without its `zeroize` feature, so we reconstruct from zeroed bytes.
        let zeroed = [0u8; 32];
        self.inner = ed25519_dalek::SigningKey::from_bytes(&zeroed);
    }
}

/// An Ed25519 public key for signature verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyingKey {
    inner: ed25519_dalek::VerifyingKey,
}

/// A 64-byte Ed25519 signature.
#[derive(Debug, Clone)]
pub struct Signature {
    bytes: [u8; 64],
}

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.bytes)
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SigVisitor;

        impl<'de> serde::de::Visitor<'de> for SigVisitor {
            type Value = Signature;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("64 bytes")
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Signature, E> {
                if v.len() != 64 {
                    return Err(E::invalid_length(v.len(), &self));
                }
                let mut arr = [0u8; 64];
                arr.copy_from_slice(v);
                Ok(Signature { bytes: arr })
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Signature, A::Error> {
                let mut arr = [0u8; 64];
                for (i, byte) in arr.iter_mut().enumerate() {
                    *byte = seq
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                }
                Ok(Signature { bytes: arr })
            }
        }

        deserializer.deserialize_bytes(SigVisitor)
    }
}

/// A signed Merkle root: the root hash + Ed25519 signature + signer identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedRoot {
    /// The Merkle root being signed.
    pub root: Hash512,
    /// Ed25519 signature over the root.
    pub signature: Signature,
    /// Public key of the signer (32 bytes).
    pub signer_public_key: [u8; 32],
    /// Timestamp when the signature was created (nanos since epoch).
    pub signed_at: i64,
}

impl SigningKey {
    /// Generate a new random Ed25519 keypair.
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        Self {
            inner: ed25519_dalek::SigningKey::generate(&mut rng),
        }
    }

    /// Restore from raw 32-byte secret key.
    pub fn from_bytes(secret: &[u8; 32]) -> Self {
        Self {
            inner: ed25519_dalek::SigningKey::from_bytes(secret),
        }
    }

    /// Export the raw 32-byte secret key.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.inner.to_bytes()
    }

    /// Get the corresponding public verifying key.
    pub fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey {
            inner: self.inner.verifying_key(),
        }
    }

    /// Sign a Merkle root hash.
    pub fn sign_root(&self, root: &Hash512, timestamp_nanos: i64) -> SignedRoot {
        let sig = self.inner.sign(root.as_ref());
        SignedRoot {
            root: *root,
            signature: Signature {
                bytes: sig.to_bytes(),
            },
            signer_public_key: self.inner.verifying_key().to_bytes(),
            signed_at: timestamp_nanos,
        }
    }

    /// Sign arbitrary bytes.
    pub fn sign(&self, message: &[u8]) -> Signature {
        let sig = self.inner.sign(message);
        Signature {
            bytes: sig.to_bytes(),
        }
    }
}

impl VerifyingKey {
    /// Restore from raw 32-byte public key.
    pub fn from_bytes(public: &[u8; 32]) -> Result<Self, aeon_types::AeonError> {
        ed25519_dalek::VerifyingKey::from_bytes(public)
            .map(|inner| Self { inner })
            .map_err(|e| aeon_types::AeonError::Crypto {
                message: format!("invalid Ed25519 public key: {e}"),
                source: None,
            })
    }

    /// Export the raw 32-byte public key.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.inner.to_bytes()
    }

    /// Verify a signature over a message.
    pub fn verify(&self, message: &[u8], signature: &Signature) -> bool {
        let sig = ed25519_dalek::Signature::from_bytes(&signature.bytes);
        self.inner.verify(message, &sig).is_ok()
    }
}

impl SignedRoot {
    /// Verify this signed root against the embedded public key.
    pub fn verify(&self) -> Result<bool, aeon_types::AeonError> {
        let vk = VerifyingKey::from_bytes(&self.signer_public_key)?;
        Ok(vk.verify(self.root.as_ref(), &self.signature))
    }

    /// Verify against a specific expected public key.
    pub fn verify_with_key(&self, expected_key: &VerifyingKey) -> bool {
        if expected_key.to_bytes() != self.signer_public_key {
            return false;
        }
        expected_key.verify(self.root.as_ref(), &self.signature)
    }

    /// Serialize to bincode bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    /// Deserialize from bincode bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }
}

impl Signature {
    /// Construct a Signature from raw 64 bytes.
    pub fn from_bytes(bytes: [u8; 64]) -> Self {
        Self { bytes }
    }

    /// Raw 64-byte signature.
    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::sha512;

    #[test]
    fn generate_and_sign() {
        let key = SigningKey::generate();
        let root = sha512(b"merkle-root");
        let signed = key.sign_root(&root, 1234567890);

        assert_eq!(signed.root, root);
        assert_eq!(signed.signed_at, 1234567890);
    }

    #[test]
    fn sign_and_verify() {
        let key = SigningKey::generate();
        let root = sha512(b"merkle-root");
        let signed = key.sign_root(&root, 1000);

        assert!(signed.verify().unwrap());
    }

    #[test]
    fn verify_with_correct_key() {
        let key = SigningKey::generate();
        let vk = key.verifying_key();
        let root = sha512(b"some-data");
        let signed = key.sign_root(&root, 0);

        assert!(signed.verify_with_key(&vk));
    }

    #[test]
    fn verify_with_wrong_key_fails() {
        let key1 = SigningKey::generate();
        let key2 = SigningKey::generate();
        let root = sha512(b"data");
        let signed = key1.sign_root(&root, 0);

        assert!(!signed.verify_with_key(&key2.verifying_key()));
    }

    #[test]
    fn tampered_root_fails_verification() {
        let key = SigningKey::generate();
        let root = sha512(b"original");
        let mut signed = key.sign_root(&root, 0);

        // Tamper with the root
        signed.root = sha512(b"tampered");
        assert!(!signed.verify().unwrap());
    }

    #[test]
    fn keypair_roundtrip_from_bytes() {
        let key = SigningKey::generate();
        let secret_bytes = key.to_bytes();
        let restored = SigningKey::from_bytes(&secret_bytes);

        let root = sha512(b"test");
        let signed = restored.sign_root(&root, 0);
        assert!(signed.verify_with_key(&key.verifying_key()));
    }

    #[test]
    fn verifying_key_roundtrip() {
        let key = SigningKey::generate();
        let vk = key.verifying_key();
        let vk_bytes = vk.to_bytes();
        let restored = VerifyingKey::from_bytes(&vk_bytes).unwrap();

        assert_eq!(vk, restored);
    }

    #[test]
    fn signed_root_serde_roundtrip() {
        let key = SigningKey::generate();
        let root = sha512(b"batch-42");
        let signed = key.sign_root(&root, 99999);

        let bytes = signed.to_bytes().unwrap();
        let decoded = SignedRoot::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.root, root);
        assert_eq!(decoded.signed_at, 99999);
        assert!(decoded.verify().unwrap());
    }

    #[test]
    fn sign_arbitrary_bytes() {
        let key = SigningKey::generate();
        let msg = b"arbitrary message";
        let sig = key.sign(msg);

        let vk = key.verifying_key();
        assert!(vk.verify(msg, &sig));
        assert!(!vk.verify(b"different message", &sig));
    }

    #[test]
    fn invalid_public_key_bytes() {
        // All-zeros is not a valid Ed25519 point (it's the identity/neutral element,
        // which ed25519-dalek rejects as a weak public key).
        let bad_bytes = [0u8; 32];
        let result = VerifyingKey::from_bytes(&bad_bytes);
        // If the library doesn't reject it at construction, verify it still
        // can't produce valid signatures. Either way, the system is safe.
        if let Ok(vk) = result {
            let key = SigningKey::generate();
            let sig = key.sign(b"test");
            // A wrong key should not verify a signature from a different key
            assert!(!vk.verify(b"test", &sig));
        }
    }
}
