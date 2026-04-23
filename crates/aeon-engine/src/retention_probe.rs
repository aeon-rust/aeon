//! S5 — Resolve `RetentionBlock` (declarative, YAML-facing) into a
//! `RetentionPlan` that the supervisor can stamp onto `PipelineConfig`.
//!
//! Mirrors `encryption_probe`: runs at pipeline start, fails early on
//! malformed strings so a mis-configured manifest never boots a
//! pipeline that would later crash during its first GC sweep.
//!
//! Scope:
//!
//! - Parses `l2.body.hold_after_ack` ("300s", "5m", "1h", "30ms") into a
//!   `Duration`. Unit suffixes accepted: `ms`, `s`, `m`, `h`. Bare
//!   numbers are rejected — the unit is mandatory so nobody ships a
//!   pipeline that silently waits 0 seconds because they thought "5"
//!   meant five minutes.
//! - Passes `l3.ack.max_records` through unchanged — it's already a
//!   structured `u32`.
//!
//! The resolved plan stashes the parsed `Duration` so downstream code
//! (EO-2 L2 registry, L3 checkpoint store) consumes strongly-typed
//! values and never touches the raw YAML string.

use aeon_types::{AeonError, RetentionBlock};
use std::time::Duration;

/// Resolved, strongly-typed retention plan. Default = inert.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionPlan {
    /// `Duration::ZERO` when the manifest did not set
    /// `retention.l2_body.hold_after_ack`. The L2 GC sweep interprets
    /// zero as "reclaim immediately on ack" (pre-S5 behaviour).
    pub l2_hold_after_ack: Duration,
    /// `None` when the manifest did not set
    /// `retention.l3_ack.max_records`. The L3 checkpoint store
    /// interprets `None` as "keep every record forever" (pre-S5).
    pub l3_max_records: Option<u32>,
}

impl RetentionPlan {
    /// `true` when the plan imposes no retention overrides — used by
    /// the supervisor to avoid stamping a redundant `None` on
    /// downstream config fields.
    pub fn is_inert(&self) -> bool {
        self.l2_hold_after_ack.is_zero() && self.l3_max_records.is_none()
    }
}

/// Parse a manifest-style duration string ("300s", "5m", "30ms", "1h")
/// into a `Duration`. Returns `AeonError::config` on malformed input.
/// Exported so the YAML loader and the probe share a single parser.
pub fn parse_hold_duration(s: &str) -> Result<Duration, AeonError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(AeonError::config("retention: empty hold_after_ack string"));
    }

    // Find the first non-digit character — everything before it is the
    // numeric body, everything from it onwards is the unit suffix.
    let split = trimmed
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i);

    let (num_str, unit) = match split {
        Some(i) => (&trimmed[..i], &trimmed[i..]),
        None => {
            return Err(AeonError::config(format!(
                "retention: hold_after_ack '{s}' requires a unit suffix (ms/s/m/h)"
            )));
        }
    };

    if num_str.is_empty() {
        return Err(AeonError::config(format!(
            "retention: hold_after_ack '{s}' has no numeric body"
        )));
    }

    let n: u64 = num_str
        .parse()
        .map_err(|e| AeonError::config(format!("retention: bad number in '{s}': {e}")))?;

    let dur = match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.saturating_mul(60)),
        "h" => Duration::from_secs(n.saturating_mul(60 * 60)),
        _ => {
            return Err(AeonError::config(format!(
                "retention: unknown unit '{unit}' in '{s}' (expected ms/s/m/h)"
            )));
        }
    };

    Ok(dur)
}

/// Resolve a `RetentionBlock` from a pipeline manifest into a
/// strongly-typed plan. Never fails for inert blocks; returns a
/// `config` error only when a provided field is malformed.
pub fn resolve_retention_plan(block: &RetentionBlock) -> Result<RetentionPlan, AeonError> {
    let l2_hold = match &block.l2_body.hold_after_ack {
        Some(s) => parse_hold_duration(s)?,
        None => Duration::ZERO,
    };
    Ok(RetentionPlan {
        l2_hold_after_ack: l2_hold,
        l3_max_records: block.l3_ack.max_records,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::{L2RetentionBlock, L3RetentionBlock};

    #[test]
    fn default_block_resolves_to_inert_plan() {
        let p = resolve_retention_plan(&RetentionBlock::default()).unwrap();
        assert!(p.is_inert());
        assert_eq!(p.l2_hold_after_ack, Duration::ZERO);
        assert_eq!(p.l3_max_records, None);
    }

    #[test]
    fn parse_seconds() {
        assert_eq!(parse_hold_duration("300s").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_hold_duration("0s").unwrap(), Duration::ZERO);
    }

    #[test]
    fn parse_minutes_and_hours() {
        assert_eq!(parse_hold_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_hold_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn parse_millis() {
        assert_eq!(parse_hold_duration("250ms").unwrap(), Duration::from_millis(250));
    }

    #[test]
    fn trimmed_leading_and_trailing_whitespace() {
        assert_eq!(parse_hold_duration("  5m  ").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn missing_unit_errors() {
        assert!(parse_hold_duration("300").is_err());
    }

    #[test]
    fn unknown_unit_errors() {
        assert!(parse_hold_duration("5d").is_err());
        assert!(parse_hold_duration("5sec").is_err());
    }

    #[test]
    fn empty_string_errors() {
        assert!(parse_hold_duration("").is_err());
        assert!(parse_hold_duration("   ").is_err());
    }

    #[test]
    fn non_numeric_errors() {
        assert!(parse_hold_duration("abc").is_err());
        assert!(parse_hold_duration("m").is_err());
    }

    #[test]
    fn populated_block_resolves_both_fields() {
        let b = RetentionBlock {
            l2_body: L2RetentionBlock {
                hold_after_ack: Some("30s".into()),
            },
            l3_ack: L3RetentionBlock {
                max_records: Some(1_000),
            },
        };
        let p = resolve_retention_plan(&b).unwrap();
        assert_eq!(p.l2_hold_after_ack, Duration::from_secs(30));
        assert_eq!(p.l3_max_records, Some(1_000));
        assert!(!p.is_inert());
    }

    #[test]
    fn malformed_hold_string_is_config_error() {
        let b = RetentionBlock {
            l2_body: L2RetentionBlock {
                hold_after_ack: Some("notaduration".into()),
            },
            l3_ack: L3RetentionBlock::default(),
        };
        assert!(resolve_retention_plan(&b).is_err());
    }
}
