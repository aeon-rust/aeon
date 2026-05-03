//! CL-6d.2 — Per-partition transfer progress metrics.
//!
//! A cluster-wide shared gauge set, one pair of counters
//! (`bytes_transferred`, `bytes_total`) per `(pipeline, partition, role)`
//! where role is either the `source` (emitting side) or the `target`
//! (receiving side). The same transfer is visible on both nodes, each
//! recording its own half — operators can watch either end or diff them
//! to catch asymmetries.
//!
//! The engine owns one `PartitionTransferMetrics` instance and plumbs it
//! into both the source-side provider (`PartitionTransferProvider::metrics`)
//! and the target-side orchestrator (`drive_partition_transfer`). The
//! cluster crate never reads the value — it only writes. The engine's
//! `/metrics` handler renders the Prometheus text via
//! [`PartitionTransferMetrics::render_prometheus`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aeon_types::PartitionId;
use dashmap::DashMap;

/// Which side of a partition transfer is emitting the sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferRole {
    /// The node sourcing chunks onto the wire.
    Source,
    /// The node receiving + persisting chunks.
    Target,
}

impl TransferRole {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Target => "target",
        }
    }
}

#[derive(Debug, Default)]
struct Entry {
    transferred: AtomicU64,
    total: AtomicU64,
}

/// Registry of in-flight partition transfers indexed by pipeline +
/// partition + role.
#[derive(Debug, Default)]
pub struct PartitionTransferMetrics {
    entries: DashMap<(String, PartitionId, TransferRole), Arc<Entry>>,
}

impl PartitionTransferMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the expected total byte count for a transfer (typically
    /// sourced from `SegmentManifest::total_bytes()` on the manifest
    /// frame). Subsequent calls overwrite — callers should only call
    /// this once per transfer.
    pub fn record_total(
        &self,
        pipeline: &str,
        partition: PartitionId,
        role: TransferRole,
        total: u64,
    ) {
        let entry = self.slot(pipeline, partition, role);
        entry.total.store(total, Ordering::Relaxed);
    }

    /// Accumulate `bytes` into the transferred gauge. Safe to call from
    /// the hot path — a single relaxed fetch-add per chunk.
    pub fn add_transferred(
        &self,
        pipeline: &str,
        partition: PartitionId,
        role: TransferRole,
        bytes: u64,
    ) {
        let entry = self.slot(pipeline, partition, role);
        entry.transferred.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Drop the bookkeeping for a finished transfer. Called from the
    /// orchestrator once the tracker reaches a terminal state, so stale
    /// gauges don't pile up after a transfer completes.
    pub fn clear(&self, pipeline: &str, partition: PartitionId, role: TransferRole) {
        self.entries
            .remove(&(pipeline.to_string(), partition, role));
    }

    /// Read a snapshot of `(transferred, total)` for a transfer. Used by
    /// tests + the engine's `/metrics` renderer.
    pub fn snapshot(
        &self,
        pipeline: &str,
        partition: PartitionId,
        role: TransferRole,
    ) -> Option<(u64, u64)> {
        self.entries
            .get(&(pipeline.to_string(), partition, role))
            .map(|e| {
                (
                    e.transferred.load(Ordering::Relaxed),
                    e.total.load(Ordering::Relaxed),
                )
            })
    }

    /// Render both gauges in Prometheus text exposition format, sorted
    /// deterministically so equivalent states produce byte-identical
    /// output (easier to test, easier to diff).
    pub fn render_prometheus(&self) -> String {
        let mut rows: Vec<(String, PartitionId, TransferRole, u64, u64)> = self
            .entries
            .iter()
            .map(|e| {
                let (pipeline, partition, role) = e.key();
                (
                    pipeline.clone(),
                    *partition,
                    *role,
                    e.transferred.load(Ordering::Relaxed),
                    e.total.load(Ordering::Relaxed),
                )
            })
            .collect();
        rows.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.cmp(&b.1))
                .then(a.2.as_str().cmp(b.2.as_str()))
        });

        let mut out = String::with_capacity(256 + rows.len() * 128);
        out.push_str(
            "# HELP aeon_partition_transfer_bytes_transferred Bytes moved so far on an in-flight partition transfer\n",
        );
        out.push_str("# TYPE aeon_partition_transfer_bytes_transferred gauge\n");
        for (pipeline, partition, role, transferred, _) in &rows {
            out.push_str(&format!(
                "aeon_partition_transfer_bytes_transferred{{pipeline=\"{}\",partition=\"{}\",role=\"{}\"}} {}\n",
                pipeline,
                partition.as_u16(),
                role.as_str(),
                transferred,
            ));
        }
        out.push_str(
            "# HELP aeon_partition_transfer_bytes_total Expected total bytes for an in-flight partition transfer\n",
        );
        out.push_str("# TYPE aeon_partition_transfer_bytes_total gauge\n");
        for (pipeline, partition, role, _, total) in &rows {
            out.push_str(&format!(
                "aeon_partition_transfer_bytes_total{{pipeline=\"{}\",partition=\"{}\",role=\"{}\"}} {}\n",
                pipeline,
                partition.as_u16(),
                role.as_str(),
                total,
            ));
        }
        out
    }

    fn slot(&self, pipeline: &str, partition: PartitionId, role: TransferRole) -> Arc<Entry> {
        self.entries
            .entry((pipeline.to_string(), partition, role))
            .or_insert_with(|| Arc::new(Entry::default()))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_transferred_accumulates() {
        let m = PartitionTransferMetrics::new();
        let p = PartitionId::new(3);
        m.add_transferred("pl", p, TransferRole::Source, 100);
        m.add_transferred("pl", p, TransferRole::Source, 50);
        assert_eq!(
            m.snapshot("pl", p, TransferRole::Source),
            Some((150, 0)),
            "two add_transferred calls must sum"
        );
    }

    #[test]
    fn source_and_target_tracked_separately() {
        let m = PartitionTransferMetrics::new();
        let p = PartitionId::new(7);
        m.record_total("pl", p, TransferRole::Source, 1_000);
        m.record_total("pl", p, TransferRole::Target, 1_000);
        m.add_transferred("pl", p, TransferRole::Source, 400);
        m.add_transferred("pl", p, TransferRole::Target, 200);
        assert_eq!(
            m.snapshot("pl", p, TransferRole::Source),
            Some((400, 1_000))
        );
        assert_eq!(
            m.snapshot("pl", p, TransferRole::Target),
            Some((200, 1_000))
        );
    }

    #[test]
    fn clear_drops_the_entry() {
        let m = PartitionTransferMetrics::new();
        let p = PartitionId::new(1);
        m.add_transferred("pl", p, TransferRole::Source, 42);
        assert!(m.snapshot("pl", p, TransferRole::Source).is_some());
        m.clear("pl", p, TransferRole::Source);
        assert!(m.snapshot("pl", p, TransferRole::Source).is_none());
    }

    #[test]
    fn render_prometheus_is_deterministic_and_well_formed() {
        let m = PartitionTransferMetrics::new();
        m.record_total("a", PartitionId::new(2), TransferRole::Source, 1_000);
        m.add_transferred("a", PartitionId::new(2), TransferRole::Source, 100);
        m.record_total("a", PartitionId::new(2), TransferRole::Target, 1_000);
        m.add_transferred("a", PartitionId::new(2), TransferRole::Target, 80);
        m.record_total("b", PartitionId::new(5), TransferRole::Source, 500);
        m.add_transferred("b", PartitionId::new(5), TransferRole::Source, 250);

        let text = m.render_prometheus();

        assert!(text.contains("# TYPE aeon_partition_transfer_bytes_transferred gauge"));
        assert!(text.contains("# TYPE aeon_partition_transfer_bytes_total gauge"));
        assert!(text.contains(
            "aeon_partition_transfer_bytes_transferred{pipeline=\"a\",partition=\"2\",role=\"source\"} 100"
        ));
        assert!(text.contains(
            "aeon_partition_transfer_bytes_transferred{pipeline=\"a\",partition=\"2\",role=\"target\"} 80"
        ));
        assert!(text.contains(
            "aeon_partition_transfer_bytes_total{pipeline=\"b\",partition=\"5\",role=\"source\"} 500"
        ));

        // Rendering twice must be byte-identical — DashMap iteration is
        // unordered, so the renderer sorts internally.
        assert_eq!(text, m.render_prometheus());
    }

    #[test]
    fn empty_registry_renders_only_headers() {
        let m = PartitionTransferMetrics::new();
        let text = m.render_prometheus();
        assert!(text.contains("# TYPE aeon_partition_transfer_bytes_transferred gauge"));
        assert!(text.contains("# TYPE aeon_partition_transfer_bytes_total gauge"));
        // No sample rows.
        assert!(!text.contains("{pipeline="));
    }
}
