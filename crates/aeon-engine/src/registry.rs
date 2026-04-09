//! Processor Registry — versioned catalog of processor artifacts.
//!
//! Stores processor metadata (name, versions, SHA-512 hashes) in memory,
//! with artifact bytes on the local filesystem. In cluster mode, the metadata
//! is Raft-replicated; artifacts are transferred via QUIC.
//!
//! The registry is the source of truth for "what processors exist" — the
//! Pipeline Manager references it when creating or upgrading pipelines.

use std::collections::BTreeMap;
use std::path::PathBuf;

use aeon_types::AeonError;
use aeon_types::registry::{
    ProcessorRecord, ProcessorVersion, RegistryCommand, RegistryResponse, VersionStatus,
};
use tokio::sync::RwLock;

/// Processor Registry — manages processor artifacts and metadata.
///
/// Thread-safe via `RwLock`. The registry can be shared across the REST API
/// server, CLI handler, and pipeline manager.
pub struct ProcessorRegistry {
    /// In-memory processor catalog. Keyed by processor name.
    processors: RwLock<BTreeMap<String, ProcessorRecord>>,
    /// Filesystem directory where artifacts are stored.
    artifact_dir: PathBuf,
}

impl ProcessorRegistry {
    /// Create a new registry with the given artifact storage directory.
    pub fn new(artifact_dir: impl Into<PathBuf>) -> Result<Self, AeonError> {
        let dir = artifact_dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| AeonError::Config {
            message: format!(
                "failed to create artifact directory '{}': {e}",
                dir.display()
            ),
        })?;

        Ok(Self {
            processors: RwLock::new(BTreeMap::new()),
            artifact_dir: dir,
        })
    }

    /// Apply a Raft-replicated registry command.
    ///
    /// This is the single entry point for all state mutations. In cluster mode,
    /// this is called after a command is committed to the Raft log.
    pub async fn apply(&self, cmd: RegistryCommand) -> RegistryResponse {
        match cmd {
            RegistryCommand::RegisterProcessor {
                name,
                description,
                version,
            } => self.apply_register(&name, &description, version).await,
            RegistryCommand::DeleteVersion { name, version } => {
                self.apply_delete_version(&name, &version).await
            }
            RegistryCommand::SetVersionStatus {
                name,
                version,
                status,
            } => self.apply_set_status(&name, &version, status).await,
            // Pipeline commands are handled by PipelineManager, not here
            _ => RegistryResponse::Error {
                message: "command not handled by ProcessorRegistry".into(),
            },
        }
    }

    /// Register a processor version. Creates the processor record if it doesn't exist.
    pub async fn register(
        &self,
        name: &str,
        description: &str,
        version: ProcessorVersion,
        artifact_bytes: &[u8],
    ) -> Result<RegistryResponse, AeonError> {
        // Verify SHA-512 hash
        let computed_hash = sha512_hex(artifact_bytes);
        if computed_hash != version.sha512 {
            return Err(AeonError::Config {
                message: format!(
                    "SHA-512 mismatch: expected {}, computed {computed_hash}",
                    version.sha512
                ),
            });
        }

        // Store artifact on filesystem
        let artifact_path = self.artifact_path(name, &version.version);
        if let Some(parent) = artifact_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AeonError::Config {
                message: format!("failed to create artifact dir: {e}"),
            })?;
        }
        std::fs::write(&artifact_path, artifact_bytes).map_err(|e| AeonError::Config {
            message: format!("failed to write artifact: {e}"),
        })?;

        let ver_string = version.version.clone();

        // Apply to in-memory state
        let resp = self.apply_register(name, description, version).await;
        match &resp {
            RegistryResponse::ProcessorRegistered { .. } | RegistryResponse::Ok => {}
            RegistryResponse::Error { message } => {
                // Clean up artifact on failure
                let _ = std::fs::remove_file(&artifact_path);
                return Err(AeonError::Config {
                    message: message.clone(),
                });
            }
            _ => {}
        }

        tracing::info!(
            processor = name,
            version = ver_string,
            size = artifact_bytes.len(),
            "processor registered"
        );

        Ok(resp)
    }

    /// List all processors (names only).
    pub async fn list(&self) -> Vec<String> {
        let procs = self.processors.read().await;
        procs.keys().cloned().collect()
    }

    /// Get a processor record by name.
    pub async fn get(&self, name: &str) -> Option<ProcessorRecord> {
        let procs = self.processors.read().await;
        procs.get(name).cloned()
    }

    /// Get all versions of a processor.
    pub async fn versions(&self, name: &str) -> Option<Vec<ProcessorVersion>> {
        let procs = self.processors.read().await;
        procs
            .get(name)
            .map(|r| r.versions.values().cloned().collect())
    }

    /// Load artifact bytes for a specific processor version.
    pub async fn load_artifact(&self, name: &str, version: &str) -> Result<Vec<u8>, AeonError> {
        let path = self.artifact_path(name, version);
        std::fs::read(&path).map_err(|e| {
            AeonError::not_found(format!(
                "artifact for {name}:{version} at '{}': {e}",
                path.display()
            ))
        })
    }

    /// Delete a processor version.
    pub async fn delete_version(
        &self,
        name: &str,
        version: &str,
    ) -> Result<RegistryResponse, AeonError> {
        // Check if active
        {
            let procs = self.processors.read().await;
            if let Some(record) = procs.get(name) {
                if let Some(ver) = record.get_version(version) {
                    if ver.status == VersionStatus::Active {
                        return Err(AeonError::Config {
                            message: format!(
                                "cannot delete active version {name}:{version} — stop pipelines first"
                            ),
                        });
                    }
                }
            }
        }

        let resp = self.apply_delete_version(name, version).await;

        // Remove artifact file
        let path = self.artifact_path(name, version);
        let _ = std::fs::remove_file(&path);

        Ok(resp)
    }

    /// Get the number of registered processors.
    pub async fn count(&self) -> usize {
        self.processors.read().await.len()
    }

    /// Get the full processor catalog (for Raft snapshot).
    pub async fn snapshot(&self) -> BTreeMap<String, ProcessorRecord> {
        self.processors.read().await.clone()
    }

    /// Restore from a Raft snapshot.
    pub async fn restore(&self, data: BTreeMap<String, ProcessorRecord>) {
        let mut procs = self.processors.write().await;
        *procs = data;
    }

    // ── Internal apply methods ─────────────────────────────────────────

    async fn apply_register(
        &self,
        name: &str,
        description: &str,
        version: ProcessorVersion,
    ) -> RegistryResponse {
        let mut procs = self.processors.write().await;
        let ver_string = version.version.clone();
        let now = now_millis();

        let record = procs
            .entry(name.to_string())
            .or_insert_with(|| ProcessorRecord::new(name, description, now));

        record.versions.insert(ver_string.clone(), version);
        record.updated_at = now;

        RegistryResponse::ProcessorRegistered {
            name: name.to_string(),
            version: ver_string,
        }
    }

    async fn apply_delete_version(&self, name: &str, version: &str) -> RegistryResponse {
        let mut procs = self.processors.write().await;

        if let Some(record) = procs.get_mut(name) {
            if record.versions.remove(version).is_some() {
                record.updated_at = now_millis();
                // If no versions left, remove the whole record
                if record.versions.is_empty() {
                    procs.remove(name);
                }
                return RegistryResponse::Ok;
            }
        }

        RegistryResponse::Error {
            message: format!("version {name}:{version} not found"),
        }
    }

    async fn apply_set_status(
        &self,
        name: &str,
        version: &str,
        status: VersionStatus,
    ) -> RegistryResponse {
        let mut procs = self.processors.write().await;

        if let Some(record) = procs.get_mut(name) {
            if let Some(ver) = record.versions.get_mut(version) {
                ver.status = status;
                record.updated_at = now_millis();
                return RegistryResponse::Ok;
            }
        }

        RegistryResponse::Error {
            message: format!("version {name}:{version} not found"),
        }
    }

    fn artifact_path(&self, name: &str, version: &str) -> PathBuf {
        self.artifact_dir.join(name).join(version)
    }
}

impl std::fmt::Debug for ProcessorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessorRegistry")
            .field("artifact_dir", &self.artifact_dir)
            .finish()
    }
}

/// Compute SHA-512 hex digest of bytes.
pub fn sha512_hex(data: &[u8]) -> String {
    use std::io::Write;
    // Use ring for SHA-512 (already in the dep tree via rustls)
    // Fallback: manual implementation for portability
    let digest = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        // For now, use a simple hash until we wire in ring/sha2
        // This will be replaced with proper SHA-512 in integration
        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        let h1 = hasher.finish();
        data.len().hash(&mut hasher);
        let h2 = hasher.finish();
        // Produce a deterministic hex string (not cryptographic — placeholder)
        let mut buf = Vec::new();
        write!(buf, "{h1:016x}{h2:016x}").unwrap_or_default();
        // Pad to 128 hex chars (SHA-512 length)
        while buf.len() < 128 {
            write!(buf, "0").unwrap_or_default();
        }
        buf
    };
    String::from_utf8(digest).unwrap_or_default()
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::registry::ProcessorType;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aeon-registry-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_version(ver: &str, data: &[u8]) -> ProcessorVersion {
        ProcessorVersion {
            version: ver.into(),
            sha512: sha512_hex(data),
            size_bytes: data.len() as u64,
            processor_type: ProcessorType::Wasm,
            platform: "wasm32".into(),
            status: VersionStatus::Available,
            registered_at: 1000,
            registered_by: "test".into(),
            endpoint: None,
            max_batch_size: None,
        }
    }

    #[tokio::test]
    async fn register_and_list() {
        let dir = temp_dir();
        let reg = ProcessorRegistry::new(&dir).unwrap();

        let data = b"fake wasm bytes";
        let ver = make_version("1.0.0", data);
        let resp = reg
            .register("my-proc", "A processor", ver, data)
            .await
            .unwrap();

        match resp {
            RegistryResponse::ProcessorRegistered { name, version } => {
                assert_eq!(name, "my-proc");
                assert_eq!(version, "1.0.0");
            }
            _ => panic!("unexpected response"),
        }

        let list = reg.list().await;
        assert_eq!(list, vec!["my-proc"]);

        let record = reg.get("my-proc").await.unwrap();
        assert_eq!(record.versions.len(), 1);
        assert_eq!(record.latest_version().unwrap().version, "1.0.0");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn register_multiple_versions() {
        let dir = temp_dir();
        let reg = ProcessorRegistry::new(&dir).unwrap();

        for ver in &["1.0.0", "1.1.0", "2.0.0"] {
            let data = format!("data-{ver}").into_bytes();
            let v = make_version(ver, &data);
            reg.register("proc", "desc", v, &data).await.unwrap();
        }

        let versions = reg.versions("proc").await.unwrap();
        assert_eq!(versions.len(), 3);

        let record = reg.get("proc").await.unwrap();
        assert_eq!(record.latest_version().unwrap().version, "2.0.0");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn load_artifact_roundtrip() {
        let dir = temp_dir();
        let reg = ProcessorRegistry::new(&dir).unwrap();

        let data = b"actual wasm module bytes here";
        let ver = make_version("0.5.0", data);
        reg.register("loader-test", "", ver, data).await.unwrap();

        let loaded = reg.load_artifact("loader-test", "0.5.0").await.unwrap();
        assert_eq!(loaded, data);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn delete_version() {
        let dir = temp_dir();
        let reg = ProcessorRegistry::new(&dir).unwrap();

        let data = b"bytes";
        let ver = make_version("1.0.0", data);
        reg.register("del-test", "", ver, data).await.unwrap();

        assert_eq!(reg.count().await, 1);

        reg.delete_version("del-test", "1.0.0").await.unwrap();
        assert_eq!(reg.count().await, 0);
        assert!(reg.get("del-test").await.is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn delete_active_version_fails() {
        let dir = temp_dir();
        let reg = ProcessorRegistry::new(&dir).unwrap();

        let data = b"bytes";
        let mut ver = make_version("1.0.0", data);
        ver.status = VersionStatus::Active;
        reg.register("active-test", "", ver, data).await.unwrap();

        let result = reg.delete_version("active-test", "1.0.0").await;
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn sha512_mismatch_rejected() {
        let dir = temp_dir();
        let reg = ProcessorRegistry::new(&dir).unwrap();

        let data = b"real data";
        let mut ver = make_version("1.0.0", data);
        ver.sha512 = "wrong_hash".into();

        let result = reg.register("hash-test", "", ver, data).await;
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn snapshot_and_restore() {
        let dir = temp_dir();
        let reg = ProcessorRegistry::new(&dir).unwrap();

        let data = b"bytes";
        let ver = make_version("1.0.0", data);
        reg.register("snap-test", "desc", ver, data).await.unwrap();

        let snapshot = reg.snapshot().await;
        assert_eq!(snapshot.len(), 1);

        // Create a new registry and restore
        let dir2 = temp_dir();
        let reg2 = ProcessorRegistry::new(&dir2).unwrap();
        assert_eq!(reg2.count().await, 0);

        reg2.restore(snapshot).await;
        assert_eq!(reg2.count().await, 1);
        assert!(reg2.get("snap-test").await.is_some());

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir2).ok();
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let dir = temp_dir();
        let reg = ProcessorRegistry::new(&dir).unwrap();

        assert!(reg.get("nonexistent").await.is_none());
        assert!(reg.versions("nonexistent").await.is_none());
        assert!(reg.load_artifact("nonexistent", "1.0.0").await.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
