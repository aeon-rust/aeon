//! EO-2 Phase P7 — L2 capacity hierarchy and backpressure escalation.
//!
//! Implements §10 of `docs/EO-2-DURABILITY-DESIGN.md`. Three nested caps,
//! each with its own remedy:
//!
//! ```text
//!   ┌── Node global cap  ──── throttle all pipelines  (level = "node")
//!   │   ┌── Pipeline cap  ─── throttle all partitions (level = "pipeline")
//!   │   │   ┌── Partition ──  throttle one source     (level = "partition")
//! ```
//!
//! This module owns only the decision logic + byte accounting. Source-kind
//! remedies (push 503, pull pause, poll skip) live in the connector side —
//! this module hands them a [`BackpressureDecision`] and the connector
//! decides how to enact it.
//!
//! Single-pipeline deployments leave every cap unset → decisions always
//! return [`BackpressureDecision::None`].

use aeon_types::traits::SourceKind;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ── Limits ─────────────────────────────────────────────────────────────

/// Three-level L2 byte capacity limits. Any level can be `None` (unset).
///
/// `partition_soft_target` is derived, not configured: `pipeline_cap /
/// partitions`. Stored explicitly rather than recomputed on every tick so
/// the derivation happens once at pipeline start.
#[derive(Debug, Clone, Copy)]
pub struct CapacityLimits {
    pub node_max_bytes: Option<u64>,
    pub pipeline_max_bytes: Option<u64>,
    pub partition_soft_target: Option<u64>,
}

impl CapacityLimits {
    /// Build from raw config, deriving the per-partition soft target from
    /// the pipeline cap. Returns `partition_soft_target = None` when the
    /// pipeline cap is unset — bare node cap alone cannot safely derive a
    /// per-partition floor because pipelines share the node cap.
    pub fn from_config(
        node_max_bytes: Option<u64>,
        pipeline_max_bytes: Option<u64>,
        partitions: u16,
    ) -> Self {
        let partition_soft_target = match (pipeline_max_bytes, partitions) {
            (Some(cap), p) if p > 0 => Some(cap / p as u64),
            _ => None,
        };
        Self {
            node_max_bytes,
            pipeline_max_bytes,
            partition_soft_target,
        }
    }

    /// All caps unset — pipeline runs without L2 pressure accounting.
    pub fn unlimited() -> Self {
        Self {
            node_max_bytes: None,
            pipeline_max_bytes: None,
            partition_soft_target: None,
        }
    }
}

// ── Decision ────────────────────────────────────────────────────────────

/// Level at which a pressure limit has been breached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PressureLevel {
    /// Partition exceeded its soft target — slow only this partition's source.
    Partition,
    /// Pipeline approached its cap — throttle all partitions in the pipeline.
    Pipeline,
    /// Node approached its global cap — throttle all pipelines on this node.
    Node,
}

impl PressureLevel {
    /// Label for `aeon_l2_pressure{level=...}` metric and logs.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Partition => "partition",
            Self::Pipeline => "pipeline",
            Self::Node => "node",
        }
    }
}

/// Remedy the source side must enact, tailored to source kind.
///
/// Returned by [`PipelineCapacity::decide`]; the runtime dispatches to the
/// right connector-level control (Phase 3 `PushBuffer` protocol, pause
/// polling, skip poll tick, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureDecision {
    /// No caps breached — proceed normally.
    None,
    /// Push source: engage Phase 3 protocol-level flow control
    /// (reject 503, `basic.cancel`, WS backpressure frame, etc.).
    PushReject { level: PressureLevel },
    /// Pull source: stop polling the broker; broker queues up.
    PullPause { level: PressureLevel },
    /// Poll source: widen the poll interval or skip this tick.
    PollSkip { level: PressureLevel },
}

impl BackpressureDecision {
    /// Map `(source_kind, level)` to the source-kind-specific remedy.
    pub fn for_kind(kind: SourceKind, level: PressureLevel) -> Self {
        match kind {
            SourceKind::Push => Self::PushReject { level },
            SourceKind::Pull => Self::PullPause { level },
            SourceKind::Poll => Self::PollSkip { level },
        }
    }

    /// Extract the pressure level if any, for metric emission.
    pub fn level(self) -> Option<PressureLevel> {
        match self {
            Self::None => None,
            Self::PushReject { level } | Self::PullPause { level } | Self::PollSkip { level } => {
                Some(level)
            }
        }
    }

    pub fn is_engaged(self) -> bool {
        !matches!(self, Self::None)
    }
}

// ── Node accountant ─────────────────────────────────────────────────────

/// Node-wide L2 byte counter, shared across all pipelines on the node.
/// Cheap `Arc<Mutex<u64>>` — updates are coarse (on segment rollover /
/// GC sweep), not per-event.
#[derive(Clone, Default)]
pub struct NodeCapacity {
    inner: Arc<Mutex<NodeCapacityInner>>,
}

#[derive(Default)]
struct NodeCapacityInner {
    bytes: u64,
    cap: Option<u64>,
}

impl NodeCapacity {
    pub fn new(cap: Option<u64>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(NodeCapacityInner { bytes: 0, cap })),
        }
    }

    /// Add `delta` bytes (positive on segment create, negative on GC).
    pub fn adjust(&self, delta: i64) {
        if let Ok(mut g) = self.inner.lock() {
            if delta >= 0 {
                g.bytes = g.bytes.saturating_add(delta as u64);
            } else {
                g.bytes = g.bytes.saturating_sub((-delta) as u64);
            }
        }
    }

    pub fn bytes(&self) -> u64 {
        self.inner.lock().map(|g| g.bytes).unwrap_or(0)
    }

    pub fn cap(&self) -> Option<u64> {
        self.inner.lock().ok().and_then(|g| g.cap)
    }

    /// Node cap breached — crossed ≥ 95 % of the cap.
    pub fn is_pressured(&self) -> bool {
        let Ok(g) = self.inner.lock() else {
            return false;
        };
        match g.cap {
            Some(cap) if cap > 0 => g.bytes * 20 >= cap * 19,
            _ => false,
        }
    }
}

// ── Pipeline accountant ─────────────────────────────────────────────────

/// Per-pipeline L2 byte accountant. Tracks per-partition bytes, owns the
/// capacity limits, and returns escalation decisions.
#[derive(Clone)]
pub struct PipelineCapacity {
    inner: Arc<Mutex<PipelineCapacityInner>>,
    node: NodeCapacity,
    caps: CapacityLimits,
}

struct PipelineCapacityInner {
    /// Per-partition L2 bytes. Sparse — only populated partitions appear.
    partitions: HashMap<u16, u64>,
}

impl PipelineCapacity {
    pub fn new(caps: CapacityLimits, node: NodeCapacity) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PipelineCapacityInner {
                partitions: HashMap::new(),
            })),
            node,
            caps,
        }
    }

    /// Adjust partition byte count. Positive on L2 append/rollover,
    /// negative on GC sweep. Delta is mirrored into the node counter.
    pub fn adjust(&self, partition: u16, delta: i64) {
        if let Ok(mut g) = self.inner.lock() {
            let entry = g.partitions.entry(partition).or_insert(0);
            if delta >= 0 {
                *entry = entry.saturating_add(delta as u64);
            } else {
                *entry = entry.saturating_sub((-delta) as u64);
            }
        }
        self.node.adjust(delta);
    }

    /// Sum of per-partition bytes for this pipeline.
    pub fn pipeline_bytes(&self) -> u64 {
        self.inner
            .lock()
            .map(|g| g.partitions.values().sum())
            .unwrap_or(0)
    }

    pub fn partition_bytes(&self, partition: u16) -> u64 {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.partitions.get(&partition).copied())
            .unwrap_or(0)
    }

    pub fn caps(&self) -> CapacityLimits {
        self.caps
    }

    /// Evaluate the three caps in order of severity (node → pipeline →
    /// partition) and return the appropriate source-side remedy. The
    /// highest engaged level wins, since a node-level throttle
    /// already handles pipeline and partition pressure implicitly.
    pub fn decide(&self, partition: u16, kind: SourceKind) -> BackpressureDecision {
        // Node level first — most severe.
        if self.node.is_pressured() {
            return BackpressureDecision::for_kind(kind, PressureLevel::Node);
        }

        // Pipeline level — ≥95% of pipeline cap.
        if let Some(cap) = self.caps.pipeline_max_bytes {
            let used = self.pipeline_bytes();
            if cap > 0 && used * 20 >= cap * 19 {
                return BackpressureDecision::for_kind(kind, PressureLevel::Pipeline);
            }
        }

        // Partition soft target — any breach flips it on (this is a
        // warning threshold, not a hard cap).
        if let Some(target) = self.caps.partition_soft_target {
            let used = self.partition_bytes(partition);
            if target > 0 && used >= target {
                return BackpressureDecision::for_kind(kind, PressureLevel::Partition);
            }
        }

        BackpressureDecision::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_derives_partition_target_from_pipeline_cap() {
        let c = CapacityLimits::from_config(None, Some(8_000), 8);
        assert_eq!(c.partition_soft_target, Some(1_000));
    }

    #[test]
    fn limits_no_pipeline_cap_means_no_partition_target() {
        let c = CapacityLimits::from_config(Some(100_000), None, 8);
        assert_eq!(c.partition_soft_target, None);
    }

    #[test]
    fn limits_zero_partitions_guards_divide() {
        let c = CapacityLimits::from_config(None, Some(1_000), 0);
        assert_eq!(c.partition_soft_target, None);
    }

    #[test]
    fn unlimited_returns_none_decisions() {
        let pb = PipelineCapacity::new(CapacityLimits::unlimited(), NodeCapacity::new(None));
        pb.adjust(0, 1_000_000);
        assert_eq!(pb.decide(0, SourceKind::Push), BackpressureDecision::None);
    }

    #[test]
    fn partition_target_breach_throttles_that_partition() {
        let caps = CapacityLimits::from_config(None, Some(800), 8); // target=100
        let pb = PipelineCapacity::new(caps, NodeCapacity::new(None));
        pb.adjust(0, 100);
        assert_eq!(
            pb.decide(0, SourceKind::Push),
            BackpressureDecision::PushReject {
                level: PressureLevel::Partition
            }
        );
        // A different partition with no bytes is unaffected.
        assert_eq!(pb.decide(1, SourceKind::Push), BackpressureDecision::None);
    }

    #[test]
    fn pipeline_cap_approach_throttles_all_partitions() {
        let caps = CapacityLimits::from_config(None, Some(1_000), 10); // target=100
        let pb = PipelineCapacity::new(caps, NodeCapacity::new(None));
        // Load 95% of the pipeline cap across partitions.
        pb.adjust(0, 480);
        pb.adjust(1, 480);
        // Partition 2 has zero bytes but still gets throttled.
        assert_eq!(
            pb.decide(2, SourceKind::Pull),
            BackpressureDecision::PullPause {
                level: PressureLevel::Pipeline
            }
        );
    }

    #[test]
    fn node_cap_approach_wins_over_pipeline() {
        let node = NodeCapacity::new(Some(1_000));
        let caps = CapacityLimits::from_config(Some(1_000), Some(10_000), 10);
        let pb = PipelineCapacity::new(caps, node.clone());
        pb.adjust(0, 950); // 95% of node cap
        assert_eq!(
            pb.decide(0, SourceKind::Poll),
            BackpressureDecision::PollSkip {
                level: PressureLevel::Node
            }
        );
    }

    #[test]
    fn decision_for_kind_maps_source_kinds() {
        assert_eq!(
            BackpressureDecision::for_kind(SourceKind::Push, PressureLevel::Partition),
            BackpressureDecision::PushReject {
                level: PressureLevel::Partition
            }
        );
        assert_eq!(
            BackpressureDecision::for_kind(SourceKind::Pull, PressureLevel::Node),
            BackpressureDecision::PullPause {
                level: PressureLevel::Node
            }
        );
        assert_eq!(
            BackpressureDecision::for_kind(SourceKind::Poll, PressureLevel::Pipeline),
            BackpressureDecision::PollSkip {
                level: PressureLevel::Pipeline
            }
        );
    }

    #[test]
    fn negative_adjust_releases_capacity() {
        let caps = CapacityLimits::from_config(None, Some(1_000), 10); // target=100
        let pb = PipelineCapacity::new(caps, NodeCapacity::new(None));
        pb.adjust(0, 150);
        assert!(pb.decide(0, SourceKind::Push).is_engaged());
        pb.adjust(0, -100);
        assert_eq!(pb.partition_bytes(0), 50);
        assert_eq!(pb.decide(0, SourceKind::Push), BackpressureDecision::None);
    }

    #[test]
    fn node_capacity_pressure_threshold_is_95_percent() {
        let n = NodeCapacity::new(Some(100));
        n.adjust(94);
        assert!(!n.is_pressured());
        n.adjust(1); // 95
        assert!(n.is_pressured());
    }

    #[test]
    fn node_capacity_saturating_sub_never_underflows() {
        let n = NodeCapacity::new(Some(100));
        n.adjust(-500);
        assert_eq!(n.bytes(), 0);
    }

    #[test]
    fn pressure_level_labels_distinct() {
        assert_eq!(PressureLevel::Partition.label(), "partition");
        assert_eq!(PressureLevel::Pipeline.label(), "pipeline");
        assert_eq!(PressureLevel::Node.label(), "node");
    }

    #[test]
    fn node_cap_shared_across_pipelines() {
        let node = NodeCapacity::new(Some(1_000));
        let pb_a = PipelineCapacity::new(CapacityLimits::unlimited(), node.clone());
        let pb_b = PipelineCapacity::new(CapacityLimits::unlimited(), node.clone());
        pb_a.adjust(0, 600);
        pb_b.adjust(0, 350); // total 950 → node pressure at 95%
        assert_eq!(
            pb_b.decide(0, SourceKind::Push),
            BackpressureDecision::PushReject {
                level: PressureLevel::Node
            }
        );
    }

    #[test]
    fn decision_level_extraction() {
        assert_eq!(BackpressureDecision::None.level(), None);
        assert_eq!(
            BackpressureDecision::PullPause {
                level: PressureLevel::Pipeline
            }
            .level(),
            Some(PressureLevel::Pipeline)
        );
    }
}
