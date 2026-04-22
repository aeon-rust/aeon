//! S7: SSRF (Server-Side Request Forgery) guard for outbound dial sites.
//!
//! Aeon accepts URLs in pipeline manifests, processor registrations, and CLI
//! arguments — any of which a low-trust operator could set to
//! `http://169.254.169.254/latest/meta-data/iam/security-credentials/`
//! (EC2 IMDS credential theft), `http://127.0.0.1:4471/cluster/shutdown`
//! (admin-plane hijack), or `http://10.0.0.53:53` (internal port scan).
//!
//! [`SsrfPolicy`] holds the allow/deny knobs; [`SsrfPolicy::check_host`] and
//! [`SsrfPolicy::check_url`] are the guard entrypoints. They:
//!   1. Parse the URL / extract host+port.
//!   2. Resolve the host once (DNS).
//!   3. Filter every resolved IP against the policy.
//!   4. Return the list of allowed [`SocketAddr`]s, or fail closed.
//!
//! Callers should dial the returned `SocketAddr`s directly (with the original
//! host preserved in the TLS SNI / `Host` header) rather than passing the
//! hostname to the transport library. That pattern defeats DNS-rebinding
//! (where a hostile resolver returns a public IP at guard-check time and a
//! private IP at connect time).
//!
//! ## Defaults
//!
//! [`SsrfPolicy::default`] (a.k.a. `production()`):
//!   - `allow_private = true` — RFC1918 is the norm inside Kubernetes /
//!     EC2 / DO / GCP VPCs; blocking it by default would make the engine
//!     useless for internal webhooks.
//!   - `allow_loopback = false` — denies `127.0.0.0/8` and `::1`. Stops a
//!     malicious pipeline YAML from pointing a sink at Aeon's own admin
//!     port or any other localhost service in the pod.
//!   - `allow_link_local = false` — denies `169.254.0.0/16` and
//!     `fe80::/10`. **This is the cloud-metadata IMDS defence**.
//!   - `allow_cgnat = false` — denies `100.64.0.0/10`. Rarely legitimate;
//!     when it is, operators opt in explicitly.
//!
//! Preset helpers: [`SsrfPolicy::strict`] (also denies private; for
//! internet-only dialers), [`SsrfPolicy::permissive_for_tests`] (allows
//! everything; the only way tests can bind to `127.0.0.1`).
//!
//! ## Always-deny list
//!
//! Regardless of policy, [`ALWAYS_DENIED_HOSTS`] covers the specific IMDS
//! endpoints that mustn't be reachable even in `permissive_for_tests` mode,
//! because accidental test coverage would have ruined Capital One's day
//! (169.254.169.254, CVE-2019-11229 class). Hosts on this list are refused
//! even if an extra-allow rule would otherwise match.

use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

/// The result of an SSRF policy check. Errors are a thin set on purpose —
/// the caller usually maps them into a single [`AeonError::Security`]
/// bubble with the human-readable reason.
#[derive(Debug, thiserror::Error)]
pub enum SsrfError {
    /// The URL didn't have `scheme://host...` shape, or had no host, or had
    /// a host the URL parser couldn't extract.
    #[error("SSRF: failed to parse URL `{redacted}`: {reason}")]
    UrlParseFailed {
        /// URL with userinfo stripped (safe to log).
        redacted: String,
        reason: &'static str,
    },
    /// DNS resolution failed — typically no A/AAAA record, timeout, or
    /// network partition. Fail-closed: a host we can't resolve can't be
    /// proven to be allowed.
    #[error("SSRF: DNS resolution failed for `{host}`: {source}")]
    ResolutionFailed {
        host: String,
        #[source]
        source: std::io::Error,
    },
    /// The host resolved, but every returned IP is denied by the policy.
    /// `first_denied` is one representative IP for the error message — the
    /// full set isn't exposed because an attacker probing the guard could
    /// use it to enumerate what IPs the deployment considers internal.
    #[error(
        "SSRF: host `{host}` resolved to denied address {first_denied} \
         (reason: {reason}). Adjust SsrfPolicy or use an allowed target."
    )]
    AddressDenied {
        host: String,
        first_denied: IpAddr,
        reason: &'static str,
    },
    /// The host resolved but to zero addresses (shouldn't happen with
    /// `ToSocketAddrs` but covered explicitly — fail closed).
    #[error("SSRF: host `{host}` resolved to no addresses")]
    NoAddresses { host: String },
}

/// SSRF policy knobs.
///
/// Fields are public so operators can build a policy from YAML via serde.
/// The [`Default`] impl matches [`SsrfPolicy::production`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SsrfPolicy {
    /// Allow `127.0.0.0/8` and `::1`. Default: `false`.
    #[serde(default)]
    pub allow_loopback: bool,
    /// Allow RFC1918 (`10/8`, `172.16/12`, `192.168/16`) and RFC4193
    /// (`fc00::/7`). Default: `true` — in-VPC is the expected deployment.
    #[serde(default = "default_true")]
    pub allow_private: bool,
    /// Allow `169.254.0.0/16` and `fe80::/10`. Default: `false` — this is
    /// the primary defence against EC2/GCE/Azure metadata-server SSRF.
    #[serde(default)]
    pub allow_link_local: bool,
    /// Allow `100.64.0.0/10` (CGNAT / carrier-NAT). Default: `false`.
    #[serde(default)]
    pub allow_cgnat: bool,
    /// Extra CIDRs the operator wants denied on top of the defaults.
    /// Useful for "block our secrets subnet" rules.
    #[serde(default)]
    pub extra_deny: Vec<IpNet>,
    /// Extra CIDRs the operator wants allowed, overriding the policy
    /// defaults (but NOT the [`ALWAYS_DENIED_HOSTS`] list).
    #[serde(default)]
    pub extra_allow: Vec<IpNet>,
}

fn default_true() -> bool {
    true
}

impl Default for SsrfPolicy {
    fn default() -> Self {
        Self::production()
    }
}

/// Metadata endpoints that are refused regardless of policy. These are the
/// specific cloud-credential theft targets; a connector must never reach
/// them even in a `permissive_for_tests` context, because if a test can
/// dial IMDS, the prod guard has a bug.
pub const ALWAYS_DENIED_HOSTS: &[IpAddr] = &[
    // AWS EC2 IMDS v1/v2 — the most-exploited SSRF target on the internet.
    IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
    // Alibaba Cloud ECS metadata.
    IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200)),
    // GCP metadata.google.internal (when resolved — rare, usually accessed
    // by name, but if the operator bakes the IP they shouldn't).
    IpAddr::V4(Ipv4Addr::new(169, 254, 169, 253)),
    // Oracle Cloud metadata.
    IpAddr::V4(Ipv4Addr::new(192, 0, 0, 192)),
];

impl SsrfPolicy {
    /// Default production posture: deny loopback, link-local, CGNAT; allow
    /// private networks. Blocks the IMDS SSRF finding (C3) while keeping
    /// in-VPC webhooks workable.
    pub const fn production() -> Self {
        Self {
            allow_loopback: false,
            allow_private: true,
            allow_link_local: false,
            allow_cgnat: false,
            extra_deny: Vec::new(),
            extra_allow: Vec::new(),
        }
    }

    /// Internet-only dialer: deny every non-public address space. Use for
    /// connectors that should only ever reach SaaS endpoints.
    pub const fn strict() -> Self {
        Self {
            allow_loopback: false,
            allow_private: false,
            allow_link_local: false,
            allow_cgnat: false,
            extra_deny: Vec::new(),
            extra_allow: Vec::new(),
        }
    }

    /// Test fixture: allow loopback + private networks so `127.0.0.1`-based
    /// integration tests don't need to plumb overrides. **Still denies the
    /// [`ALWAYS_DENIED_HOSTS`] endpoints** — catching an accidental IMDS
    /// dial in tests is the whole point.
    pub const fn permissive_for_tests() -> Self {
        Self {
            allow_loopback: true,
            allow_private: true,
            allow_link_local: true,
            allow_cgnat: true,
            extra_deny: Vec::new(),
            extra_allow: Vec::new(),
        }
    }

    /// Check a single resolved IP against the policy. Returns `Ok(())` if
    /// allowed, `Err(reason_str)` if denied. The reason string is stable
    /// and safe to include in error messages.
    pub fn check_addr(&self, ip: IpAddr) -> Result<(), &'static str> {
        // 1. Hard-deny list — short-circuits even permissive/extra_allow.
        if ALWAYS_DENIED_HOSTS.contains(&ip) {
            return Err("cloud-metadata endpoint (always denied)");
        }

        // 2. Unspecified / broadcast / multicast are never legitimate
        //    dial targets for a webhook-style connector.
        if ip.is_unspecified() {
            return Err("unspecified address (0.0.0.0 / ::)");
        }
        if ip.is_multicast() {
            return Err("multicast address");
        }
        if let IpAddr::V4(v4) = ip {
            if v4.is_broadcast() {
                return Err("broadcast address");
            }
        }

        // 3. extra_deny takes precedence over extra_allow and policy
        //    defaults. Operators use this to carve out holes on top of a
        //    permissive base.
        if self.extra_deny.iter().any(|net| net.contains(&ip)) {
            return Err("operator extra_deny match");
        }

        // 4. extra_allow can unblock any address that isn't in the hard
        //    lists above. Useful for "allow this one internal webhook
        //    even though the base policy denies loopback."
        if self.extra_allow.iter().any(|net| net.contains(&ip)) {
            return Ok(());
        }

        // 5. Policy flags.
        if is_loopback(ip) && !self.allow_loopback {
            return Err("loopback address (allow_loopback=false)");
        }
        if is_link_local(ip) && !self.allow_link_local {
            return Err("link-local address (allow_link_local=false)");
        }
        if is_cgnat(ip) && !self.allow_cgnat {
            return Err("CGNAT 100.64.0.0/10 (allow_cgnat=false)");
        }
        if is_private(ip) && !self.allow_private {
            return Err("private address (allow_private=false)");
        }

        Ok(())
    }

    /// Resolve `host:port` and return the allowed [`SocketAddr`]s. Fails
    /// closed if resolution errors, every IP is denied, or the set is
    /// empty.
    ///
    /// Typical call: `policy.check_host("api.example.com", 443)?`.
    pub fn check_host(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, SsrfError> {
        let addrs: Vec<SocketAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|e| SsrfError::ResolutionFailed {
                host: host.to_owned(),
                source: e,
            })?
            .collect();

        if addrs.is_empty() {
            return Err(SsrfError::NoAddresses {
                host: host.to_owned(),
            });
        }

        let mut allowed = Vec::with_capacity(addrs.len());
        let mut first_denied: Option<(IpAddr, &'static str)> = None;
        for addr in addrs {
            match self.check_addr(addr.ip()) {
                Ok(()) => allowed.push(addr),
                Err(reason) => {
                    if first_denied.is_none() {
                        first_denied = Some((addr.ip(), reason));
                    }
                }
            }
        }

        if allowed.is_empty() {
            // By construction `first_denied` is Some whenever `addrs` was
            // non-empty and every entry was denied — but prove it
            // structurally rather than `.expect()`ing to satisfy the
            // no-panic policy in this crate.
            let (ip, reason) = match first_denied {
                Some(v) => v,
                None => {
                    return Err(SsrfError::NoAddresses {
                        host: host.to_owned(),
                    });
                }
            };
            return Err(SsrfError::AddressDenied {
                host: host.to_owned(),
                first_denied: ip,
                reason,
            });
        }

        Ok(allowed)
    }

    /// Parse a URL, extract host+port, and check. Equivalent to calling
    /// [`parse_host_port`] then [`Self::check_host`]. `url` is only used for
    /// error context — we redact userinfo before mentioning it.
    pub fn check_url(&self, url: &str) -> Result<Vec<SocketAddr>, SsrfError> {
        let (host, port) = parse_host_port(url).ok_or_else(|| SsrfError::UrlParseFailed {
            redacted: crate::redact_uri(url).into_owned(),
            reason: "missing scheme, host, or port",
        })?;
        self.check_host(&host, port)
    }
}

// ---------- internal predicates (std::net's unstable `ip` feature
//           duplicated here so we compile on stable) -------------------

fn is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(v6) => is_unique_local_v6(v6),
    }
}

fn is_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => is_unicast_link_local_v6(v6),
    }
}

fn is_cgnat(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            // 100.64.0.0/10 — stdlib's `is_shared()` is still unstable.
            let [a, b, _, _] = v4.octets();
            a == 100 && (b & 0b1100_0000) == 0b0100_0000
        }
        IpAddr::V6(_) => false,
    }
}

/// IPv6 ULA (`fc00::/7`).
fn is_unique_local_v6(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// IPv6 link-local (`fe80::/10`).
fn is_unicast_link_local_v6(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

// ---------- URL parsing helper --------------------------------------

/// Extract `(host, port)` from a URL. Returns `None` when the URL lacks a
/// scheme, a host, or has a scheme we don't know a default port for.
///
/// This is deliberately a minimal parser — it's only used for the SSRF
/// guard's host+port, not for full URL validation. The connector's own
/// transport library does the real parse right after we accept the
/// address. Handles:
///   - `scheme://host/...` (default port by scheme)
///   - `scheme://host:port/...`
///   - `scheme://user:pass@host:port/...` (userinfo skipped)
///   - `scheme://[::1]:port/...` (IPv6 bracket literal)
pub fn parse_host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    // Trim path / query / fragment.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return None;
    }

    // Strip userinfo — `rfind('@')` so a literal '@' in a password doesn't
    // split the host boundary the wrong way (matches `redact_uri`'s logic).
    let host_port = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };

    // IPv6 literal — bracketed.
    if let Some(rest6) = host_port.strip_prefix('[') {
        let (host, after) = rest6.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = if let Some(p) = after.strip_prefix(':') {
            p.parse().ok()?
        } else if after.is_empty() {
            default_port(scheme)?
        } else {
            return None;
        };
        return Some((host.to_owned(), port));
    }

    // Plain host[:port].
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (host_port, default_port(scheme)?),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_owned(), port))
}

fn default_port(scheme: &str) -> Option<u16> {
    // Schemes Aeon actually dials. Missing scheme → caller must specify
    // the port explicitly.
    Some(match scheme {
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        "amqp" => 5672,
        "amqps" => 5671,
        "redis" => 6379,
        "rediss" => 6380,
        "nats" => 4222,
        "mqtt" => 1883,
        "mqtts" => 8883,
        "postgres" | "postgresql" => 5432,
        "mysql" => 3306,
        "mongodb" | "mongodb+srv" => 27017,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn production_defaults() {
        let p = SsrfPolicy::production();
        assert!(!p.allow_loopback);
        assert!(p.allow_private);
        assert!(!p.allow_link_local);
        assert!(!p.allow_cgnat);
    }

    #[test]
    fn production_denies_imds() {
        let p = SsrfPolicy::production();
        assert!(p.check_addr(ip("169.254.169.254")).is_err());
    }

    #[test]
    fn permissive_tests_still_denies_imds() {
        // Critical: the test preset must not regress the metadata defence,
        // otherwise an integration test hitting IMDS could silently pass.
        let p = SsrfPolicy::permissive_for_tests();
        let err = p.check_addr(ip("169.254.169.254")).unwrap_err();
        assert!(err.contains("always denied"));
    }

    #[test]
    fn production_denies_loopback_v4_and_v6() {
        let p = SsrfPolicy::production();
        assert!(p.check_addr(ip("127.0.0.1")).is_err());
        assert!(p.check_addr(ip("::1")).is_err());
    }

    #[test]
    fn production_denies_link_local_v4_and_v6() {
        let p = SsrfPolicy::production();
        assert!(p.check_addr(ip("169.254.10.10")).is_err());
        assert!(p.check_addr(ip("fe80::1")).is_err());
    }

    #[test]
    fn production_denies_cgnat() {
        let p = SsrfPolicy::production();
        assert!(p.check_addr(ip("100.64.0.1")).is_err());
        assert!(p.check_addr(ip("100.127.255.254")).is_err());
        // 100.128/9 is public — outside CGNAT.
        assert!(p.check_addr(ip("100.128.0.1")).is_ok());
    }

    #[test]
    fn production_allows_public_and_private() {
        let p = SsrfPolicy::production();
        assert!(p.check_addr(ip("8.8.8.8")).is_ok());
        assert!(p.check_addr(ip("10.0.0.5")).is_ok());
        assert!(p.check_addr(ip("192.168.1.1")).is_ok());
        assert!(p.check_addr(ip("172.20.0.1")).is_ok());
        assert!(p.check_addr(ip("fc00::1")).is_ok());
    }

    #[test]
    fn strict_denies_private() {
        let p = SsrfPolicy::strict();
        assert!(p.check_addr(ip("10.0.0.1")).is_err());
        assert!(p.check_addr(ip("192.168.1.1")).is_err());
        assert!(p.check_addr(ip("8.8.8.8")).is_ok());
    }

    #[test]
    fn unspecified_multicast_broadcast_denied() {
        let p = SsrfPolicy::permissive_for_tests();
        assert!(p.check_addr(ip("0.0.0.0")).is_err());
        assert!(p.check_addr(ip("::")).is_err());
        assert!(p.check_addr(ip("224.0.0.1")).is_err());
        assert!(p.check_addr(ip("255.255.255.255")).is_err());
    }

    #[test]
    fn extra_deny_overrides_allow_flags() {
        let p = SsrfPolicy {
            allow_private: true,
            extra_deny: vec!["10.0.0.0/8".parse().unwrap()],
            ..SsrfPolicy::permissive_for_tests()
        };
        assert!(p.check_addr(ip("10.1.2.3")).is_err());
        assert!(p.check_addr(ip("192.168.1.1")).is_ok());
    }

    #[test]
    fn extra_allow_overrides_policy_default_denial() {
        let p = SsrfPolicy {
            extra_allow: vec!["127.0.0.0/8".parse().unwrap()],
            ..SsrfPolicy::production()
        };
        assert!(p.check_addr(ip("127.0.0.1")).is_ok());
    }

    #[test]
    fn extra_allow_cannot_override_always_denied() {
        let p = SsrfPolicy {
            extra_allow: vec!["169.254.169.254/32".parse().unwrap()],
            ..SsrfPolicy::production()
        };
        assert!(p.check_addr(ip("169.254.169.254")).is_err());
    }

    #[test]
    fn parse_host_port_basic() {
        assert_eq!(parse_host_port("https://host/x"), Some(("host".into(), 443)));
        assert_eq!(parse_host_port("http://host:8080/x"), Some(("host".into(), 8080)));
        assert_eq!(parse_host_port("ws://h:9/p?q"), Some(("h".into(), 9)));
        assert_eq!(parse_host_port("amqp://h"), Some(("h".into(), 5672)));
    }

    #[test]
    fn parse_host_port_ipv6_literal() {
        assert_eq!(
            parse_host_port("https://[::1]:8443/x"),
            Some(("::1".into(), 8443))
        );
        assert_eq!(
            parse_host_port("http://[fe80::1]/"),
            Some(("fe80::1".into(), 80))
        );
    }

    #[test]
    fn parse_host_port_strips_userinfo() {
        assert_eq!(
            parse_host_port("amqp://u:p@host:5672/vh"),
            Some(("host".into(), 5672))
        );
        // Literal '@' in password — rfind should still find the right boundary.
        assert_eq!(
            parse_host_port("https://user:p@ss@host:8443/"),
            Some(("host".into(), 8443))
        );
    }

    #[test]
    fn parse_host_port_rejects_malformed() {
        assert_eq!(parse_host_port(""), None);
        assert_eq!(parse_host_port("no-scheme"), None);
        assert_eq!(parse_host_port("scheme://"), None);
        assert_eq!(parse_host_port("unknown-scheme://host"), None);
    }

    #[test]
    fn check_url_resolves_and_denies_imds_hostname() {
        // Operators sometimes use the symbolic IP directly in a URL.
        let p = SsrfPolicy::production();
        let err = p.check_url("http://169.254.169.254/latest/meta-data/").unwrap_err();
        assert!(matches!(err, SsrfError::AddressDenied { .. }));
    }

    #[test]
    fn check_url_allows_public_resolvable_host() {
        let p = SsrfPolicy::production();
        // 8.8.8.8 is a literal IP — no real DNS needed; ToSocketAddrs still works.
        let addrs = p.check_url("https://8.8.8.8:443/").unwrap();
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].port(), 443);
    }

    #[test]
    fn check_host_literal_ipv6() {
        let p = SsrfPolicy::permissive_for_tests();
        let addrs = p.check_host("::1", 1234).unwrap();
        assert_eq!(addrs[0].port(), 1234);
    }

    #[test]
    fn ssrf_policy_json_roundtrip() {
        // Operators supply policy via YAML in the manifest; aeon-types
        // ships JSON serde support since serde_json is already a dep.
        // The YAML codec (aeon-cli) rides on top of the same serde derives.
        let json = r#"{
            "allow_loopback": true,
            "allow_private": true,
            "allow_link_local": false,
            "allow_cgnat": false,
            "extra_deny": ["10.0.0.0/8"],
            "extra_allow": ["127.0.0.0/8"]
        }"#;
        let p: SsrfPolicy = serde_json::from_str(json).unwrap();
        assert!(p.allow_loopback);
        assert_eq!(p.extra_deny.len(), 1);
        assert_eq!(p.extra_allow.len(), 1);
        // Serde defaults fill in when a field is omitted.
        let partial: SsrfPolicy = serde_json::from_str("{}").unwrap();
        assert!(partial.allow_private); // default_true
        assert!(!partial.allow_loopback);
    }
}
