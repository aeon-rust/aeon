//! `RegistryApplier` implementation that fans `RegistryCommand`s out to the
//! node-local `ProcessorRegistry` and `PipelineManager`.
//!
//! Every node in the Raft cluster holds one of these; the cluster state
//! machine calls [`ClusterRegistryApplier::apply`] after committing a
//! `ClusterRequest::Registry` log entry, so every node converges on the
//! same local view of pipelines and processors. Installed on the
//! `ClusterNode` at startup via `install_registry_applier`.

use std::sync::Arc;

use aeon_types::registry::PipelineState;
use aeon_types::{RegistryApplier, RegistryCommand, RegistryResponse};

use crate::pipeline_manager::PipelineManager;
use crate::pipeline_supervisor::PipelineSupervisor;
use crate::registry::ProcessorRegistry;

/// Applier that routes committed `RegistryCommand`s to the local
/// `ProcessorRegistry` (processor metadata commands) or `PipelineManager`
/// (pipeline lifecycle commands).
///
/// After a `SetPipelineState` command commits, also drives the local
/// `PipelineSupervisor` so that the running-task set converges with the
/// replicated declarative state on every node. `supervisor` is `None` in
/// tests / tooling contexts where no runtime is wired up — in that case
/// the applier behaves like the pre-supervisor version.
pub struct ClusterRegistryApplier {
    registry: Arc<ProcessorRegistry>,
    pipelines: Arc<PipelineManager>,
    supervisor: Option<Arc<PipelineSupervisor>>,
}

impl ClusterRegistryApplier {
    pub fn new(registry: Arc<ProcessorRegistry>, pipelines: Arc<PipelineManager>) -> Self {
        Self {
            registry,
            pipelines,
            supervisor: None,
        }
    }

    /// Construct an applier that also drives a local `PipelineSupervisor`
    /// when pipeline-state commands commit. Used by `cmd_serve` when the
    /// REST/cluster server is running.
    pub fn with_supervisor(
        registry: Arc<ProcessorRegistry>,
        pipelines: Arc<PipelineManager>,
        supervisor: Arc<PipelineSupervisor>,
    ) -> Self {
        Self {
            registry,
            pipelines,
            supervisor: Some(supervisor),
        }
    }
}

impl RegistryApplier for ClusterRegistryApplier {
    fn apply(
        &self,
        cmd: RegistryCommand,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RegistryResponse> + Send + '_>> {
        let registry = Arc::clone(&self.registry);
        let pipelines = Arc::clone(&self.pipelines);
        let supervisor = self.supervisor.clone();
        Box::pin(async move {
            match &cmd {
                RegistryCommand::RegisterProcessor { .. }
                | RegistryCommand::DeleteVersion { .. }
                | RegistryCommand::SetVersionStatus { .. } => registry.apply(cmd).await,
                RegistryCommand::CreatePipeline { .. } => pipelines.apply(cmd).await,
                // G9.c.4 — backfill: UpgradePipeline now triggers the
                // local supervisor's drain-and-swap on every node, not
                // just the leader's REST handler. Pre-fix only the
                // leader's pod ran the runtime swap; followers updated
                // declarative state but kept the old processor alive
                // in their PipelineControl.
                RegistryCommand::UpgradePipeline {
                    name,
                    new_processor,
                } => {
                    let target_name = name.clone();
                    let new_proc = new_processor.clone();
                    let resp = pipelines.apply(cmd).await;
                    if matches!(&resp, RegistryResponse::Error { .. }) {
                        return resp;
                    }
                    if let Some(sup) = supervisor.as_ref() {
                        if let Err(e) = sup.upgrade_running(&target_name, new_proc).await {
                            tracing::warn!(
                                pipeline = %target_name,
                                error = %e,
                                "supervisor upgrade_running failed after Raft commit \
                                 (declarative state did update)"
                            );
                        }
                    }
                    resp
                }
                // G9.c — lifecycle ops: declarative state via Raft +
                // local supervisor side-effect on every node. Same
                // best-effort runtime convergence pattern as
                // SetPipelineState::Running.
                RegistryCommand::BlueGreenStart { name, .. } => {
                    let target_name = name.clone();
                    let resp = pipelines.apply(cmd).await;
                    if matches!(&resp, RegistryResponse::Error { .. }) {
                        return resp;
                    }
                    if let Some(sup) = supervisor.as_ref() {
                        if let Err(e) = sup.cluster_blue_green_start(&target_name).await {
                            tracing::warn!(
                                pipeline = %target_name,
                                error = %e,
                                "supervisor blue_green_start failed after Raft commit"
                            );
                        }
                    }
                    resp
                }
                RegistryCommand::BlueGreenCutover { name, .. } => {
                    let target_name = name.clone();
                    let resp = pipelines.apply(cmd).await;
                    if matches!(&resp, RegistryResponse::Error { .. }) {
                        return resp;
                    }
                    if let Some(sup) = supervisor.as_ref() {
                        if let Err(e) = sup.cluster_blue_green_cutover(&target_name).await {
                            tracing::warn!(
                                pipeline = %target_name,
                                error = %e,
                                "supervisor blue_green_cutover failed after Raft commit"
                            );
                        }
                    }
                    resp
                }
                RegistryCommand::RollbackUpgrade { name, .. } => {
                    let target_name = name.clone();
                    let resp = pipelines.apply(cmd).await;
                    if matches!(&resp, RegistryResponse::Error { .. }) {
                        return resp;
                    }
                    if let Some(sup) = supervisor.as_ref() {
                        if let Err(e) = sup.cluster_rollback_upgrade(&target_name).await {
                            tracing::warn!(
                                pipeline = %target_name,
                                error = %e,
                                "supervisor rollback_upgrade failed after Raft commit"
                            );
                        }
                    }
                    resp
                }
                RegistryCommand::CanaryStart { name, .. } => {
                    let target_name = name.clone();
                    let resp = pipelines.apply(cmd).await;
                    if matches!(&resp, RegistryResponse::Error { .. }) {
                        return resp;
                    }
                    if let Some(sup) = supervisor.as_ref() {
                        if let Err(e) = sup.cluster_canary_start(&target_name).await {
                            tracing::warn!(
                                pipeline = %target_name,
                                error = %e,
                                "supervisor canary_start failed after Raft commit"
                            );
                        }
                    }
                    resp
                }
                RegistryCommand::CanaryPromote { name, .. } => {
                    let target_name = name.clone();
                    let resp = pipelines.apply(cmd).await;
                    if matches!(&resp, RegistryResponse::Error { .. }) {
                        return resp;
                    }
                    if let Some(sup) = supervisor.as_ref() {
                        if let Err(e) = sup.cluster_promote_canary(&target_name).await {
                            tracing::warn!(
                                pipeline = %target_name,
                                error = %e,
                                "supervisor promote_canary failed after Raft commit"
                            );
                        }
                    }
                    resp
                }
                RegistryCommand::ReconfigureSource {
                    name, new_source, ..
                } => {
                    let target_name = name.clone();
                    let new_src = new_source.clone();
                    let resp = pipelines.apply(cmd).await;
                    if matches!(&resp, RegistryResponse::Error { .. }) {
                        return resp;
                    }
                    if let Some(sup) = supervisor.as_ref() {
                        if let Err(e) = sup.cluster_reconfigure_source(&target_name, new_src).await
                        {
                            tracing::warn!(
                                pipeline = %target_name,
                                error = %e,
                                "supervisor reconfigure_source failed after Raft commit"
                            );
                        }
                    }
                    resp
                }
                RegistryCommand::ReconfigureSink { name, new_sink, .. } => {
                    let target_name = name.clone();
                    let new_sk = new_sink.clone();
                    let resp = pipelines.apply(cmd).await;
                    if matches!(&resp, RegistryResponse::Error { .. }) {
                        return resp;
                    }
                    if let Some(sup) = supervisor.as_ref() {
                        if let Err(e) = sup.cluster_reconfigure_sink(&target_name, new_sk).await {
                            tracing::warn!(
                                pipeline = %target_name,
                                error = %e,
                                "supervisor reconfigure_sink failed after Raft commit"
                            );
                        }
                    }
                    resp
                }
                RegistryCommand::SetPipelineState { name, state } => {
                    let target_name = name.clone();
                    let target_state = *state;
                    // Update declarative state first so a subsequent
                    // supervisor.start() can read the latest definition.
                    let resp = pipelines.apply(cmd).await;
                    if matches!(&resp, RegistryResponse::Error { .. }) {
                        return resp;
                    }
                    if let Some(sup) = supervisor.as_ref() {
                        match target_state {
                            PipelineState::Running => {
                                if let Some(def) = pipelines.get(&target_name).await {
                                    if let Err(e) = sup.start(&def).await {
                                        tracing::error!(
                                            pipeline = %target_name,
                                            error = %e,
                                            "supervisor failed to start pipeline after Raft commit"
                                        );
                                    }
                                }
                            }
                            PipelineState::Stopped | PipelineState::Failed => {
                                if let Err(e) = sup.stop(&target_name).await {
                                    tracing::error!(
                                        pipeline = %target_name,
                                        error = %e,
                                        "supervisor failed to stop pipeline after Raft commit"
                                    );
                                }
                            }
                            // Upgrading / Created: no supervisor action —
                            // the running task (if any) keeps going, or
                            // the pipeline has not yet been started.
                            _ => {}
                        }
                    }
                    resp
                }
                RegistryCommand::DeletePipeline { name } => {
                    // Ensure the running task stops before we wipe the
                    // declarative entry; otherwise a lingering task would
                    // hold references to a now-deleted definition.
                    if let Some(sup) = supervisor.as_ref() {
                        if let Err(e) = sup.stop(name).await {
                            tracing::warn!(
                                pipeline = %name,
                                error = %e,
                                "supervisor stop on delete failed"
                            );
                        }
                    }
                    pipelines.apply(cmd).await
                }
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
