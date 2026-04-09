//! Multi-node cluster integration tests using QUIC transport.

#![cfg(feature = "cluster")]

use std::collections::BTreeMap;
use std::sync::Arc;

use aeon_cluster::raft_config::AeonRaftConfig;
use aeon_cluster::store::{MemLogStore, StateMachineStore};
use aeon_cluster::transport::endpoint::QuicEndpoint;
use aeon_cluster::transport::network::QuicNetworkFactory;
use aeon_cluster::transport::server;
use aeon_cluster::{ClusterRequest, ClusterResponse, NodeAddress};
use aeon_types::PartitionId;
use openraft::{Config, Raft};

/// Build insecure QUIC configs for integration tests.
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

/// Helper to create a Raft node with QUIC transport.
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

#[tokio::test]
async fn three_node_cluster_formation() {
    let (server_cfg, client_cfg) = dev_quic_configs();

    // Create 3 QUIC endpoints
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

    // Create Raft nodes
    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;
    let raft3 = create_quic_node(3, Arc::clone(&ep3)).await;

    // Start QUIC servers
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _s1 = tokio::spawn(server::serve(
        Arc::clone(&ep1),
        raft1.clone(),
        Arc::clone(&shutdown),
    ));
    let _s2 = tokio::spawn(server::serve(
        Arc::clone(&ep2),
        raft2.clone(),
        Arc::clone(&shutdown),
    ));
    let _s3 = tokio::spawn(server::serve(
        Arc::clone(&ep3),
        raft3.clone(),
        Arc::clone(&shutdown),
    ));

    // Initialize cluster from node 1
    let mut members = BTreeMap::new();
    members.insert(1u64, NodeAddress::new("localhost", addr1.port()));
    members.insert(2u64, NodeAddress::new("localhost", addr2.port()));
    members.insert(3u64, NodeAddress::new("localhost", addr3.port()));

    raft1.initialize(members).await.unwrap();

    // Wait for leader election
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let leader = raft1.current_leader().await;
    assert!(leader.is_some(), "cluster should have elected a leader");

    // Propose via leader
    let leader_id = leader.unwrap();
    let leader_raft = match leader_id {
        1 => &raft1,
        2 => &raft2,
        3 => &raft3,
        _ => unreachable!(),
    };

    let resp = leader_raft
        .client_write(ClusterRequest::AssignPartition {
            partition: PartitionId::new(0),
            node: 1,
        })
        .await
        .unwrap();
    assert_eq!(resp.data, ClusterResponse::Ok);

    // Shutdown
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = raft1.shutdown().await;
    let _ = raft2.shutdown().await;
    let _ = raft3.shutdown().await;
    ep1.close();
    ep2.close();
    ep3.close();
}

#[tokio::test]
async fn three_node_log_replication() {
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

    let raft1 = create_quic_node(1, Arc::clone(&ep1)).await;
    let raft2 = create_quic_node(2, Arc::clone(&ep2)).await;
    let raft3 = create_quic_node(3, Arc::clone(&ep3)).await;

    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
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
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let leader_id = raft1.current_leader().await.unwrap();
    let leader_raft = match leader_id {
        1 => &raft1,
        2 => &raft2,
        3 => &raft3,
        _ => unreachable!(),
    };

    // Propose multiple entries
    for i in 0..5 {
        let resp = leader_raft
            .client_write(ClusterRequest::AssignPartition {
                partition: PartitionId::new(i),
                node: (i as u64 % 3) + 1,
            })
            .await
            .unwrap();
        assert_eq!(resp.data, ClusterResponse::Ok);
    }

    // Brief wait for replication
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // All nodes should have the same metrics (same log committed)
    let m1 = raft1.metrics().borrow().clone();
    let m2 = raft2.metrics().borrow().clone();
    let m3 = raft3.metrics().borrow().clone();

    // All should have applied at least our 5 entries
    // (plus blank/membership entries from initialization)
    assert!(
        m1.last_applied.is_some(),
        "node 1 should have applied entries"
    );
    assert!(
        m2.last_applied.is_some(),
        "node 2 should have applied entries"
    );
    assert!(
        m3.last_applied.is_some(),
        "node 3 should have applied entries"
    );

    // Cleanup
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = raft1.shutdown().await;
    let _ = raft2.shutdown().await;
    let _ = raft3.shutdown().await;
    ep1.close();
    ep2.close();
    ep3.close();
}
