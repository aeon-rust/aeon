//! CPU core pinning for hot-path pipeline tasks.
//!
//! Pins pipeline stages (source, processor, sink) to specific CPU cores
//! to prevent context switches and cache thrashing. Each stage runs on
//! a dedicated core, keeping L1/L2 caches warm for its working set.
//!
//! Core affinity is best-effort: if pinning fails (insufficient cores,
//! permission denied), the pipeline runs without pinning.

/// Pin the current thread to a specific CPU core.
///
/// Returns `true` if pinning succeeded, `false` if it failed (best-effort).
pub fn pin_to_core(core_id: usize) -> bool {
    let core = core_affinity::CoreId { id: core_id };
    core_affinity::set_for_current(core)
}

/// Get the number of available CPU cores.
pub fn available_cores() -> usize {
    core_affinity::get_core_ids()
        .map(|ids| ids.len())
        .unwrap_or(1)
}

/// Get available core IDs.
pub fn core_ids() -> Vec<usize> {
    core_affinity::get_core_ids()
        .map(|ids| ids.into_iter().map(|id| id.id).collect())
        .unwrap_or_else(|| vec![0])
}

/// Assigns core IDs for a 3-stage pipeline (source, processor, sink).
///
/// Strategy:
/// - If >= 3 cores: assign one core per stage, starting from core 1
///   (core 0 is left for the OS/runtime)
/// - If < 3 cores: return None (don't pin — let the OS schedule)
pub fn pipeline_core_assignment() -> Option<PipelineCores> {
    let cores = core_ids();
    if cores.len() >= 4 {
        // Skip core 0 (OS), use cores 1, 2, 3 for pipeline stages
        Some(PipelineCores {
            source: cores[1],
            processor: cores[2],
            sink: cores[3],
        })
    } else if cores.len() >= 3 {
        Some(PipelineCores {
            source: cores[0],
            processor: cores[1],
            sink: cores[2],
        })
    } else {
        None
    }
}

/// Assigns core IDs for multiple partition pipelines.
///
/// Each partition gets 3 dedicated cores (source, processor, sink).
/// Core 0 is reserved for OS/runtime. Returns `None` if insufficient cores.
///
/// Layout: `[OS | P0-src P0-proc P0-sink | P1-src P1-proc P1-sink | ...]`
pub fn multi_pipeline_core_assignment(partition_count: usize) -> Option<Vec<PipelineCores>> {
    if partition_count == 0 {
        return Some(vec![]);
    }
    let cores = core_ids();
    let needed = 1 + (partition_count * 3); // core 0 + 3 per partition
    if cores.len() < needed {
        return None;
    }
    let mut assignments = Vec::with_capacity(partition_count);
    for i in 0..partition_count {
        let base = 1 + (i * 3); // skip core 0
        assignments.push(PipelineCores {
            source: cores[base],
            processor: cores[base + 1],
            sink: cores[base + 2],
        });
    }
    Some(assignments)
}

/// Core assignments for the three pipeline stages.
#[derive(Debug, Clone, Copy)]
pub struct PipelineCores {
    /// Core for the source task.
    pub source: usize,
    /// Core for the processor task.
    pub processor: usize,
    /// Core for the sink task.
    pub sink: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_cores_nonzero() {
        assert!(available_cores() >= 1);
    }

    #[test]
    fn core_ids_nonempty() {
        let ids = core_ids();
        assert!(!ids.is_empty());
    }

    #[test]
    fn pin_to_core_0_succeeds() {
        // Core 0 should always exist
        assert!(pin_to_core(0));
    }

    #[test]
    fn pipeline_assignment_logic() {
        let cores = available_cores();
        let assignment = pipeline_core_assignment();

        if cores >= 3 {
            assert!(assignment.is_some());
            let a = assignment.unwrap();
            // All three cores should be different
            assert_ne!(a.source, a.processor);
            assert_ne!(a.processor, a.sink);
            assert_ne!(a.source, a.sink);
        }
        // If < 3 cores, assignment is None — which is fine
    }

    #[test]
    fn multi_pipeline_zero_partitions() {
        let result = multi_pipeline_core_assignment(0);
        assert!(result.is_some());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn multi_pipeline_assignment_logic() {
        let cores = available_cores();
        // Try to assign for 1 partition (needs 4 cores: OS + 3)
        let result = multi_pipeline_core_assignment(1);
        if cores >= 4 {
            assert!(result.is_some());
            let assignments = result.unwrap();
            assert_eq!(assignments.len(), 1);
            // Core 0 reserved, so all assigned cores > 0
            let a = &assignments[0];
            assert_ne!(a.source, 0);
            assert_ne!(a.source, a.processor);
            assert_ne!(a.processor, a.sink);
        }

        // Try 2 partitions (needs 7 cores)
        let result = multi_pipeline_core_assignment(2);
        if cores >= 7 {
            assert!(result.is_some());
            let assignments = result.unwrap();
            assert_eq!(assignments.len(), 2);
            // Partition 0 and 1 should use different cores
            let p0 = &assignments[0];
            let p1 = &assignments[1];
            assert_ne!(p0.source, p1.source);
            assert_ne!(p0.processor, p1.processor);
            assert_ne!(p0.sink, p1.sink);
        }
    }

    #[test]
    fn multi_pipeline_insufficient_cores() {
        // Request more partitions than cores can support
        let cores = available_cores();
        let too_many = (cores / 3) + 2;
        let result = multi_pipeline_core_assignment(too_many);
        // Should return None if insufficient
        if cores < 1 + (too_many * 3) {
            assert!(result.is_none());
        }
    }
}
