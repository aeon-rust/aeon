//! Single-node cluster integration tests.

#![cfg(feature = "cluster")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aeon_cluster::{ClusterConfig, ClusterNode, ClusterRequest, ClusterResponse};
use aeon_types::{
    PartitionId, PipelineDefinition, ProcessorRef, RegistryApplier, RegistryCommand,
    RegistryResponse, SinkConfig, SourceConfig,
};

#[tokio::test]
async fn bootstrap_single_node_becomes_leader() {
    let config = ClusterConfig::single_node(1, 16);
    let node = ClusterNode::bootstrap_single(config).await.unwrap();

    let leader = node.current_leader().await;
    assert_eq!(leader, Some(1), "single-node should be its own leader");

    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn bootstrap_assigns_all_partitions() {
    let config = ClusterConfig::single_node(1, 8);
    let node = ClusterNode::bootstrap_single(config).await.unwrap();

    // Propose a no-op assign to verify the partition exists
    for i in 0..8 {
        let resp = node
            .propose(ClusterRequest::AssignPartition {
                partition: PartitionId::new(i),
                node: 1,
            })
            .await
            .unwrap();
        assert_eq!(resp, ClusterResponse::Ok);
    }

    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn single_node_instant_commits() {
    let config = ClusterConfig::single_node(1, 4);
    let node = ClusterNode::bootstrap_single(config).await.unwrap();

    // Multiple rapid proposals should all succeed (quorum=1 = instant)
    for i in 0..20 {
        let resp = node
            .propose(ClusterRequest::UpdateConfig {
                key: format!("key-{i}"),
                value: format!("value-{i}"),
            })
            .await
            .unwrap();
        assert_eq!(resp, ClusterResponse::Ok);
    }

    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn single_node_full_transfer_lifecycle() {
    let config = ClusterConfig::single_node(1, 4);
    let node = ClusterNode::bootstrap_single(config).await.unwrap();

    // Begin transfer P0: 1 → 2
    let resp = node
        .propose(ClusterRequest::BeginTransfer {
            partition: PartitionId::new(0),
            source: 1,
            target: 2,
        })
        .await
        .unwrap();
    assert_eq!(resp, ClusterResponse::Ok);

    // Complete transfer
    let resp = node
        .propose(ClusterRequest::CompleteTransfer {
            partition: PartitionId::new(0),
            new_owner: 2,
        })
        .await
        .unwrap();
    assert_eq!(resp, ClusterResponse::Ok);

    // Verify P0 is now owned by 2 (assign back to 1 to verify prior state)
    let resp = node
        .propose(ClusterRequest::BeginTransfer {
            partition: PartitionId::new(0),
            source: 2,
            target: 1,
        })
        .await
        .unwrap();
    assert_eq!(resp, ClusterResponse::Ok);

    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn single_node_transfer_wrong_source_fails() {
    let config = ClusterConfig::single_node(1, 4);
    let node = ClusterNode::bootstrap_single(config).await.unwrap();

    // P0 is owned by node 1, try to transfer from node 5 (wrong)
    let resp = node
        .propose(ClusterRequest::BeginTransfer {
            partition: PartitionId::new(0),
            source: 5,
            target: 2,
        })
        .await
        .unwrap();
    assert!(matches!(resp, ClusterResponse::Error(_)));

    node.shutdown().await.unwrap();
}

/// Counter applier — records every `RegistryCommand` it sees and returns
/// `PipelineCreated` / `Ok` so the cluster path exercises both the encode
/// and decode halves of `ClusterResponse::Registry`.
struct CountingApplier {
    calls: Arc<AtomicUsize>,
    last_pipeline_name: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl RegistryApplier for CountingApplier {
    fn apply(
        &self,
        cmd: RegistryCommand,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RegistryResponse> + Send + '_>> {
        let calls = Arc::clone(&self.calls);
        let last = Arc::clone(&self.last_pipeline_name);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            match cmd {
                RegistryCommand::CreatePipeline { definition } => {
                    *last.lock().await = Some(definition.name.clone());
                    RegistryResponse::PipelineCreated {
                        name: definition.name,
                    }
                }
                _ => RegistryResponse::Ok,
            }
        })
    }
}

#[tokio::test]
async fn registry_command_replicates_through_raft_and_hits_applier() {
    // Proves the pipeline-replication gap fix: a RegistryCommand submitted
    // via propose_registry() is serialized into ClusterRequest::Registry,
    // committed through Raft, dispatched to the installed applier on apply,
    // and its RegistryResponse is decoded back out of ClusterResponse::Registry.
    let config = ClusterConfig::single_node(1, 4);
    let node = ClusterNode::bootstrap_single(config).await.unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(tokio::sync::Mutex::new(None));
    let applier = Arc::new(CountingApplier {
        calls: Arc::clone(&calls),
        last_pipeline_name: Arc::clone(&last),
    });
    node.install_registry_applier(applier).await;

    let def = PipelineDefinition::new(
        "replicated-pipeline",
        SourceConfig {
            source_type: "memory".into(),
            topic: None,
            partitions: vec![],
            config: Default::default(),
        },
        ProcessorRef::new("passthrough", "v1"),
        SinkConfig {
            sink_type: "blackhole".into(),
            topic: None,
            config: Default::default(),
        },
        0,
    );

    let resp = node
        .propose_registry(RegistryCommand::CreatePipeline {
            definition: Box::new(def),
        })
        .await
        .unwrap();

    match resp {
        RegistryResponse::PipelineCreated { name } => {
            assert_eq!(name, "replicated-pipeline");
        }
        other => panic!("unexpected response: {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        last.lock().await.as_deref(),
        Some("replicated-pipeline"),
        "applier must observe the pipeline definition from the committed Raft entry"
    );

    node.shutdown().await.unwrap();
}

#[tokio::test]
async fn registry_command_without_applier_replicates_silently() {
    // Without an applier installed, the state machine should still accept
    // and commit Registry entries (so they don't block other cluster work)
    // but report an Ok response since no local apply happened.
    let config = ClusterConfig::single_node(2, 4);
    let node = ClusterNode::bootstrap_single(config).await.unwrap();

    let def = PipelineDefinition::new(
        "no-applier-pipeline",
        SourceConfig {
            source_type: "memory".into(),
            topic: None,
            partitions: vec![],
            config: Default::default(),
        },
        ProcessorRef::new("passthrough", "v1"),
        SinkConfig {
            sink_type: "blackhole".into(),
            topic: None,
            config: Default::default(),
        },
        0,
    );

    // Raw propose so we observe the ClusterResponse::Ok from the no-applier path.
    let req = ClusterRequest::registry(&RegistryCommand::CreatePipeline {
        definition: Box::new(def),
    })
    .unwrap();
    let resp = node.propose(req).await.unwrap();
    assert_eq!(resp, ClusterResponse::Ok);

    node.shutdown().await.unwrap();
}
