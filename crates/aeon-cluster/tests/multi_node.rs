//! Multi-node cluster integration tests using QUIC transport.
//!
//! Covers: 3-node bootstrap, dynamic node addition (1→2, 2→3, 3→5),
//! node removal, leader re-election after shutdown, even-number handling,
//! and log replication verification.

#![cfg(feature = "cluster")]

use std::collections::BTreeMap;
use std::sync::Arc;

use aeon_cluster::raft_config::AeonRaftConfig;
use aeon_cluster::store::{MemLogStore, StateMachineStore};
use aeon_cluster::transport::endpoint::QuicEndpoint;
use aeon_cluster::transport::network::QuicNetworkFactory;
use aeon_cluster::transport::server;
use aeon_cluster::transport::tls::dev_quic_configs_insecure;
use aeon_cluster::{ClusterRequest, ClusterResponse, NodeAddress};
use aeon_types::PartitionId;
use openraft::{Config, Raft};

// ── Helpers ────────────────────────────────────────────────────────────

/// Build QUIC configs for integration tests (AcceptAnyCert on client side).
fn dev_quic_configs() -> (quinn::ServerConfig, quinn::ClientConfig) {
    dev_quic_configs_insecure()
}

/// Create a QUIC endpoint bound to an ephemeral port on 127.0.0.1.
fn create_endpoint(
    server_cfg: quinn::ServerConfig,
    client_cfg: quinn::ClientConfig,
) -> Arc<QuicEndpoint> {
    Arc::new(
        QuicEndpoint::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_cfg,
            client_cfg,
        )
        .unwrap(),
    )
}

/// Create a Raft node with QUIC transport.
async fn create_quic_node(node_id: u64, endpoint: Arc<QuicEndpoint>) -> Raft<AeonRaftConfig> {
    let config = Arc::new(Config {
        cluster_name: "aeon-test".to_string(),
        heartbeat_interval: 200,
        election_timeout_min: 500,
        election_timeout_max: 1000,
        ..Config::default()
    });

    let log_store = MemLogStore::new();
    let state_machine = StateMachineStore::new();
    let network = QuicNetworkFactory::new(endpoint);

    Raft::new(node_id, config, network, log_store, state_machine)
        .await
        .unwrap()
}

/// Start the QUIC RPC server task for a Raft node.
fn start_server(
    endpoint: &Arc<QuicEndpoint>,
    raft: &Raft<AeonRaftConfig>,
    shutdown: &Arc<std::sync::atomic::AtomicBool>,
) {
    let ep = Arc::clone(endpoint);
    let r = raft.clone();
    let s = Arc::clone(shutdown);
    tokio::spawn(async move { server::serve(ep, r, s).await });
}

/// Poll until a leader is elected (up to timeout_secs).
async fn wait_for_leader(raft: &Raft<AeonRaftConfig>, timeout_secs: u64) -> Option<u64> {
    for _ in 0..(timeout_secs * 10) {
        if let Some(leader) = raft.current_leader().await {
            return Some(leader);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

/// Get the Raft reference for a leader from a slice of (id, raft) pairs.
fn leader_raft<'a>(
    leader_id: u64,
    nodes: &'a [(u64, Raft<AeonRaftConfig>)],
) -> &'a Raft<AeonRaftConfig> {
    nodes
        .iter()
        .find(|(id, _)| *id == leader_id)
        .map(|(_, r)| r)
        .unwrap()
}

/// Shutdown all nodes and endpoints.
async fn shutdown_all(
    shutdown: &std::sync::atomic::AtomicBool,
    nodes: &[Raft<AeonRaftConfig>],
    endpoints: &[Arc<QuicEndpoint>],
) {
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    for r in nodes {
        let _ = r.shutdown().await;
    }
    for ep in endpoints {
        ep.close();
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn three_node_cluster_formation() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    let ep1 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep2 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep3 = create_endpoint(server_cfg, client_cfg);

    let addr1 = ep1.local_addr().unwrap();
    let addr2 = ep2.local_addr().unwrap();
    let addr3 = ep3.local_addr().unwrap();

    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;
    let raft3 = create_quic_node(3, Arc::clone(&ep3)).await;

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    start_server(&ep1, &raft1, &shutdown);
    start_server(&ep2, &raft2, &shutdown);
    start_server(&ep3, &raft3, &shutdown);

    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    members.insert(2u64, NodeAddress::new("127.0.0.1", addr2.port()));
    members.insert(3u64, NodeAddress::new("127.0.0.1", addr3.port()));

    raft1.initialize(members.clone()).await.unwrap();
    raft2.initialize(members.clone()).await.unwrap();
    raft3.initialize(members).await.unwrap();

    let leader_id = wait_for_leader(&raft1, 10).await;
    assert!(leader_id.is_some(), "cluster should have elected a leader");

    // All 3 nodes should agree on the leader
    let l1 = raft1.current_leader().await;
    let l2 = raft2.current_leader().await;
    let l3 = raft3.current_leader().await;
    assert_eq!(l1, l2, "nodes 1 and 2 should agree on leader");
    assert_eq!(l2, l3, "nodes 2 and 3 should agree on leader");

    // Propose via leader
    let nodes = [(1u64, raft1.clone()), (2, raft2.clone()), (3, raft3.clone())];
    let leader = leader_raft(leader_id.unwrap(), &nodes);
    let resp = leader
        .client_write(ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 1,
        })
        .await
        .unwrap();
    assert_eq!(resp.data, ClusterResponse::Ok);

    shutdown_all(&shutdown, &[raft1, raft2, raft3], &[ep1, ep2, ep3]).await;
}

#[tokio::test]
async fn three_node_log_replication() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    let ep1 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep2 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep3 = create_endpoint(server_cfg, client_cfg);

    let addr1 = ep1.local_addr().unwrap();
    let addr2 = ep2.local_addr().unwrap();
    let addr3 = ep3.local_addr().unwrap();

    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;
    let raft3 = create_quic_node(3, Arc::clone(&ep3)).await;

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    start_server(&ep1, &raft1, &shutdown);
    start_server(&ep2, &raft2, &shutdown);
    start_server(&ep3, &raft3, &shutdown);

    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    members.insert(2u64, NodeAddress::new("127.0.0.1", addr2.port()));
    members.insert(3u64, NodeAddress::new("127.0.0.1", addr3.port()));

    raft1.initialize(members.clone()).await.unwrap();
    raft2.initialize(members.clone()).await.unwrap();
    raft3.initialize(members).await.unwrap();

    let leader_id = wait_for_leader(&raft1, 10).await.unwrap();
    let nodes = [(1u64, raft1.clone()), (2, raft2.clone()), (3, raft3.clone())];
    let leader = leader_raft(leader_id, &nodes);

    // Propose multiple entries
    for i in 0..5 {
        let resp = leader
            .client_write(ClusterRequest::AssignPartition {
                partition: PartitionId::new(i),
                node: (i as u64 % 3) + 1,
            })
            .await
            .unwrap();
        assert_eq!(resp.data, ClusterResponse::Ok);
    }

    // Wait for replication
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // All nodes should have applied entries
    let m1 = raft1.metrics().borrow().clone();
    let m2 = raft2.metrics().borrow().clone();
    let m3 = raft3.metrics().borrow().clone();

    assert!(m1.last_applied.is_some(), "node 1 should have applied entries");
    assert!(m2.last_applied.is_some(), "node 2 should have applied entries");
    assert!(m3.last_applied.is_some(), "node 3 should have applied entries");

    shutdown_all(&shutdown, &[raft1, raft2, raft3], &[ep1, ep2, ep3]).await;
}

/// Test: Start a single-node cluster, dynamically add a 2nd node.
/// Verifies: even-number cluster (2 nodes) works, log replicates to new node.
#[tokio::test]
async fn dynamic_add_second_node() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    let ep1 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep2 = create_endpoint(server_cfg, client_cfg);

    let addr1 = ep1.local_addr().unwrap();
    let addr2 = ep2.local_addr().unwrap();

    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    start_server(&ep1, &raft1, &shutdown);
    start_server(&ep2, &raft2, &shutdown);

    // Bootstrap single-node cluster (just node 1)
    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    raft1.initialize(members).await.unwrap();

    // Wait for node 1 to become leader
    let leader_id = wait_for_leader(&raft1, 5).await;
    assert_eq!(leader_id, Some(1), "node 1 should be the leader of single-node cluster");

    // Propose an entry before adding the second node
    let resp = raft1
        .client_write(ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 1,
        })
        .await
        .unwrap();
    assert_eq!(resp.data, ClusterResponse::Ok);

    // Step 1: Add node 2 as learner (blocking = true waits for log catch-up)
    let addr2_na = NodeAddress::new("127.0.0.1", addr2.port());
    raft1.add_learner(2, addr2_na, true).await.unwrap();

    // Step 2: Promote node 2 to voter
    let mut voters = std::collections::BTreeSet::new();
    voters.insert(2u64);
    raft1
        .change_membership(openraft::ChangeMembers::AddVoterIds(voters), false)
        .await
        .unwrap();

    // Verify: 2-node cluster (even number, should work)
    let m1 = raft1.metrics().borrow().clone();
    let voter_count = m1.membership_config.membership().voter_ids().count();
    assert_eq!(voter_count, 2, "cluster should have 2 voters");

    // Verify node 2 has replicated data
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let m2 = raft2.metrics().borrow().clone();
    assert!(m2.last_applied.is_some(), "node 2 should have replicated entries");

    // Propose another entry — quorum is 2/2 so both nodes must agree
    let resp = raft1
        .client_write(ClusterRequest::AssignPartition {
            partition: PartitionId::new(1),
            node: 2,
        })
        .await
        .unwrap();
    assert_eq!(resp.data, ClusterResponse::Ok);

    shutdown_all(&shutdown, &[raft1, raft2], &[ep1, ep2]).await;
}

/// Test: Start single node, add 2nd then 3rd node (1→2→3).
/// Verifies: sequential dynamic scaling to a proper 3-node quorum.
#[tokio::test]
async fn dynamic_scale_one_to_three() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    let ep1 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep2 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep3 = create_endpoint(server_cfg, client_cfg);

    let addr1 = ep1.local_addr().unwrap();
    let addr2 = ep2.local_addr().unwrap();
    let addr3 = ep3.local_addr().unwrap();

    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;
    let raft3 = create_quic_node(3, Arc::clone(&ep3)).await;

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    start_server(&ep1, &raft1, &shutdown);
    start_server(&ep2, &raft2, &shutdown);
    start_server(&ep3, &raft3, &shutdown);

    // Bootstrap single-node
    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    raft1.initialize(members).await.unwrap();
    wait_for_leader(&raft1, 5).await.unwrap();

    // Add node 2
    raft1
        .add_learner(2, NodeAddress::new("127.0.0.1", addr2.port()), true)
        .await
        .unwrap();
    let mut voters = std::collections::BTreeSet::new();
    voters.insert(2u64);
    raft1
        .change_membership(openraft::ChangeMembers::AddVoterIds(voters), false)
        .await
        .unwrap();

    // Verify 2-node cluster
    let m = raft1.metrics().borrow().clone();
    assert_eq!(m.membership_config.membership().voter_ids().count(), 2);

    // Add node 3
    raft1
        .add_learner(3, NodeAddress::new("127.0.0.1", addr3.port()), true)
        .await
        .unwrap();
    let mut voters = std::collections::BTreeSet::new();
    voters.insert(3u64);
    raft1
        .change_membership(openraft::ChangeMembers::AddVoterIds(voters), false)
        .await
        .unwrap();

    // Verify 3-node cluster
    let m = raft1.metrics().borrow().clone();
    assert_eq!(m.membership_config.membership().voter_ids().count(), 3);

    // All nodes should agree on leader
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let l1 = raft1.current_leader().await;
    let l2 = raft2.current_leader().await;
    let l3 = raft3.current_leader().await;
    assert_eq!(l1, l2);
    assert_eq!(l2, l3);
    assert!(l1.is_some());

    // Propose via leader — quorum is 2/3
    let nodes = [(1u64, raft1.clone()), (2, raft2.clone()), (3, raft3.clone())];
    let leader = leader_raft(l1.unwrap(), &nodes);
    let resp = leader
        .client_write(ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 1,
        })
        .await
        .unwrap();
    assert_eq!(resp.data, ClusterResponse::Ok);

    shutdown_all(&shutdown, &[raft1, raft2, raft3], &[ep1, ep2, ep3]).await;
}

/// Test: 3-node cluster → scale to 5 nodes.
/// Verifies: larger odd-number clusters work, all nodes replicate.
#[tokio::test]
async fn scale_three_to_five() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    let eps: Vec<Arc<QuicEndpoint>> = (0..5)
        .map(|_| create_endpoint(server_cfg.clone(), client_cfg.clone()))
        .collect();
    let addrs: Vec<_> = eps.iter().map(|ep| ep.local_addr().unwrap()).collect();

    let mut rafts = Vec::new();
    for (i, ep) in eps.iter().enumerate() {
        rafts.push(create_quic_node((i + 1) as u64, Arc::clone(ep)).await);
    }

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for (ep, raft) in eps.iter().zip(rafts.iter()) {
        start_server(ep, raft, &shutdown);
    }

    // Bootstrap 3-node cluster
    let mut members = BTreeMap::new();
    for i in 0..3 {
        members.insert((i + 1) as u64, NodeAddress::new("127.0.0.1", addrs[i].port()));
    }
    for raft in &rafts[0..3] {
        raft.initialize(members.clone()).await.unwrap();
    }

    let leader_id = wait_for_leader(&rafts[0], 10).await.unwrap();
    let leader_idx = (leader_id - 1) as usize;

    // Add node 4
    rafts[leader_idx]
        .add_learner(4, NodeAddress::new("127.0.0.1", addrs[3].port()), true)
        .await
        .unwrap();
    let mut voters = std::collections::BTreeSet::new();
    voters.insert(4u64);
    rafts[leader_idx]
        .change_membership(openraft::ChangeMembers::AddVoterIds(voters), false)
        .await
        .unwrap();

    // Now 4 nodes (even) — add node 5 to reach odd
    rafts[leader_idx]
        .add_learner(5, NodeAddress::new("127.0.0.1", addrs[4].port()), true)
        .await
        .unwrap();
    let mut voters = std::collections::BTreeSet::new();
    voters.insert(5u64);
    rafts[leader_idx]
        .change_membership(openraft::ChangeMembers::AddVoterIds(voters), false)
        .await
        .unwrap();

    // Verify 5-node cluster
    let m = rafts[leader_idx].metrics().borrow().clone();
    assert_eq!(m.membership_config.membership().voter_ids().count(), 5);

    // Propose and verify replication
    let resp = rafts[leader_idx]
        .client_write(ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 1,
        })
        .await
        .unwrap();
    assert_eq!(resp.data, ClusterResponse::Ok);

    // Wait for replication to all nodes
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    for (i, raft) in rafts.iter().enumerate() {
        let m = raft.metrics().borrow().clone();
        assert!(
            m.last_applied.is_some(),
            "node {} should have replicated entries",
            i + 1
        );
    }

    shutdown_all(
        &shutdown,
        &rafts,
        &eps,
    )
    .await;
}

/// Test: Remove a non-leader node from a 3-node cluster.
/// Verifies: 3→2 node removal, cluster still functional with 2 nodes.
#[tokio::test]
async fn remove_node_from_three_node_cluster() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    let ep1 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep2 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep3 = create_endpoint(server_cfg, client_cfg);

    let addr1 = ep1.local_addr().unwrap();
    let addr2 = ep2.local_addr().unwrap();
    let addr3 = ep3.local_addr().unwrap();

    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;
    let raft3 = create_quic_node(3, Arc::clone(&ep3)).await;

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    start_server(&ep1, &raft1, &shutdown);
    start_server(&ep2, &raft2, &shutdown);
    start_server(&ep3, &raft3, &shutdown);

    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    members.insert(2u64, NodeAddress::new("127.0.0.1", addr2.port()));
    members.insert(3u64, NodeAddress::new("127.0.0.1", addr3.port()));

    raft1.initialize(members.clone()).await.unwrap();
    raft2.initialize(members.clone()).await.unwrap();
    raft3.initialize(members).await.unwrap();

    let leader_id = wait_for_leader(&raft1, 10).await.unwrap();
    let nodes = [(1u64, raft1.clone()), (2, raft2.clone()), (3, raft3.clone())];
    let leader = leader_raft(leader_id, &nodes);

    // Pick a non-leader node to remove
    let remove_id = if leader_id == 3 { 2 } else { 3 };

    // Step 1: Demote from voter
    let mut to_remove = std::collections::BTreeSet::new();
    to_remove.insert(remove_id);
    leader
        .change_membership(
            openraft::ChangeMembers::RemoveVoters(to_remove.clone()),
            false,
        )
        .await
        .unwrap();

    // Step 2: Remove from cluster
    leader
        .change_membership(openraft::ChangeMembers::RemoveNodes(to_remove), false)
        .await
        .unwrap();

    // Verify 2-node cluster
    let m = leader.metrics().borrow().clone();
    let voter_count = m.membership_config.membership().voter_ids().count();
    assert_eq!(voter_count, 2, "cluster should have 2 voters after removal");

    // Cluster should still function (quorum is 2/2)
    let resp = leader
        .client_write(ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: leader_id,
        })
        .await
        .unwrap();
    assert_eq!(resp.data, ClusterResponse::Ok);

    shutdown_all(&shutdown, &[raft1, raft2, raft3], &[ep1, ep2, ep3]).await;
}

/// Test: Shut down the leader in a 3-node cluster.
/// Verifies: remaining 2 nodes elect a new leader and continue serving.
#[tokio::test]
async fn leader_failover() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    let ep1 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep2 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep3 = create_endpoint(server_cfg, client_cfg);

    let addr1 = ep1.local_addr().unwrap();
    let addr2 = ep2.local_addr().unwrap();
    let addr3 = ep3.local_addr().unwrap();

    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;
    let raft3 = create_quic_node(3, Arc::clone(&ep3)).await;

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    start_server(&ep1, &raft1, &shutdown);
    start_server(&ep2, &raft2, &shutdown);
    start_server(&ep3, &raft3, &shutdown);

    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    members.insert(2u64, NodeAddress::new("127.0.0.1", addr2.port()));
    members.insert(3u64, NodeAddress::new("127.0.0.1", addr3.port()));

    raft1.initialize(members.clone()).await.unwrap();
    raft2.initialize(members.clone()).await.unwrap();
    raft3.initialize(members).await.unwrap();

    let leader_id = wait_for_leader(&raft1, 10).await.unwrap();

    // Shut down the leader
    let (remaining_rafts, remaining_eps): (Vec<_>, Vec<_>) = match leader_id {
        1 => {
            let _ = raft1.shutdown().await;
            ep1.close();
            (vec![raft2.clone(), raft3.clone()], vec![ep2.clone(), ep3.clone()])
        }
        2 => {
            let _ = raft2.shutdown().await;
            ep2.close();
            (vec![raft1.clone(), raft3.clone()], vec![ep1.clone(), ep3.clone()])
        }
        3 => {
            let _ = raft3.shutdown().await;
            ep3.close();
            (vec![raft1.clone(), raft2.clone()], vec![ep1.clone(), ep2.clone()])
        }
        _ => unreachable!(),
    };

    // Wait for a new leader to be elected among remaining nodes.
    // Poll until a leader is elected that is NOT the old leader (the old leader
    // may briefly appear in stale metrics).
    let mut new_leader = None;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Some(l) = remaining_rafts[0].current_leader().await {
            if l != leader_id {
                new_leader = Some(l);
                break;
            }
        }
    }
    assert!(
        new_leader.is_some(),
        "remaining nodes should elect a new leader (not the failed node {leader_id})"
    );

    shutdown_all(
        &shutdown,
        &remaining_rafts,
        &remaining_eps,
    )
    .await;
}

/// Test: Join via RPC (AddNodeRequest message through QUIC).
/// Simulates the seed-based join flow end-to-end.
#[tokio::test]
async fn join_via_rpc_add_node_request() {
    use aeon_cluster::transport::network::send_join_request;
    use aeon_cluster::types::JoinRequest;

    let (server_cfg, client_cfg) = dev_quic_configs();

    let ep1 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep2 = create_endpoint(server_cfg, client_cfg);

    let addr1 = ep1.local_addr().unwrap();
    let addr2 = ep2.local_addr().unwrap();

    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    start_server(&ep1, &raft1, &shutdown);
    start_server(&ep2, &raft2, &shutdown);

    // Bootstrap single-node cluster
    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    raft1.initialize(members).await.unwrap();
    wait_for_leader(&raft1, 5).await.unwrap();

    // Node 2 sends a JoinRequest to node 1 via QUIC RPC
    let join_req = JoinRequest {
        node_id: 2,
        addr: NodeAddress::new("127.0.0.1", addr2.port()),
    };

    let resp = send_join_request(
        &ep2,
        0, // target_id for connection caching
        &NodeAddress::new("127.0.0.1", addr1.port()),
        &join_req,
    )
    .await
    .unwrap();

    assert!(resp.success, "join should succeed: {}", resp.message);
    assert_eq!(resp.leader_id, Some(1));

    // Verify 2-node cluster
    let m = raft1.metrics().borrow().clone();
    let voter_count = m.membership_config.membership().voter_ids().count();
    assert_eq!(voter_count, 2, "cluster should have 2 voters after join");

    // Verify node 2 is receiving replication
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let m2 = raft2.metrics().borrow().clone();
    assert!(m2.last_applied.is_some(), "node 2 should have replicated entries");

    shutdown_all(&shutdown, &[raft1, raft2], &[ep1, ep2]).await;
}

/// Test: RemoveNodeRequest via QUIC RPC.
#[tokio::test]
async fn remove_via_rpc() {
    use aeon_cluster::transport::network::{send_join_request, send_remove_request};
    use aeon_cluster::types::{JoinRequest, RemoveNodeRequest};

    let (server_cfg, client_cfg) = dev_quic_configs();

    let ep1 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep2 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep3 = create_endpoint(server_cfg, client_cfg);

    let addr1 = ep1.local_addr().unwrap();
    let addr2 = ep2.local_addr().unwrap();
    let addr3 = ep3.local_addr().unwrap();

    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;
    let raft3 = create_quic_node(3, Arc::clone(&ep3)).await;

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    start_server(&ep1, &raft1, &shutdown);
    start_server(&ep2, &raft2, &shutdown);
    start_server(&ep3, &raft3, &shutdown);

    // Bootstrap single-node, add 2 and 3 via RPC
    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    raft1.initialize(members).await.unwrap();
    wait_for_leader(&raft1, 5).await.unwrap();

    let seed = NodeAddress::new("127.0.0.1", addr1.port());

    let r2 = send_join_request(
        &ep2,
        0,
        &seed,
        &JoinRequest {
            node_id: 2,
            addr: NodeAddress::new("127.0.0.1", addr2.port()),
        },
    )
    .await
    .unwrap();
    assert!(r2.success, "node 2 join failed: {}", r2.message);

    let r3 = send_join_request(
        &ep3,
        0,
        &seed,
        &JoinRequest {
            node_id: 3,
            addr: NodeAddress::new("127.0.0.1", addr3.port()),
        },
    )
    .await
    .unwrap();
    assert!(r3.success, "node 3 join failed: {}", r3.message);

    // Verify 3-node cluster
    let m = raft1.metrics().borrow().clone();
    assert_eq!(m.membership_config.membership().voter_ids().count(), 3);

    // Remove node 3 via RPC
    let remove_resp = send_remove_request(
        &ep1,
        0,
        &seed,
        &RemoveNodeRequest { node_id: 3 },
    )
    .await
    .unwrap();
    assert!(remove_resp.success, "remove failed: {}", remove_resp.message);

    // Verify back to 2-node cluster
    let m = raft1.metrics().borrow().clone();
    assert_eq!(m.membership_config.membership().voter_ids().count(), 2);

    shutdown_all(&shutdown, &[raft1, raft2, raft3], &[ep1, ep2, ep3]).await;
}

/// Test: Multiple proposals across different nodes via leader forwarding.
/// Verifies: entries are replicated consistently to all nodes.
#[tokio::test]
async fn replication_consistency_across_nodes() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    let ep1 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep2 = create_endpoint(server_cfg.clone(), client_cfg.clone());
    let ep3 = create_endpoint(server_cfg, client_cfg);

    let addr1 = ep1.local_addr().unwrap();
    let addr2 = ep2.local_addr().unwrap();
    let addr3 = ep3.local_addr().unwrap();

    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;
    let raft3 = create_quic_node(3, Arc::clone(&ep3)).await;

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    start_server(&ep1, &raft1, &shutdown);
    start_server(&ep2, &raft2, &shutdown);
    start_server(&ep3, &raft3, &shutdown);

    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("127.0.0.1", addr1.port()));
    members.insert(2u64, NodeAddress::new("127.0.0.1", addr2.port()));
    members.insert(3u64, NodeAddress::new("127.0.0.1", addr3.port()));

    raft1.initialize(members.clone()).await.unwrap();
    raft2.initialize(members.clone()).await.unwrap();
    raft3.initialize(members).await.unwrap();

    let leader_id = wait_for_leader(&raft1, 10).await.unwrap();
    let nodes = [(1u64, raft1.clone()), (2, raft2.clone()), (3, raft3.clone())];
    let leader = leader_raft(leader_id, &nodes);

    // Propose 20 entries
    for i in 0..20 {
        let resp = leader
            .client_write(ClusterRequest::AssignPartition {
                partition: PartitionId::new(i % 16),
                node: (i as u64 % 3) + 1,
            })
            .await
            .unwrap();
        assert_eq!(resp.data, ClusterResponse::Ok);
    }

    // Wait for full replication
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // All nodes should have the same last_applied index
    let m1 = raft1.metrics().borrow().clone();
    let m2 = raft2.metrics().borrow().clone();
    let m3 = raft3.metrics().borrow().clone();

    assert_eq!(
        m1.last_applied, m2.last_applied,
        "nodes 1 and 2 should have same last_applied"
    );
    assert_eq!(
        m2.last_applied, m3.last_applied,
        "nodes 2 and 3 should have same last_applied"
    );

    shutdown_all(&shutdown, &[raft1, raft2, raft3], &[ep1, ep2, ep3]).await;
}
