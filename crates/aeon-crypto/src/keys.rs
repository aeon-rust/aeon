//! Key management: KeyProvider trait and implementations.
//!
//! Key providers load cryptographic key material from various backends:
//! - `EnvKeyProvider` — from environment variables
//! - `FileKeyProvider` — from a file on disk
//! - Future: Vault, PKCS#11, AWS KMS, GCP KMS, Azure Key Vault
//!
//! All key material is zeroized on Drop.

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::encryption::EtmKey;
use crate::signing::SigningKey;

/// A loaded key with metadata, zeroized on Drop.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct KeyMaterial {
    /// Raw key bytes.
    bytes: Vec<u8>,
    /// Key identifier (not zeroized — it's metadata).
    #[zeroize(skip)]
    pub id: String,
    /// Key purpose.
    #[zeroize(skip)]
    pub purpose: KeyPurpose,
}

/// What a key is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPurpose {
    /// AES-256-CTR + HMAC-SHA-512 encryption (64 bytes: 32 AES + 32 HMAC).
    Encryption,
    /// Ed25519 signing (32 bytes).
    Signing,
}

impl KeyMaterial {
    /// Create from raw bytes with an ID and purpose.
    pub fn new(id: impl Into<String>, purpose: KeyPurpose, bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            id: id.into(),
            purpose,
        }
    }

    /// Access the raw key bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Convert to an EtM encryption key (requires 64 bytes).
    pub fn to_etm_key(&self) -> Result<EtmKey, aeon_types::AeonError> {
        if self.purpose != KeyPurpose::Encryption {
            return Err(aeon_types::AeonError::Crypto {
                message: format!("key '{}' is not an encryption key", self.id),
                source: None,
            });
        }
        if self.bytes.len() != 64 {
            return Err(aeon_types::AeonError::Crypto {
                message: format!(
                    "encryption key '{}' must be 64 bytes, got {}",
                    self.id,
                    self.bytes.len()
                ),
                source: None,
            });
        }
        let mut arr = [0u8; 64];
        arr.copy_from_slice(&self.bytes);
        let key = EtmKey::from_bytes(&arr);
        arr.zeroize();
        Ok(key)
    }

    /// Convert to an Ed25519 signing key (requires 32 bytes).
    pub fn to_signing_key(&self) -> Result<SigningKey, aeon_types::AeonError> {
        if self.purpose != KeyPurpose::Signing {
            return Err(aeon_types::AeonError::Crypto {
                message: format!("key '{}' is not a signing key", self.id),
                source: None,
            });
        }
        if self.bytes.len() != 32 {
            return Err(aeon_types::AeonError::Crypto {
                message: format!(
                    "signing key '{}' must be 32 bytes, got {}",
                    self.id,
                    self.bytes.len()
                ),
                source: None,
            });
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&self.bytes);
        let key = SigningKey::from_bytes(&arr);
        arr.zeroize();
        Ok(key)
    }
}

impl std::fmt::Debug for KeyMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyMaterial")
            .field("id", &self.id)
            .field("purpose", &self.purpose)
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

/// Trait for loading key material from a backend.
///
/// Implementations must zeroize any intermediate buffers.
pub trait KeyProvider: Send + Sync {
    /// Load a key by its identifier.
    fn load_key(
        &self,
        key_id: &str,
        purpose: KeyPurpose,
    ) -> Result<KeyMaterial, aeon_types::AeonError>;

    /// List available key IDs (for inventory/audit).
    fn list_keys(&self) -> Result<Vec<String>, aeon_types::AeonError>;
}

// ─── EnvKeyProvider ────────────────────────────────────────────────

/// Loads keys from environment variables.
///
/// Key format: the env var value is hex-encoded key bytes.
/// Variable naming: `{prefix}_{KEY_ID}` (e.g., `AEON_KEY_node_signing`).
pub struct EnvKeyProvider {
    prefix: String,
}

impl EnvKeyProvider {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }
}

impl KeyProvider for EnvKeyProvider {
    fn load_key(
        &self,
        key_id: &str,
        purpose: KeyPurpose,
    ) -> Result<KeyMaterial, aeon_types::AeonError> {
        let var_name = format!("{}_{}", self.prefix, key_id);
        let hex_value = std::env::var(&var_name).map_err(|_| aeon_types::AeonError::Crypto {
            message: format!("environment variable '{var_name}' not set"),
            source: None,
        })?;

        let bytes = hex_decode(&hex_value).map_err(|e| aeon_types::AeonError::Crypto {
            message: format!("invalid hex in '{var_name}': {e}"),
            source: None,
        })?;

        Ok(KeyMaterial::new(key_id, purpose, bytes))
    }

    fn list_keys(&self) -> Result<Vec<String>, aeon_types::AeonError> {
        let prefix_with_sep = format!("{}_", self.prefix);
        Ok(std::env::vars()
            .filter_map(|(k, _)| k.strip_prefix(&prefix_with_sep).map(String::from))
            .collect())
    }
}

// ─── FileKeyProvider ───────────────────────────────────────────────

/// Loads keys from files in a directory.
///
/// Key format: raw binary file (not hex). File name is the key ID.
/// Directory structure: `{base_dir}/{key_id}.key`
pub struct FileKeyProvider {
    base_dir: std::path::PathBuf,
}

impl FileKeyProvider {
    pub fn new(base_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

impl KeyProvider for FileKeyProvider {
    fn load_key(
        &self,
        key_id: &str,
        purpose: KeyPurpose,
    ) -> Result<KeyMaterial, aeon_types::AeonError> {
        // Prevent path traversal
        if key_id.contains('/') || key_id.contains('\\') || key_id.contains("..") {
            return Err(aeon_types::AeonError::Crypto {
                message: format!("invalid key ID: '{key_id}' (path traversal rejected)"),
                source: None,
            });
        }

        let path = self.base_dir.join(format!("{key_id}.key"));
        let bytes = std::fs::read(&path).map_err(|e| aeon_types::AeonError::Crypto {
            message: format!("failed to read key file '{}': {e}", path.display()),
            source: None,
        })?;

        Ok(KeyMaterial::new(key_id, purpose, bytes))
    }

    fn list_keys(&self) -> Result<Vec<String>, aeon_types::AeonError> {
        let entries =
            std::fs::read_dir(&self.base_dir).map_err(|e| aeon_types::AeonError::Crypto {
                message: format!(
                    "failed to read key directory '{}': {e}",
                    self.base_dir.display()
                ),
                source: None,
            })?;

        Ok(entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.strip_suffix(".key").map(String::from)
            })
            .collect())
    }
}

/// Decode hex string to bytes.
fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    let hex = hex.trim();
    if hex.len() % 2 != 0 {
        return Err("odd-length hex string".into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| format!("invalid hex at position {i}: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_material_to_etm_key() {
        let bytes = vec![0xAA; 64];
        let km = KeyMaterial::new("test", KeyPurpose::Encryption, bytes);
        let etm = km.to_etm_key().unwrap();
        let plaintext = b"test data";
        let encrypted = etm.encrypt(plaintext).unwrap();
        assert_eq!(etm.decrypt(&encrypted).unwrap(), plaintext);
    }

    #[test]
    fn key_material_wrong_purpose() {
        let bytes = vec![0xAA; 64];
        let km = KeyMaterial::new("test", KeyPurpose::Signing, bytes);
        assert!(km.to_etm_key().is_err());
    }

    #[test]
    fn key_material_wrong_size() {
        let bytes = vec![0xAA; 32]; // Too short for EtM (needs 64)
        let km = KeyMaterial::new("test", KeyPurpose::Encryption, bytes);
        assert!(km.to_etm_key().is_err());
    }

    #[test]
    fn key_material_to_signing_key() {
        let bytes = vec![0xBB; 32];
        let km = KeyMaterial::new("signer", KeyPurpose::Signing, bytes);
        let _sk = km.to_signing_key().unwrap();
    }

    #[test]
    fn key_material_debug_redacts() {
        let km = KeyMaterial::new("secret", KeyPurpose::Encryption, vec![0xFF; 64]);
        let debug = format!("{km:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains("255")); // 0xFF = 255
    }

    #[test]
    fn hex_decode_valid() {
        assert_eq!(
            hex_decode("deadbeef").unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
        assert_eq!(hex_decode("00ff").unwrap(), vec![0x00, 0xFF]);
    }

    #[test]
    fn hex_decode_invalid() {
        assert!(hex_decode("0").is_err()); // Odd length
        assert!(hex_decode("zz").is_err()); // Invalid chars
    }

    #[test]
    fn env_key_provider_load() {
        let hex_key = "aa".repeat(64); // 64 bytes hex-encoded = 128 hex chars
        // SAFETY: test is single-threaded, no concurrent env access.
        unsafe { std::env::set_var("AEON_TEST_KEY_encryption", &hex_key) };
        let provider = EnvKeyProvider::new("AEON_TEST_KEY");
        let km = provider
            .load_key("encryption", KeyPurpose::Encryption)
            .unwrap();
        assert_eq!(km.as_bytes().len(), 64);
        assert_eq!(km.id, "encryption");
        unsafe { std::env::remove_var("AEON_TEST_KEY_encryption") };
    }

    #[test]
    fn env_key_provider_missing_var() {
        let provider = EnvKeyProvider::new("AEON_MISSING");
        assert!(
            provider
                .load_key("nonexistent", KeyPurpose::Encryption)
                .is_err()
        );
    }

    #[test]
    fn file_key_provider_load() {
        let dir = std::env::temp_dir().join("aeon_key_test");
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("testkey.key");
        std::fs::write(&key_path, vec![0xCC; 32]).unwrap();

        let provider = FileKeyProvider::new(&dir);
        let km = provider.load_key("testkey", KeyPurpose::Signing).unwrap();
        assert_eq!(km.as_bytes(), &[0xCC; 32]);
        assert_eq!(km.id, "testkey");

        std::fs::remove_file(key_path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn file_key_provider_path_traversal_rejected() {
        let provider = FileKeyProvider::new("/tmp");
        assert!(
            provider
                .load_key("../etc/shadow", KeyPurpose::Signing)
                .is_err()
        );
        assert!(
            provider
                .load_key("../../passwd", KeyPurpose::Signing)
                .is_err()
        );
    }

    #[test]
    fn file_key_provider_list_keys() {
        let dir = std::env::temp_dir().join("aeon_key_list_test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("alpha.key"), b"key1").unwrap();
        std::fs::write(dir.join("beta.key"), b"key2").unwrap();
        std::fs::write(dir.join("notakey.txt"), b"skip").unwrap();

        let provider = FileKeyProvider::new(&dir);
        let mut keys = provider.list_keys().unwrap();
        keys.sort();
        assert_eq!(keys, vec!["alpha", "beta"]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
