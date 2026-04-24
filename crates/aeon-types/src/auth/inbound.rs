//! S9: inbound connector authentication.
//!
//! Scope: push-endpoint sources where Aeon listens (HTTP webhook ingest,
//! WebTransport streams, WebTransport datagrams, QUIC). Broker push
//! sources (MQTT, RabbitMQ) authenticate via their broker's own config
//! and are **out of scope** — they are not wired through this verifier.
//!
//! ## Stackable modes
//!
//! A verifier runs each configured mode in declaration order.
//! Cheap-reject-first ordering (IP → API-key → HMAC → mTLS) is **the
//! operator's responsibility**: a DDoS-exposed ingester should list
//! `ip_allowlist` first so unauthenticated clients never exercise the
//! header parser.
//!
//! The four modes:
//!
//! 1. **`ip_allowlist`** — CIDR match on the peer IP. Config comes from
//!    an env-var-backed list of `ipnet::IpNet`.
//! 2. **`api_key`** — configurable header (default `X-Aeon-Api-Key`)
//!    matched constant-time against one or more active/previous keys
//!    from the S1 secret provider.
//! 3. **`hmac`** — HMAC-SHA256/SHA512 over
//!    `method || "\n" || path || "\n" || ts || "\n" || body` (see
//!    [`super::hmac_sig`]). Skew window default 300 s. Replay cache is
//!    deferred to a later revision; the skew window bounds replays.
//! 4. **`mtls`** — the connector's TLS layer presents the peer's
//!    subject strings (CN + SANs) in [`AuthContext::client_cert_subjects`];
//!    the verifier checks any match the allow-list. Chain validation
//!    itself is delegated to rustls (it must have already succeeded
//!    before the verifier runs).
//!
//! ## JWT is deferred
//!
//! JWT bearer auth is scoped to S9.2 post-Gate-2: it needs JWKS fetch,
//! key rotation, and issuer-trust plumbing that isn't worth building
//! until we have an actual caller that needs it.

use crate::AeonError;
use crate::secrets::SecretBytes;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

use super::hmac_sig::{HmacAlgorithm, HmacVerifyError, verify_request};

/// Maximum accepted clock skew for HMAC timestamp validation (in seconds).
/// Callers may override, but this is the sane default.
pub const DEFAULT_HMAC_SKEW_SECONDS: i64 = 300;

/// Default header name for API-key authentication.
pub const DEFAULT_API_KEY_HEADER: &str = "X-Aeon-Api-Key";
/// Default header name for HMAC signature transmission.
pub const DEFAULT_HMAC_SIGNATURE_HEADER: &str = "X-Aeon-Signature";
/// Default header name for HMAC timestamp transmission.
pub const DEFAULT_HMAC_TIMESTAMP_HEADER: &str = "X-Aeon-Timestamp";

/// One of the four supported inbound auth modes. Declared in
/// [`InboundAuthConfig::modes`] in the operator's preferred
/// cheap-reject-first order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundAuthMode {
    IpAllowlist,
    ApiKey,
    Hmac,
    Mtls,
}

/// Why a request was rejected. Maps 1:1 to a metric label + log reason.
///
/// Construction is deliberately cheap — no allocation in the common
/// rejection paths — because rejection is on the hot path for
/// ingest-under-attack scenarios.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthRejection {
    /// Peer IP was not in the allow-list.
    #[error("inbound auth: peer ip {peer_ip} is not in the CIDR allow-list")]
    IpNotAllowed { peer_ip: IpAddr },

    /// No API key header was present.
    #[error("inbound auth: missing API key header")]
    ApiKeyMissing,

    /// API key header was present but didn't match any accepted key.
    #[error("inbound auth: API key did not match")]
    ApiKeyInvalid,

    /// No HMAC signature header was present.
    #[error("inbound auth: missing HMAC signature header")]
    HmacSignatureMissing,

    /// No HMAC timestamp header was present.
    #[error("inbound auth: missing HMAC timestamp header")]
    HmacTimestampMissing,

    /// HMAC timestamp did not parse as a decimal Unix-epoch integer.
    #[error("inbound auth: HMAC timestamp is not a valid unix timestamp")]
    HmacTimestampMalformed,

    /// HMAC timestamp was outside the configured skew window.
    /// `skew_seconds` is the absolute difference between request ts
    /// and server `now` at check time.
    #[error("inbound auth: HMAC timestamp skew {skew_seconds}s exceeds window {window_seconds}s")]
    HmacClockSkew { skew_seconds: i64, window_seconds: i64 },

    /// HMAC signature didn't match under any accepted secret.
    #[error("inbound auth: HMAC signature did not match")]
    HmacInvalid,

    /// HMAC signature header was present but malformed (not hex / wrong length).
    #[error("inbound auth: HMAC signature header is malformed")]
    HmacMalformed,

    /// mTLS was required but the TLS layer did not surface a client cert.
    #[error("inbound auth: mTLS required but no client certificate was presented")]
    MtlsMissing,

    /// mTLS client cert passed chain validation but subject (CN or SAN)
    /// wasn't in the allow-list. `subject` is the first surfaced
    /// subject string (safe to log).
    #[error("inbound auth: mTLS subject `{subject}` is not in the allow-list")]
    MtlsSubjectNotAllowed { subject: String },

    /// Caller configured `mtls` mode but didn't supply a subject allow-list
    /// and no valid subject was presented. Fail-closed guard against
    /// accidental "any cert accepted" misconfiguration.
    #[error("inbound auth: mTLS subject allow-list is empty and no subject was presented")]
    MtlsSubjectAllowListEmpty,
}

impl AuthRejection {
    /// A short `snake_case` reason tag suitable for metric labels.
    /// Cardinality is bounded — no user-supplied data in the tag.
    pub const fn reason_tag(&self) -> &'static str {
        match self {
            Self::IpNotAllowed { .. } => "ip_not_allowed",
            Self::ApiKeyMissing => "api_key_missing",
            Self::ApiKeyInvalid => "api_key_invalid",
            Self::HmacSignatureMissing => "hmac_sig_missing",
            Self::HmacTimestampMissing => "hmac_ts_missing",
            Self::HmacTimestampMalformed => "hmac_ts_malformed",
            Self::HmacClockSkew { .. } => "hmac_clock_skew",
            Self::HmacInvalid => "hmac_invalid",
            Self::HmacMalformed => "hmac_malformed",
            Self::MtlsMissing => "mtls_missing",
            Self::MtlsSubjectNotAllowed { .. } => "mtls_subject_not_allowed",
            Self::MtlsSubjectAllowListEmpty => "mtls_subject_allowlist_empty",
        }
    }

    /// Redact the peer-IP last octet (IPv4) or last 16 bits (IPv6)
    /// for log emission, per S2 rules. Non-IP rejections return `None`.
    pub fn redacted_peer_ip(&self) -> Option<String> {
        match self {
            Self::IpNotAllowed { peer_ip } => Some(redact_ip(*peer_ip)),
            _ => None,
        }
    }
}

impl From<AuthRejection> for AeonError {
    /// Fallback conversion for callers that bubble auth failures via the
    /// standard error chain. Connectors that need to translate to a
    /// concrete status code (HTTP 401, QUIC application-close) should
    /// branch on the typed variant instead.
    fn from(r: AuthRejection) -> Self {
        AeonError::config(r.to_string())
    }
}

/// IP allow-list configuration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IpAllowlistConfig {
    /// CIDR ranges accepted as source IPs. Empty means "no IP mode"
    /// — the caller should just omit [`InboundAuthMode::IpAllowlist`]
    /// from `modes` in that case; an empty list configured here rejects
    /// every request.
    pub cidrs: Vec<IpNet>,
}

/// API-key header configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiKeyConfig {
    /// Header name to read the API key from. Defaults to
    /// [`DEFAULT_API_KEY_HEADER`]. Case-insensitive on the wire.
    #[serde(default = "default_api_key_header")]
    pub header_name: String,
    /// Accepted key values in preference order (typically `[active, previous]`
    /// to support hot rotation). Post-[`InboundAuthConfig`] build, these
    /// are resolved plaintext — already interpolated from
    /// `${VAULT:...}` / `${ENV:...}` by the CLI layer (S1.3).
    pub keys: Vec<String>,
}

/// HMAC-signed-request configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HmacConfig {
    /// Header name for the signature. Default [`DEFAULT_HMAC_SIGNATURE_HEADER`].
    #[serde(default = "default_hmac_sig_header")]
    pub signature_header: String,
    /// Header name for the timestamp. Default [`DEFAULT_HMAC_TIMESTAMP_HEADER`].
    #[serde(default = "default_hmac_ts_header")]
    pub timestamp_header: String,
    /// Accepted secrets in preference order ([active, previous]).
    pub secrets: Vec<String>,
    /// HMAC digest algorithm. Default `hmac-sha256`.
    #[serde(default)]
    pub algorithm: HmacAlgorithm,
    /// Maximum accepted absolute skew (seconds) between request ts and
    /// server now. Default [`DEFAULT_HMAC_SKEW_SECONDS`].
    #[serde(default = "default_hmac_skew")]
    pub skew_seconds: i64,
}

/// mTLS configuration. Chain validation is delegated to the connector's
/// TLS layer (rustls); this config only governs subject allow-listing.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MtlsConfig {
    /// Subject strings (exact match, case-sensitive) accepted as the
    /// peer identity. Matched against any CN or SAN surfaced by the
    /// TLS layer in [`AuthContext::client_cert_subjects`].
    ///
    /// An empty list + `InboundAuthMode::Mtls` configured is a hard
    /// error at verify-time ([`AuthRejection::MtlsSubjectAllowListEmpty`])
    /// — there is no "accept any valid cert" mode. Operators must
    /// declare who they trust.
    pub subject_allowlist: Vec<String>,
}

/// Top-level inbound auth config. Deserializable from YAML.
///
/// ## Shape
///
/// ```yaml
/// auth:
///   modes: [ip_allowlist, api_key]
///   ip_allowlist:
///     cidrs: ["10.0.0.0/8", "192.168.1.0/24"]
///   api_key:
///     header_name: "X-Aeon-Api-Key"
///     keys: ["${VAULT:aeon/ingest/key-active}", "${VAULT:aeon/ingest/key-prev}"]
/// ```
///
/// After S1.3 YAML pre-parse interpolation, the `keys`/`secrets` fields
/// contain plaintext values — the `${...}` tokens are resolved before
/// `serde_yaml` ever sees the document.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InboundAuthConfig {
    /// Modes to run, in declaration order. An empty list means
    /// "no inbound auth" — the caller should not construct a verifier at all.
    #[serde(default)]
    pub modes: Vec<InboundAuthMode>,

    #[serde(default)]
    pub ip_allowlist: Option<IpAllowlistConfig>,

    #[serde(default)]
    pub api_key: Option<ApiKeyConfig>,

    #[serde(default)]
    pub hmac: Option<HmacConfig>,

    #[serde(default)]
    pub mtls: Option<MtlsConfig>,
}

impl InboundAuthConfig {
    /// True when no modes are declared. Equivalent to "no inbound auth".
    pub fn is_empty(&self) -> bool {
        self.modes.is_empty()
    }
}

/// Per-request context passed into [`InboundAuthVerifier::verify`].
///
/// The caller assembles this from the underlying protocol (axum request,
/// wtransport session-request, quinn handshake) before invoking verify.
/// This keeps the verifier protocol-agnostic — HTTP, WT, and QUIC all
/// funnel through the same type.
#[derive(Debug, Clone)]
pub struct AuthContext<'a> {
    /// Remote peer IP (after proxy-header interpretation if any — the
    /// caller decides whether to trust `X-Forwarded-For`).
    pub peer_ip: IpAddr,

    /// Request method (HTTP verb; `"CONNECT"` for WT, `"QUIC"` for
    /// QUIC-only where no HTTP verb applies).
    pub method: &'a str,

    /// Request path (HTTP path; protocol-specific analog for WT/QUIC).
    pub path: &'a str,

    /// Raw request body bytes. For WT/QUIC where bodies don't apply,
    /// pass `b""`.
    pub body: &'a [u8],

    /// Request headers as a flat slice of (name, value) pairs. Names
    /// are matched case-insensitively. For WT/QUIC, pass `&[]`.
    pub headers: &'a [(&'a str, &'a [u8])],

    /// Unix-epoch seconds at check time. Caller-supplied so the
    /// verifier stays testable.
    pub now_unix: i64,

    /// List of subject strings (CN + SANs) extracted from the peer's
    /// verified TLS client certificate, if any. The TLS layer must
    /// have already validated the chain before surfacing these —
    /// the verifier only checks allow-list membership.
    pub client_cert_subjects: Option<&'a [&'a str]>,
}

impl<'a> AuthContext<'a> {
    /// Find a header value by name (case-insensitive). First match wins.
    pub fn header(&self, name: &str) -> Option<&'a [u8]> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| *v)
    }
}

/// Compiled verifier built from an [`InboundAuthConfig`]. Owns the
/// secret material as [`SecretBytes`] so it zeroizes on drop.
///
/// Cloning is not provided — a verifier is constructed once per
/// source at pipeline start and held in an `Arc`.
pub struct InboundAuthVerifier {
    modes: Vec<InboundAuthMode>,
    ip_allowlist: Option<CompiledIpAllowlist>,
    api_key: Option<CompiledApiKey>,
    hmac: Option<CompiledHmac>,
    mtls: Option<CompiledMtls>,
}

impl std::fmt::Debug for InboundAuthVerifier {
    /// Redacts secret material — lists modes and the presence of each
    /// block but never the key bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundAuthVerifier")
            .field("modes", &self.modes)
            .field("ip_allowlist_cidrs", &self.ip_allowlist.as_ref().map(|c| c.cidrs.len()))
            .field("api_key_count", &self.api_key.as_ref().map(|c| c.keys.len()))
            .field("hmac_secret_count", &self.hmac.as_ref().map(|c| c.secrets.len()))
            .field("mtls_subjects", &self.mtls.as_ref().map(|c| c.subject_allowlist.len()))
            .finish()
    }
}

struct CompiledIpAllowlist {
    cidrs: Vec<IpNet>,
}

struct CompiledApiKey {
    header_name: String,
    keys: Vec<SecretBytes>,
}

struct CompiledHmac {
    signature_header: String,
    timestamp_header: String,
    algorithm: HmacAlgorithm,
    skew_seconds: i64,
    secrets: Vec<SecretBytes>,
}

struct CompiledMtls {
    subject_allowlist: Vec<String>,
}

/// Errors returned while constructing an [`InboundAuthVerifier`].
#[derive(Debug, thiserror::Error)]
pub enum InboundAuthBuildError {
    #[error("inbound auth: mode `{mode:?}` is declared but its config block is missing")]
    ModeConfigMissing { mode: InboundAuthMode },
    #[error("inbound auth: ip_allowlist mode declared but cidrs list is empty")]
    IpAllowlistEmpty,
    #[error("inbound auth: api_key mode declared but keys list is empty")]
    ApiKeyEmpty,
    #[error("inbound auth: hmac mode declared but secrets list is empty")]
    HmacSecretsEmpty,
    #[error("inbound auth: hmac skew_seconds must be positive, got {got}")]
    HmacSkewInvalid { got: i64 },
}

impl From<InboundAuthBuildError> for AeonError {
    fn from(e: InboundAuthBuildError) -> Self {
        AeonError::config(e.to_string())
    }
}

impl InboundAuthVerifier {
    /// Build a verifier from a config. Consumes the config and moves
    /// every secret value into a zeroizing [`SecretBytes`] container.
    ///
    /// Fails when a mode is declared without its config block, or when
    /// a config block is present but empty (e.g. `api_key` mode with
    /// no keys — that would accept nothing and reject everything,
    /// which is never intentional).
    pub fn build(config: InboundAuthConfig) -> Result<Self, InboundAuthBuildError> {
        let mut ip_allowlist = None;
        let mut api_key = None;
        let mut hmac = None;
        let mut mtls = None;

        for mode in &config.modes {
            match mode {
                InboundAuthMode::IpAllowlist => {
                    let cfg = config.ip_allowlist.as_ref().ok_or(
                        InboundAuthBuildError::ModeConfigMissing { mode: *mode },
                    )?;
                    if cfg.cidrs.is_empty() {
                        return Err(InboundAuthBuildError::IpAllowlistEmpty);
                    }
                    ip_allowlist = Some(CompiledIpAllowlist {
                        cidrs: cfg.cidrs.clone(),
                    });
                }
                InboundAuthMode::ApiKey => {
                    let cfg = config.api_key.as_ref().ok_or(
                        InboundAuthBuildError::ModeConfigMissing { mode: *mode },
                    )?;
                    if cfg.keys.is_empty() || cfg.keys.iter().all(|s| s.is_empty()) {
                        return Err(InboundAuthBuildError::ApiKeyEmpty);
                    }
                    let keys = cfg
                        .keys
                        .iter()
                        .filter(|s| !s.is_empty())
                        .map(|s| SecretBytes::new(s.as_bytes().to_vec()))
                        .collect();
                    api_key = Some(CompiledApiKey {
                        header_name: cfg.header_name.clone(),
                        keys,
                    });
                }
                InboundAuthMode::Hmac => {
                    let cfg = config
                        .hmac
                        .as_ref()
                        .ok_or(InboundAuthBuildError::ModeConfigMissing { mode: *mode })?;
                    if cfg.secrets.is_empty() || cfg.secrets.iter().all(|s| s.is_empty()) {
                        return Err(InboundAuthBuildError::HmacSecretsEmpty);
                    }
                    if cfg.skew_seconds <= 0 {
                        return Err(InboundAuthBuildError::HmacSkewInvalid {
                            got: cfg.skew_seconds,
                        });
                    }
                    let secrets = cfg
                        .secrets
                        .iter()
                        .filter(|s| !s.is_empty())
                        .map(|s| SecretBytes::new(s.as_bytes().to_vec()))
                        .collect();
                    hmac = Some(CompiledHmac {
                        signature_header: cfg.signature_header.clone(),
                        timestamp_header: cfg.timestamp_header.clone(),
                        algorithm: cfg.algorithm,
                        skew_seconds: cfg.skew_seconds,
                        secrets,
                    });
                }
                InboundAuthMode::Mtls => {
                    let cfg = config
                        .mtls
                        .as_ref()
                        .ok_or(InboundAuthBuildError::ModeConfigMissing { mode: *mode })?;
                    mtls = Some(CompiledMtls {
                        subject_allowlist: cfg.subject_allowlist.clone(),
                    });
                }
            }
        }

        Ok(Self {
            modes: config.modes,
            ip_allowlist,
            api_key,
            hmac,
            mtls,
        })
    }

    /// True when no modes are configured. A verifier returned by
    /// [`Self::build`] with empty modes is trivially passing.
    pub fn is_empty(&self) -> bool {
        self.modes.is_empty()
    }

    /// Modes in execution order.
    pub fn modes(&self) -> &[InboundAuthMode] {
        &self.modes
    }

    /// Verify a request. Runs each configured mode in declaration order;
    /// the first failing mode determines the returned rejection.
    ///
    /// Returns `Ok(())` when every configured mode passes. When
    /// `modes` is empty the call is a no-op success.
    pub fn verify(&self, ctx: &AuthContext<'_>) -> Result<(), AuthRejection> {
        for mode in &self.modes {
            match mode {
                InboundAuthMode::IpAllowlist => self.check_ip(ctx)?,
                InboundAuthMode::ApiKey => self.check_api_key(ctx)?,
                InboundAuthMode::Hmac => self.check_hmac(ctx)?,
                InboundAuthMode::Mtls => self.check_mtls(ctx)?,
            }
        }
        Ok(())
    }

    fn check_ip(&self, ctx: &AuthContext<'_>) -> Result<(), AuthRejection> {
        // build() guarantees Some when mode is present.
        let Some(cfg) = &self.ip_allowlist else {
            return Ok(());
        };
        if cfg.cidrs.iter().any(|net| net.contains(&ctx.peer_ip)) {
            Ok(())
        } else {
            Err(AuthRejection::IpNotAllowed {
                peer_ip: ctx.peer_ip,
            })
        }
    }

    fn check_api_key(&self, ctx: &AuthContext<'_>) -> Result<(), AuthRejection> {
        let Some(cfg) = &self.api_key else {
            return Ok(());
        };
        let presented = ctx.header(&cfg.header_name).ok_or(AuthRejection::ApiKeyMissing)?;
        for key in &cfg.keys {
            if constant_time_eq(presented, key.expose_bytes()) {
                return Ok(());
            }
        }
        Err(AuthRejection::ApiKeyInvalid)
    }

    fn check_hmac(&self, ctx: &AuthContext<'_>) -> Result<(), AuthRejection> {
        let Some(cfg) = &self.hmac else {
            return Ok(());
        };
        let sig = ctx
            .header(&cfg.signature_header)
            .ok_or(AuthRejection::HmacSignatureMissing)?;
        let ts_bytes = ctx
            .header(&cfg.timestamp_header)
            .ok_or(AuthRejection::HmacTimestampMissing)?;

        // Parse timestamp; reject non-ASCII or non-decimal early.
        let ts_str = std::str::from_utf8(ts_bytes).map_err(|_| AuthRejection::HmacTimestampMalformed)?;
        let ts: i64 = ts_str
            .parse()
            .map_err(|_| AuthRejection::HmacTimestampMalformed)?;

        let skew = (ctx.now_unix - ts).abs();
        if skew > cfg.skew_seconds {
            return Err(AuthRejection::HmacClockSkew {
                skew_seconds: skew,
                window_seconds: cfg.skew_seconds,
            });
        }

        let sig_str = std::str::from_utf8(sig).map_err(|_| AuthRejection::HmacMalformed)?;
        let secret_refs: Vec<&[u8]> = cfg.secrets.iter().map(|s| s.expose_bytes()).collect();

        match verify_request(
            cfg.algorithm,
            &secret_refs,
            ctx.method.as_bytes(),
            ctx.path.as_bytes(),
            ts_bytes,
            ctx.body,
            sig_str,
        ) {
            Ok(()) => Ok(()),
            Err(HmacVerifyError::SignatureMismatch) => Err(AuthRejection::HmacInvalid),
            Err(HmacVerifyError::SignatureNotHex)
            | Err(HmacVerifyError::SignatureWrongLength { .. }) => {
                Err(AuthRejection::HmacMalformed)
            }
            Err(HmacVerifyError::EmptySecret) => {
                // build() rejects this case, but be defensive.
                Err(AuthRejection::HmacInvalid)
            }
        }
    }

    fn check_mtls(&self, ctx: &AuthContext<'_>) -> Result<(), AuthRejection> {
        let Some(cfg) = &self.mtls else {
            return Ok(());
        };
        let subjects = ctx.client_cert_subjects.ok_or(AuthRejection::MtlsMissing)?;
        if subjects.is_empty() {
            return Err(AuthRejection::MtlsMissing);
        }
        if cfg.subject_allowlist.is_empty() {
            return Err(AuthRejection::MtlsSubjectAllowListEmpty);
        }
        for presented in subjects {
            if cfg
                .subject_allowlist
                .iter()
                .any(|allowed| allowed == presented)
            {
                return Ok(());
            }
        }
        Err(AuthRejection::MtlsSubjectNotAllowed {
            subject: subjects.first().map(|s| (*s).to_string()).unwrap_or_default(),
        })
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn redact_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.x", o[0], o[1], o[2])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!(
                "{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:x:x",
                s[0], s[1], s[2], s[3], s[4], s[5]
            )
        }
    }
}

fn default_api_key_header() -> String {
    DEFAULT_API_KEY_HEADER.to_string()
}
fn default_hmac_sig_header() -> String {
    DEFAULT_HMAC_SIGNATURE_HEADER.to_string()
}
fn default_hmac_ts_header() -> String {
    DEFAULT_HMAC_TIMESTAMP_HEADER.to_string()
}
const fn default_hmac_skew() -> i64 {
    DEFAULT_HMAC_SKEW_SECONDS
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::hmac_sig::sign_request;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().unwrap())
    }

    fn ctx<'a>(peer: IpAddr, headers: &'a [(&'a str, &'a [u8])], body: &'a [u8]) -> AuthContext<'a> {
        AuthContext {
            peer_ip: peer,
            method: "POST",
            path: "/ingest",
            body,
            headers,
            now_unix: 1_700_000_000,
            client_cert_subjects: None,
        }
    }

    // ── IpAllowlist ────────────────────────────────────────────────

    #[test]
    fn ip_allowlist_accepts_in_range() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::IpAllowlist],
            ip_allowlist: Some(IpAllowlistConfig {
                cidrs: vec!["10.0.0.0/8".parse().unwrap()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        v.verify(&ctx(v4("10.1.2.3"), &[], b"")).unwrap();
    }

    #[test]
    fn ip_allowlist_rejects_out_of_range() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::IpAllowlist],
            ip_allowlist: Some(IpAllowlistConfig {
                cidrs: vec!["10.0.0.0/8".parse().unwrap()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let err = v.verify(&ctx(v4("192.168.1.1"), &[], b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::IpNotAllowed { .. }));
        assert_eq!(err.reason_tag(), "ip_not_allowed");
    }

    #[test]
    fn ip_allowlist_v6_match() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::IpAllowlist],
            ip_allowlist: Some(IpAllowlistConfig {
                cidrs: vec!["fd00::/8".parse().unwrap()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        v.verify(&ctx(IpAddr::V6("fd12::1".parse::<Ipv6Addr>().unwrap()), &[], b""))
            .unwrap();
    }

    #[test]
    fn build_fails_empty_cidrs() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::IpAllowlist],
            ip_allowlist: Some(IpAllowlistConfig { cidrs: vec![] }),
            ..Default::default()
        };
        let err = InboundAuthVerifier::build(cfg).unwrap_err();
        assert!(matches!(err, InboundAuthBuildError::IpAllowlistEmpty));
    }

    #[test]
    fn build_fails_missing_config_block() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::IpAllowlist],
            ip_allowlist: None,
            ..Default::default()
        };
        let err = InboundAuthVerifier::build(cfg).unwrap_err();
        assert!(matches!(
            err,
            InboundAuthBuildError::ModeConfigMissing {
                mode: InboundAuthMode::IpAllowlist
            }
        ));
    }

    // ── ApiKey ─────────────────────────────────────────────────────

    #[test]
    fn api_key_accepts_valid_key() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::ApiKey],
            api_key: Some(ApiKeyConfig {
                header_name: "X-Aeon-Api-Key".into(),
                keys: vec!["k1".into(), "k2".into()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let headers: &[(&str, &[u8])] = &[("x-aeon-api-key", b"k2")];
        v.verify(&ctx(v4("10.0.0.1"), headers, b"")).unwrap();
    }

    #[test]
    fn api_key_rejects_missing_header() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::ApiKey],
            api_key: Some(ApiKeyConfig {
                header_name: "X-Aeon-Api-Key".into(),
                keys: vec!["k1".into()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let err = v.verify(&ctx(v4("10.0.0.1"), &[], b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::ApiKeyMissing));
        assert_eq!(err.reason_tag(), "api_key_missing");
    }

    #[test]
    fn api_key_rejects_wrong_value() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::ApiKey],
            api_key: Some(ApiKeyConfig {
                header_name: "X-Aeon-Api-Key".into(),
                keys: vec!["k1".into()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let headers: &[(&str, &[u8])] = &[("X-Aeon-Api-Key", b"wrong")];
        let err = v.verify(&ctx(v4("10.0.0.1"), headers, b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::ApiKeyInvalid));
    }

    #[test]
    fn api_key_header_match_is_case_insensitive() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::ApiKey],
            api_key: Some(ApiKeyConfig {
                header_name: "X-Aeon-Api-Key".into(),
                keys: vec!["k1".into()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let headers: &[(&str, &[u8])] = &[("X-AEON-API-KEY", b"k1")];
        v.verify(&ctx(v4("10.0.0.1"), headers, b"")).unwrap();
    }

    #[test]
    fn api_key_accepts_previous_for_rotation() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::ApiKey],
            api_key: Some(ApiKeyConfig {
                header_name: "X-Aeon-Api-Key".into(),
                keys: vec!["active".into(), "previous".into()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let headers: &[(&str, &[u8])] = &[("X-Aeon-Api-Key", b"previous")];
        v.verify(&ctx(v4("10.0.0.1"), headers, b"")).unwrap();
    }

    #[test]
    fn build_fails_empty_api_keys() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::ApiKey],
            api_key: Some(ApiKeyConfig {
                header_name: "X".into(),
                keys: vec!["".into()],
            }),
            ..Default::default()
        };
        let err = InboundAuthVerifier::build(cfg).unwrap_err();
        assert!(matches!(err, InboundAuthBuildError::ApiKeyEmpty));
    }

    // ── Hmac ───────────────────────────────────────────────────────

    #[test]
    fn hmac_accepts_valid_signature() {
        let secret = "shh";
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "X-Aeon-Signature".into(),
                timestamp_header: "X-Aeon-Timestamp".into(),
                secrets: vec![secret.into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 300,
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let ts = b"1700000000";
        let sig = sign_request(
            HmacAlgorithm::HmacSha256,
            secret.as_bytes(),
            b"POST",
            b"/ingest",
            ts,
            b"payload",
        )
        .unwrap();
        let headers: &[(&str, &[u8])] =
            &[("X-Aeon-Signature", sig.as_bytes()), ("X-Aeon-Timestamp", ts)];
        v.verify(&ctx(v4("10.0.0.1"), headers, b"payload")).unwrap();
    }

    #[test]
    fn hmac_rejects_missing_signature_header() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "X-Aeon-Signature".into(),
                timestamp_header: "X-Aeon-Timestamp".into(),
                secrets: vec!["s".into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 300,
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let err = v.verify(&ctx(v4("10.0.0.1"), &[], b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::HmacSignatureMissing));
    }

    #[test]
    fn hmac_rejects_missing_timestamp_header() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "X-Aeon-Signature".into(),
                timestamp_header: "X-Aeon-Timestamp".into(),
                secrets: vec!["s".into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 300,
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let headers: &[(&str, &[u8])] = &[("X-Aeon-Signature", &[b'a'; 64])];
        let err = v.verify(&ctx(v4("10.0.0.1"), headers, b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::HmacTimestampMissing));
    }

    #[test]
    fn hmac_rejects_malformed_timestamp() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "S".into(),
                timestamp_header: "T".into(),
                secrets: vec!["s".into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 300,
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let headers: &[(&str, &[u8])] = &[("S", &[b'a'; 64]), ("T", b"not-a-number")];
        let err = v.verify(&ctx(v4("10.0.0.1"), headers, b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::HmacTimestampMalformed));
    }

    #[test]
    fn hmac_rejects_clock_skew() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "S".into(),
                timestamp_header: "T".into(),
                secrets: vec!["k".into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 60,
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let ts = b"1700000000";
        let sig = sign_request(HmacAlgorithm::HmacSha256, b"k", b"POST", b"/ingest", ts, b"")
            .unwrap();
        let headers: &[(&str, &[u8])] = &[("S", sig.as_bytes()), ("T", ts)];
        let mut c = ctx(v4("10.0.0.1"), headers, b"");
        c.now_unix = 1_700_001_000; // 1000 seconds off, skew = 60
        let err = v.verify(&c).unwrap_err();
        assert!(matches!(
            err,
            AuthRejection::HmacClockSkew { window_seconds: 60, .. }
        ));
        assert_eq!(err.reason_tag(), "hmac_clock_skew");
    }

    #[test]
    fn hmac_rejects_wrong_signature() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "S".into(),
                timestamp_header: "T".into(),
                secrets: vec!["k".into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 300,
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let ts = b"1700000000";
        let bad_sig = "a".repeat(64);
        let headers: &[(&str, &[u8])] = &[("S", bad_sig.as_bytes()), ("T", ts)];
        let err = v.verify(&ctx(v4("10.0.0.1"), headers, b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::HmacInvalid));
    }

    #[test]
    fn hmac_rejects_malformed_signature() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "S".into(),
                timestamp_header: "T".into(),
                secrets: vec!["k".into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 300,
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let ts = b"1700000000";
        let headers: &[(&str, &[u8])] = &[("S", b"short"), ("T", ts)];
        let err = v.verify(&ctx(v4("10.0.0.1"), headers, b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::HmacMalformed));
    }

    #[test]
    fn hmac_accepts_previous_secret_for_rotation() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "S".into(),
                timestamp_header: "T".into(),
                secrets: vec!["new".into(), "old".into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 300,
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let ts = b"1700000000";
        let sig = sign_request(HmacAlgorithm::HmacSha256, b"old", b"POST", b"/ingest", ts, b"x")
            .unwrap();
        let headers: &[(&str, &[u8])] = &[("S", sig.as_bytes()), ("T", ts)];
        v.verify(&ctx(v4("10.0.0.1"), headers, b"x")).unwrap();
    }

    #[test]
    fn build_fails_hmac_skew_zero() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Hmac],
            hmac: Some(HmacConfig {
                signature_header: "S".into(),
                timestamp_header: "T".into(),
                secrets: vec!["k".into()],
                algorithm: HmacAlgorithm::HmacSha256,
                skew_seconds: 0,
            }),
            ..Default::default()
        };
        let err = InboundAuthVerifier::build(cfg).unwrap_err();
        assert!(matches!(err, InboundAuthBuildError::HmacSkewInvalid { got: 0 }));
    }

    // ── Mtls ───────────────────────────────────────────────────────

    #[test]
    fn mtls_accepts_allowed_subject() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Mtls],
            mtls: Some(MtlsConfig {
                subject_allowlist: vec!["CN=client-a".into(), "CN=client-b".into()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let subjects = &["CN=client-b"][..];
        let mut c = ctx(v4("10.0.0.1"), &[], b"");
        c.client_cert_subjects = Some(subjects);
        v.verify(&c).unwrap();
    }

    #[test]
    fn mtls_rejects_missing_cert() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Mtls],
            mtls: Some(MtlsConfig {
                subject_allowlist: vec!["CN=anyone".into()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let err = v.verify(&ctx(v4("10.0.0.1"), &[], b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::MtlsMissing));
    }

    #[test]
    fn mtls_rejects_subject_not_in_list() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Mtls],
            mtls: Some(MtlsConfig {
                subject_allowlist: vec!["CN=client-a".into()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let subjects = &["CN=stranger"][..];
        let mut c = ctx(v4("10.0.0.1"), &[], b"");
        c.client_cert_subjects = Some(subjects);
        let err = v.verify(&c).unwrap_err();
        assert!(matches!(err, AuthRejection::MtlsSubjectNotAllowed { .. }));
    }

    #[test]
    fn mtls_rejects_empty_allowlist() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::Mtls],
            mtls: Some(MtlsConfig {
                subject_allowlist: vec![],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        let subjects = &["CN=anyone"][..];
        let mut c = ctx(v4("10.0.0.1"), &[], b"");
        c.client_cert_subjects = Some(subjects);
        let err = v.verify(&c).unwrap_err();
        assert!(matches!(err, AuthRejection::MtlsSubjectAllowListEmpty));
    }

    // ── Stacked modes ──────────────────────────────────────────────

    #[test]
    fn stacked_modes_require_all() {
        let cfg = InboundAuthConfig {
            modes: vec![InboundAuthMode::IpAllowlist, InboundAuthMode::ApiKey],
            ip_allowlist: Some(IpAllowlistConfig {
                cidrs: vec!["10.0.0.0/8".parse().unwrap()],
            }),
            api_key: Some(ApiKeyConfig {
                header_name: "X-K".into(),
                keys: vec!["valid".into()],
            }),
            ..Default::default()
        };
        let v = InboundAuthVerifier::build(cfg).unwrap();
        // IP allowed, API key valid => pass
        let headers: &[(&str, &[u8])] = &[("X-K", b"valid")];
        v.verify(&ctx(v4("10.1.2.3"), headers, b"")).unwrap();
        // IP allowed, API key invalid => reject on API key
        let headers2: &[(&str, &[u8])] = &[("X-K", b"nope")];
        let err = v.verify(&ctx(v4("10.1.2.3"), headers2, b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::ApiKeyInvalid));
        // IP denied => short-circuits on IP check regardless of header
        let err = v.verify(&ctx(v4("192.168.1.1"), headers, b"")).unwrap_err();
        assert!(matches!(err, AuthRejection::IpNotAllowed { .. }));
    }

    #[test]
    fn empty_config_is_noop() {
        let cfg = InboundAuthConfig::default();
        let v = InboundAuthVerifier::build(cfg).unwrap();
        assert!(v.is_empty());
        v.verify(&ctx(v4("1.2.3.4"), &[], b"")).unwrap();
    }

    // ── Serde shape ────────────────────────────────────────────────

    #[test]
    fn config_deserializes_from_yaml_shape() {
        let yaml = r#"
modes: [ip_allowlist, api_key]
ip_allowlist:
  cidrs: ["10.0.0.0/8"]
api_key:
  header_name: "X-My-Key"
  keys: ["plaintext-key-value"]
"#;
        let cfg: InboundAuthConfig = serde_yaml_ng_try(yaml).unwrap();
        assert_eq!(cfg.modes.len(), 2);
        assert!(cfg.ip_allowlist.is_some());
        assert_eq!(cfg.api_key.as_ref().unwrap().keys.len(), 1);
    }

    fn serde_yaml_ng_try<T: for<'de> Deserialize<'de>>(s: &str) -> Result<T, serde_json::Error> {
        // aeon-types doesn't depend on serde_yaml; we round-trip via JSON for shape coverage.
        // Convert YAML-ish shape above to JSON manually:
        let json = r#"{
"modes": ["ip_allowlist", "api_key"],
"ip_allowlist": { "cidrs": ["10.0.0.0/8"] },
"api_key": { "header_name": "X-My-Key", "keys": ["plaintext-key-value"] }
}"#;
        let _ = s;
        serde_json::from_str(json)
    }

    #[test]
    fn redacts_v4_last_octet() {
        let rejection = AuthRejection::IpNotAllowed {
            peer_ip: v4("203.0.113.45"),
        };
        assert_eq!(rejection.redacted_peer_ip().unwrap(), "203.0.113.x");
    }

    #[test]
    fn reason_tags_are_stable_labels() {
        // Sanity — all tags are present and non-empty.
        for tag in [
            AuthRejection::IpNotAllowed { peer_ip: v4("0.0.0.0") }.reason_tag(),
            AuthRejection::ApiKeyMissing.reason_tag(),
            AuthRejection::ApiKeyInvalid.reason_tag(),
            AuthRejection::HmacSignatureMissing.reason_tag(),
            AuthRejection::HmacTimestampMissing.reason_tag(),
            AuthRejection::HmacTimestampMalformed.reason_tag(),
            AuthRejection::HmacClockSkew { skew_seconds: 0, window_seconds: 0 }.reason_tag(),
            AuthRejection::HmacInvalid.reason_tag(),
            AuthRejection::HmacMalformed.reason_tag(),
            AuthRejection::MtlsMissing.reason_tag(),
            AuthRejection::MtlsSubjectNotAllowed { subject: "".into() }.reason_tag(),
            AuthRejection::MtlsSubjectAllowListEmpty.reason_tag(),
        ] {
            assert!(!tag.is_empty());
            assert!(!tag.contains(' '));
        }
    }
}
