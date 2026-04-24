//! S10 MongoDB mTLS — secure tempfile carrier.
//!
//! The `mongodb` crate's `TlsOptions::cert_key_file_path` takes a
//! `PathBuf`, not inline PEM bytes. Aeon's signer-first design keeps
//! cert + key in `SecretBytes` and forbids writing secrets to long-lived
//! disk paths. This module bridges the gap: for the lifetime of the
//! `MongoDbCdcSource` we hold a process-owned tempfile carrying the
//! concatenated cert+key, 0600 permissions on Unix, and zeroise the file
//! before dropping it.
//!
//! Trade-offs vs. the "no secrets on disk" rule:
//! - The footprint is scoped to the source's lifetime — not a manifest,
//!   not a config, not a secret file left behind.
//! - `tempfile::NamedTempFile` deletes the file when the handle drops,
//!   even on panic (tempfile uses Drop semantics, not cleanup hooks).
//! - The tempfile prefers the OS temp dir, which on Linux can be
//!   `/dev/shm` via `TMPDIR=/dev/shm` (operator opt-in) for tmpfs backing.
//! - MongoDB's rustls/openssl TLS config loaders read the path once when
//!   building the connection pool's TlsConfig; subsequent reconnects
//!   reuse the in-memory config. So the tempfile only has to survive the
//!   Client construction — but we hold it longer (as a field of the
//!   source struct) to stay robust against future driver-side behaviour
//!   changes that might re-read the path.
//!
//! Not used when the signer is in `None` / `BrokerNative` / HTTP-style
//! modes — only `Mtls` triggers a tempfile.

use aeon_types::AeonError;
use std::io::Write;
use std::path::Path;

/// Owner of a tempfile containing `cert_pem || "\n" || key_pem`. The
/// tempfile is deleted when this struct is dropped.
#[derive(Debug)]
pub(super) struct SecureMtlsTempFile {
    inner: tempfile::NamedTempFile,
}

impl SecureMtlsTempFile {
    /// Write `cert` + `key` to a new process-owned tempfile with
    /// restrictive permissions. MongoDB's rustls loader expects the PEMs
    /// concatenated in a single file; order is cert(s) then key.
    pub(super) fn write_pem(cert: &[u8], key: &[u8]) -> Result<Self, AeonError> {
        let mut inner = tempfile::Builder::new()
            .prefix("aeon-mongo-mtls-")
            .suffix(".pem")
            .rand_bytes(12)
            .tempfile()
            .map_err(|e| {
                AeonError::config(format!("mongo mTLS: tempfile create failed: {e}"))
            })?;

        // 0600 perms on Unix so only the connector process reads the key.
        // Windows: NamedTempFile creates with owner-only ACLs by default.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = inner
                .as_file()
                .metadata()
                .map_err(|e| {
                    AeonError::config(format!("mongo mTLS: stat tempfile: {e}"))
                })?
                .permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(inner.path(), perms).map_err(|e| {
                AeonError::config(format!("mongo mTLS: chmod tempfile: {e}"))
            })?;
        }

        inner
            .write_all(cert)
            .map_err(|e| AeonError::config(format!("mongo mTLS: write cert: {e}")))?;
        inner
            .write_all(b"\n")
            .map_err(|e| AeonError::config(format!("mongo mTLS: write sep: {e}")))?;
        inner
            .write_all(key)
            .map_err(|e| AeonError::config(format!("mongo mTLS: write key: {e}")))?;
        inner
            .as_file_mut()
            .sync_all()
            .map_err(|e| AeonError::config(format!("mongo mTLS: fsync tempfile: {e}")))?;

        Ok(Self { inner })
    }

    pub(super) fn path(&self) -> &Path {
        self.inner.path()
    }
}

impl Drop for SecureMtlsTempFile {
    fn drop(&mut self) {
        // Best-effort zeroise before delete: overwrite the file with
        // zero bytes of the same length, then let NamedTempFile's own
        // Drop handler remove it. If any step fails, we still delete —
        // keeping the key on disk is strictly worse than a noisy log.
        use std::io::Seek;
        if let Ok(metadata) = self.inner.as_file().metadata() {
            let len = metadata.len() as usize;
            if len > 0 {
                let zeros = vec![0u8; len];
                let _ = self.inner.as_file_mut().rewind();
                let _ = self.inner.as_file_mut().write_all(&zeros);
                let _ = self.inner.as_file_mut().sync_all();
            }
        }
        // NamedTempFile::drop removes the file path.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_pem_creates_file_then_drop_removes_it() {
        let path = {
            let f = SecureMtlsTempFile::write_pem(b"cert", b"key").unwrap();
            let p = f.path().to_path_buf();
            assert!(p.exists(), "tempfile must exist while held");
            p
        };
        assert!(!path.exists(), "tempfile must be removed on drop");
    }

    #[test]
    fn write_pem_concatenates_cert_then_key() {
        let f = SecureMtlsTempFile::write_pem(b"CERT-BYTES", b"KEY-BYTES").unwrap();
        let contents = std::fs::read(f.path()).unwrap();
        // Expect: "CERT-BYTES\nKEY-BYTES"
        assert_eq!(contents, b"CERT-BYTES\nKEY-BYTES");
    }

    #[cfg(unix)]
    #[test]
    fn write_pem_sets_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let f = SecureMtlsTempFile::write_pem(b"c", b"k").unwrap();
        let mode = std::fs::metadata(f.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "tempfile must be mode 0600");
    }
}
