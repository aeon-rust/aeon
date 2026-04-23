//! S6.5 — GDPR right-to-export.
//!
//! Given an [`ErasureSelector`] (a specific subject or a namespace
//! wildcard), walk a partition's L2 body segments and surface every
//! event whose metadata carries a matching `aeon.subject_id` entry.
//!
//! # v0.1 — no subject → event index
//!
//! Per the design note at `docs/aeon-dev-notes.txt` §6.1.j, v0.1 does
//! not maintain an L3 subject → event index. An export request
//! therefore performs a full scan of every live L2 segment belonging
//! to the partition, filtering by the `aeon.subject_id` metadata key
//! as each event is decoded.
//!
//! This is acceptable because:
//!
//! - Right-to-export is a cold, human-triggered operation. p99 latency
//!   is measured in seconds, not microseconds.
//! - L2 segments are bounded (256 MiB default) and already fully
//!   decoded on the compaction path, so the scan logic is shared.
//! - Most pipelines do not declare `compliance.regime = gdpr`. The
//!   scan never runs for the other 99% of events.
//!
//! An L3 secondary index can be added later behind a feature flag if
//! operators report slow exports on very large partitions.
//!
//! # Streaming contract
//!
//! [`SubjectExporter::scan`] takes a visitor closure so the REST
//! handler can write each matching event out as a single NDJSON line
//! without buffering the full result in memory. Tests and small
//! callers use [`SubjectExporter::collect`].
//!
//! # Scope
//!
//! This module operates on a single partition's [`L2BodyStore`].
//! Fanning out across every partition owned by a pipeline is the
//! REST handler's job (S6.5 wiring, separate sub-atom).

use aeon_types::{
    AeonError, ErasureSelector, Event, SubjectId, collect_subject_ids,
};

use crate::l2_body::{L2BodySegment, L2BodyStore};

/// One event matched by an export scan. The sequence number is the
/// partition-local delivery seq assigned at L2 append time — not the
/// event's own UUID. Callers that need stable cross-partition
/// ordering must combine it with the partition id themselves.
#[derive(Debug, Clone)]
pub struct SubjectExportRecord {
    pub seq: u64,
    pub event: Event,
}

/// Scan a single partition's L2 body segments for events matching a
/// subject selector. Holds only a borrow of the body store; cheap to
/// construct per request.
pub struct SubjectExporter<'a> {
    store: &'a L2BodyStore,
}

impl<'a> SubjectExporter<'a> {
    pub fn new(store: &'a L2BodyStore) -> Self {
        Self { store }
    }

    /// Walk every segment, invoking `on_match(seq, event)` for each
    /// event whose metadata contains at least one `aeon.subject_id`
    /// matched by `selector`. Returns the total number of matches.
    ///
    /// Events with malformed subject ids are silently skipped by
    /// [`collect_subject_ids`] — matching the tolerant runtime
    /// contract used elsewhere in S6. A malformed id should not hide
    /// a matching event from an export because a neighbouring entry
    /// parsed correctly, so we match against whatever *did* parse.
    pub fn scan<F>(
        &self,
        selector: &ErasureSelector,
        mut on_match: F,
    ) -> Result<usize, AeonError>
    where
        F: FnMut(u64, Event) -> Result<(), AeonError>,
    {
        let kek = self.store.kek();
        let mut matches: usize = 0;
        for path in self.store.segment_paths() {
            for rec in L2BodySegment::iter_records(&path, kek)? {
                let (seq, event) = rec?;
                if event_matches(&event, selector) {
                    on_match(seq, event)?;
                    matches += 1;
                }
            }
        }
        Ok(matches)
    }

    /// Convenience wrapper that collects every match into a `Vec`.
    /// Useful for tests and callers with a small, known-bounded
    /// expected result size. Prefer [`Self::scan`] for REST streaming.
    pub fn collect(
        &self,
        selector: &ErasureSelector,
    ) -> Result<Vec<SubjectExportRecord>, AeonError> {
        let mut out = Vec::new();
        self.scan(selector, |seq, event| {
            out.push(SubjectExportRecord { seq, event });
            Ok(())
        })?;
        Ok(out)
    }
}

/// Does `event.metadata` carry a subject id matched by `selector`? A
/// multi-subject event yields `true` if **any** of its subjects
/// matches; the visitor still fires exactly once (per-event, not
/// per-match) so NDJSON output never contains the same event twice.
fn event_matches(event: &Event, selector: &ErasureSelector) -> bool {
    let subjects = collect_subject_ids(event.metadata.iter());
    subjects.iter().any(|s| selector.matches(s))
}

/// Standalone predicate exposed for callers that hold a `Vec<SubjectId>`
/// already (e.g. tests, or the erasure compactor that decodes subjects
/// once per event for both match + PoH hashing). Returns `true` if any
/// of `subjects` is covered by `selector`.
pub fn selector_matches_any(selector: &ErasureSelector, subjects: &[SubjectId]) -> bool {
    subjects.iter().any(|s| selector.matches(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{Event, PartitionId, SubjectId};
    use bytes::Bytes;
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::l2_body::{L2BodyConfig, L2BodyStore};

    fn tmp_dir() -> PathBuf {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().to_path_buf();
        std::mem::forget(d);
        p
    }

    fn event_with_subjects(n: u64, subjects: &[&str]) -> Event {
        let mut ev = Event::new(
            uuid::Uuid::now_v7(),
            n as i64,
            Arc::from("src"),
            PartitionId::new(0),
            Bytes::from(format!("payload-{n}")),
        );
        for s in subjects {
            ev = ev.with_metadata(
                Arc::from("aeon.subject_id"),
                Arc::from(*s),
            );
        }
        ev
    }

    fn open_store() -> L2BodyStore {
        L2BodyStore::open(tmp_dir(), L2BodyConfig::default()).unwrap()
    }

    #[test]
    fn empty_store_yields_no_matches() {
        let store = open_store();
        let exporter = SubjectExporter::new(&store);
        let sel = ErasureSelector::parse("tenant-a/user-1").unwrap();
        let out = exporter.collect(&sel).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn single_subject_match() {
        let mut store = open_store();
        store
            .append(&event_with_subjects(0, &["tenant-a/user-1"]))
            .unwrap();
        store
            .append(&event_with_subjects(1, &["tenant-a/user-2"]))
            .unwrap();
        store
            .append(&event_with_subjects(2, &["tenant-a/user-1"]))
            .unwrap();

        let sel = ErasureSelector::parse("tenant-a/user-1").unwrap();
        let out = SubjectExporter::new(&store).collect(&sel).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].seq, 0);
        assert_eq!(out[1].seq, 2);
    }

    #[test]
    fn namespace_wildcard_match() {
        let mut store = open_store();
        store
            .append(&event_with_subjects(0, &["tenant-a/user-1"]))
            .unwrap();
        store
            .append(&event_with_subjects(1, &["tenant-b/user-9"]))
            .unwrap();
        store
            .append(&event_with_subjects(2, &["tenant-a/user-2"]))
            .unwrap();

        let sel = ErasureSelector::parse("tenant-a/*").unwrap();
        let out = SubjectExporter::new(&store).collect(&sel).unwrap();
        assert_eq!(out.len(), 2);
        let seqs: Vec<u64> = out.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![0, 2]);
    }

    #[test]
    fn multi_subject_event_emitted_once() {
        let mut store = open_store();
        // Event with both "tenant-a/user-1" and "tenant-a/user-2" —
        // the wildcard matches both, but we must emit the event once.
        store
            .append(&event_with_subjects(
                0,
                &["tenant-a/user-1", "tenant-a/user-2"],
            ))
            .unwrap();

        let sel = ErasureSelector::parse("tenant-a/*").unwrap();
        let out = SubjectExporter::new(&store).collect(&sel).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn events_without_subject_id_are_skipped() {
        let mut store = open_store();
        store.append(&event_with_subjects(0, &[])).unwrap();
        store
            .append(&event_with_subjects(1, &["tenant-a/user-1"]))
            .unwrap();

        let sel = ErasureSelector::parse("tenant-a/user-1").unwrap();
        let out = SubjectExporter::new(&store).collect(&sel).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seq, 1);
    }

    #[test]
    fn unknown_subject_yields_empty() {
        let mut store = open_store();
        store
            .append(&event_with_subjects(0, &["tenant-a/user-1"]))
            .unwrap();

        let sel = ErasureSelector::parse("tenant-a/ghost").unwrap();
        let out = SubjectExporter::new(&store).collect(&sel).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn scan_spans_multiple_segments() {
        // Force rollover by using a very small segment_bytes threshold
        // so each event lands in its own segment.
        let cfg = L2BodyConfig {
            segment_bytes: 1,
            ..Default::default()
        };
        let mut store = L2BodyStore::open(tmp_dir(), cfg).unwrap();
        for i in 0..5u64 {
            let subject = if i % 2 == 0 {
                "tenant-a/user-1"
            } else {
                "tenant-b/user-9"
            };
            store.append(&event_with_subjects(i, &[subject])).unwrap();
        }
        assert!(store.segment_count() >= 2);

        let sel = ErasureSelector::parse("tenant-a/user-1").unwrap();
        let out = SubjectExporter::new(&store).collect(&sel).unwrap();
        assert_eq!(out.len(), 3);
        let seqs: Vec<u64> = out.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![0, 2, 4]);
    }

    #[test]
    fn visitor_error_aborts_scan() {
        let mut store = open_store();
        for i in 0..3u64 {
            store
                .append(&event_with_subjects(i, &["tenant-a/user-1"]))
                .unwrap();
        }
        let sel = ErasureSelector::parse("tenant-a/user-1").unwrap();
        let mut seen: Vec<u64> = Vec::new();
        let err = SubjectExporter::new(&store)
            .scan(&sel, |seq, _| {
                seen.push(seq);
                if seq == 1 {
                    Err(AeonError::state("boom".to_string()))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        // Scan must short-circuit at the failing record, not keep walking.
        assert_eq!(seen, vec![0, 1]);
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn selector_matches_any_helper() {
        let sel = ErasureSelector::parse("tenant-a/*").unwrap();
        let subs = vec![
            SubjectId::parse("tenant-b/user-9").unwrap(),
            SubjectId::parse("tenant-a/user-1").unwrap(),
        ];
        assert!(selector_matches_any(&sel, &subs));

        let sel_miss = ErasureSelector::parse("tenant-c/*").unwrap();
        assert!(!selector_matches_any(&sel_miss, &subs));
    }

    #[test]
    fn match_count_returned() {
        let mut store = open_store();
        store
            .append(&event_with_subjects(0, &["tenant-a/user-1"]))
            .unwrap();
        store
            .append(&event_with_subjects(1, &["tenant-a/user-1"]))
            .unwrap();
        store
            .append(&event_with_subjects(2, &["tenant-b/user-9"]))
            .unwrap();

        let sel = ErasureSelector::parse("tenant-a/user-1").unwrap();
        let mut count = 0;
        let returned = SubjectExporter::new(&store)
            .scan(&sel, |_, _| {
                count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(returned, 2);
        assert_eq!(count, 2);
    }
}
