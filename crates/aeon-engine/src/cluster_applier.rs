//! `RegistryApplier` implementation that fans `RegistryCommand`s out to the
//! node-local `ProcessorRegistry` and `PipelineManager`.
//!
//! Every node in the Raft cluster holds one of these; the cluster state
//! machine calls [`ClusterRegistryApplier::apply`] after committing a
//! `ClusterRequest::Registry` log entry, so every node converges on the
//! same local view of pipelines and processors. Installed on the
//! `ClusterNode` at startup via `install_registry_applier`.

use std::sync::Arc;

use aeon_types::{RegistryApplier, RegistryCommand, RegistryResponse};

use crate::pipeline_manager::PipelineManager;
use crate::registry::ProcessorRegistry;

/// Applier that routes committed `RegistryCommand`s to the local
/// `ProcessorRegistry` (processor metadata commands) or `PipelineManager`
/// (pipeline lifecycle commands).
pub struct ClusterRegistryApplier {
    registry: Arc<ProcessorRegistry>,
    pipelines: Arc<PipelineManager>,
}

impl ClusterRegistryApplier {
    pub fn new(registry: Arc<ProcessorRegistry>, pipelines: Arc<PipelineManager>) -> Self {
        Self { registry, pipelines }
    }
}

impl RegistryApplier for ClusterRegistryApplier {
    fn apply(
        &self,
        cmd: RegistryCommand,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = RegistryResponse> + Send + '_>,
    > {
        let registry = Arc::clone(&self.registry);
        let pipelines = Arc::clone(&self.pipelines);
        Box::pin(async move {
            match &cmd {
                RegistryCommand::RegisterProcessor { .. }
                | RegistryCommand::DeleteVersion { .. }
                | RegistryCommand::SetVersionStatus { .. } => registry.apply(cmd).await,
                RegistryCommand::CreatePipeline { .. }
                | RegistryCommand::SetPipelineState { .. }
                | RegistryCommand::UpgradePipeline { .. }
                | RegistryCommand::DeletePipeline { .. } => pipelines.apply(cmd).await,
                RegistryCommand::RegisterIdentity { .. }
                | RegistryCommand::RevokeIdentity { .. } => {
                    // Identity commands are applied by ProcessorIdentityStore
                    // directly on the REST-side path today; Raft replication
                    // for identities will route through a dedicated applier
                    // once that store also participates in cluster state.
                    RegistryResponse::Error {
                        message: "identity commands not yet Raft-applied".into(),
                    }
                }
            }
        })
    }
}
