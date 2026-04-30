//! CL-6c.4 — engine-side providers consumed by the cluster's QUIC
//! source-side dispatch.
//!
//! Two production impls ship here:
//!
//! * [`L2SegmentTransferProvider`] — implements
//!   [`aeon_cluster::transport::partition_transfer::PartitionTransferProvider`].
//!   Walks the per-(pipeline, partition) L2 body directory using the
//!   same layout as [`crate::eo2::PipelineL2Registry`] /
//!   [`crate::partition_install::L2SegmentInstaller`] and streams
//!   segments via [`crate::l2_transfer::SegmentReader`].
//!
//! * [`PohChainExportProvider`] — implements
//!   [`aeon_cluster::transport::poh_transfer::PohChainProvider`]. Reads
//!   the live `Arc<Mutex<PohChain>>` for `(pipeline, partition)` from
//!   the engine's [`crate::partition_install::LivePohChainRegistry`]
//!   and serializes its current state via
//!   `PohChainState::to_bytes`.
//!
//! Both are cheap `Arc`-cloneable handles. They are installed into the
//! cluster node's `SourceProviderSlots` post-bootstrap by the
//! `aeon-cli` `install_partition_transfer_driver` path, paired with
//! [`crate::engine_cutover::EngineCutoverCoordinator`] to form the
//! source-side half of the CL-6 handover (source freezes via the
//! coordinator, target pulls L2 + PoH via these providers).

#![cfg(all(feature = "cluster", feature = "processor-auth"))]

use std::pin::Pin;
use std::sync::Arc;

use aeon_cluster::transport::partition_transfer::{
    ChunkIter, PartitionTransferProvider,
};
use aeon_cluster::transport::poh_transfer::PohChainProvider;
use aeon_cluster::types::{PartitionTransferRequest, PohChainTransferRequest};
use aeon_types::l2_transfer::{DEFAULT_CHUNK_BYTES, SegmentChunk, SegmentManifest};
use aeon_types::AeonError;

use crate::eo2::PipelineL2Registry;
use crate::l2_transfer::{SegmentReader, read_manifest};
use crate::partition_install::LivePohChainRegistry;

// ── L2 segment provider ───────────────────────────────────────────────

/// Production [`PartitionTransferProvider`] backed by a
/// [`PipelineL2Registry`]. The registry's `partition_dir` accessor
/// gives a deterministic on-disk path for any `(pipeline, partition)`
/// the source node has ever opened — `serve` walks that directory's
/// `.l2b` files via [`SegmentReader`].
///
/// Multi-pipeline deployments share one provider per node — the
/// `(pipeline, partition)` key is read from the request itself, not
/// captured at construction.
pub struct L2SegmentTransferProvider {
    registry: PipelineL2Registry,
}

impl L2SegmentTransferProvider {
    pub fn new(registry: PipelineL2Registry) -> Self {
        Self { registry }
    }
}

impl PartitionTransferProvider for L2SegmentTransferProvider {
    fn serve(
        &self,
        req: &PartitionTransferRequest,
    ) -> Result<(SegmentManifest, ChunkIter<'static>), AeonError> {
        let dir = self
            .registry
            .partition_dir(&req.pipeline, req.partition)
            .ok_or_else(|| {
                AeonError::state(
                    "l2-export: PipelineL2Registry constructed without a root — \
                     cannot serve partition transfer",
                )
            })?;

        let manifest = read_manifest(&dir)?;
        let entries = manifest.entries.clone();
        let dir_owned = dir;
        let mut segs = entries.into_iter();
        let mut current: Option<SegmentReader> = None;

        let chunks: ChunkIter<'static> = Box::new(move || -> Result<Option<SegmentChunk>, AeonError> {
            loop {
                if let Some(reader) = current.as_mut() {
                    match reader.next_chunk()? {
                        Some(c) => return Ok(Some(c)),
                        None => current = None,
                    }
                }
                let Some(entry) = segs.next() else {
                    return Ok(None);
                };
                let reader = SegmentReader::open(
                    &dir_owned,
                    entry.start_seq,
                    DEFAULT_CHUNK_BYTES,
                )?;
                current = Some(reader);
            }
        });

        Ok((manifest, chunks))
    }
}

// ── PoH chain export provider ─────────────────────────────────────────

/// Production [`PohChainProvider`] backed by a [`LivePohChainRegistry`].
/// Looks up the live `Arc<Mutex<PohChain>>` for `(pipeline, partition)`,
/// awaits the async lock briefly (the chain is uncontended once the
/// WriteGate has flipped to `FreezeRequested`/`Frozen`), and exports
/// the current `PohChainState` as bincode bytes.
///
/// When no chain is registered for `(pipeline, partition)`, returns
/// `Ok(Vec::new())` — the sentinel the partition driver interprets as
/// "this pipeline has no PoH leg, skip install". Before
/// [2026-04-25] this returned `AeonError::State`, which aborted the
/// whole partition transfer for every pipeline that did not opt into
/// PoH (which is all of them today — no fixture enables PoH yet).
/// Breaking-change surface is narrow: only the partition-transfer
/// wire contract grows an "empty response means skip" rule.
pub struct PohChainExportProvider {
    registry: Arc<LivePohChainRegistry>,
}

impl PohChainExportProvider {
    pub fn new(registry: Arc<LivePohChainRegistry>) -> Self {
        Self { registry }
    }
}

impl PohChainProvider for PohChainExportProvider {
    fn export_state<'a>(
        &'a self,
        req: &'a PohChainTransferRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, AeonError>> + Send + 'a>>
    {
        Box::pin(async move {
            let Some(chain) = self.registry.get(&req.pipeline, req.partition) else {
                // Pipeline has no PoH leg registered for this partition.
                // Signal "skip the install step" to the target driver
                // with an empty response rather than aborting the whole
                // partition transfer. See struct doc for the rationale.
                tracing::debug!(
                    pipeline = %req.pipeline,
                    partition = req.partition.as_u16(),
                    "poh-export: no live chain, returning empty-response sentinel"
                );
                return Ok(Vec::new());
            };
            let state = chain.lock().await.export_state();
            state.to_bytes().map_err(|e| AeonError::Serialization {
                message: format!("poh-export: serialize PohChainState: {e}"),
                source: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::L2BodyStoreConfig;
    use crate::l2_body::{L2BodyConfig, L2BodyStore};
    use aeon_crypto::poh::PohChain;
    use aeon_types::PartitionId;
    use bytes::Bytes;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Mutex as TokioMutex;

    fn tmp_root() -> PathBuf {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().to_path_buf();
        std::mem::forget(d);
        p
    }

    fn write_one_segment(root: &std::path::Path, pipeline: &str, partition: PartitionId) -> Vec<u8> {
        use aeon_types::Event;
        let dir = root.join(pipeline).join(format!("p{:05}", partition.as_u16()));
        std::fs::create_dir_all(&dir).unwrap();
        let store_dir = dir.clone();
        let store = L2BodyStore::open(
            store_dir,
            L2BodyConfig {
                segment_bytes: 4096,
                kek: None,
                gc_min_hold: std::time::Duration::ZERO,
            },
        )
        .unwrap();
        let store = Arc::new(StdMutex::new(store));
        for i in 0..3u32 {
            let mut g = store.lock().unwrap();
            let event = Event::new(
                uuid::Uuid::nil(),
                i as i64,
                Arc::from("t"),
                partition,
                Bytes::from(format!("body-{i}").into_bytes()),
            );
            g.append(&event).unwrap();
        }
        // Read what we wrote back, so the test can compare against the on-wire bytes.
        let manifest = read_manifest(&dir).unwrap();
        assert!(!manifest.entries.is_empty());
        let entry = &manifest.entries[0];
        std::fs::read(dir.join(format!("{:020}.l2b", entry.start_seq))).unwrap()
    }

    #[test]
    fn l2_provider_streams_full_file_in_chunks() {
        let root = tmp_root();
        let pipeline = "pl";
        let partition = PartitionId::new(2);
        let expected_bytes = write_one_segment(&root, pipeline, partition);

        let registry = PipelineL2Registry::new(L2BodyStoreConfig {
            root: Some(root.clone()),
            segment_bytes: 4096,
        });
        let provider = L2SegmentTransferProvider::new(registry);

        let req = PartitionTransferRequest {
            pipeline: pipeline.to_string(),
            partition,
        };
        let (manifest, mut chunks) = provider.serve(&req).unwrap();
        assert_eq!(manifest.entries.len(), 1);

        let mut got = Vec::new();
        while let Some(chunk) = chunks().unwrap() {
            assert_eq!(chunk.start_seq, manifest.entries[0].start_seq);
            assert_eq!(chunk.offset as usize, got.len());
            got.extend_from_slice(&chunk.data);
            if chunk.is_last {
                assert_eq!(got.len(), expected_bytes.len());
            }
        }
        assert_eq!(got, expected_bytes);
    }

    #[test]
    fn l2_provider_empty_partition_returns_empty_manifest() {
        let root = tmp_root();
        let registry = PipelineL2Registry::new(L2BodyStoreConfig {
            root: Some(root),
            segment_bytes: 4096,
        });
        let provider = L2SegmentTransferProvider::new(registry);

        let req = PartitionTransferRequest {
            pipeline: "missing".to_string(),
            partition: PartitionId::new(0),
        };
        let (manifest, mut chunks) = provider.serve(&req).unwrap();
        assert!(manifest.entries.is_empty());
        assert!(chunks().unwrap().is_none());
    }

    #[test]
    fn l2_provider_rejects_registry_without_root() {
        let registry = PipelineL2Registry::default();
        let provider = L2SegmentTransferProvider::new(registry);
        let req = PartitionTransferRequest {
            pipeline: "x".to_string(),
            partition: PartitionId::new(0),
        };
        let err = match provider.serve(&req) {
            Ok(_) => panic!("expected error from rootless registry"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("without a root"),
            "got: {err}",
        );
    }

    #[tokio::test]
    async fn poh_export_returns_serialized_state_for_registered_chain() {
        let registry = Arc::new(LivePohChainRegistry::new());
        let mut chain = PohChain::new(PartitionId::new(5), 64);
        chain.append_batch(&[b"e0"], 1, None);
        chain.append_batch(&[b"e1"], 2, None);
        let live: Arc<TokioMutex<PohChain>> = Arc::new(TokioMutex::new(chain));
        registry
            .register("pl", PartitionId::new(5), Arc::clone(&live))
            .unwrap();

        let provider = PohChainExportProvider::new(Arc::clone(&registry));
        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(5),
        };
        let bytes = provider.export_state(&req).await.unwrap();
        // Decode back and assert sequence preserved.
        let state = aeon_crypto::poh::PohChainState::from_bytes(&bytes).unwrap();
        assert_eq!(state.partition, PartitionId::new(5));
        assert_eq!(state.sequence, 2);
    }

    #[tokio::test]
    async fn poh_export_unregistered_partition_returns_empty_sentinel() {
        // 2026-04-25: flipped from Err("no live chain") to Ok(empty
        // bytes) so partition transfer doesn't abort on pipelines
        // without a PoH leg. The partition-driver interprets an empty
        // response as "skip the install step" — see
        // `aeon_cluster::partition_driver::drive_one` where
        // `poh_bytes.is_empty()` short-circuits the installer call.
        let registry = Arc::new(LivePohChainRegistry::new());
        let provider = PohChainExportProvider::new(Arc::clone(&registry));
        let req = PohChainTransferRequest {
            pipeline: "pl".to_string(),
            partition: PartitionId::new(0),
        };
        let bytes = provider.export_state(&req).await.unwrap();
        assert!(
            bytes.is_empty(),
            "unregistered partition must yield empty-bytes sentinel, got {} bytes",
            bytes.len()
        );
    }
}
