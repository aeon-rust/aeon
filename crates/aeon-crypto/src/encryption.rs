//! Encrypt-then-MAC (EtM) symmetric encryption: AES-256-CTR + HMAC-SHA-512.
//!
//! Wire format: `[16-byte nonce][ciphertext][64-byte HMAC]`
//!
//! - Encryption: AES-256-CTR with a random 128-bit nonce
//! - Authentication: HMAC-SHA-512 over `nonce || ciphertext`
//! - Two independent 32-byte keys: one for AES, one for HMAC (64 bytes total)
//!
//! All key material is zeroized on Drop via the `zeroize` crate.

use aes::cipher::{KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use sha2::Sha512;
use zeroize::{Zeroize, ZeroizeOnDrop};

type Aes256Ctr = ctr::Ctr64BE<aes::Aes256>;
type HmacSha512 = Hmac<Sha512>;

const NONCE_LEN: usize = 16;
const MAC_LEN: usize = 64; // SHA-512 output
const AES_KEY_LEN: usize = 32;
const HMAC_KEY_LEN: usize = 32;

/// Combined encryption + MAC key pair (64 bytes total, zeroized on Drop).
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct EtmKey {
    /// AES-256 encryption key (32 bytes).
    enc_key: [u8; AES_KEY_LEN],
    /// HMAC-SHA-512 authentication key (32 bytes).
    mac_key: [u8; HMAC_KEY_LEN],
}

impl EtmKey {
    /// Create from two separate 32-byte keys.
    pub fn new(enc_key: [u8; AES_KEY_LEN], mac_key: [u8; HMAC_KEY_LEN]) -> Self {
        Self { enc_key, mac_key }
    }

    /// Create from a single 64-byte master key (first 32 = AES, last 32 = HMAC).
    pub fn from_bytes(master: &[u8; 64]) -> Self {
        let mut enc_key = [0u8; AES_KEY_LEN];
        let mut mac_key = [0u8; HMAC_KEY_LEN];
        enc_key.copy_from_slice(&master[..32]);
        mac_key.copy_from_slice(&master[32..]);
        Self { enc_key, mac_key }
    }

    /// Export the combined 64-byte key material.
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&self.enc_key);
        out[32..].copy_from_slice(&self.mac_key);
        out
    }

    /// Generate a random EtM key pair.
    pub fn generate() -> Self {
        let mut enc_key = [0u8; AES_KEY_LEN];
        let mut mac_key = [0u8; HMAC_KEY_LEN];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut enc_key);
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut mac_key);
        Self { enc_key, mac_key }
    }

    /// Encrypt plaintext using AES-256-CTR, then authenticate with HMAC-SHA-512.
    ///
    /// Returns `nonce || ciphertext || hmac` (overhead: 80 bytes).
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, aeon_types::AeonError> {
        #[cfg(feature = "fips")]
        crate::fips::assert_fips_approved("AES-256-CTR + HMAC-SHA-512")?;

        // Generate random nonce
        let mut nonce = [0u8; NONCE_LEN];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut nonce);

        // Encrypt: AES-256-CTR
        let mut ciphertext = plaintext.to_vec();
        let mut cipher = Aes256Ctr::new(self.enc_key.as_ref().into(), nonce.as_ref().into());
        cipher.apply_keystream(&mut ciphertext);

        // Authenticate: HMAC-SHA-512 over nonce || ciphertext
        let mut mac = <HmacSha512 as Mac>::new_from_slice(&self.mac_key).map_err(|e| {
            aeon_types::AeonError::Crypto {
                message: format!("HMAC key error: {e}"),
                source: None,
            }
        })?;
        mac.update(&nonce);
        mac.update(&ciphertext);
        let tag = mac.finalize().into_bytes();

        // Pack: nonce || ciphertext || tag
        let mut output = Vec::with_capacity(NONCE_LEN + ciphertext.len() + MAC_LEN);
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&ciphertext);
        output.extend_from_slice(&tag);
        Ok(output)
    }

    /// Verify HMAC, then decrypt. Returns plaintext or error on tamper/wrong key.
    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, aeon_types::AeonError> {
        #[cfg(feature = "fips")]
        crate::fips::assert_fips_approved("AES-256-CTR + HMAC-SHA-512")?;

        let min_len = NONCE_LEN + MAC_LEN;
        if data.len() < min_len {
            return Err(aeon_types::AeonError::Crypto {
                message: format!(
                    "ciphertext too short: {} bytes (minimum {})",
                    data.len(),
                    min_len
                ),
                source: None,
            });
        }

        let nonce = &data[..NONCE_LEN];
        let ciphertext = &data[NONCE_LEN..data.len() - MAC_LEN];
        let expected_tag = &data[data.len() - MAC_LEN..];

        // Verify MAC first (before decryption — this is EtM)
        let mut mac = <HmacSha512 as Mac>::new_from_slice(&self.mac_key).map_err(|e| {
            aeon_types::AeonError::Crypto {
                message: format!("HMAC key error: {e}"),
                source: None,
            }
        })?;
        mac.update(nonce);
        mac.update(ciphertext);
        mac.verify_slice(expected_tag)
            .map_err(|_| aeon_types::AeonError::Crypto {
                message: "HMAC verification failed: data tampered or wrong key".into(),
                source: None,
            })?;

        // Decrypt: AES-256-CTR
        let mut plaintext = ciphertext.to_vec();
        let mut cipher = Aes256Ctr::new(self.enc_key.as_ref().into(), nonce.into());
        cipher.apply_keystream(&mut plaintext);

        Ok(plaintext)
    }

    /// Overhead in bytes added by encryption (nonce + HMAC tag).
    pub const fn overhead() -> usize {
        NONCE_LEN + MAC_LEN
    }
}

impl std::fmt::Debug for EtmKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EtmKey")
            .field("enc_key", &"[REDACTED]")
            .field("mac_key", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = EtmKey::generate();
        let plaintext = b"hello, aeon!";
        let encrypted = key.encrypt(plaintext).unwrap();
        let decrypted = key.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_decrypt_empty() {
        let key = EtmKey::generate();
        let encrypted = key.encrypt(b"").unwrap();
        assert_eq!(encrypted.len(), EtmKey::overhead());
        let decrypted = key.decrypt(&encrypted).unwrap();
        assert!(decrypted.is_empty());
    }

    #[test]
    fn encrypt_decrypt_large_payload() {
        let key = EtmKey::generate();
        let plaintext = vec![0xABu8; 1_000_000];
        let encrypted = key.encrypt(&plaintext).unwrap();
        assert_eq!(encrypted.len(), plaintext.len() + EtmKey::overhead());
        let decrypted = key.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = EtmKey::generate();
        let mut encrypted = key.encrypt(b"secret data").unwrap();
        // Flip a byte in the ciphertext region
        encrypted[NONCE_LEN] ^= 0xFF;
        let result = key.decrypt(&encrypted);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("HMAC verification failed"));
    }

    #[test]
    fn tampered_nonce_fails() {
        let key = EtmKey::generate();
        let mut encrypted = key.encrypt(b"secret").unwrap();
        encrypted[0] ^= 0x01;
        assert!(key.decrypt(&encrypted).is_err());
    }

    #[test]
    fn tampered_mac_fails() {
        let key = EtmKey::generate();
        let mut encrypted = key.encrypt(b"secret").unwrap();
        let last = encrypted.len() - 1;
        encrypted[last] ^= 0x01;
        assert!(key.decrypt(&encrypted).is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = EtmKey::generate();
        let key2 = EtmKey::generate();
        let encrypted = key1.encrypt(b"for key1 only").unwrap();
        assert!(key2.decrypt(&encrypted).is_err());
    }

    #[test]
    fn too_short_data_fails() {
        let key = EtmKey::generate();
        let short = vec![0u8; NONCE_LEN + MAC_LEN - 1];
        let result = key.decrypt(&short);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("too short"));
    }

    #[test]
    fn different_encryptions_produce_different_ciphertexts() {
        let key = EtmKey::generate();
        let plaintext = b"same input";
        let e1 = key.encrypt(plaintext).unwrap();
        let e2 = key.encrypt(plaintext).unwrap();
        // Different nonces → different ciphertext
        assert_ne!(e1, e2);
        // But both decrypt to the same plaintext
        assert_eq!(key.decrypt(&e1).unwrap(), key.decrypt(&e2).unwrap());
    }

    #[test]
    fn key_from_bytes_roundtrip() {
        let key = EtmKey::generate();
        let bytes = key.to_bytes();
        let restored = EtmKey::from_bytes(&bytes);
        let plaintext = b"roundtrip test";
        let encrypted = key.encrypt(plaintext).unwrap();
        assert_eq!(restored.decrypt(&encrypted).unwrap(), plaintext);
    }

    #[test]
    fn key_debug_redacts_secrets() {
        let key = EtmKey::generate();
        let debug = format!("{key:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains(&format!("{:?}", key.enc_key)));
    }

    #[test]
    fn overhead_is_80_bytes() {
        assert_eq!(EtmKey::overhead(), 80);
    }
}
