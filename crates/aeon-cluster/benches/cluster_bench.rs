//! Cluster benchmarks: single-node and multi-node Raft + QUIC performance.
//!
//! Measures:
//! - Single-node bootstrap and commit latency
//! - Single-node propose throughput (partition assignments)
//! - Multi-node (3-node) cluster formation time
//! - Multi-node commit latency and throughput
//! - Multi-node log replication convergence
//! - Partition rebalance computation

#![cfg(feature = "cluster")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aeon_cluster::partition_manager;
use aeon_cluster::raft_config::AeonRaftConfig;
use aeon_cluster::store::{MemLogStore, StateMachineStore};
use aeon_cluster::transport::endpoint::QuicEndpoint;
use aeon_cluster::transport::network::QuicNetworkFactory;
use aeon_cluster::transport::server;
use aeon_cluster::types::PartitionTable;
use aeon_cluster::{ClusterRequest, ClusterResponse, NodeAddress};
use aeon_types::PartitionId;
use openraft::{Config, Raft};

fn dev_quic_configs() -> (quinn::ServerConfig, quinn::ClientConfig) {
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let cert_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let cert = cert_params.self_signed(&key_pair).unwrap();
    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();

    let server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der.clone_key())
        .unwrap();
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto).unwrap(),
    ));

    let mut root_store = rustls::RootCertStore::empty();
    root_store.add(cert_der).unwrap();
    let client_crypto = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let client_config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto).unwrap(),
    ));

    (server_config, client_config)
}

fn raft_config() -> Arc<Config> {
    Arc::new(Config {
        cluster_name: "aeon-bench".to_string(),
        heartbeat_interval: 200,
        election_timeout_min: 500,
        election_timeout_max: 1000,
        ..Config::default()
    })
}

async fn create_node(node_id: u64, endpoint: Arc<QuicEndpoint>) -> Raft<AeonRaftConfig> {
    let config = raft_config();
    Raft::new(
        node_id,
        config,
        QuicNetworkFactory::new(endpoint),
        MemLogStore::new(),
        StateMachineStore::new(),
    )
    .await
    .unwrap()
}

/// Single-node cluster: bootstrap and propose.
async fn bench_single_node_bootstrap() -> (Duration, Duration) {
    use aeon_cluster::{ClusterConfig, ClusterNode};

    // Measure bootstrap time
    let start = Instant::now();
    let node = ClusterNode::bootstrap_single(ClusterConfig::single_node(1, 16))
        .await
        .unwrap();
    let bootstrap_time = start.elapsed();

    // Measure propose latency (single commit on single-node — instant commit)
    let start = Instant::now();
    let _resp = node
        .propose(ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 1,
        })
        .await
        .unwrap();
    let propose_latency = start.elapsed();

    let _ = node.shutdown().await;

    (bootstrap_time, propose_latency)
}

/// Single-node cluster: throughput of N proposals.
async fn bench_single_node_throughput(n: usize) -> (Duration, f64) {
    use aeon_cluster::{ClusterConfig, ClusterNode};

    let node = ClusterNode::bootstrap_single(ClusterConfig::single_node(1, 256))
        .await
        .unwrap();

    let start = Instant::now();
    for i in 0..n {
        node.propose(ClusterRequest::AssignPartition {
            partition: PartitionId::new((i % 256) as u16),
            node: 1,
        })
        .await
        .unwrap();
    }
    let elapsed = start.elapsed();
    let throughput = n as f64 / elapsed.as_secs_f64();

    let _ = node.shutdown().await;

    (elapsed, throughput)
}

struct ThreeNodeCluster {
    raft1: Raft<AeonRaftConfig>,
    raft2: Raft<AeonRaftConfig>,
    raft3: Raft<AeonRaftConfig>,
    ep1: Arc<QuicEndpoint>,
    ep2: Arc<QuicEndpoint>,
    ep3: Arc<QuicEndpoint>,
    shutdown: Arc<AtomicBool>,
    leader_id: u64,
}

impl ThreeNodeCluster {
    async fn create() -> Self {
        let (server_cfg, client_cfg) = dev_quic_configs();

        let ep1 = Arc::new(
            QuicEndpoint::bind(
                "127.0.0.1:0".parse().unwrap(),
                server_cfg.clone(),
                client_cfg.clone(),
            )
            .unwrap(),
        );
        let ep2 = Arc::new(
            QuicEndpoint::bind(
                "127.0.0.1:0".parse().unwrap(),
                server_cfg.clone(),
                client_cfg.clone(),
            )
            .unwrap(),
        );
        let ep3 = Arc::new(
            QuicEndpoint::bind("127.0.0.1:0".parse().unwrap(), server_cfg, client_cfg).unwrap(),
        );

        let addr1 = ep1.local_addr().unwrap();
        let addr2 = ep2.local_addr().unwrap();
        let addr3 = ep3.local_addr().unwrap();

        let raft1 = create_node(1, Arc::clone(&ep1)).await;
        let raft2 = create_node(2, Arc::clone(&ep2)).await;
        let raft3 = create_node(3, Arc::clone(&ep3)).await;

        let shutdown = Arc::new(AtomicBool::new(false));
        tokio::spawn(server::serve(
            Arc::clone(&ep1),
            raft1.clone(),
            Arc::clone(&shutdown),
        ));
        tokio::spawn(server::serve(
            Arc::clone(&ep2),
            raft2.clone(),
            Arc::clone(&shutdown),
        ));
        tokio::spawn(server::serve(
            Arc::clone(&ep3),
            raft3.clone(),
            Arc::clone(&shutdown),
        ));

        let mut members = BTreeMap::new();
        members.insert(1u64, NodeAddress::new("localhost", addr1.port()));
        members.insert(2u64, NodeAddress::new("localhost", addr2.port()));
        members.insert(3u64, NodeAddress::new("localhost", addr3.port()));

        raft1.initialize(members).await.unwrap();

        // Wait for leader election
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut leader_id = None;
        while Instant::now() < deadline {
            if let Some(id) = raft1.current_leader().await {
                leader_id = Some(id);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Self {
            raft1,
            raft2,
            raft3,
            ep1,
            ep2,
            ep3,
            shutdown,
            leader_id: leader_id.expect("leader should be elected within 10s"),
        }
    }

    fn leader(&self) -> &Raft<AeonRaftConfig> {
        match self.leader_id {
            1 => &self.raft1,
            2 => &self.raft2,
            3 => &self.raft3,
            _ => unreachable!(),
        }
    }

    async fn shutdown(self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.raft1.shutdown().await;
        let _ = self.raft2.shutdown().await;
        let _ = self.raft3.shutdown().await;
        self.ep1.close();
        self.ep2.close();
        self.ep3.close();
    }
}

/// Three-node cluster formation time.
async fn bench_three_node_formation() -> Duration {
    let start = Instant::now();
    let cluster = ThreeNodeCluster::create().await;
    let elapsed = start.elapsed();
    cluster.shutdown().await;
    elapsed
}

/// Three-node commit latency (single proposal).
async fn bench_three_node_commit_latency() -> Duration {
    let cluster = ThreeNodeCluster::create().await;

    let start = Instant::now();
    let resp = cluster
        .leader()
        .client_write(ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 1,
        })
        .await
        .unwrap();
    let latency = start.elapsed();
    assert_eq!(resp.data, ClusterResponse::Ok);

    cluster.shutdown().await;
    latency
}

/// Three-node throughput: N sequential proposals.
async fn bench_three_node_throughput(n: usize) -> (Duration, f64) {
    let cluster = ThreeNodeCluster::create().await;

    let start = Instant::now();
    for i in 0..n {
        let resp = cluster
            .leader()
            .client_write(ClusterRequest::AssignPartition {
                partition: PartitionId::new((i % 256) as u16),
                node: ((i as u64) % 3) + 1,
            })
            .await
            .unwrap();
        assert_eq!(resp.data, ClusterResponse::Ok);
    }
    let elapsed = start.elapsed();
    let throughput = n as f64 / elapsed.as_secs_f64();

    cluster.shutdown().await;
    (elapsed, throughput)
}

/// Three-node log replication: propose N, verify all nodes converge.
async fn bench_three_node_replication(n: usize) -> (Duration, Duration) {
    let cluster = ThreeNodeCluster::create().await;

    // Propose N entries
    let propose_start = Instant::now();
    for i in 0..n {
        cluster
            .leader()
            .client_write(ClusterRequest::AssignPartition {
                partition: PartitionId::new((i % 256) as u16),
                node: ((i as u64) % 3) + 1,
            })
            .await
            .unwrap();
    }
    let propose_time = propose_start.elapsed();

    // Wait for replication convergence (all nodes have same last_applied)
    let converge_start = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let m1 = cluster.raft1.metrics().borrow().clone();
        let m2 = cluster.raft2.metrics().borrow().clone();
        let m3 = cluster.raft3.metrics().borrow().clone();

        if m1.last_applied == m2.last_applied && m2.last_applied == m3.last_applied {
            if let Some(applied) = m1.last_applied {
                if applied.index >= n as u64 {
                    break;
                }
            }
        }

        if Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let converge_time = converge_start.elapsed();

    cluster.shutdown().await;
    (propose_time, converge_time)
}

/// Pure computation: partition rebalance.
fn bench_partition_rebalance(num_partitions: u16, num_nodes: usize) -> Duration {
    let nodes: Vec<u64> = (1..=num_nodes as u64).collect();

    // Create an initial table with all partitions on node 1
    let mut table = PartitionTable::single_node(num_partitions, 1);

    let start = Instant::now();
    let _moves = partition_manager::compute_rebalance(&table, &nodes);
    let elapsed = start.elapsed();

    // Apply moves to verify
    for (partition, _from, to) in partition_manager::compute_rebalance(&table, &nodes) {
        table.assign(partition, to);
    }

    elapsed
}

#[tokio::main]
async fn main() {
    println!("=== Aeon Cluster Benchmarks ===\n");

    // ── Single-Node ──────────────────────────────────────────────
    println!("── Single-Node Cluster ──");

    let (bootstrap, propose_latency) = bench_single_node_bootstrap().await;
    println!(
        "  Bootstrap (16 partitions):     {:>10.3} ms",
        bootstrap.as_secs_f64() * 1000.0
    );
    println!(
        "  Single propose latency:        {:>10.3} ms",
        propose_latency.as_secs_f64() * 1000.0
    );

    for &n in &[100, 1000] {
        let (elapsed, throughput) = bench_single_node_throughput(n).await;
        println!(
            "  {} proposals:  {:>10.3} ms  ({:.0} proposals/sec)",
            n,
            elapsed.as_secs_f64() * 1000.0,
            throughput
        );
    }

    // ── Multi-Node (3-node) ──────────────────────────────────────
    println!("\n── Three-Node Cluster (QUIC) ──");

    let formation_time = bench_three_node_formation().await;
    println!(
        "  Cluster formation:             {:>10.3} ms",
        formation_time.as_secs_f64() * 1000.0
    );

    let commit_latency = bench_three_node_commit_latency().await;
    println!(
        "  Single commit latency:         {:>10.3} ms",
        commit_latency.as_secs_f64() * 1000.0
    );

    for &n in &[50, 200] {
        let (elapsed, throughput) = bench_three_node_throughput(n).await;
        println!(
            "  {} proposals:  {:>10.3} ms  ({:.0} proposals/sec)",
            n,
            elapsed.as_secs_f64() * 1000.0,
            throughput
        );
    }

    // ── Replication Convergence ──────────────────────────────────
    println!("\n── Replication Convergence ──");

    for &n in &[10, 50] {
        let (propose_time, converge_time) = bench_three_node_replication(n).await;
        println!(
            "  {} entries: propose {:>8.3} ms, converge {:>8.3} ms",
            n,
            propose_time.as_secs_f64() * 1000.0,
            converge_time.as_secs_f64() * 1000.0
        );
    }

    // ── Partition Rebalance (Pure Computation) ───────────────────
    println!("\n── Partition Rebalance ──");

    for &(partitions, nodes) in &[(16, 3), (256, 5), (1024, 10)] {
        let elapsed = bench_partition_rebalance(partitions, nodes);
        println!(
            "  {} partitions / {} nodes: {:>10.3} µs",
            partitions,
            nodes,
            elapsed.as_secs_f64() * 1_000_000.0
        );
    }

    println!("\n=== Done ===");
}
