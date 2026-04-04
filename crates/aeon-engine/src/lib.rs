//! Pipeline orchestrator, SPSC wiring, and backpressure management.

pub mod affinity;
pub mod batch_tuner;
pub mod circuit_breaker;
pub mod dag;
pub mod dlq;
pub mod health;
pub mod metrics_server;
pub mod pipeline;
pub mod processor;
pub mod retry;
pub mod shutdown;

#[cfg(feature = "native-loader")]
pub mod native_loader;

pub use affinity::{PipelineCores, available_cores, pin_to_core, pipeline_core_assignment};
pub use batch_tuner::BatchTuner;
pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitState};
pub use dag::{DagGraph, NodeKind, run_chain, run_fan_in, run_fan_out, run_routed};
pub use dlq::{DeadLetterQueue, DlqConfig, DlqRecord};
pub use health::{HealthState, serve_health};
pub use metrics_server::serve_metrics;
pub use pipeline::{PipelineConfig, PipelineMetrics, run, run_buffered};
pub use processor::PassthroughProcessor;
pub use retry::{RetryConfig, RetryOutcome, backoff_delay, retry_async, retry_sync};
pub use shutdown::{ShutdownConfig, ShutdownCoordinator};

#[cfg(feature = "native-loader")]
pub use native_loader::NativeProcessor;
