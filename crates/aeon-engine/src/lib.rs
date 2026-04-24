//! Pipeline orchestrator, SPSC wiring, and backpressure management.

// FT-10: no-panic policy. Production code in this crate must not use
// `.unwrap()` or `.expect()` except for explicitly-documented startup-time
// invariants, which must carry an `#[allow(...)]` attribute with rationale.
// Test modules and benches are exempt (`cfg(not(test))`).
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

// S2: `debug-payload-logging` is a dev-only diagnostic feature. Future gated
// `trace!(?payload, ...)` sites hang off it; a release build that accidentally
// enables it must fail to compile so the payload-never-in-logs invariant
// survives the build pipeline. `debug_assertions` is on in debug builds and
// off in `--release`, so this check fires exactly when it should.
#[cfg(all(feature = "debug-payload-logging", not(debug_assertions)))]
compile_error!(
    "feature `debug-payload-logging` is a dev-only diagnostic and refuses to \
     compile in release builds (S2: payload never in logs, ever). Drop \
     `--features debug-payload-logging` or build without `--release`."
);

pub mod affinity;
#[cfg(feature = "cluster")]
pub mod cluster_applier;
#[cfg(feature = "cluster")]
pub mod engine_cutover;
#[cfg(feature = "cluster")]
pub mod partition_ownership;
#[cfg(all(feature = "cluster", feature = "processor-auth"))]
pub mod partition_install;
#[cfg(all(feature = "cluster", feature = "processor-auth"))]
pub mod engine_providers;
pub mod batch_tuner;
pub mod batch_wire;
pub mod checkpoint;
pub mod circuit_breaker;
pub mod compliance_validator;
pub mod connector_registry;
pub mod dag;
pub mod delivery;
pub mod delivery_ledger;
pub mod dlq;
pub mod eo2;
pub mod eo2_backpressure;
pub mod eo2_content_hash;
pub mod eo2_metrics;
pub mod encryption_probe;
pub mod eo2_recovery;
pub mod erasure_policy;
pub mod erasure_probe;
pub mod erasure_store;
pub mod retention_probe;
pub mod health;
pub mod identity_store;
pub mod l2_body;
pub mod l2_transfer;
pub mod subject_export;
pub mod metrics_server;
pub mod pipeline;
pub mod pipeline_manager;
pub mod pipeline_supervisor;
pub mod processor;
pub mod registry;
pub mod retry;
pub mod shutdown;
pub mod transport;
pub mod write_gate;

#[cfg(feature = "native-loader")]
pub mod native_loader;

#[cfg(feature = "processor-auth")]
pub mod processor_auth;

#[cfg(feature = "rest-api")]
pub mod rest_api;

pub use affinity::{
    PipelineCores, available_cores, multi_pipeline_core_assignment, pin_to_core,
    pipeline_core_assignment,
};
pub use batch_tuner::{BatchTuner, FlushTuner};
pub use batch_wire::{
    BatchRequest, BatchResponse, DecodedBatchResponse, decode_batch_request, decode_batch_response,
    deserialize_batch_request, deserialize_batch_response, encode_batch_request,
    encode_batch_response, serialize_batch_request, serialize_batch_response,
};

pub use checkpoint::{
    CheckpointPersist, CheckpointReader, CheckpointRecord, CheckpointWriter, L3CheckpointStore,
    WalCheckpointStore,
};
pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitState};
pub use connector_registry::{
    BoxedSinkAdapter, BoxedSourceAdapter, ConnectorRegistry, DynSink, DynSource,
    PartitionOwnershipResolver, SinkFactory, SourceFactory,
};
pub use dag::{DagGraph, NodeKind, Topology, run_chain, run_fan_in, run_fan_out, run_routed, run_topology};
pub use delivery::{CheckpointBackend, CheckpointConfig, DeliveryConfig, FlushStrategy};
pub use delivery_ledger::{DeliveryLedger, DeliveryState, FailedEntry, LedgerEntry};
pub use dlq::{DeadLetterQueue, DlqConfig, DlqRecord};
pub use health::{HealthState, serve_health};
pub use identity_store::ProcessorIdentityStore;
pub use metrics_server::{MetricsConfig, serve_metrics, serve_metrics_with_config};
pub use pipeline::{
    CorePinning, MultiPartitionConfig, PipelineConfig, PipelineControl, PipelineMetrics, run,
    run_buffered, run_buffered_managed, run_multi_partition, run_with_delivery,
};
#[cfg(feature = "processor-auth")]
pub use pipeline::{PohConfig, PohState, create_poh_state};
pub use pipeline_manager::PipelineManager;
pub use pipeline_supervisor::{IDENTITY_PROCESSOR, PipelineSupervisor};
pub use processor::PassthroughProcessor;
pub use registry::ProcessorRegistry;
pub use retry::{RetryConfig, RetryOutcome, backoff_delay, retry_async, retry_sync};
pub use shutdown::{ShutdownConfig, ShutdownCoordinator};
pub use transport::InProcessTransport;
pub use write_gate::{DrainError, DrainGuard, GateState, WriteGate, WriteGateRegistry};
pub use transport::{
    AwppSession, BatchInflight, ControlChannel, HandshakeConfig, PipelineResolver, SessionState,
};

#[cfg(feature = "native-loader")]
pub use native_loader::NativeProcessor;

#[cfg(feature = "cluster")]
pub use cluster_applier::ClusterRegistryApplier;

#[cfg(feature = "cluster")]
pub use engine_cutover::{CutoverWatermarkReader, EngineCutoverCoordinator};

#[cfg(feature = "cluster")]
pub use partition_ownership::ClusterPartitionOwnership;

#[cfg(all(feature = "cluster", feature = "processor-auth"))]
pub use partition_install::{
    InstalledPohChainRegistry, L2SegmentInstaller, LivePohChainRegistry, PohChainInstallerImpl,
};

#[cfg(all(feature = "cluster", feature = "processor-auth"))]
pub use engine_providers::{L2SegmentTransferProvider, PohChainExportProvider};

#[cfg(feature = "rest-api")]
pub use rest_api::{AppState, api_router, serve};

#[cfg(feature = "webtransport-host")]
pub use transport::WebTransportProcessorHost;

#[cfg(feature = "websocket-host")]
pub use transport::{
    WebSocketProcessorHost,
    websocket_host::{build_ws_data_frame, parse_ws_routing_header},
};
