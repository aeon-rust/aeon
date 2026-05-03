//! Core types, traits, and error definitions for Aeon.
//!
//! This crate is the foundation of the Aeon workspace. All other crates
//! depend on `aeon-types` for the canonical Event/Output envelopes,
//! error types, and trait definitions.

// FT-10: no-panic policy. Production code in this crate must not use
// `.unwrap()` or `.expect()` except for explicitly-documented startup-time
// invariants, which must carry an `#[allow(...)]` attribute with rationale.
// Test modules and benches are exempt (`cfg(not(test))`).
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod audit;
pub mod auth;
pub mod awpp;
pub mod backoff;
pub mod compliance;
pub mod consumer_mode;
pub mod delivery;
pub mod durability;
pub mod encryption;
pub mod erasure;
pub mod error;
pub mod event;
pub mod event_time;
pub mod interner;
pub mod l2_transfer;
pub mod manifest;
pub mod oauth;
pub mod partition;
pub mod poh;
pub mod processor_identity;
pub mod processor_transport;
pub mod redact;
pub mod registry;
pub mod retention;
pub mod scanner;
pub mod secrets;
pub mod ssrf;
pub mod state;
pub mod subject_id;
pub mod traits;
pub mod transport_codec;
pub mod uuid;

// Re-export primary types at crate root for convenience.
pub use audit::{AuditCategory, AuditEvent, AuditOutcome, AuditSink};
pub use auth::{
    ApiKeyConfig, AuthContext, AuthRejection, BasicConfig, BearerConfig, BrokerNativeConfig,
    HmacAlgorithm, HmacConfig, HmacSignConfig, HmacSignError, HmacVerifyError, InboundAuthConfig,
    InboundAuthMode, InboundAuthVerifier, IpAllowlistConfig, MtlsConfig, OutboundApiKeyConfig,
    OutboundAuthBuildError, OutboundAuthConfig, OutboundAuthMode, OutboundAuthSigner,
    OutboundMtlsConfig, OutboundSignContext, OutboundSignError,
};
pub use backoff::{Backoff, BackoffPolicy};
pub use compliance::{
    ComplianceBlock, ComplianceRegime, DEFAULT_ERASURE_MAX_DELAY_HOURS, DataClass,
    EnforcementLevel, ErasureConfig, PayloadFormat, PiiSelector,
};
pub use consumer_mode::ConsumerMode;
pub use delivery::{BatchFailurePolicy, BatchResult, DeliverySemantics, DeliveryStrategy};
pub use durability::DurabilityMode;
pub use encryption::{AtRestEncryption, EncryptionBlock};
pub use erasure::{ErasureRequest, ErasureSelector, ErasureTombstone, TombstoneState};
pub use error::{AeonError, Result};
pub use event::{Event, Output};
pub use event_time::EventTime;
pub use interner::StringInterner;
pub use l2_transfer::{DEFAULT_CHUNK_BYTES, SegmentChunk, SegmentEntry, SegmentManifest};
pub use manifest::{
    BackpressureConfig, CheckpointBackendDecl, CheckpointBlock, ContentHashConfig, DurabilityBlock,
    FlushBlock, IdentityConfig, PipelineManifest, ProcessorManifest, SinkManifest, SinkTierDecl,
    SourceManifest, SubjectExtractConfig, validate_pipeline_shape, validate_sink_tier,
    validate_source_shape,
};
pub use oauth::OAuthConfig;
pub use partition::PartitionId;
pub use poh::PohBlock;
pub use processor_identity::{PipelineScope, ProcessorIdentity};
pub use processor_transport::{
    ProcessorBinding, ProcessorConnectionConfig, ProcessorHealth, ProcessorInfo, ProcessorTier,
};
pub use redact::{
    METADATA_REDACT_DENYLIST, is_redacted_metadata_key, redact_metadata_value, redact_uri,
};
pub use registry::{
    BlueGreenActive, BlueGreenState, CanaryState, CanaryThresholds, PipelineAction,
    PipelineDefinition, PipelineHistoryEntry, PipelineState, ProcessorRecord, ProcessorRef,
    ProcessorType, ProcessorVersion, RegistryApplier, RegistryCommand, RegistryResponse,
    SinkConfig, SourceConfig, UpgradeInfo, UpgradeStrategy, VersionStatus,
};
pub use retention::{L2RetentionBlock, L3RetentionBlock, RetentionBlock};
pub use scanner::{
    BytesFinder, contains_byte, contains_bytes, find_byte, find_bytes, json_field_value,
};
pub use secrets::{
    DotEnvProvider, EnvProvider, LiteralProvider, SecretBytes, SecretError, SecretProvider,
    SecretRef, SecretRegistry, SecretScheme,
};
pub use ssrf::{SsrfError, SsrfPolicy, parse_host_port};
pub use state::{BatchEntry, BatchOp, KvPairs, L3Backend, L3Store};
pub use subject_id::{
    MAX_COMPONENT_LEN, METADATA_KEY_SUBJECT_ID, MetaPair, SubjectId, collect_subject_ids,
    try_collect_subject_ids, validate_namespace_for_wildcard,
};
pub use traits::{
    CheckpointReplicator, IdempotentSink, Processor, ProcessorTransport, Seekable, Sink,
    SinkAckCallback, SinkEosTier, Source, SourceKind, StateOps, TransactionalSink,
};
pub use transport_codec::{TransportCodec, WireEvent, WireOutput};
pub use uuid::{CoreLocalUuidGenerator, derive_pull_uuid_v7};
