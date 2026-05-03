//! S2: URI userinfo redaction for logging.
//!
//! [`redact_uri`] strips the `user:password@` portion of any URL-shaped string
//! while preserving scheme, host, port, path, query, and fragment. Called at
//! every connector log site that emits a connection string so credentials
//! never reach the log pipeline — the payload-never-in-logs invariant extends
//! to URI userinfo (audit finding C4 for Kafka SASL; the same class of leak
//! for AMQP / Redis / Mongo / Postgres / MySQL / WebSocket URLs).
//!
//! Cheap manual parser: no regex, no allocation on the clean path (when there
//! is no userinfo to strip, the input is returned as a borrowed `Cow`). Lives
//! in `aeon-types` so every crate — connectors, engine, cluster, CLI — can
//! reach it without adding a new dependency on `aeon-observability`.
//!
//! ## Policy
//!
//! | Input                                  | Output                              |
//! | -------------------------------------- | ----------------------------------- |
//! | `https://host/path`                    | unchanged (borrowed)                |
//! | `amqp://u:p@host:5672/vhost`           | `amqp://***@host:5672/vhost`        |
//! | `redis://:pass@host/0`                 | `redis://***@host/0`                |
//! | `mongodb+srv://u:p@cluster/db?x=y`     | `mongodb+srv://***@cluster/db?x=y`  |
//! | `plain text` (no `://`)                | unchanged (borrowed)                |
//! | `""`                                   | unchanged (borrowed)                |
//!
//! ## Non-goals
//!
//! Does not parse query-string secrets (`?password=...`), does not mask host
//! or port, does not handle raw `user:pass@host:port` without a scheme. Those
//! are caller-side concerns — the policy here is narrow on purpose so it
//! never accidentally mangles a non-URI value that happened to land in a
//! logged field.
//!
//! ## S6.6 — metadata-key deny-list
//!
//! In addition to URI redaction, this module owns the **metadata-key**
//! deny-list: a list of `Event.metadata` / `Output.headers` keys whose
//! *values* must never appear in log output. The current list:
//!
//! | Key                | Reason                                  |
//! | ------------------ | --------------------------------------- |
//! | `aeon.subject_id`  | GDPR Art. 4(5) pseudonymous PII (S6.6)  |
//!
//! Callers that log metadata or headers must either skip denied keys or
//! replace the value with `***`. The policy lives here (and not in the
//! observability crate) so leaf crates that have no `tracing` dependency
//! can still consult it from their own log hooks.

use std::borrow::Cow;

const REDACTED: &str = "***";

/// Metadata keys whose values must be redacted at every log site that
/// prints `Event.metadata` or `Output.headers`. See [`is_redacted_metadata_key`].
pub const METADATA_REDACT_DENYLIST: &[&str] = &[
    // S6.6 — GDPR subject-id is pseudonymous PII under Art. 4(5). Metrics
    // use a salted hash (see `aeon_crypto::null_receipt::salted_subject_hash`);
    // logs drop the value entirely.
    "aeon.subject_id",
];

/// Replace `user[:password]@` in any URI with `***@`. Returns `Cow::Borrowed`
/// when the input has no userinfo (the hot path), or `Cow::Owned` when a
/// rewrite was required.
///
/// See the module docstring for exact behaviour.
pub fn redact_uri(s: &str) -> Cow<'_, str> {
    let Some(scheme_end) = s.find("://") else {
        return Cow::Borrowed(s);
    };
    let after_scheme_start = scheme_end + 3;
    let rest = &s[after_scheme_start..];
    // The authority component ends at the first '/', '?', or '#' — whatever
    // comes first. Without one, the whole remainder is the authority.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    // rfind('@') handles the edge case where the password contains '@'
    // (which RFC 3986 allows when percent-encoded, but some operators pass
    // a literal '@' anyway). Take the last '@' — anything before it is
    // userinfo by definition.
    let Some(at) = authority.rfind('@') else {
        return Cow::Borrowed(s);
    };
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..after_scheme_start]);
    out.push_str(REDACTED);
    out.push_str(&authority[at..]); // includes the '@' separator
    out.push_str(&rest[authority_end..]);
    Cow::Owned(out)
}

/// Is this metadata key on the redaction deny-list? Log sites that
/// iterate `Event.metadata` / `Output.headers` must call this per key
/// and drop the value (or replace with `***`) when it returns `true`.
///
/// Match is case-sensitive — the canonical key casing is the source
/// of truth; Aeon code never emits the same key with a different
/// case. An inbound connector that forwards mixed-case headers must
/// normalise before looking up, or use the generic `redact_*` helpers
/// the call site already reaches for.
#[inline]
pub fn is_redacted_metadata_key(k: &str) -> bool {
    METADATA_REDACT_DENYLIST.contains(&k)
}

/// Return the log-safe value for a `(key, value)` pair. If the key is
/// on the deny-list, returns `"***"`; otherwise returns the original
/// value borrowed. Zero-allocation on the clean path.
#[inline]
pub fn redact_metadata_value<'v>(k: &str, v: &'v str) -> Cow<'v, str> {
    if is_redacted_metadata_key(k) {
        Cow::Borrowed(REDACTED)
    } else {
        Cow::Borrowed(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[allow(clippy::ptr_arg)] // Intentional: the helper's job is to inspect the Cow variant itself.
    fn is_borrowed<T: ToOwned + ?Sized>(c: &Cow<'_, T>) -> bool {
        matches!(c, Cow::Borrowed(_))
    }

    #[test]
    fn no_scheme_returns_borrowed_unchanged() {
        let c = redact_uri("plain text");
        assert!(is_borrowed(&c));
        assert_eq!(c, "plain text");
    }

    #[test]
    fn empty_string_returns_borrowed() {
        let c = redact_uri("");
        assert!(is_borrowed(&c));
        assert_eq!(c, "");
    }

    #[test]
    fn no_userinfo_returns_borrowed() {
        for input in [
            "https://example.com/path",
            "http://host:8080",
            "amqp://localhost:5672/%2f",
            "redis://host:6379/0",
            "mongodb+srv://cluster0.example.net/db?x=y",
            "nats://nats.default.svc.cluster.local:4222",
            "ws://host",
        ] {
            let c = redact_uri(input);
            assert!(is_borrowed(&c), "expected borrowed for {input}");
            assert_eq!(c, input);
        }
    }

    #[test]
    fn userinfo_is_redacted() {
        let cases = [
            ("amqp://u:p@host:5672/vhost", "amqp://***@host:5672/vhost"),
            ("redis://:pass@host/0", "redis://***@host/0"),
            (
                "postgres://user:pw@db.example.com:5432/mydb?sslmode=require",
                "postgres://***@db.example.com:5432/mydb?sslmode=require",
            ),
            (
                "mongodb+srv://u:p@cluster0.example.net/db?authSource=admin",
                "mongodb+srv://***@cluster0.example.net/db?authSource=admin",
            ),
            (
                "wss://user:token@api.example.com/stream",
                "wss://***@api.example.com/stream",
            ),
            (
                "https://admin:secret@dashboard.example/",
                "https://***@dashboard.example/",
            ),
        ];
        for (input, expected) in cases {
            let c = redact_uri(input);
            assert_eq!(c, expected, "failed on {input}");
            assert!(!is_borrowed(&c), "should have rewritten {input}");
        }
    }

    #[test]
    fn userinfo_without_password_is_redacted() {
        assert_eq!(redact_uri("amqp://user@host"), "amqp://***@host");
    }

    #[test]
    fn multiple_at_in_userinfo_takes_last() {
        // RFC 3986 percent-encodes '@' in the password, but operators sometimes
        // paste literal '@' — rfind('@') keeps the right host boundary.
        let c = redact_uri("https://user:p@ss@host/path");
        assert_eq!(c, "https://***@host/path");
    }

    #[test]
    fn query_with_nested_scheme_does_not_confuse_authority_parser() {
        // The authority ends at '?', so the nested "https://" in the query
        // string must NOT be re-scanned as a userinfo boundary.
        let c = redact_uri("http://host?callback=https://other.example/cb");
        assert!(is_borrowed(&c));
        assert_eq!(c, "http://host?callback=https://other.example/cb");
    }

    #[test]
    fn fragment_does_not_confuse_parser() {
        let c = redact_uri("https://u:p@host/path#frag");
        assert_eq!(c, "https://***@host/path#frag");
    }

    #[test]
    fn no_path_component() {
        let c = redact_uri("amqp://u:p@host:5672");
        assert_eq!(c, "amqp://***@host:5672");
    }

    #[test]
    fn unusual_scheme_still_works() {
        // Aeon uses custom schemes for QUIC/WebTransport in some configs.
        assert_eq!(
            redact_uri("quic+wt://u:p@node.example:4472/pipe"),
            "quic+wt://***@node.example:4472/pipe"
        );
    }

    // ── S6.6 metadata-key deny-list ─────────────────────────────────────

    #[test]
    fn subject_id_is_denied() {
        assert!(is_redacted_metadata_key("aeon.subject_id"));
    }

    #[test]
    fn unrelated_keys_are_not_denied() {
        for k in [
            "content-type",
            "x-request-id",
            "trace-id",
            "aeon.subjectId",  // wrong casing
            "aeon_subject_id", // wrong separator
            "subject_id",      // missing namespace prefix
            "",
        ] {
            assert!(!is_redacted_metadata_key(k), "should NOT be denied: {k:?}");
        }
    }

    #[test]
    fn redact_metadata_value_masks_subject_id() {
        let v = redact_metadata_value("aeon.subject_id", "tenant-a/user-1");
        assert_eq!(v, "***");
    }

    #[test]
    fn redact_metadata_value_passes_through_safe_keys() {
        let v = redact_metadata_value("content-type", "application/json");
        assert_eq!(v, "application/json");
        // Clean-path guarantee: returned Cow must be borrowed (no alloc).
        assert!(matches!(v, Cow::Borrowed(_)));
    }

    #[test]
    fn denylist_is_nonempty_and_contains_subject_id() {
        // Defensive: a refactor that accidentally empties the denylist
        // must be caught at test time, not in production.
        assert!(!METADATA_REDACT_DENYLIST.is_empty());
        assert!(METADATA_REDACT_DENYLIST.contains(&"aeon.subject_id"));
    }
}
