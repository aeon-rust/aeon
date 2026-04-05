//! Pipeline Manager — lifecycle management for Aeon pipelines.
//!
//! Each pipeline runs independently with its own source, processor, sink,
//! ring buffers, and metrics. The manager handles create/start/stop/upgrade
//! operations and maintains pipeline state.
//!
//! In cluster mode, pipeline definitions are Raft-replicated. The manager
//! on each node only runs pipelines assigned to that node.

use std::collections::BTreeMap;

use aeon_types::AeonError;
use aeon_types::registry::{
    BlueGreenActive, BlueGreenState, CanaryState, CanaryThresholds, PipelineAction,
    PipelineDefinition, PipelineHistoryEntry, PipelineState, ProcessorRef, RegistryCommand,
    RegistryResponse, UpgradeInfo,
};
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
            RegistryCommand::CreatePipeline { definition } => self.apply_create(*definition).await,
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
        let pipeline = pipelines
            .get_mut(name)
            .ok_or_else(|| AeonError::not_found(format!("pipeline '{name}'")))?;

        match pipeline.state {
            PipelineState::Created | PipelineState::Stopped => {
                let from = pipeline.state;
                pipeline.state = PipelineState::Running;
                pipeline.updated_at = now_millis();
                self.record_history(
                    name,
                    PipelineAction::Started,
                    actor,
                    from,
                    PipelineState::Running,
                    None,
                )
                .await;
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
        let pipeline = pipelines
            .get_mut(name)
            .ok_or_else(|| AeonError::not_found(format!("pipeline '{name}'")))?;

        match pipeline.state {
            PipelineState::Running | PipelineState::Upgrading => {
                let from = pipeline.state;
                pipeline.state = PipelineState::Stopped;
                pipeline.updated_at = now_millis();
                self.record_history(
                    name,
                    PipelineAction::Stopped,
                    actor,
                    from,
                    PipelineState::Stopped,
                    None,
                )
                .await;
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
        let pipeline = pipelines
            .get_mut(name)
            .ok_or_else(|| AeonError::not_found(format!("pipeline '{name}'")))?;

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

    // ── Blue-Green upgrade ──────────────────────────────────────────────

    /// Start a blue-green upgrade: deploy the green (new) processor alongside
    /// the blue (old) one. Traffic continues to blue until `cutover()`.
    pub async fn upgrade_blue_green(
        &self,
        name: &str,
        new_processor: ProcessorRef,
        actor: &str,
    ) -> Result<(), AeonError> {
        let mut pipelines = self.pipelines.write().await;
        let pipeline = pipelines
            .get_mut(name)
            .ok_or_else(|| AeonError::not_found(format!("pipeline '{name}'")))?;

        if pipeline.state != PipelineState::Running {
            return Err(AeonError::Config {
                message: format!(
                    "can only start blue-green on running pipelines, current state: {}",
                    pipeline.state
                ),
            });
        }
        if pipeline.upgrade_state.is_some() {
            return Err(AeonError::Config {
                message: format!("pipeline '{name}' already has an upgrade in progress"),
            });
        }

        let old_proc = pipeline.processor.clone();
        let new_proc_str = new_processor.to_string();

        pipeline.state = PipelineState::Upgrading;
        pipeline.upgrade_state = Some(UpgradeInfo::BlueGreen(BlueGreenState {
            blue_processor: old_proc,
            green_processor: new_processor,
            active: BlueGreenActive::Blue,
            started_at: now_millis(),
        }));
        pipeline.updated_at = now_millis();

        drop(pipelines);
        self.record_history(
            name,
            PipelineAction::BlueGreenStarted,
            actor,
            PipelineState::Running,
            PipelineState::Upgrading,
            Some(format!("green: {new_proc_str}")),
        )
        .await;

        tracing::info!(pipeline = name, green = %new_proc_str, "blue-green upgrade started");
        Ok(())
    }

    /// Cut over traffic from blue to green processor.
    pub async fn cutover(&self, name: &str, actor: &str) -> Result<(), AeonError> {
        let mut pipelines = self.pipelines.write().await;
        let pipeline = pipelines
            .get_mut(name)
            .ok_or_else(|| AeonError::not_found(format!("pipeline '{name}'")))?;

        let bg = match &mut pipeline.upgrade_state {
            Some(UpgradeInfo::BlueGreen(bg)) => bg,
            _ => {
                return Err(AeonError::Config {
                    message: format!("pipeline '{name}' is not in a blue-green upgrade"),
                });
            }
        };

        if bg.active == BlueGreenActive::Green {
            return Err(AeonError::Config {
                message: "already cut over to green".into(),
            });
        }

        bg.active = BlueGreenActive::Green;
        // Promote green to the pipeline's primary processor
        pipeline.processor = bg.green_processor.clone();
        pipeline.state = PipelineState::Running;
        let new_proc_str = pipeline.processor.to_string();
        pipeline.upgrade_state = None;
        pipeline.updated_at = now_millis();

        drop(pipelines);
        self.record_history(
            name,
            PipelineAction::BlueGreenCutover,
            actor,
            PipelineState::Upgrading,
            PipelineState::Running,
            Some(new_proc_str.clone()),
        )
        .await;

        tracing::info!(pipeline = name, processor = %new_proc_str, "blue-green cutover complete");
        Ok(())
    }

    /// Roll back any in-progress upgrade (blue-green or canary).
    pub async fn rollback_upgrade(&self, name: &str, actor: &str) -> Result<(), AeonError> {
        let mut pipelines = self.pipelines.write().await;
        let pipeline = pipelines
            .get_mut(name)
            .ok_or_else(|| AeonError::not_found(format!("pipeline '{name}'")))?;

        let original_proc = match &pipeline.upgrade_state {
            Some(UpgradeInfo::BlueGreen(bg)) => bg.blue_processor.clone(),
            Some(UpgradeInfo::Canary(cs)) => cs.baseline_processor.clone(),
            None => {
                return Err(AeonError::Config {
                    message: format!("pipeline '{name}' has no upgrade in progress"),
                });
            }
        };

        pipeline.processor = original_proc;
        pipeline.state = PipelineState::Running;
        pipeline.upgrade_state = None;
        pipeline.updated_at = now_millis();

        drop(pipelines);
        self.record_history(
            name,
            PipelineAction::UpgradeRolledBack,
            actor,
            PipelineState::Upgrading,
            PipelineState::Running,
            Some("rolled back to previous processor".into()),
        )
        .await;

        tracing::info!(pipeline = name, "upgrade rolled back");
        Ok(())
    }

    // ── Canary upgrade ────────────────────────────────────────────────

    /// Start a canary upgrade with gradual traffic shifting.
    pub async fn upgrade_canary(
        &self,
        name: &str,
        new_processor: ProcessorRef,
        steps: Vec<u8>,
        thresholds: CanaryThresholds,
        actor: &str,
    ) -> Result<(), AeonError> {
        let mut pipelines = self.pipelines.write().await;
        let pipeline = pipelines
            .get_mut(name)
            .ok_or_else(|| AeonError::not_found(format!("pipeline '{name}'")))?;

        if pipeline.state != PipelineState::Running {
            return Err(AeonError::Config {
                message: format!(
                    "can only start canary on running pipelines, current state: {}",
                    pipeline.state
                ),
            });
        }
        if pipeline.upgrade_state.is_some() {
            return Err(AeonError::Config {
                message: format!("pipeline '{name}' already has an upgrade in progress"),
            });
        }
        if steps.is_empty() {
            return Err(AeonError::Config {
                message: "canary steps must not be empty".into(),
            });
        }

        let old_proc = pipeline.processor.clone();
        let new_proc_str = new_processor.to_string();
        let initial_pct = steps[0];

        pipeline.state = PipelineState::Upgrading;
        pipeline.upgrade_state = Some(UpgradeInfo::Canary(CanaryState {
            baseline_processor: old_proc,
            canary_processor: new_processor,
            traffic_pct: initial_pct,
            steps,
            current_step: 0,
            thresholds,
            started_at: now_millis(),
        }));
        pipeline.updated_at = now_millis();

        drop(pipelines);
        self.record_history(
            name,
            PipelineAction::CanaryStarted,
            actor,
            PipelineState::Running,
            PipelineState::Upgrading,
            Some(format!("{new_proc_str} at {initial_pct}%")),
        )
        .await;

        tracing::info!(pipeline = name, canary = %new_proc_str, pct = initial_pct, "canary upgrade started");
        Ok(())
    }

    /// Promote canary to the next traffic step. If already at the last step,
    /// completes the canary by making the new processor primary.
    pub async fn promote_canary(&self, name: &str, actor: &str) -> Result<(), AeonError> {
        let mut pipelines = self.pipelines.write().await;
        let pipeline = pipelines
            .get_mut(name)
            .ok_or_else(|| AeonError::not_found(format!("pipeline '{name}'")))?;

        let cs = match &mut pipeline.upgrade_state {
            Some(UpgradeInfo::Canary(cs)) => cs,
            _ => {
                return Err(AeonError::Config {
                    message: format!("pipeline '{name}' is not in a canary upgrade"),
                });
            }
        };

        let next_step = cs.current_step + 1;
        if next_step < cs.steps.len() {
            // Advance to next step
            cs.current_step = next_step;
            cs.traffic_pct = cs.steps[next_step];
            let pct = cs.traffic_pct;
            let proc_str = cs.canary_processor.to_string();
            pipeline.updated_at = now_millis();

            drop(pipelines);
            self.record_history(
                name,
                PipelineAction::CanaryPromoted,
                actor,
                PipelineState::Upgrading,
                PipelineState::Upgrading,
                Some(format!("{proc_str} at {pct}%")),
            )
            .await;

            tracing::info!(pipeline = name, pct = pct, "canary promoted");
        } else {
            // Already at last step — complete the canary
            let new_proc = cs.canary_processor.clone();
            let new_proc_str = new_proc.to_string();
            pipeline.processor = new_proc;
            pipeline.state = PipelineState::Running;
            pipeline.upgrade_state = None;
            pipeline.updated_at = now_millis();

            drop(pipelines);
            self.record_history(
                name,
                PipelineAction::CanaryCompleted,
                actor,
                PipelineState::Upgrading,
                PipelineState::Running,
                Some(format!("{new_proc_str} at 100%")),
            )
            .await;

            tracing::info!(pipeline = name, processor = %new_proc_str, "canary completed");
        }

        Ok(())
    }

    /// Get the current canary status for a pipeline.
    pub async fn canary_status(&self, name: &str) -> Option<CanaryState> {
        let pipelines = self.pipelines.read().await;
        let pipeline = pipelines.get(name)?;
        match &pipeline.upgrade_state {
            Some(UpgradeInfo::Canary(cs)) => Some(cs.clone()),
            _ => None,
        }
    }

    /// Delete a pipeline.
    pub async fn delete(&self, name: &str) -> Result<(), AeonError> {
        let mut pipelines = self.pipelines.write().await;
        if let Some(pipeline) = pipelines.get(name) {
            if pipeline.state == PipelineState::Running
                || pipeline.state == PipelineState::Upgrading
            {
                return Err(AeonError::Config {
                    message: format!(
                        "cannot delete pipeline in state '{}' — stop it first",
                        pipeline.state
                    ),
                });
            }
        }
        pipelines
            .remove(name)
            .ok_or_else(|| AeonError::not_found(format!("pipeline '{name}'")))?;

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

    async fn apply_upgrade(&self, name: &str, new_processor: ProcessorRef) -> RegistryResponse {
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
    use aeon_types::registry::{CanaryThresholds, ProcessorRef, SinkConfig, SourceConfig};
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
            ProcessorRef::new("proc", "1.0.0"),
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

        let new_proc = ProcessorRef::new("proc", "2.0.0");
        mgr.upgrade("upg", new_proc, "test").await.unwrap();

        let pipeline = mgr.get("upg").await.unwrap();
        assert_eq!(pipeline.processor.version, "2.0.0");
        assert_eq!(pipeline.state, PipelineState::Running);
    }

    #[tokio::test]
    async fn upgrade_stopped_fails() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("upg-fail")).await.unwrap();

        let new_proc = ProcessorRef::new("proc", "2.0.0");
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

    // ── Blue-Green tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn blue_green_lifecycle() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("bg")).await.unwrap();
        mgr.start("bg", "test").await.unwrap();

        let new_proc = ProcessorRef::new("proc", "2.0.0");
        mgr.upgrade_blue_green("bg", new_proc, "test")
            .await
            .unwrap();

        // Should be in Upgrading state with blue-green info
        let p = mgr.get("bg").await.unwrap();
        assert_eq!(p.state, PipelineState::Upgrading);
        assert!(p.upgrade_state.is_some());

        // Cutover to green
        mgr.cutover("bg", "test").await.unwrap();

        let p = mgr.get("bg").await.unwrap();
        assert_eq!(p.state, PipelineState::Running);
        assert_eq!(p.processor.version, "2.0.0");
        assert!(p.upgrade_state.is_none());
    }

    #[tokio::test]
    async fn blue_green_rollback() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("bg-rb")).await.unwrap();
        mgr.start("bg-rb", "test").await.unwrap();

        let new_proc = ProcessorRef::new("proc", "2.0.0");
        mgr.upgrade_blue_green("bg-rb", new_proc, "test")
            .await
            .unwrap();

        // Rollback instead of cutover
        mgr.rollback_upgrade("bg-rb", "test").await.unwrap();

        let p = mgr.get("bg-rb").await.unwrap();
        assert_eq!(p.state, PipelineState::Running);
        assert_eq!(p.processor.version, "1.0.0"); // Back to original
        assert!(p.upgrade_state.is_none());
    }

    #[tokio::test]
    async fn blue_green_on_stopped_fails() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("bg-stop")).await.unwrap();

        let new_proc = ProcessorRef::new("proc", "2.0.0");
        let result = mgr.upgrade_blue_green("bg-stop", new_proc, "test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn double_upgrade_fails() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("bg-dbl")).await.unwrap();
        mgr.start("bg-dbl", "test").await.unwrap();

        let p1 = ProcessorRef::new("proc", "2.0.0");
        mgr.upgrade_blue_green("bg-dbl", p1, "test").await.unwrap();

        // Second upgrade while first is in progress should fail
        let p2 = ProcessorRef::new("proc", "3.0.0");
        let result = mgr.upgrade_blue_green("bg-dbl", p2, "test").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn blue_green_history() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("bg-hist")).await.unwrap();
        mgr.start("bg-hist", "test").await.unwrap();

        let new_proc = ProcessorRef::new("proc", "2.0.0");
        mgr.upgrade_blue_green("bg-hist", new_proc, "test")
            .await
            .unwrap();
        mgr.cutover("bg-hist", "test").await.unwrap();

        let history = mgr.history("bg-hist").await;
        // created + started + blue-green-started + cutover
        assert_eq!(history.len(), 4);
    }

    // ── Canary tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn canary_lifecycle() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("can")).await.unwrap();
        mgr.start("can", "test").await.unwrap();

        let new_proc = ProcessorRef::new("proc", "2.0.0");
        mgr.upgrade_canary(
            "can",
            new_proc,
            vec![10, 50, 100],
            CanaryThresholds::default(),
            "test",
        )
        .await
        .unwrap();

        // Should be at 10%
        let cs = mgr.canary_status("can").await.unwrap();
        assert_eq!(cs.traffic_pct, 10);
        assert_eq!(cs.current_step, 0);

        // Promote to 50%
        mgr.promote_canary("can", "test").await.unwrap();
        let cs = mgr.canary_status("can").await.unwrap();
        assert_eq!(cs.traffic_pct, 50);
        assert_eq!(cs.current_step, 1);

        // Promote to 100%
        mgr.promote_canary("can", "test").await.unwrap();
        let cs = mgr.canary_status("can").await.unwrap();
        assert_eq!(cs.traffic_pct, 100);
        assert_eq!(cs.current_step, 2);

        // Promote again → completes the canary
        mgr.promote_canary("can", "test").await.unwrap();
        assert!(mgr.canary_status("can").await.is_none());

        let p = mgr.get("can").await.unwrap();
        assert_eq!(p.state, PipelineState::Running);
        assert_eq!(p.processor.version, "2.0.0");
    }

    #[tokio::test]
    async fn canary_rollback() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("can-rb")).await.unwrap();
        mgr.start("can-rb", "test").await.unwrap();

        let new_proc = ProcessorRef::new("proc", "2.0.0");
        mgr.upgrade_canary(
            "can-rb",
            new_proc,
            vec![10, 50, 100],
            CanaryThresholds::default(),
            "test",
        )
        .await
        .unwrap();

        mgr.promote_canary("can-rb", "test").await.unwrap(); // 50%

        // Rollback at 50%
        mgr.rollback_upgrade("can-rb", "test").await.unwrap();

        let p = mgr.get("can-rb").await.unwrap();
        assert_eq!(p.state, PipelineState::Running);
        assert_eq!(p.processor.version, "1.0.0");
        assert!(p.upgrade_state.is_none());
    }

    #[tokio::test]
    async fn canary_empty_steps_fails() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("can-empty")).await.unwrap();
        mgr.start("can-empty", "test").await.unwrap();

        let new_proc = ProcessorRef::new("proc", "2.0.0");
        let result = mgr
            .upgrade_canary(
                "can-empty",
                new_proc,
                vec![],
                CanaryThresholds::default(),
                "test",
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn canary_history() {
        let mgr = PipelineManager::new();
        mgr.create(make_pipeline("can-hist")).await.unwrap();
        mgr.start("can-hist", "test").await.unwrap();

        let new_proc = ProcessorRef::new("proc", "2.0.0");
        mgr.upgrade_canary(
            "can-hist",
            new_proc,
            vec![10, 100],
            CanaryThresholds::default(),
            "test",
        )
        .await
        .unwrap();
        mgr.promote_canary("can-hist", "test").await.unwrap(); // 100%
        mgr.promote_canary("can-hist", "test").await.unwrap(); // complete

        let history = mgr.history("can-hist").await;
        // created + started + canary-started + promoted(100%) + completed
        assert_eq!(history.len(), 5);
    }
}
