//! Pipeline orchestrator, SPSC wiring, and backpressure management.

pub mod affinity;
pub mod batch_tuner;
pub mod batch_wire;
pub mod checkpoint;
pub mod circuit_breaker;
pub mod dag;
pub mod delivery;
pub mod delivery_ledger;
pub mod dlq;
pub mod health;
pub mod identity_store;
pub mod metrics_server;
pub mod pipeline;
pub mod pipeline_manager;
pub mod processor;
pub mod registry;
pub mod retry;
pub mod shutdown;
pub mod transport;

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
    BatchRequest, BatchResponse, DecodedBatchResponse, decode_batch_request,
    decode_batch_response, deserialize_batch_request, deserialize_batch_response,
    encode_batch_request, encode_batch_response, serialize_batch_request,
    serialize_batch_response,
};

pub use checkpoint::{CheckpointReader, CheckpointRecord, CheckpointWriter};
pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitState};
pub use dag::{DagGraph, NodeKind, run_chain, run_fan_in, run_fan_out, run_routed};
pub use delivery::{CheckpointBackend, CheckpointConfig, DeliveryConfig, FlushStrategy};
pub use delivery_ledger::{DeliveryLedger, DeliveryState, FailedEntry, LedgerEntry};
pub use dlq::{DeadLetterQueue, DlqConfig, DlqRecord};
pub use health::{HealthState, serve_health};
pub use metrics_server::serve_metrics;
pub use pipeline::{
    CorePinning, MultiPartitionConfig, PipelineConfig, PipelineMetrics, run, run_buffered,
    run_multi_partition,
};
pub use pipeline_manager::PipelineManager;
pub use processor::PassthroughProcessor;
pub use registry::ProcessorRegistry;
pub use retry::{RetryConfig, RetryOutcome, backoff_delay, retry_async, retry_sync};
pub use shutdown::{ShutdownConfig, ShutdownCoordinator};
pub use identity_store::ProcessorIdentityStore;
pub use transport::InProcessTransport;

#[cfg(feature = "native-loader")]
pub use native_loader::NativeProcessor;

#[cfg(feature = "rest-api")]
pub use rest_api::{AppState, api_router, serve};
