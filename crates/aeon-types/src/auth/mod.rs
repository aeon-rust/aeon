//! S9/S10: Connector authentication primitives.
//!
//! This module groups inbound (S9, where Aeon is the server) and outbound
//! (S10, where Aeon is the client) authentication configuration, together
//! with the shared HMAC signing/verification helper used by both sides.
//!
//! ## Layout
//!
//! - [`inbound`] — [`InboundAuthConfig`] + [`InboundAuthVerifier`] for
//!   HTTP webhook / WebTransport / QUIC sources where Aeon accepts
//!   connections from upstream producers.
//! - [`hmac_sig`] — shared HMAC-SHA256 signer/verifier used by both S9
//!   (inbound verification) and S10 (outbound signing).
//!
//! Outbound (S10) config types land in this module in a later workstream.

pub mod hmac_sig;
pub mod inbound;
pub mod outbound;

pub use hmac_sig::{HmacAlgorithm, HmacSignError, HmacVerifyError, sign_request, verify_request};
pub use inbound::{
    ApiKeyConfig, AuthContext, AuthRejection, HmacConfig, InboundAuthConfig, InboundAuthMode,
    InboundAuthVerifier, IpAllowlistConfig, MtlsConfig,
};
pub use outbound::{
    BasicConfig, BearerConfig, BrokerNativeConfig, HmacSignConfig, OutboundApiKeyConfig,
    OutboundAuthBuildError, OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner,
    OutboundMtlsConfig, OutboundSignContext, OutboundSignError,
};
