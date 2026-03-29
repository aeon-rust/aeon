//! Single-node cluster integration tests.

#![cfg(feature = "cluster")]

use aeon_cluster::{ClusterConfig, ClusterNode, ClusterRequest, ClusterResponse};
use aeon_types::PartitionId;

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
