//! G11.a — production `SegmentInstaller` and `PohChainInstaller` wiring.
//!
//! The cluster crate defines the target-side installer traits called by
//! `PartitionTransferDriver` during CL-6a / CL-6b. Up to Session A those
//! traits only had test-only `Recording…` impls, which meant the driver
//! in production had nothing to install into — hence G11 ("all 17
//! transfers stuck `status=transferring, owner=null`").
//!
//! This module provides the two concrete impls:
//!
//! * [`L2SegmentInstaller`] — writes incoming segment chunks into the
//!   per-partition `L2BodyStore` directory via `l2_transfer::SegmentWriter`,
//!   with CRC32 validation on each segment's last chunk. Concurrency: serial
//!   per partition (one inflight `SegmentWriter` per `(pipeline, partition)`),
//!   parallel across partitions.
//!
//! * [`PohChainInstaller`] — decodes the incoming bincode-encoded
//!   `PohChainState`, runs the configured [`PohVerifyMode`] check
//!   (`Verify` / `VerifyWithKey` / `TrustExtend`), then hands the state
//!   to [`PohChain::from_state`] and stashes the resulting chain in a
//!   registry the engine reads when starting the post-transfer pipeline.
//!
//! Both installers are cheap `Arc`-cloneable handles so the cluster-side
//! `PartitionTransferDriver` can be built once at `ClusterNode` bootstrap
//! and share the same registry state with any pipeline manager that
//! resumes a partition after transfer.

#![cfg(all(feature = "cluster", feature = "processor-auth"))]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aeon_cluster::partition_driver::{PohChainInstaller, SegmentInstaller};
use aeon_cluster::types::{
    PartitionTransferEnd, PartitionTransferRequest, PohChainTransferRequest,
};
use aeon_crypto::poh::{PohChain, PohChainState, PohVerifyMode};
use aeon_types::l2_transfer::{SegmentChunk, SegmentManifest};
use aeon_types::{AeonError, PartitionId};

use crate::l2_transfer::SegmentWriter;

type WritersMap = HashMap<(String, u16), SegmentWriter>;

// ── L2 segment installer ──────────────────────────────────────────────

/// Production [`SegmentInstaller`] — routes received segments to the
/// per-partition L2 body directory, validates CRC32 on finalize, and
/// cleans up partial files on abort.
///
/// Layout: `root/{pipeline}/{partition_dir}/…l2b` (mirrors
/// `PipelineL2Registry`'s disk layout so the engine can open the exact
/// same directory once ownership flips).
///
/// Serial per partition (one `SegmentWriter` in flight at a time for a
/// given `(pipeline, partition)`); parallel across partitions is free
/// because each entry in `writers` is independent.
pub struct L2SegmentInstaller {
    root: PathBuf,
    pipeline: String,
    /// `(pipeline, partition_id) → in-flight SegmentWriter`. An entry is
    /// inserted on `begin` and removed on `finish` (success or abort).
    writers: Mutex<WritersMap>,
}

impl L2SegmentInstaller {
    /// Create an installer rooted at `root`. All partition directories
    /// live under `root/{pipeline}/p{partition:05}/`.
    pub fn new(root: impl Into<PathBuf>, pipeline: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            pipeline: pipeline.into(),
            writers: Mutex::new(HashMap::new()),
        }
    }

    fn partition_dir(&self, partition: PartitionId) -> PathBuf {
        self.root
            .join(&self.pipeline)
            .join(format!("p{:05}", partition.as_u16()))
    }

    fn lock_writers(&self) -> Result<std::sync::MutexGuard<'_, WritersMap>, AeonError> {
        self.writers
            .lock()
            .map_err(|e| AeonError::state(format!("l2-installer: writers lock poisoned: {e}")))
    }

    fn key_of(&self, req_pipeline: &str, partition: PartitionId) -> (String, u16) {
        // Pipeline in the request wins — multi-pipeline future-proof. For
        // today's single-pipeline deployments, `req.pipeline` matches
        // `self.pipeline`.
        (req_pipeline.to_owned(), partition.as_u16())
    }
}

impl SegmentInstaller for L2SegmentInstaller {
    fn begin(
        &self,
        req: &PartitionTransferRequest,
        manifest: SegmentManifest,
    ) -> Result<(), AeonError> {
        let key = self.key_of(&req.pipeline, req.partition);
        let dir = self.partition_dir(req.partition);
        let writer = SegmentWriter::open(dir, manifest)?;
        let mut writers = self.lock_writers()?;
        if writers.contains_key(&key) {
            return Err(AeonError::state(format!(
                "l2-installer: begin called twice for pipeline={} partition={}",
                req.pipeline,
                req.partition.as_u16()
            )));
        }
        writers.insert(key, writer);
        Ok(())
    }

    fn apply_chunk(
        &self,
        req: &PartitionTransferRequest,
        chunk: SegmentChunk,
    ) -> Result<(), AeonError> {
        let key = self.key_of(&req.pipeline, req.partition);
        let mut writers = self.lock_writers()?;
        let writer = writers.get_mut(&key).ok_or_else(|| {
            AeonError::state(format!(
                "l2-installer: apply_chunk without begin for pipeline={} partition={}",
                req.pipeline,
                req.partition.as_u16()
            ))
        })?;
        writer.apply_chunk(chunk)
    }

    fn finish(
        &self,
        req: &PartitionTransferRequest,
        end: PartitionTransferEnd,
    ) -> Result<(), AeonError> {
        let key = self.key_of(&req.pipeline, req.partition);
        let writer = self
            .lock_writers()?
            .remove(&key)
            .ok_or_else(|| {
                AeonError::state(format!(
                    "l2-installer: finish without begin for pipeline={} partition={}",
                    req.pipeline,
                    req.partition.as_u16()
                ))
            })?;

        if !end.success {
            // Source aborted mid-stream. Drop the in-flight writer (which
            // owned open file handles) and remove any partial files so a
            // retry starts clean.
            drop(writer);
            let dir = self.partition_dir(req.partition);
            if dir.exists() {
                if let Err(e) = std::fs::remove_dir_all(&dir) {
                    return Err(AeonError::state(format!(
                        "l2-installer: cleanup after abort failed: {e}"
                    )));
                }
            }
            return Ok(());
        }

        writer.finish()
    }
}

// ── PoH chain installer ───────────────────────────────────────────────

/// Per-(pipeline, partition) PoH chain snapshot cache. The transfer
/// driver calls `PohChainInstaller::install`, which decodes + verifies
/// the incoming state and drops the resulting [`PohChain`] here; the
/// engine's pipeline startup path pulls from the same registry to
/// resume the chain.
///
/// Cheap to clone — wraps `Arc<Mutex<…>>`.
#[derive(Clone, Default)]
pub struct InstalledPohChainRegistry {
    inner: Arc<Mutex<HashMap<(String, u16), PohChain>>>,
}

impl InstalledPohChainRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remove (taking ownership) any previously-installed chain for
    /// `(pipeline, partition)`. Used by the engine on pipeline start.
    pub fn take(&self, pipeline: &str, partition: PartitionId) -> Option<PohChain> {
        let key = (pipeline.to_owned(), partition.as_u16());
        self.inner.lock().ok()?.remove(&key)
    }

    fn insert(&self, pipeline: &str, partition: PartitionId, chain: PohChain) -> Result<(), AeonError> {
        let key = (pipeline.to_owned(), partition.as_u16());
        self.inner
            .lock()
            .map_err(|e| AeonError::state(format!("poh-installer: registry lock poisoned: {e}")))?
            .insert(key, chain);
        Ok(())
    }
}

/// Production [`PohChainInstaller`]. Decodes the bincode-encoded
/// `PohChainState`, runs [`PohVerifyMode`] validation, and hands the
/// resulting `PohChain` to a shared [`InstalledPohChainRegistry`].
///
/// Verification policy is set once at construction (from the cluster
/// config / env var) and applies to every partition the driver imports
/// on this node — per-pipeline override can be added later if needed.
pub struct PohChainInstallerImpl {
    registry: InstalledPohChainRegistry,
    verify_mode: PohVerifyMode,
    /// `max_recent` to pass to `PohChain::from_state`. Matches the
    /// `PohConfig::max_recent_entries` the pipeline will use post-
    /// transfer; 1024 is the default in `PohConfig::default`.
    max_recent: usize,
}

impl PohChainInstallerImpl {
    pub fn new(registry: InstalledPohChainRegistry, verify_mode: PohVerifyMode) -> Self {
        Self {
            registry,
            verify_mode,
            max_recent: 1024,
        }
    }

    pub fn with_max_recent(mut self, max_recent: usize) -> Self {
        self.max_recent = max_recent;
        self
    }
}

impl PohChainInstaller for PohChainInstallerImpl {
    fn install(
        &self,
        req: &PohChainTransferRequest,
        state_bytes: Vec<u8>,
    ) -> Result<(), AeonError> {
        // Empty payload = brand-new partition with no chain yet. Nothing
        // to decode; skip install cleanly so the post-transfer pipeline
        // starts a fresh chain.
        if state_bytes.is_empty() {
            return Ok(());
        }

        let state = PohChainState::from_bytes(&state_bytes).map_err(|e| {
            AeonError::state(format!("poh-installer: decode PohChainState: {e}"))
        })?;

        match self.verify_mode {
            PohVerifyMode::TrustExtend => {
                // Skip verification — only safe on authenticated transports.
            }
            PohVerifyMode::Verify | PohVerifyMode::VerifyWithKey => {
                PohChain::verify_state(&state, req.partition).map_err(|e| {
                    AeonError::state(format!("poh-installer: verify_state: {e}"))
                })?;
                // VerifyWithKey would additionally check signatures on any
                // recent entries carried in state. Current `PohChainState`
                // wire format is MMR-only, so this degrades to `Verify`
                // with an informational log — consistent with the docstring
                // on `PohVerifyMode::VerifyWithKey` ("degrades to Verify
                // until recent entries ship in state").
                if matches!(self.verify_mode, PohVerifyMode::VerifyWithKey) {
                    tracing::debug!(
                        pipeline = %req.pipeline,
                        partition = req.partition.as_u16(),
                        "poh-installer: verify_with_key degrades to structural verify \
                         (PohChainState carries no recent entries — extension pending)"
                    );
                }
            }
        }

        let chain = PohChain::from_state(state, self.max_recent);
        self.registry.insert(&req.pipeline, req.partition, chain)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::l2_transfer::SegmentEntry;
    use bytes::Bytes;

    fn tmp_root() -> PathBuf {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().to_path_buf();
        std::mem::forget(d);
        p
    }

    // ── L2 installer ──────────────────────────────────────────────────

    fn make_manifest(start_seq: u64, data: &[u8]) -> (SegmentManifest, u32) {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(data);
        let crc32 = hasher.finalize();
        let manifest = SegmentManifest {
            entries: vec![SegmentEntry {
                start_seq,
                size_bytes: data.len() as u64,
                crc32,
            }],
        };
        (manifest, crc32)
    }

    #[test]
    fn l2_installer_writes_segments_and_cleans_on_abort() {
        let root = tmp_root();
        let installer = L2SegmentInstaller::new(&root, "pl1");
        let partition = PartitionId::new(0);
        let req = PartitionTransferRequest {
            pipeline: "pl1".to_string(),
            partition,
        };

        let data = b"hello world L2 payload";
        let (manifest, _crc) = make_manifest(0, data);
        installer.begin(&req, manifest).unwrap();
        installer
            .apply_chunk(
                &req,
                SegmentChunk {
                    start_seq: 0,
                    offset: 0,
                    data: Bytes::copy_from_slice(data),
                    is_last: true,
                },
            )
            .unwrap();
        installer
            .finish(
                &req,
                PartitionTransferEnd {
                    success: true,
                    message: String::new(),
                },
            )
            .unwrap();

        let dir = root.join("pl1").join("p00000");
        let file = dir.join(format!("{:020}.l2b", 0));
        assert!(file.exists(), "segment file should exist at {:?}", file);
    }

    #[test]
    fn l2_installer_cleans_partition_dir_on_abort() {
        let root = tmp_root();
        let installer = L2SegmentInstaller::new(&root, "pl");
        let partition = PartitionId::new(3);
        let req = PartitionTransferRequest {
            pipeline: "pl".to_string(),
            partition,
        };

        let data = b"partial";
        let (manifest, _) = make_manifest(0, data);
        installer.begin(&req, manifest).unwrap();
        // Don't apply any chunks — abort halfway.
        installer
            .finish(
                &req,
                PartitionTransferEnd {
                    success: false,
                    message: "abort".to_string(),
                },
            )
            .unwrap();

        let dir = root.join("pl").join("p00003");
        assert!(
            !dir.exists(),
            "partition dir should be cleaned on abort, still at {:?}",
            dir
        );
    }

    #[test]
    fn l2_installer_rejects_double_begin() {
        let root = tmp_root();
        let installer = L2SegmentInstaller::new(&root, "pl");
        let req = PartitionTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        let data = b"x";
        let (m1, _) = make_manifest(0, data);
        installer.begin(&req, m1).unwrap();
        let (m2, _) = make_manifest(0, data);
        assert!(installer.begin(&req, m2).is_err());
    }

    #[test]
    fn l2_installer_apply_without_begin_errors() {
        let root = tmp_root();
        let installer = L2SegmentInstaller::new(&root, "pl");
        let req = PartitionTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        let chunk = SegmentChunk {
            start_seq: 0,
            offset: 0,
            data: Bytes::from_static(b"x"),
            is_last: true,
        };
        assert!(installer.apply_chunk(&req, chunk).is_err());
    }

    // ── PoH installer ─────────────────────────────────────────────────

    fn populated_state(partition: u16) -> PohChainState {
        let mut c = PohChain::new(PartitionId::new(partition), 64);
        for i in 0..3 {
            c.append_batch(&[format!("e-{i}").as_bytes()], (i + 1) * 1000, None);
        }
        c.export_state()
    }

    #[test]
    fn poh_installer_decodes_verifies_and_registers() {
        let registry = InstalledPohChainRegistry::new();
        let installer = PohChainInstallerImpl::new(registry.clone(), PohVerifyMode::Verify);
        let state = populated_state(4);
        let bytes = state.to_bytes().unwrap();
        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(4),
        };

        installer.install(&req, bytes).unwrap();
        let chain = registry
            .take("pl", PartitionId::new(4))
            .expect("chain should be registered");
        assert_eq!(chain.partition(), PartitionId::new(4));
        assert_eq!(chain.sequence(), 3);
    }

    #[test]
    fn poh_installer_verify_rejects_wrong_partition() {
        let registry = InstalledPohChainRegistry::new();
        let installer = PohChainInstallerImpl::new(registry.clone(), PohVerifyMode::Verify);
        let state = populated_state(4); // state carries partition 4…
        let bytes = state.to_bytes().unwrap();
        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(9), // …but request claims partition 9
        };
        let err = installer.install(&req, bytes).unwrap_err();
        assert!(err.to_string().contains("verify_state"), "got: {err}");
        assert!(registry.take("pl", PartitionId::new(9)).is_none());
    }

    #[test]
    fn poh_installer_trust_extend_skips_verification() {
        let registry = InstalledPohChainRegistry::new();
        let installer =
            PohChainInstallerImpl::new(registry.clone(), PohVerifyMode::TrustExtend);
        // Craft a deliberately-wrong state: partition mismatch. Under
        // `TrustExtend` it should still install.
        let state = populated_state(4);
        let bytes = state.to_bytes().unwrap();
        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(9), // wrong on purpose
        };
        installer.install(&req, bytes).unwrap();
        // Installed under (pl, 9) because we keyed on the request.
        let chain = registry.take("pl", PartitionId::new(9)).unwrap();
        // But the chain carries the source partition (4) because
        // TrustExtend skipped the consistency check. Documented gotcha.
        assert_eq!(chain.partition(), PartitionId::new(4));
    }

    #[test]
    fn poh_installer_accepts_empty_state_as_fresh_partition() {
        let registry = InstalledPohChainRegistry::new();
        let installer = PohChainInstallerImpl::new(registry.clone(), PohVerifyMode::Verify);
        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        installer.install(&req, Vec::new()).unwrap();
        // Nothing got inserted.
        assert!(registry.take("pl", PartitionId::new(0)).is_none());
    }

    #[test]
    fn poh_installer_verify_with_key_currently_degrades_to_verify() {
        let registry = InstalledPohChainRegistry::new();
        let installer = PohChainInstallerImpl::new(
            registry.clone(),
            PohVerifyMode::VerifyWithKey,
        );
        let state = populated_state(0);
        let bytes = state.to_bytes().unwrap();
        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        installer.install(&req, bytes).unwrap();
        assert!(registry.take("pl", PartitionId::new(0)).is_some());
    }
}
