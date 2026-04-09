//! Core types, traits, and error definitions for Aeon.
//!
//! This crate is the foundation of the Aeon workspace. All other crates
//! depend on `aeon-types` for the canonical Event/Output envelopes,
//! error types, and trait definitions.

pub mod awpp;
pub mod delivery;
pub mod error;
pub mod event;
pub mod interner;
pub mod oauth;
pub mod partition;
pub mod processor_identity;
pub mod processor_transport;
pub mod registry;
pub mod scanner;
pub mod traits;
pub mod transport_codec;
pub mod uuid;

// Re-export primary types at crate root for convenience.
pub use delivery::{BatchFailurePolicy, BatchResult, DeliverySemantics, DeliveryStrategy};
pub use error::{AeonError, Result};
pub use event::{Event, Output};
pub use interner::StringInterner;
pub use oauth::OAuthConfig;
pub use partition::PartitionId;
pub use processor_identity::{PipelineScope, ProcessorIdentity};
pub use processor_transport::{
    ProcessorBinding, ProcessorConnectionConfig, ProcessorHealth, ProcessorInfo, ProcessorTier,
};
pub use registry::{
    BlueGreenActive, BlueGreenState, CanaryState, CanaryThresholds, PipelineAction,
    PipelineDefinition, PipelineHistoryEntry, PipelineState, ProcessorRecord, ProcessorRef,
    ProcessorType, ProcessorVersion, RegistryCommand, RegistryResponse, SinkConfig, SourceConfig,
    UpgradeInfo, UpgradeStrategy, VersionStatus,
};
pub use scanner::{
    BytesFinder, contains_byte, contains_bytes, find_byte, find_bytes, json_field_value,
};
pub use traits::{IdempotentSink, Processor, ProcessorTransport, Seekable, Sink, Source, StateOps};
pub use transport_codec::{TransportCodec, WireEvent, WireOutput};
pub use uuid::CoreLocalUuidGenerator;
