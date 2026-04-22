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

use std::borrow::Cow;

const REDACTED: &str = "***";

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
            ("wss://user:token@api.example.com/stream", "wss://***@api.example.com/stream"),
            ("https://admin:secret@dashboard.example/", "https://***@dashboard.example/"),
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
}
