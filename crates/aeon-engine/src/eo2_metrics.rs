//! EO-2 Phase P8 — observability metrics.
//!
//! Implements the metric set listed in ROADMAP P8 / design doc §15:
//!
//! | Metric                              | Type    | Labels                 |
//! |-------------------------------------|---------|------------------------|
//! | `aeon_l2_bytes`                     | gauge   | pipeline, partition    |
//! | `aeon_l2_segments`                  | gauge   | pipeline, partition    |
//! | `aeon_l2_gc_lag_seq`                | gauge   | pipeline, partition    |
//! | `aeon_l2_pressure`                  | gauge   | level                  |
//! | `aeon_sink_ack_seq`                 | gauge   | pipeline, sink         |
//! | `aeon_checkpoint_fallback_wal_total`| counter | (none)                 |
//! | `aeon_uuid_identity_collisions_total`| counter| (none)                 |
//!
//! The registry is a plain `Mutex<HashMap<K, AtomicU64>>` — updates happen
//! on coarse events (segment rollover, GC, fallback transition), never per
//! event, so lock contention is not a hot-path concern. The
//! `render_prometheus()` method emits standard text-format exposition;
//! `metrics_server::format_prometheus` will splice it in a follow-up
//! wiring pass.

use crate::eo2_backpressure::PressureLevel;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

// ── Registry ────────────────────────────────────────────────────────────

type PipelinePartition = (String, u16);
type PipelineSink = (String, String);

/// Shared EO-2 metrics registry. Clone is cheap — the whole thing lives
/// behind an `Arc<>` in practice; owners typically pass a `&Eo2Metrics`.
#[derive(Default)]
pub struct Eo2Metrics {
    l2_bytes: Mutex<BTreeMap<PipelinePartition, u64>>,
    l2_segments: Mutex<BTreeMap<PipelinePartition, u64>>,
    l2_gc_lag_seq: Mutex<BTreeMap<PipelinePartition, u64>>,
    l2_pressure: Mutex<BTreeMap<&'static str, u64>>,
    sink_ack_seq: Mutex<BTreeMap<PipelineSink, u64>>,
    checkpoint_fallback_wal_total: AtomicU64,
    uuid_identity_collisions_total: AtomicU64,
}

impl Eo2Metrics {
    pub fn new() -> Self {
        // Pre-populate pressure levels so `aeon_l2_pressure{level=...} 0`
        // lines always render — dashboards don't need to wait for the
        // first engagement to see the series.
        let mut pressure = BTreeMap::new();
        pressure.insert(PressureLevel::Partition.label(), 0);
        pressure.insert(PressureLevel::Pipeline.label(), 0);
        pressure.insert(PressureLevel::Node.label(), 0);
        Self {
            l2_pressure: Mutex::new(pressure),
            ..Self::default()
        }
    }

    // ── L2 bytes / segments / GC lag ────────────────────────────────────

    pub fn set_l2_bytes(&self, pipeline: &str, partition: u16, bytes: u64) {
        if let Ok(mut g) = self.l2_bytes.lock() {
            g.insert((pipeline.to_owned(), partition), bytes);
        }
    }

    pub fn set_l2_segments(&self, pipeline: &str, partition: u16, n: u64) {
        if let Ok(mut g) = self.l2_segments.lock() {
            g.insert((pipeline.to_owned(), partition), n);
        }
    }

    /// GC lag = `(head_seq - min_ack_seq)` — how many L2 entries are held
    /// past the commit frontier because a sink is behind.
    pub fn set_l2_gc_lag_seq(&self, pipeline: &str, partition: u16, lag: u64) {
        if let Ok(mut g) = self.l2_gc_lag_seq.lock() {
            g.insert((pipeline.to_owned(), partition), lag);
        }
    }

    // ── Pressure ────────────────────────────────────────────────────────

    /// Flip a pressure gauge for `level` to `engaged` (1) or clear (0).
    pub fn set_l2_pressure(&self, level: PressureLevel, engaged: bool) {
        if let Ok(mut g) = self.l2_pressure.lock() {
            g.insert(level.label(), if engaged { 1 } else { 0 });
        }
    }

    // ── Sink ack frontier ───────────────────────────────────────────────

    pub fn set_sink_ack_seq(&self, pipeline: &str, sink: &str, seq: u64) {
        if let Ok(mut g) = self.sink_ack_seq.lock() {
            g.insert((pipeline.to_owned(), sink.to_owned()), seq);
        }
    }

    // ── Counters ────────────────────────────────────────────────────────

    /// Increment when `FallbackCheckpointStore` engages the WAL path.
    pub fn inc_checkpoint_fallback_wal(&self) {
        self.checkpoint_fallback_wal_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Increment when a pull-source deterministic UUIDv7 collides with
    /// another event's id in the dedup window.
    pub fn inc_uuid_identity_collision(&self) {
        self.uuid_identity_collisions_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn checkpoint_fallback_wal_total(&self) -> u64 {
        self.checkpoint_fallback_wal_total.load(Ordering::Relaxed)
    }

    pub fn uuid_identity_collisions_total(&self) -> u64 {
        self.uuid_identity_collisions_total.load(Ordering::Relaxed)
    }

    // ── Prometheus text rendering ───────────────────────────────────────

    pub fn render_prometheus(&self) -> String {
        let mut s = String::with_capacity(1024);

        render_pipeline_partition_gauge(
            &mut s,
            "aeon_l2_bytes",
            "L2 event-body store bytes",
            &self.l2_bytes,
        );
        render_pipeline_partition_gauge(
            &mut s,
            "aeon_l2_segments",
            "L2 segment count",
            &self.l2_segments,
        );
        render_pipeline_partition_gauge(
            &mut s,
            "aeon_l2_gc_lag_seq",
            "L2 sequences held past commit frontier",
            &self.l2_gc_lag_seq,
        );

        s.push_str("# HELP aeon_l2_pressure L2 pressure engaged per level (0/1)\n");
        s.push_str("# TYPE aeon_l2_pressure gauge\n");
        if let Ok(g) = self.l2_pressure.lock() {
            for (level, v) in g.iter() {
                let _ = writeln!(s, "aeon_l2_pressure{{level=\"{level}\"}} {v}");
            }
        }

        s.push_str(
            "# HELP aeon_sink_ack_seq Highest contiguously-acked delivery sequence per sink\n",
        );
        s.push_str("# TYPE aeon_sink_ack_seq gauge\n");
        if let Ok(g) = self.sink_ack_seq.lock() {
            for ((pipeline, sink), v) in g.iter() {
                let _ = writeln!(
                    s,
                    "aeon_sink_ack_seq{{pipeline=\"{pipeline}\",sink=\"{sink}\"}} {v}"
                );
            }
        }

        let fb = self.checkpoint_fallback_wal_total();
        let _ = writeln!(
            s,
            "# HELP aeon_checkpoint_fallback_wal_total L3→WAL fallback engagements\n\
             # TYPE aeon_checkpoint_fallback_wal_total counter\n\
             aeon_checkpoint_fallback_wal_total {fb}"
        );

        let cx = self.uuid_identity_collisions_total();
        let _ = writeln!(
            s,
            "# HELP aeon_uuid_identity_collisions_total Pull-source UUIDv7 collisions within dedup window\n\
             # TYPE aeon_uuid_identity_collisions_total counter\n\
             aeon_uuid_identity_collisions_total {cx}"
        );

        s
    }
}

fn render_pipeline_partition_gauge(
    out: &mut String,
    name: &str,
    help: &str,
    map: &Mutex<BTreeMap<PipelinePartition, u64>>,
) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    if let Ok(g) = map.lock() {
        for ((pipeline, partition), v) in g.iter() {
            let _ = writeln!(
                out,
                "{name}{{pipeline=\"{pipeline}\",partition=\"{partition}\"}} {v}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_levels_prepopulate_to_zero() {
        let m = Eo2Metrics::new();
        let s = m.render_prometheus();
        assert!(s.contains(r#"aeon_l2_pressure{level="partition"} 0"#));
        assert!(s.contains(r#"aeon_l2_pressure{level="pipeline"} 0"#));
        assert!(s.contains(r#"aeon_l2_pressure{level="node"} 0"#));
    }

    #[test]
    fn pressure_flip_sets_one_and_clears_back() {
        let m = Eo2Metrics::new();
        m.set_l2_pressure(PressureLevel::Pipeline, true);
        assert!(
            m.render_prometheus()
                .contains(r#"aeon_l2_pressure{level="pipeline"} 1"#)
        );
        m.set_l2_pressure(PressureLevel::Pipeline, false);
        assert!(
            m.render_prometheus()
                .contains(r#"aeon_l2_pressure{level="pipeline"} 0"#)
        );
    }

    #[test]
    fn l2_bytes_renders_with_labels() {
        let m = Eo2Metrics::new();
        m.set_l2_bytes("orders", 3, 4096);
        let s = m.render_prometheus();
        assert!(s.contains(r#"aeon_l2_bytes{pipeline="orders",partition="3"} 4096"#));
    }

    #[test]
    fn l2_segments_and_gc_lag_render() {
        let m = Eo2Metrics::new();
        m.set_l2_segments("p", 0, 5);
        m.set_l2_gc_lag_seq("p", 0, 42);
        let s = m.render_prometheus();
        assert!(s.contains(r#"aeon_l2_segments{pipeline="p",partition="0"} 5"#));
        assert!(s.contains(r#"aeon_l2_gc_lag_seq{pipeline="p",partition="0"} 42"#));
    }

    #[test]
    fn set_l2_bytes_overwrites_same_key() {
        let m = Eo2Metrics::new();
        m.set_l2_bytes("p", 0, 100);
        m.set_l2_bytes("p", 0, 200);
        let s = m.render_prometheus();
        assert!(s.contains(r#"aeon_l2_bytes{pipeline="p",partition="0"} 200"#));
        assert!(!s.contains(" 100\n"));
    }

    #[test]
    fn sink_ack_seq_gauges_per_sink() {
        let m = Eo2Metrics::new();
        m.set_sink_ack_seq("orders", "kafka-primary", 99);
        m.set_sink_ack_seq("orders", "webhook-audit", 40);
        let s = m.render_prometheus();
        assert!(s.contains(r#"aeon_sink_ack_seq{pipeline="orders",sink="kafka-primary"} 99"#));
        assert!(s.contains(r#"aeon_sink_ack_seq{pipeline="orders",sink="webhook-audit"} 40"#));
    }

    #[test]
    fn fallback_and_collision_counters_are_monotonic() {
        let m = Eo2Metrics::new();
        m.inc_checkpoint_fallback_wal();
        m.inc_checkpoint_fallback_wal();
        m.inc_uuid_identity_collision();
        assert_eq!(m.checkpoint_fallback_wal_total(), 2);
        assert_eq!(m.uuid_identity_collisions_total(), 1);
    }

    #[test]
    fn counter_lines_use_total_suffix_and_type_counter() {
        let m = Eo2Metrics::new();
        m.inc_checkpoint_fallback_wal();
        let s = m.render_prometheus();
        assert!(s.contains("# TYPE aeon_checkpoint_fallback_wal_total counter"));
        assert!(s.contains("aeon_checkpoint_fallback_wal_total 1"));
        assert!(s.contains("# TYPE aeon_uuid_identity_collisions_total counter"));
    }

    #[test]
    fn all_gauges_have_help_and_type_headers() {
        let m = Eo2Metrics::new();
        let s = m.render_prometheus();
        for name in [
            "aeon_l2_bytes",
            "aeon_l2_segments",
            "aeon_l2_gc_lag_seq",
            "aeon_l2_pressure",
            "aeon_sink_ack_seq",
        ] {
            assert!(
                s.contains(&format!("# HELP {name} ")),
                "missing HELP for {name}"
            );
            assert!(
                s.contains(&format!("# TYPE {name} gauge")),
                "missing TYPE gauge for {name}"
            );
        }
    }

    #[test]
    fn output_rows_are_deterministic_sorted_by_key() {
        // BTreeMap iteration is ordered — rendered lines are stable across runs.
        let m = Eo2Metrics::new();
        m.set_l2_bytes("b", 0, 1);
        m.set_l2_bytes("a", 1, 2);
        m.set_l2_bytes("a", 0, 3);
        let s = m.render_prometheus();
        let a0 = s.find("pipeline=\"a\",partition=\"0\"").unwrap();
        let a1 = s.find("pipeline=\"a\",partition=\"1\"").unwrap();
        let b0 = s.find("pipeline=\"b\",partition=\"0\"").unwrap();
        assert!(a0 < a1 && a1 < b0);
    }

    #[test]
    fn empty_gauge_maps_still_emit_help_and_type() {
        let m = Eo2Metrics::new();
        let s = m.render_prometheus();
        assert!(s.contains("# TYPE aeon_l2_bytes gauge"));
    }
}
