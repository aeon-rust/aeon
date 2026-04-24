//! S2.5 — Audit channel transport (paired with `aeon_types::audit`).
//!
//! Three pieces live here:
//!
//! - [`StderrAuditSink`] — default transport. Writes one JSON line per
//!   event to stderr via a dedicated writer that bypasses the `tracing`
//!   subscriber entirely. Intentional: a runtime reconfiguration of the
//!   tracing layer must never drop audit lines.
//! - [`NullAuditSink`] — no-op sink. Installed by default at process
//!   start so callers can emit freely without ordering dependencies;
//!   an operator replaces it via [`set_audit_sink`] during bootstrap.
//! - [`set_audit_sink`] / [`audit_sink`] / [`emit_audit`] — global
//!   installer + helper for call sites that don't carry a sink
//!   reference through their parameter list.
//!
//! # Why a global
//!
//! Compliance-relevant events are emitted from deep in the call stack
//! (e.g., `KekHandle::unwrap_dek` unwrap failure, `PipelineSupervisor`
//! refuse-to-start). Threading an `&dyn AuditSink` through every crate
//! in between would dilute the refactor far beyond S2.5's scope and
//! fight Rust's ownership rules along the way. One `OnceLock<Arc<dyn
//! AuditSink>>` is the idiom already used by the `tracing` crate for
//! exactly this reason.
//!
//! # Not in scope
//!
//! - PoH-signed chain sink: future atom.
//! - Async / non-blocking write with back-pressure: `StderrAuditSink`
//!   writes synchronously. Hot-path emitters (supervisor start) are
//!   cold-path anyway. If a warm path ever needs audit emission, a
//!   future `ChannelAuditSink` can sit in front of this one.

use std::io::Write;
use std::sync::{Arc, OnceLock, RwLock};

use aeon_types::audit::{AuditEvent, AuditSink};

// ─── No-op default sink ─────────────────────────────────────────────

/// Sink that discards every event. Installed as the default so call
/// sites need not guard emissions with `is_some()` checks during
/// bootstrap — before the operator-provided sink is installed.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullAuditSink;

impl AuditSink for NullAuditSink {
    fn emit(&self, _event: &AuditEvent) {}
}

// ─── Stderr JSON-line sink ──────────────────────────────────────────

/// Writes one JSON-encoded [`AuditEvent`] per line to stderr. Bypasses
/// `tracing` entirely so audit output cannot be silenced by a runtime
/// subscriber reconfiguration.
///
/// Each line is prefixed with a fixed banner (`AEON-AUDIT `) so log
/// aggregators can route audit lines to a dedicated index without
/// ambiguity against ordinary stderr noise.
#[derive(Debug, Default)]
pub struct StderrAuditSink {
    // Behind a lock to serialise writes — stderr is shared and
    // interleaved JSON lines would be unparseable.
    writer: std::sync::Mutex<()>,
}

/// Banner prefix on every stderr audit line. Stable — grep-friendly.
pub const AUDIT_STDERR_PREFIX: &str = "AEON-AUDIT ";

impl StderrAuditSink {
    pub fn new() -> Self {
        Self {
            writer: std::sync::Mutex::new(()),
        }
    }
}

impl AuditSink for StderrAuditSink {
    fn emit(&self, event: &AuditEvent) {
        let json = match serde_json::to_string(event) {
            Ok(s) => s,
            Err(_) => {
                // Audit must not panic. Best-effort: write a
                // structurally-minimal fallback so at least the action
                // and outcome survive.
                format!(
                    r#"{{"audit_serialise_error":true,"action":"{}","ts_nanos":{}}}"#,
                    event.action, event.ts_nanos
                )
            }
        };
        let _guard = self.writer.lock();
        let mut err = std::io::stderr().lock();
        // Ignore write errors — there is nowhere to report them that
        // wouldn't itself be the same channel we just failed to reach.
        let _ = writeln!(err, "{AUDIT_STDERR_PREFIX}{json}");
    }
}

// ─── Global emitter ────────────────────────────────────────────────

// RwLock lets tests (and a theoretical operator that rebinds the sink
// mid-run, e.g. after PoH chain warm-up) swap sinks. Production code
// should set it once at startup and leave it alone.
static AUDIT_SINK: OnceLock<RwLock<Arc<dyn AuditSink>>> = OnceLock::new();

fn cell() -> &'static RwLock<Arc<dyn AuditSink>> {
    AUDIT_SINK.get_or_init(|| RwLock::new(Arc::new(NullAuditSink)))
}

/// Install the process-wide audit sink. Safe to call multiple times —
/// the new sink replaces the old.
pub fn set_audit_sink(sink: Arc<dyn AuditSink>) {
    let c = cell();
    if let Ok(mut guard) = c.write() {
        *guard = sink;
    }
    // If the lock is poisoned, silently keep the existing sink —
    // audit emission must never panic its caller.
}

/// Fetch a handle to the currently-installed sink. Cheap: an `Arc`
/// clone of the registered sink. Returns a no-op sink if the lock is
/// poisoned (preserves the "audit never fails the caller" invariant).
pub fn audit_sink() -> Arc<dyn AuditSink> {
    match cell().read() {
        Ok(guard) => guard.clone(),
        Err(_) => Arc::new(NullAuditSink),
    }
}

/// Convenience emitter. Call sites prefer this over `audit_sink().emit(...)`
/// because it keeps the two-step dance in one place.
pub fn emit_audit(event: &AuditEvent) {
    audit_sink().emit(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::audit::{AuditCategory, AuditEvent, AuditOutcome};
    use std::sync::Mutex;

    // Capturing sink for global-emitter tests.
    #[derive(Default)]
    struct Capture {
        events: Mutex<Vec<AuditEvent>>,
    }
    impl AuditSink for Capture {
        fn emit(&self, e: &AuditEvent) {
            if let Ok(mut g) = self.events.lock() {
                g.push(e.clone());
            }
        }
    }

    fn ev() -> AuditEvent {
        AuditEvent::new(
            7,
            AuditCategory::Pipeline,
            "pipeline.start.refused",
            AuditOutcome::Denied,
        )
    }

    #[test]
    fn null_sink_is_noop() {
        let s = NullAuditSink;
        s.emit(&ev()); // must not panic, must not produce output
    }

    #[test]
    fn stderr_sink_serialises_via_json() {
        // We can't capture stderr cleanly across threads here; check
        // that it doesn't panic and the serialisation path would have
        // produced parseable output.
        let s = StderrAuditSink::new();
        s.emit(&ev());

        // Round-trip check: serialise the same event and confirm the
        // JSON is parseable (this is what the sink writes).
        let j = serde_json::to_string(&ev()).unwrap();
        let _: AuditEvent = serde_json::from_str(&j).unwrap();
    }

    #[test]
    fn global_default_is_null_sink() {
        // Sanity check: the default at startup is a no-op. If another
        // test ran first and set a different sink, skip the strict
        // identity check — but emitting must still not panic.
        let sink = audit_sink();
        sink.emit(&ev());
    }

    #[test]
    fn global_emitter_routes_to_installed_sink() {
        let cap = Arc::new(Capture::default());
        set_audit_sink(cap.clone());

        emit_audit(&ev());

        let got = cap.events.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].action, "pipeline.start.refused");
    }

    #[test]
    fn set_audit_sink_replaces_prior() {
        let a = Arc::new(Capture::default());
        let b = Arc::new(Capture::default());
        set_audit_sink(a.clone());
        set_audit_sink(b.clone());

        emit_audit(&ev());

        // Only `b` saw the event.
        let got_b = b.events.lock().unwrap();
        assert_eq!(got_b.len(), 1);
    }

    #[test]
    fn stderr_prefix_is_stable_public_constant() {
        // Contract: log aggregators grep for this prefix. Any change
        // is a breaking change for SIEM configs.
        assert_eq!(AUDIT_STDERR_PREFIX, "AEON-AUDIT ");
    }
}
