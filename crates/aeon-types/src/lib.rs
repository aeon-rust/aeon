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

pub mod awpp;
pub mod backoff;
pub mod delivery;
pub mod durability;
pub mod error;
pub mod event;
pub mod event_time;
pub mod interner;
pub mod manifest;
pub mod oauth;
pub mod partition;
pub mod processor_identity;
pub mod processor_transport;
pub mod registry;
pub mod scanner;
pub mod state;
pub mod traits;
pub mod transport_codec;
pub mod uuid;

// Re-export primary types at crate root for convenience.
pub use backoff::{Backoff, BackoffPolicy};
pub use delivery::{BatchFailurePolicy, BatchResult, DeliverySemantics, DeliveryStrategy};
pub use durability::DurabilityMode;
pub use error::{AeonError, Result};
pub use event::{Event, Output};
pub use event_time::EventTime;
pub use interner::StringInterner;
pub use manifest::{
    BackpressureConfig, CheckpointBackendDecl, CheckpointBlock, ContentHashConfig,
    DurabilityBlock, FlushBlock, IdentityConfig, PipelineManifest, ProcessorManifest,
    SinkManifest, SinkTierDecl, SourceManifest, validate_pipeline_shape,
    validate_sink_tier, validate_source_shape,
};
pub use oauth::OAuthConfig;
pub use partition::PartitionId;
pub use processor_identity::{PipelineScope, ProcessorIdentity};
pub use processor_transport::{
    ProcessorBinding, ProcessorConnectionConfig, ProcessorHealth, ProcessorInfo, ProcessorTier,
};
pub use registry::{
    BlueGreenActive, BlueGreenState, CanaryState, CanaryThresholds, PipelineAction,
    PipelineDefinition, PipelineHistoryEntry, PipelineState, ProcessorRecord, ProcessorRef,
    ProcessorType, ProcessorVersion, RegistryApplier, RegistryCommand, RegistryResponse,
    SinkConfig, SourceConfig, UpgradeInfo, UpgradeStrategy, VersionStatus,
};
pub use scanner::{
    BytesFinder, contains_byte, contains_bytes, find_byte, find_bytes, json_field_value,
};
pub use state::{BatchEntry, BatchOp, KvPairs, L3Backend, L3Store};
pub use traits::{
    CheckpointReplicator, IdempotentSink, Processor, ProcessorTransport, Seekable, Sink,
    SinkEosTier, Source, SourceKind, StateOps, TransactionalSink,
};
pub use transport_codec::{TransportCodec, WireEvent, WireOutput};
pub use uuid::{CoreLocalUuidGenerator, derive_pull_uuid_v7};
