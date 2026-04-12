//! FT-1 integration test: single-node Raft log survives process restart.
//!
//! Boot a node with a redb-backed L3 log store, make partition assignments
//! via Raft proposals, drop the node, reopen the same redb file, and assert
//! that the recovered state machine contains the proposals without the second
//! boot having to re-run `initialize()` or re-propose the assignments.

#![cfg(feature = "cluster")]

use std::sync::Arc;

use aeon_cluster::{ClusterConfig, ClusterNode, ClusterRequest};
use aeon_state::{RedbConfig, RedbStore};
use aeon_types::{L3Store, PartitionId};

fn mk_config(port: u16) -> ClusterConfig {
    let mut cfg = ClusterConfig::single_node(1, 4);
    cfg.bind = format!("127.0.0.1:{port}").parse().unwrap();
    cfg
}

#[tokio::test]
async fn raft_log_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let l3_path = dir.path().join("raft.redb");

    // ── First boot: bootstrap, propose config update, shut down ────────
    {
        let l3: Arc<dyn L3Store> = Arc::new(
            RedbStore::open(RedbConfig {
                path: l3_path.clone(),
                sync_writes: true,
            })
            .unwrap(),
        );

        let node = ClusterNode::bootstrap_single_persistent(mk_config(24601), Arc::clone(&l3))
            .await
            .unwrap();

        // Propose a config update — this is a committed Raft entry that must
        // survive restart. Partition assignments are already made by bootstrap.
        node.propose(ClusterRequest::UpdateConfig {
            key: "persist_test".to_string(),
            value: "round_one".to_string(),
        })
        .await
        .unwrap();

        // Verify the proposal committed to the state machine.
        {
            let shared = node.shared_state();
            let snap = shared.read().await;
            assert_eq!(
                snap.config_overrides.get("persist_test"),
                Some(&"round_one".to_string())
            );
            // All 4 partitions should be assigned to node 1.
            for i in 0..4 {
                assert!(snap.partition_table.get(PartitionId::new(i)).is_some());
            }
        }

        // Explicit shutdown + drop to flush/close the redb handle.
        node.shutdown().await.unwrap();
        drop(node);
    }

    // Give the process a tick so any background Raft tasks holding the L3
    // handle can observe the drop (they share Arc<dyn L3Store> via the Raft
    // storage).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // ── Second boot: reopen same redb file ─────────────────────────────
    let l3: Arc<dyn L3Store> = Arc::new(
        RedbStore::open(RedbConfig {
            path: l3_path.clone(),
            sync_writes: true,
        })
        .unwrap(),
    );

    // Confirm persisted log entries exist before we hand them to the node.
    let persisted = l3.scan_prefix(b"raft/log/").unwrap();
    assert!(
        !persisted.is_empty(),
        "expected raft log entries in persisted L3 store"
    );

    // Use port 0 to avoid any OS-level "address in use" if the prior bind
    // lingers; the single-node stub transport doesn't actually listen.
    let mut cfg = mk_config(0);
    cfg.num_partitions = 4; // must match first boot

    let node = ClusterNode::bootstrap_single_persistent(cfg, Arc::clone(&l3))
        .await
        .unwrap();

    // Wait briefly for the state machine to apply recovered entries. openraft
    // replays the log on start; partition-assignment and UpdateConfig entries
    // should be applied before our read.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // The restarted node must see the config override written in the previous
    // boot — proof the Raft log actually persisted and replayed.
    let shared = node.shared_state();
    let snap = shared.read().await;
    assert_eq!(
        snap.config_overrides.get("persist_test"),
        Some(&"round_one".to_string()),
        "config override from first boot must survive restart"
    );
    for i in 0..4 {
        assert!(
            snap.partition_table.get(PartitionId::new(i)).is_some(),
            "partition {i} assignment must survive restart"
        );
    }
}
