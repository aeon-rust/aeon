//! Secret reference + provider abstraction for Aeon (S1 foundation).
//!
//! Every sensitive configuration value (API keys, tokens, passwords, KEK
//! identifiers, TLS private keys) passes through this layer. YAML manifests
//! and env vars reference secrets via `${SCHEME:path}` syntax; providers
//! resolve references at pipeline-start time.
//!
//! ## Schemes
//!
//! | Token                       | Resolves via                                               |
//! | --------------------------- | ---------------------------------------------------------- |
//! | `${ENV:NAME}`               | process environment variable (always available)            |
//! | `${DOTENV:NAME}`            | key=value file at `AEON_DOTENV_PATH` (last-resort fallback) |
//! | `${VAULT:path/key}`         | HashiCorp Vault — provider supplied externally (S1.2 stub) |
//! | `${AWS_SM:name[#json_key]}` | AWS Secrets Manager — S1.2 stub                            |
//! | `${AWS_KMS:key-id}`         | AWS KMS — S1.2 stub                                        |
//!
//! A bare string that does not match `${SCHEME:path}` is treated as a
//! literal and resolved via the LiteralProvider, which emits a warn-once
//! log event. Literal values are dev-only.
//!
//! ## Zeroization
//!
//! Resolved bytes are wrapped in [`SecretBytes`], which:
//! - zeroizes on drop
//! - has no `Debug` that prints bytes (shows `<redacted, N bytes>`)
//! - has no `Clone` (forces explicit intent if duplication is needed)
//! - exposes bytes only through `expose_bytes()` / `expose_str()` —
//!   grep-friendly names for security audit.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

// ─── Error ──────────────────────────────────────────────────────────

/// Errors raised by secret resolution or `${...}` interpolation.
#[derive(thiserror::Error, Debug)]
pub enum SecretError {
    #[error("malformed secret ref '{0}': expected `${{SCHEME:path}}`")]
    MalformedRef(String),

    #[error("unknown secret scheme '{0}'")]
    UnknownScheme(String),

    #[error("secret scheme {0:?} not registered with this SecretRegistry")]
    SchemeNotRegistered(SecretScheme),

    #[error("env var '{0}' not set")]
    EnvNotSet(String),

    #[error("dotenv path not configured: set AEON_DOTENV_PATH")]
    DotEnvPathMissing,

    #[error("dotenv file '{path}' error: {message}")]
    DotEnvIo { path: PathBuf, message: String },

    #[error("dotenv key '{0}' not found in file")]
    DotEnvKeyMissing(String),

    #[error("{scheme:?} provider error: {message}")]
    Provider {
        scheme: SecretScheme,
        message: String,
    },

    #[error("resolved secret ({0} bytes) is not valid UTF-8")]
    NotUtf8(usize),
}

// ─── Scheme enum ────────────────────────────────────────────────────

/// Which backend handles a secret reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretScheme {
    Env,
    DotEnv,
    Vault,
    AwsSm,
    AwsKms,
    /// Literal YAML value — dev-only, warns once on first resolve.
    Literal,
}

impl SecretScheme {
    pub fn as_str(&self) -> &'static str {
        match self {
            SecretScheme::Env => "ENV",
            SecretScheme::DotEnv => "DOTENV",
            SecretScheme::Vault => "VAULT",
            SecretScheme::AwsSm => "AWS_SM",
            SecretScheme::AwsKms => "AWS_KMS",
            SecretScheme::Literal => "LITERAL",
        }
    }

    /// Parse an uppercase scheme tag like `ENV` / `AWS_SM`. Returns None
    /// for any unrecognized value, including `LITERAL` — literals are the
    /// implicit fallback, never spelled explicitly in a `${...}` token.
    pub fn from_str_upper(s: &str) -> Option<Self> {
        match s {
            "ENV" => Some(SecretScheme::Env),
            "DOTENV" => Some(SecretScheme::DotEnv),
            "VAULT" => Some(SecretScheme::Vault),
            "AWS_SM" => Some(SecretScheme::AwsSm),
            "AWS_KMS" => Some(SecretScheme::AwsKms),
            _ => None,
        }
    }
}

// ─── SecretRef ──────────────────────────────────────────────────────

/// A parsed secret reference: scheme plus backend-specific path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    pub scheme: SecretScheme,
    pub path: String,
}

impl SecretRef {
    pub fn env(name: impl Into<String>) -> Self {
        Self {
            scheme: SecretScheme::Env,
            path: name.into(),
        }
    }

    pub fn vault(path: impl Into<String>) -> Self {
        Self {
            scheme: SecretScheme::Vault,
            path: path.into(),
        }
    }

    pub fn literal(v: impl Into<String>) -> Self {
        Self {
            scheme: SecretScheme::Literal,
            path: v.into(),
        }
    }

    /// Parse a single-token `${SCHEME:path}` string.
    ///
    /// Returns:
    /// - `Ok(Some(ref))` when the whole trimmed input is exactly one token.
    /// - `Ok(None)` when the input does not look like a `${...}` token at
    ///   all (caller may treat it as a literal).
    /// - `Err(...)` when the input looks like a token but is malformed
    ///   or names an unknown scheme.
    pub fn parse(s: &str) -> Result<Option<Self>, SecretError> {
        let trimmed = s.trim();
        if !trimmed.starts_with("${") || !trimmed.ends_with('}') {
            return Ok(None);
        }
        let inner = &trimmed[2..trimmed.len() - 1];
        let colon = match inner.find(':') {
            Some(i) => i,
            None => return Err(SecretError::MalformedRef(s.to_string())),
        };
        let scheme_str = &inner[..colon];
        let path = &inner[colon + 1..];
        if scheme_str.is_empty() || path.is_empty() {
            return Err(SecretError::MalformedRef(s.to_string()));
        }
        let scheme = SecretScheme::from_str_upper(scheme_str)
            .ok_or_else(|| SecretError::UnknownScheme(scheme_str.to_string()))?;
        Ok(Some(Self {
            scheme,
            path: path.to_string(),
        }))
    }
}

// ─── SecretBytes ───────────────────────────────────────────────────

/// Opaque wrapper for resolved secret bytes. Zeroizes on drop, omits
/// `Debug` / `Clone` so accidental logging / cloning fails at compile time.
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Accessor for raw bytes. Deliberately named so security audits can
    /// grep every call site.
    pub fn expose_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Accessor for UTF-8 strings. Returns `SecretError::NotUtf8` if the
    /// resolved value isn't valid UTF-8 (binary secrets — use
    /// `expose_bytes`).
    pub fn expose_str(&self) -> Result<&str, SecretError> {
        std::str::from_utf8(&self.0).map_err(|_| SecretError::NotUtf8(self.0.len()))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretBytes(<redacted, {} bytes>)", self.0.len())
    }
}

// ─── Provider trait ────────────────────────────────────────────────

/// Single-scheme secret backend. Multiple providers are composed via
/// [`SecretRegistry`].
pub trait SecretProvider: Send + Sync {
    fn scheme(&self) -> SecretScheme;
    fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError>;
}

// ─── EnvProvider ───────────────────────────────────────────────────

/// Reads from process environment variables. Values are treated as UTF-8.
pub struct EnvProvider;

impl SecretProvider for EnvProvider {
    fn scheme(&self) -> SecretScheme {
        SecretScheme::Env
    }

    fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError> {
        let val = std::env::var(path).map_err(|_| SecretError::EnvNotSet(path.to_string()))?;
        Ok(SecretBytes::new(val.into_bytes()))
    }
}

// ─── DotEnvProvider ────────────────────────────────────────────────

/// Reads `key=value` lines from a file at `AEON_DOTENV_PATH`.
///
/// The path is **explicit**: there is no auto-discovery and no default
/// location. The file is intended to live **outside** the deployed
/// artifact / container image, with 0600 perms, supplied by CI-CD.
/// Literal values in YAML and loose `.env` files inside the deploy dir
/// are explicitly not supported.
///
/// - Lines starting with `#` are comments.
/// - Blank lines are ignored.
/// - Values may be single-quoted or double-quoted; one outer pair is stripped.
/// - No escape sequences beyond the outer-quote strip (keep it simple).
pub struct DotEnvProvider {
    path: Option<PathBuf>,
}

impl DotEnvProvider {
    /// Explicit constructor. `None` means "no dotenv backing configured"
    /// — every resolve will return `DotEnvPathMissing`.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    pub fn from_env() -> Self {
        Self::new(std::env::var_os("AEON_DOTENV_PATH").map(PathBuf::from))
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self::new(Some(path))
    }
}

impl SecretProvider for DotEnvProvider {
    fn scheme(&self) -> SecretScheme {
        SecretScheme::DotEnv
    }

    fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError> {
        let file = self.path.as_ref().ok_or(SecretError::DotEnvPathMissing)?;
        let contents = std::fs::read_to_string(file).map_err(|e| SecretError::DotEnvIo {
            path: file.clone(),
            message: e.to_string(),
        })?;
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let eq = match line.find('=') {
                Some(i) => i,
                None => continue,
            };
            let key = line[..eq].trim();
            if key != path {
                continue;
            }
            let raw = line[eq + 1..].trim();
            let value = unquote(raw).into_owned();
            return Ok(SecretBytes::new(value.into_bytes()));
        }
        Err(SecretError::DotEnvKeyMissing(path.to_string()))
    }
}

fn unquote(s: &str) -> Cow<'_, str> {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return Cow::Borrowed(&s[1..s.len() - 1]);
        }
    }
    Cow::Borrowed(s)
}

// ─── LiteralProvider ───────────────────────────────────────────────

/// Returns the ref path itself as the secret value. Emits a warn-once
/// tracing event the first time any literal is resolved, so operators
/// notice when production YAML still carries inline credentials.
pub struct LiteralProvider {
    warned: AtomicBool,
}

impl LiteralProvider {
    pub fn new() -> Self {
        Self {
            warned: AtomicBool::new(false),
        }
    }
}

impl Default for LiteralProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretProvider for LiteralProvider {
    fn scheme(&self) -> SecretScheme {
        SecretScheme::Literal
    }

    fn resolve(&self, path: &str) -> Result<SecretBytes, SecretError> {
        if !self.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "aeon.secrets",
                "literal secret value in configuration — dev only; \
                 production configs should reference \
                 ${{VAULT:...}}, ${{AWS_SM:...}}, ${{ENV:...}}, or ${{DOTENV:...}} \
                 via a secret provider"
            );
        }
        Ok(SecretBytes::new(path.as_bytes().to_vec()))
    }
}

// ─── SecretRegistry ────────────────────────────────────────────────

/// Composes per-scheme providers and drives both single-ref resolution
/// and `${...}` interpolation over arbitrary strings.
///
/// [`SecretRegistry::default_local`] wires up `Env` + `DotEnv` + `Literal`
/// — the three providers that need no heavy deps. Vault / AWS providers
/// are registered by downstream crates (S1.2) via [`SecretRegistry::register`].
pub struct SecretRegistry {
    providers: HashMap<SecretScheme, Arc<dyn SecretProvider>>,
}

impl std::fmt::Debug for SecretRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let schemes: Vec<_> = self.providers.keys().collect();
        f.debug_struct("SecretRegistry")
            .field("schemes", &schemes)
            .finish()
    }
}

impl SecretRegistry {
    pub fn empty() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn default_local() -> Self {
        let mut r = Self::empty();
        r.register(Arc::new(EnvProvider));
        r.register(Arc::new(DotEnvProvider::from_env()));
        r.register(Arc::new(LiteralProvider::new()));
        r
    }

    pub fn register(&mut self, provider: Arc<dyn SecretProvider>) {
        self.providers.insert(provider.scheme(), provider);
    }

    pub fn has_scheme(&self, scheme: SecretScheme) -> bool {
        self.providers.contains_key(&scheme)
    }

    pub fn resolve(&self, r: &SecretRef) -> Result<SecretBytes, SecretError> {
        let provider = self
            .providers
            .get(&r.scheme)
            .ok_or(SecretError::SchemeNotRegistered(r.scheme))?;
        provider.resolve(&r.path)
    }

    /// Resolve a single-value string: either a `${SCHEME:path}` token or
    /// a plain literal (which routes through the Literal provider).
    pub fn resolve_str(&self, s: &str) -> Result<SecretBytes, SecretError> {
        match SecretRef::parse(s)? {
            Some(r) => self.resolve(&r),
            None => self.resolve(&SecretRef::literal(s)),
        }
    }

    /// Replace every `${SCHEME:path}` token in `s` with its resolved
    /// UTF-8 value. Escape: `$${SCHEME:path}` emits a literal
    /// `${SCHEME:path}` without resolution.
    ///
    /// Returns `Cow::Borrowed` if no `$` appears at all (hot-path friendly).
    /// Unknown schemes, malformed braces, or missing providers all fail
    /// loudly — a literal `${...}` in a value must be escaped explicitly,
    /// not silently passed through.
    pub fn interpolate_str<'a>(&self, s: &'a str) -> Result<Cow<'a, str>, SecretError> {
        if !s.contains('$') {
            return Ok(Cow::Borrowed(s));
        }
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        loop {
            let idx = match rest.find('$') {
                Some(i) => i,
                None => {
                    out.push_str(rest);
                    return Ok(Cow::Owned(out));
                }
            };
            out.push_str(&rest[..idx]);
            let tail = &rest[idx..];
            if tail.starts_with("$${") {
                let end = match tail.find('}') {
                    Some(i) => i,
                    None => return Err(SecretError::MalformedRef(s.to_string())),
                };
                out.push_str(&tail[1..=end]);
                rest = &tail[end + 1..];
                continue;
            }
            if tail.starts_with("${") {
                let end = match tail.find('}') {
                    Some(i) => i,
                    None => return Err(SecretError::MalformedRef(s.to_string())),
                };
                let token = &tail[..=end];
                let parsed = match SecretRef::parse(token)? {
                    Some(r) => r,
                    None => return Err(SecretError::MalformedRef(token.to_string())),
                };
                let secret = self.resolve(&parsed)?;
                out.push_str(secret.expose_str()?);
                rest = &tail[end + 1..];
                continue;
            }
            // Lone `$` followed by something other than `{` or `${`: emit
            // the dollar as-is and keep scanning.
            out.push('$');
            rest = &tail[1..];
        }
    }
}

impl Default for SecretRegistry {
    fn default() -> Self {
        Self::default_local()
    }
}

// ─── AeonError conversion ─────────────────────────────────────────

impl From<SecretError> for crate::AeonError {
    fn from(err: SecretError) -> Self {
        // Every SecretError is a configuration problem — the operator
        // supplied a ref the system can't resolve. Non-retryable: no amount
        // of backoff fixes a missing env var.
        crate::AeonError::Config {
            message: err.to_string(),
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // — SecretRef parser ————————————————————————————————————————

    #[test]
    fn parse_env_ref() {
        let r = SecretRef::parse("${ENV:DB_URL}").unwrap().unwrap();
        assert_eq!(r.scheme, SecretScheme::Env);
        assert_eq!(r.path, "DB_URL");
    }

    #[test]
    fn parse_vault_ref_with_path_segments() {
        let r = SecretRef::parse("${VAULT:secret/aeon/prod/db}")
            .unwrap()
            .unwrap();
        assert_eq!(r.scheme, SecretScheme::Vault);
        assert_eq!(r.path, "secret/aeon/prod/db");
    }

    #[test]
    fn parse_aws_sm_ref() {
        let r = SecretRef::parse("${AWS_SM:prod/api-keys#primary}")
            .unwrap()
            .unwrap();
        assert_eq!(r.scheme, SecretScheme::AwsSm);
        assert_eq!(r.path, "prod/api-keys#primary");
    }

    #[test]
    fn parse_aws_kms_ref() {
        let r = SecretRef::parse("${AWS_KMS:alias/aeon-kek}")
            .unwrap()
            .unwrap();
        assert_eq!(r.scheme, SecretScheme::AwsKms);
    }

    #[test]
    fn parse_dotenv_ref() {
        let r = SecretRef::parse("${DOTENV:API_KEY}").unwrap().unwrap();
        assert_eq!(r.scheme, SecretScheme::DotEnv);
    }

    #[test]
    fn parse_plain_string_returns_none() {
        assert!(SecretRef::parse("plain-literal").unwrap().is_none());
        assert!(SecretRef::parse("https://example.com").unwrap().is_none());
    }

    #[test]
    fn parse_missing_colon_errors() {
        let err = SecretRef::parse("${FOO}").unwrap_err();
        assert!(matches!(err, SecretError::MalformedRef(_)));
    }

    #[test]
    fn parse_empty_scheme_errors() {
        let err = SecretRef::parse("${:path}").unwrap_err();
        assert!(matches!(err, SecretError::MalformedRef(_)));
    }

    #[test]
    fn parse_empty_path_errors() {
        let err = SecretRef::parse("${ENV:}").unwrap_err();
        assert!(matches!(err, SecretError::MalformedRef(_)));
    }

    #[test]
    fn parse_unknown_scheme_errors() {
        let err = SecretRef::parse("${QUANTUM:foo}").unwrap_err();
        assert!(matches!(err, SecretError::UnknownScheme(s) if s == "QUANTUM"));
    }

    // — EnvProvider ————————————————————————————————————————————

    #[test]
    fn env_provider_resolves() {
        // Use a unique env var name so parallel tests can't interfere.
        const VAR: &str = "AEON_TEST_SECRET_ENV_PROVIDER_HIT";
        // SAFETY: test-only env manipulation; variable name is unique.
        unsafe { std::env::set_var(VAR, "the-value") };
        let p = EnvProvider;
        let bytes = p.resolve(VAR).unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "the-value");
        unsafe { std::env::remove_var(VAR) };
    }

    #[test]
    fn env_provider_missing_var_errors() {
        let p = EnvProvider;
        let err = p
            .resolve("AEON_TEST_DEFINITELY_NOT_SET_XYZZY_42")
            .unwrap_err();
        assert!(matches!(err, SecretError::EnvNotSet(_)));
    }

    // — DotEnvProvider ———————————————————————————————————————

    #[test]
    fn dotenv_provider_reads_key() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "# comment").unwrap();
        writeln!(file).unwrap();
        writeln!(file, "API_KEY=abc123").unwrap();
        writeln!(file, "OTHER=xyz").unwrap();
        file.flush().unwrap();
        let p = DotEnvProvider::with_path(file.path().to_path_buf());
        let bytes = p.resolve("API_KEY").unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "abc123");
    }

    #[test]
    fn dotenv_provider_strips_quotes() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "DOUBLE=\"hello world\"").unwrap();
        writeln!(file, "SINGLE='bye world'").unwrap();
        file.flush().unwrap();
        let p = DotEnvProvider::with_path(file.path().to_path_buf());
        assert_eq!(
            p.resolve("DOUBLE").unwrap().expose_str().unwrap(),
            "hello world"
        );
        assert_eq!(
            p.resolve("SINGLE").unwrap().expose_str().unwrap(),
            "bye world"
        );
    }

    #[test]
    fn dotenv_provider_missing_path() {
        let p = DotEnvProvider::new(None);
        let err = p.resolve("ANY").unwrap_err();
        assert!(matches!(err, SecretError::DotEnvPathMissing));
    }

    #[test]
    fn dotenv_provider_missing_key() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let p = DotEnvProvider::with_path(file.path().to_path_buf());
        let err = p.resolve("NOT_THERE").unwrap_err();
        assert!(matches!(err, SecretError::DotEnvKeyMissing(k) if k == "NOT_THERE"));
    }

    #[test]
    fn dotenv_provider_missing_file() {
        let p = DotEnvProvider::with_path(PathBuf::from("/definitely/does/not/exist/aeon.env"));
        let err = p.resolve("ANY").unwrap_err();
        assert!(matches!(err, SecretError::DotEnvIo { .. }));
    }

    // — LiteralProvider ——————————————————————————————————————

    #[test]
    fn literal_provider_returns_path_as_value() {
        let p = LiteralProvider::new();
        let bytes = p.resolve("plain-text-password").unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "plain-text-password");
    }

    // — SecretBytes ——————————————————————————————————————————

    #[test]
    fn secret_bytes_debug_is_redacted() {
        let b = SecretBytes::new(b"supersecret".to_vec());
        let s = format!("{b:?}");
        assert!(s.contains("redacted"));
        assert!(!s.contains("supersecret"));
    }

    #[test]
    fn secret_bytes_len_is_public() {
        let b = SecretBytes::new(b"abc".to_vec());
        assert_eq!(b.len(), 3);
        assert!(!b.is_empty());
    }

    #[test]
    fn secret_bytes_non_utf8_reports_error() {
        let b = SecretBytes::new(vec![0xff, 0xfe, 0xfd]);
        let err = b.expose_str().unwrap_err();
        assert!(matches!(err, SecretError::NotUtf8(3)));
    }

    // — SecretRegistry ———————————————————————————————————————

    #[test]
    fn registry_default_local_has_env_dotenv_literal() {
        let r = SecretRegistry::default_local();
        assert!(r.has_scheme(SecretScheme::Env));
        assert!(r.has_scheme(SecretScheme::DotEnv));
        assert!(r.has_scheme(SecretScheme::Literal));
        assert!(!r.has_scheme(SecretScheme::Vault));
        assert!(!r.has_scheme(SecretScheme::AwsSm));
    }

    #[test]
    fn registry_resolves_env_ref() {
        const VAR: &str = "AEON_TEST_SECRET_REGISTRY_HIT";
        unsafe { std::env::set_var(VAR, "hello") };
        let r = SecretRegistry::default_local();
        let bytes = r.resolve(&SecretRef::env(VAR)).unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "hello");
        unsafe { std::env::remove_var(VAR) };
    }

    #[test]
    fn registry_scheme_not_registered_errors() {
        let r = SecretRegistry::empty();
        let err = r.resolve(&SecretRef::vault("any/path")).unwrap_err();
        assert!(matches!(
            err,
            SecretError::SchemeNotRegistered(SecretScheme::Vault)
        ));
    }

    #[test]
    fn registry_resolve_str_treats_plain_as_literal() {
        let r = SecretRegistry::default_local();
        let bytes = r.resolve_str("literal-value").unwrap();
        assert_eq!(bytes.expose_str().unwrap(), "literal-value");
    }

    // — interpolation ————————————————————————————————————————

    #[test]
    fn interpolate_no_tokens_is_borrowed() {
        let r = SecretRegistry::default_local();
        let out = r.interpolate_str("plain string no dollars").unwrap();
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, "plain string no dollars");
    }

    #[test]
    fn interpolate_single_env_token() {
        const VAR: &str = "AEON_TEST_SECRET_INTERP_1";
        unsafe { std::env::set_var(VAR, "bar") };
        let r = SecretRegistry::default_local();
        let out = r
            .interpolate_str("${ENV:AEON_TEST_SECRET_INTERP_1}")
            .unwrap();
        assert_eq!(out, "bar");
        unsafe { std::env::remove_var(VAR) };
    }

    #[test]
    fn interpolate_multiple_tokens_inline() {
        const V1: &str = "AEON_TEST_SECRET_INTERP_2A";
        const V2: &str = "AEON_TEST_SECRET_INTERP_2B";
        unsafe {
            std::env::set_var(V1, "alice");
            std::env::set_var(V2, "bob");
        }
        let r = SecretRegistry::default_local();
        let out = r
            .interpolate_str(
                "user=${ENV:AEON_TEST_SECRET_INTERP_2A};peer=${ENV:AEON_TEST_SECRET_INTERP_2B}",
            )
            .unwrap();
        assert_eq!(out, "user=alice;peer=bob");
        unsafe {
            std::env::remove_var(V1);
            std::env::remove_var(V2);
        }
    }

    #[test]
    fn interpolate_escape_emits_literal_token() {
        let r = SecretRegistry::default_local();
        let out = r.interpolate_str("price $${ENV:FOO} today").unwrap();
        assert_eq!(out, "price ${ENV:FOO} today");
    }

    #[test]
    fn interpolate_lone_dollar_is_passthrough() {
        let r = SecretRegistry::default_local();
        let out = r.interpolate_str("costs $5 per unit").unwrap();
        assert_eq!(out, "costs $5 per unit");
    }

    #[test]
    fn interpolate_unclosed_brace_errors() {
        let r = SecretRegistry::default_local();
        let err = r.interpolate_str("${ENV:FOO_no_close").unwrap_err();
        assert!(matches!(err, SecretError::MalformedRef(_)));
    }

    #[test]
    fn interpolate_unknown_scheme_errors() {
        let r = SecretRegistry::default_local();
        let err = r.interpolate_str("x=${QUANTUM:foo}").unwrap_err();
        assert!(matches!(err, SecretError::UnknownScheme(_)));
    }

    #[test]
    fn interpolate_unregistered_scheme_errors() {
        let r = SecretRegistry::default_local(); // no Vault registered
        let err = r.interpolate_str("x=${VAULT:secret/foo}").unwrap_err();
        assert!(matches!(
            err,
            SecretError::SchemeNotRegistered(SecretScheme::Vault)
        ));
    }

    // — AeonError conversion ——————————————————————————————————

    #[test]
    fn secret_error_converts_to_aeon_config_error() {
        let e: crate::AeonError = SecretError::EnvNotSet("X".into()).into();
        assert!(matches!(e, crate::AeonError::Config { .. }));
        assert!(!e.is_retryable());
    }
}
