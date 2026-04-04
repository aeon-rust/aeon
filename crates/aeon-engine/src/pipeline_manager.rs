//! Pipeline Manager — lifecycle management for Aeon pipelines.
//!
//! Each pipeline runs independently with its own source, processor, sink,
//! ring buffers, and metrics. The manager handles create/start/stop/upgrade
//! operations and maintains pipeline state.
//!
//! In cluster mode, pipeline definitions are Raft-replicated. The manager
//! on each node only runs pipelines assigned to that node.

use std::collections::BTreeMap;

use aeon_types::registry::{
    PipelineAction, PipelineDefinition, PipelineHistoryEntry, PipelineState, ProcessorRef,
    RegistryCommand, RegistryResponse,
};
use aeon_types::AeonError;
use tokio::sync::RwLock;

/// Pipeline Manager — manages the lifecycle of all pipelines on this node.
pub struct PipelineManager {
    /// Pipeline definitions, keyed by name.
    pipelines: RwLock<BTreeMap<String, PipelineDefinition>>,
    /// History log per pipeline.
    history: RwLock<BTreeMap<String, Vec<PipelineHistoryEntry>>>,
}

impl PipelineManager {
    /// Create a new pipeline manager.
    pub fn new() -> Self {
        Self {
            pipelines: RwLock::new(BTreeMap::new()),
            history: RwLock::new(BTreeMap::new()),
        }
    }

    /// Apply a Raft-replicated pipeline command.
    pub async fn apply(&self, cmd: RegistryCommand) -> RegistryResponse {
        match cmd {
            RegistryCommand::CreatePipeline { definition } => {
                self.apply_create(definition).await
            }
            RegistryCommand::SetPipelineState { name, state } => {
                self.apply_set_state(&name, state, "system").await
            }
            RegistryCommand::UpgradePipeline {
                name,
                new_processor,
            } => self.apply_upgrade(&name, new_processor).await,
            RegistryCommand::DeletePipeline { name } => self.apply_delete(&name).await,
            // Processor commands are handled by ProcessorRegistry
            _ => RegistryResponse::Error {
                message: "command not handled by PipelineManager".into(),
            },
        }
    }

    /// Create a new pipeline.
    pub async fn create(
        &self,
        definition: PipelineDefinition,
    ) -> Result<RegistryResponse, AeonError> {
        let name = definition.name.clone();
        {
            let pipelines = self.pipelines.read().await;
            if pipelines.contains_key(&name) {
                return Err(AeonError::Config {
                    message: format!("pipeline '{name}' already exists"),
                });
            }
        }

        let resp = self.apply_create(definition).await;
        tracing::info!(pipeline = %name, "pipeline created");
        Ok(resp)
    }

    /// Start a pipeline.
    pub async fn start(&self, name: &str, actor: &str) -> Result<(), AeonError> {
        let mut pipelines = self.pipelines.write().await;
        let pipeline = pipelines.get_mut(name).ok_or_else(|| {
            AeonError::not_found(format!("pipeline '{name}'"))
        })?;

        match pipeline.state {
            PipelineState::Created | PipelineState::Stopped => {
                let from = pipeline.state;
                pipeline.state = PipelineState::Running;
                pipeline.updated_at = now_millis();
                self.record_history(name, PipelineAction::Started, actor, from, PipelineState::Running, None).await;
                tracing::info!(pipeline = name, "pipeline started");
                Ok(())
            }
            PipelineState::Running => Ok(()), // already running
            other => Err(AeonError::Config {
                message: format!("cannot start pipeline in state '{other}'"),
            }),
        }
    }

    /// Stop a pipeline.
    pub async fn stop(&self, name: &str, actor: &str) -> Result<(), AeonError> {
        let mut pipelines = self.pipelines.write().await;
        let pipeline = pipelines.get_mut(name).ok_or_else(|| {
            AeonError::not_found(format!("pipeline '{name}'"))
        })?;

        match pipeline.state {
            PipelineState::Running | PipelineState::Upgrading => {
                let from = pipeline.state;
                pipeline.state = PipelineState::Stopped;
                pipeline.updated_at = now_millis();
                self.record_history(name, PipelineAction::Stopped, actor, from, PipelineState::Stopped, None).await;
                tracing::info!(pipeline = name, "pipeline stopped");
                Ok(())
            }
            PipelineState::Stopped | PipelineState::Created => Ok(()),
            other => Err(AeonError::Config {
                message: format!("cannot stop pipeline in state '{other}'"),
            }),
        }
    }

    /// Get a pipeline definition by name.
    pub async fn get(&self, name: &str) -> Option<PipelineDefinition> {
        self.pipelines.read().await.get(name).cloned()
    }

    /// List all pipeline names.
    pub async fn list(&self) -> Vec<String> {
        self.pipelines.read().await.keys().cloned().collect()
    }

    /// List all pipelines with their states.
    pub async fn list_with_state(&self) -> Vec<(String, PipelineState)> {
        self.pipelines
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.state))
            .collect()
    }

    /// Get history for a pipeline.
    pub async fn history(&self, name: &str) -> Vec<PipelineHistoryEntry> {
        self.history
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// Initiate a drain-swap upgrade.
    ///
    /// This transitions the pipeline to Upgrading state, updates the processor
    /// reference, then transitions back to Running. The actual drain/swap/resume
    /// is coordinated by the engine's pipeline runner.
    pub async fn upgrade(
        &self,
        name: &str,
        new_processor: ProcessorRef,
        actor: &str,
    ) -> Result<(), AeonError> {
        let mut pipelines = self.pipelines.write().await;
        let pipeline = pipelines.get_mut(name).ok_or_else(|| {
            AeonError::not_found(format!("pipeline '{name}'"))
        })?;

        if pipeline.state != PipelineState::Running {
            return Err(AeonError::Config {
                message: format!(
                    "can only upgrade running pipelines, current state: {}",
                    pipeline.state
                ),
            });
        }

        let old_proc = pipeline.processor.to_string();
        let new_proc_str = new_processor.to_string();

        // Transition to Upgrading
        pipeline.state = PipelineState::Upgrading;
        pipeline.updated_at = now_millis();
        self.record_history(
            name,
            PipelineAction::UpgradeStarted,
            actor,
            PipelineState::Running,
            PipelineState::Upgrading,
            Some(format!("{old_proc} → {new_proc_str}")),
        )
        .await;

        // Update processor reference
        pipeline.processor = new_processor;

        // Transition back to Running (in real implementation, the engine's
        // pipeline runner handles the actual drain/swap/resume sequence)
        pipeline.state = PipelineState::Running;
        pipeline.updated_at = now_millis();
        self.record_history(
            name,
            PipelineAction::UpgradeCompleted,
            actor,
            PipelineState::Upgrading,
            PipelineState::Running,
            Some(new_proc_str.clone()),
        )
        .await;

        tracing::info!(pipeline = name, new_processor = %new_proc_str, "pipeline upgraded");
        Ok(())
    }

    /// Delete a pipeline.
    pub async fn delete(&self, name: &str) -> Result<(), AeonError> {
        let mut pipelines = self.pipelines.write().await;
        if let Some(pipeline) = pipelines.get(name) {
            if pipeline.state == PipelineState::Running
                || pipeline.state == PipelineState::Upgrading
            {
                return Err(AeonError::Config {
                    message: format!("cannot delete pipeline in state '{}' — stop it first", pipeline.state),
                });
            }
        }
        pipelines.remove(name).ok_or_else(|| {
            AeonError::not_found(format!("pipeline '{name}'"))
        })?;

        self.history.write().await.remove(name);
        tracing::info!(pipeline = name, "pipeline deleted");
        Ok(())
    }

    /// Get the number of pipelines.
    pub async fn count(&self) -> usize {
        self.pipelines.read().await.len()
    }

    /// Snapshot all pipeline state (for Raft).
    pub async fn snapshot(&self) -> BTreeMap<String, PipelineDefinition> {
        self.pipelines.read().await.clone()
    }

    /// Restore from snapshot.
    pub async fn restore(&self, data: BTreeMap<String, PipelineDefinition>) {
        *self.pipelines.write().await = data;
    }

    // ── Internal apply methods ─────────────────────────────────────────

    async fn apply_create(&self, definition: PipelineDefinition) -> RegistryResponse {
        let name = definition.name.clone();
        let mut pipelines = self.pipelines.write().await;

        if pipelines.contains_key(&name) {
            return RegistryResponse::Error {
                message: format!("pipeline '{name}' already exists"),
            };
        }

        self.record_history(
            &name,
            PipelineAction::Created,
            "system",
            PipelineState::Created,
            PipelineState::Created,
            None,
        )
        .await;

        pipelines.insert(name.clone(), definition);
        RegistryResponse::PipelineCreated { name }
    }

    async fn apply_set_state(
        &self,
        name: &str,
        state: PipelineState,
        actor: &str,
    ) -> RegistryResponse {
        let mut pipelines = self.pipelines.write().await;
        if let Some(pipeline) = pipelines.get_mut(name) {
            let from = pipeline.state;
            pipeline.state = state;
            pipeline.updated_at = now_millis();

            let action = match state {
                PipelineState::Running => PipelineAction::Started,
                PipelineState::Stopped => PipelineAction::Stopped,
                PipelineState::Failed => PipelineAction::Failed,
                PipelineState::Upgrading => PipelineAction::UpgradeStarted,
                PipelineState::Created => PipelineAction::Created,
            };
            // Drop the write lock before recording history (history has its own lock)
            drop(pipelines);
            self.record_history(name, action, actor, from, state, None)
                .await;

            RegistryResponse::Ok
        } else {
            RegistryResponse::Error {
                message: format!("pipeline '{name}' not found"),
            }
        }
    }

    async fn apply_upgrade(
        &self,
        name: &str,
        new_processor: ProcessorRef,
    ) -> RegistryResponse {
        let mut pipelines = self.pipelines.write().await;
        if let Some(pipeline) = pipelines.get_mut(name) {
            pipeline.processor = new_processor;
            pipeline.updated_at = now_millis();
            RegistryResponse::Ok
        } else {
            RegistryResponse::Error {
                message: format!("pipeline '{name}' not found"),
            }
        }
    }

    async fn apply_delete(&self, name: &str) -> RegistryResponse {
        let mut pipelines = self.pipelines.write().await;
        if pipelines.remove(name).is_some() {
            drop(pipelines);
            self.history.write().await.remove(name);
            RegistryResponse::Ok
        } else {
            RegistryResponse::Error {
                message: format!("pipeline '{name}' not found"),
            }
        }
    }

    async fn record_history(
        &self,
        name: &str,
        action: PipelineAction,
        actor: &str,
        from: PipelineState,
        to: PipelineState,
        details: Option<String>,
    ) {
        let entry = PipelineHistoryEntry {
            timestamp: now_millis(),
            action,
            actor: actor.into(),
            from_state: from,
            to_state: to,
            details,
        };
        self.history
            .write()
            .await
            .entry(name.to_string())
            .or_default()
            .push(entry);
    }
}

impl Default for PipelineManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PipelineManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineManager").finish()
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use aeon_types::registry::{ProcessorRef, SinkConfig, SourceConfig};
    use std::collections::BTreeMap;

    fn make_pipeline(name: &str) -> PipelineDefinition {
        PipelineDefinition::new(
            name,
            SourceConfig {
                source_type: "kafka".into(),
                topic: Some("input".into()),
                partitions: vec![0, 1],
                config: BTreeMap::new(),
            },
            ProcessorRef {
                name: "proc".into(),
                version: "1.0.0".into(),
            },
            SinkConfig {
                sink_type: "kafka".into(),
                topic: Some("output".into()),
                config: BTreeMap::new(),
            },
            1000,
        )
    }

    #[tokio::test]
    async fn create_and_list() {
        let mgr = PipelineManager::new();

        mgr.create(make_pipeline("pipe-1")).await.unwrap();
        mgr.create(make_pipeline("pipe-2")).await.unwrap();

        let list = mgr.list().await;
        assert_eq!(list.len(), 2);
        assert!(list.contains(&"pipe-1".to_string()));
        assert!(list.contains(&"pipe-2".to_string()));
    }

    #[tokio::test]
    async fn create_duplicate_fails() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("dup")).await.unwrap();

        let result = mgr.create(make_pipeline("dup")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn start_stop_lifecycle() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("lc")).await.unwrap();

        // Created → Running
        mgr.start("lc", "test").await.unwrap();
        assert_eq!(mgr.get("lc").await.unwrap().state, PipelineState::Running);

        // Running → Stopped
        mgr.stop("lc", "test").await.unwrap();
        assert_eq!(mgr.get("lc").await.unwrap().state, PipelineState::Stopped);

        // Stopped → Running
        mgr.start("lc", "test").await.unwrap();
        assert_eq!(mgr.get("lc").await.unwrap().state, PipelineState::Running);
    }

    #[tokio::test]
    async fn upgrade_pipeline() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("upg")).await.unwrap();
        mgr.start("upg", "test").await.unwrap();

        let new_proc = ProcessorRef {
            name: "proc".into(),
            version: "2.0.0".into(),
        };
        mgr.upgrade("upg", new_proc, "test").await.unwrap();

        let pipeline = mgr.get("upg").await.unwrap();
        assert_eq!(pipeline.processor.version, "2.0.0");
        assert_eq!(pipeline.state, PipelineState::Running);
    }

    #[tokio::test]
    async fn upgrade_stopped_fails() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("upg-fail")).await.unwrap();

        let new_proc = ProcessorRef {
            name: "proc".into(),
            version: "2.0.0".into(),
        };
        let result = mgr.upgrade("upg-fail", new_proc, "test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn delete_stopped_pipeline() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("del")).await.unwrap();

        mgr.delete("del").await.unwrap();
        assert_eq!(mgr.count().await, 0);
    }

    #[tokio::test]
    async fn delete_running_fails() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("del-run")).await.unwrap();
        mgr.start("del-run", "test").await.unwrap();

        let result = mgr.delete("del-run").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn history_tracking() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("hist")).await.unwrap();
        mgr.start("hist", "admin").await.unwrap();
        mgr.stop("hist", "admin").await.unwrap();

        let history = mgr.history("hist").await;
        assert_eq!(history.len(), 3); // created + started + stopped
        assert_eq!(history[0].actor, "system"); // created
        assert_eq!(history[1].actor, "admin"); // started
        assert_eq!(history[2].actor, "admin"); // stopped
    }

    #[tokio::test]
    async fn snapshot_and_restore() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("snap-1")).await.unwrap();
        mgr.start("snap-1", "test").await.unwrap();

        let snapshot = mgr.snapshot().await;

        let mgr2 = PipelineManager::new();
        mgr2.restore(snapshot).await;
        assert_eq!(mgr2.count().await, 1);

        let p = mgr2.get("snap-1").await.unwrap();
        assert_eq!(p.state, PipelineState::Running);
    }

    #[tokio::test]
    async fn list_with_state() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("s1")).await.unwrap();
        mgr.create(make_pipeline("s2")).await.unwrap();
        mgr.start("s1", "test").await.unwrap();

        let states = mgr.list_with_state().await;
        assert_eq!(states.len(), 2);

        let s1 = states.iter().find(|(n, _)| n == "s1").unwrap();
        let s2 = states.iter().find(|(n, _)| n == "s2").unwrap();
        assert_eq!(s1.1, PipelineState::Running);
        assert_eq!(s2.1, PipelineState::Created);
    }
}
