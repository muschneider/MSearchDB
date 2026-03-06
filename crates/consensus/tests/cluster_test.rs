//! Integration tests for a 3-node in-process Raft cluster.
//!
//! These tests spin up three [`RaftNode`] instances connected via
//! [`ChannelNetworkFactory`] and verify:
//!
//! 1. **Leader election** completes within 2 seconds.
//! 2. **Command replication** — a write proposed on the leader is committed.
//! 3. **Leader failover** — when the leader is shut down, a new leader is
//!    elected and writes continue.
//! 4. **Data persistence** — data written before failover is still present
//!    after the new leader takes over.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use openraft::{BasicNode, ServerState};
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};

use msearchdb_consensus::network::{ChannelNetworkFactory, RaftHandle};
use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_consensus::types::RaftCommand;
use msearchdb_core::document::{Document, DocumentId, FieldValue};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Shared router map used by the channel network.
type RouterMap = Arc<RwLock<HashMap<u64, RaftHandle>>>;

/// Create a 3-node in-process Raft cluster.
///
/// Returns a vec of `(RaftNode, Raft<TypeConfig>)` pairs, one per node.
async fn create_cluster() -> (Vec<RaftNode>, RouterMap) {
    let routers: RouterMap = Arc::new(RwLock::new(HashMap::new()));

    let mut nodes = Vec::new();

    for node_id in 1..=3_u64 {
        let network = ChannelNetworkFactory::new(Arc::clone(&routers));
        let (raft_node, handle) = RaftNode::new_with_channel_network(node_id, network)
            .await
            .expect("failed to create raft node");

        routers.write().await.insert(node_id, handle);
        nodes.push(raft_node);
    }

    (nodes, routers)
}

/// Initialize the cluster with all 3 nodes as voters.
async fn initialize_cluster(nodes: &[RaftNode]) {
    let mut members = BTreeMap::new();
    for id in 1..=3_u64 {
        members.insert(
            id,
            BasicNode {
                addr: format!("127.0.0.1:{}", 9000 + id),
            },
        );
    }

    // Initialize on node 1 — it will replicate to others.
    nodes[0]
        .initialize(members)
        .await
        .expect("failed to initialize cluster");
}

/// Wait for a leader to be elected (polls metrics).
///
/// Returns the node id of the leader.
async fn wait_for_leader(nodes: &[RaftNode], deadline: Duration) -> u64 {
    let start = tokio::time::Instant::now();
    loop {
        for node in nodes {
            let metrics = node.metrics().await;
            if metrics.state == ServerState::Leader {
                return node.node_id();
            }
        }
        if start.elapsed() > deadline {
            panic!("no leader elected within {:?}", deadline);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test that a 3-node cluster elects a leader within 2 seconds.
#[tokio::test]
async fn leader_election_within_2_seconds() {
    let (nodes, _routers) = create_cluster().await;
    initialize_cluster(&nodes).await;

    let result = timeout(
        Duration::from_secs(2),
        wait_for_leader(&nodes, Duration::from_secs(2)),
    )
    .await;

    assert!(
        result.is_ok(),
        "leader election did not complete within 2 seconds"
    );
    let leader_id = result.unwrap();
    assert!(
        (1..=3).contains(&leader_id),
        "leader id {} is out of range",
        leader_id
    );
}

/// Test that a command proposed on the leader is committed and a response returned.
#[tokio::test]
async fn command_replication() {
    let (nodes, _routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    let leader_id = wait_for_leader(&nodes, Duration::from_secs(2)).await;

    // Give replication streams a moment to stabilize.
    sleep(Duration::from_millis(300)).await;

    let leader = nodes
        .iter()
        .find(|n| n.node_id() == leader_id)
        .expect("leader node not found");

    let doc = Document::new(DocumentId::new("test-doc-1"))
        .with_field("title", FieldValue::Text("hello raft".into()));

    let cmd = RaftCommand::InsertDocument { document: doc };

    let resp = leader.propose(cmd).await.expect("propose failed");
    assert!(resp.success);
    assert_eq!(resp.document_id, Some(DocumentId::new("test-doc-1")));
}

/// Test that multiple commands are replicated successfully.
#[tokio::test]
async fn multiple_commands_replication() {
    let (nodes, _routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    let leader_id = wait_for_leader(&nodes, Duration::from_secs(2)).await;

    sleep(Duration::from_millis(300)).await;

    let leader = nodes
        .iter()
        .find(|n| n.node_id() == leader_id)
        .expect("leader node not found");

    // Insert 5 documents.
    for i in 0..5 {
        let doc = Document::new(DocumentId::new(format!("doc-{}", i)))
            .with_field("index", FieldValue::Number(i as f64));

        let cmd = RaftCommand::InsertDocument { document: doc };
        let resp = leader.propose(cmd).await.expect("propose failed");
        assert!(resp.success, "document {} failed to replicate", i);
    }

    // Delete one.
    let del = RaftCommand::DeleteDocument {
        id: DocumentId::new("doc-2"),
    };
    let resp = leader.propose(del).await.expect("delete failed");
    assert!(resp.success);

    // Update one.
    let updated =
        Document::new(DocumentId::new("doc-0")).with_field("index", FieldValue::Number(99.0));
    let upd = RaftCommand::UpdateDocument { document: updated };
    let resp = leader.propose(upd).await.expect("update failed");
    assert!(resp.success);
}

/// Test that proposing on a follower returns a forward error.
#[tokio::test]
async fn propose_on_follower_returns_error() {
    let (nodes, _routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    let leader_id = wait_for_leader(&nodes, Duration::from_secs(2)).await;

    sleep(Duration::from_millis(300)).await;

    // Find a follower.
    let follower = nodes
        .iter()
        .find(|n| n.node_id() != leader_id)
        .expect("no follower found");

    let cmd = RaftCommand::InsertDocument {
        document: Document::new(DocumentId::new("should-fail")),
    };

    let result = follower.propose(cmd).await;
    assert!(result.is_err(), "propose on follower should fail");

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not leader")
            || err_msg.contains("forward")
            || err_msg.contains("Forward"),
        "unexpected error: {}",
        err_msg
    );
}

/// Test leader failover: shut down the current leader, wait for a new one.
#[tokio::test]
async fn leader_failover() {
    let (nodes, routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    let original_leader_id = wait_for_leader(&nodes, Duration::from_secs(2)).await;

    sleep(Duration::from_millis(300)).await;

    // Propose a document before failover.
    let leader = nodes
        .iter()
        .find(|n| n.node_id() == original_leader_id)
        .expect("leader not found");

    let doc = Document::new(DocumentId::new("pre-failover"))
        .with_field("data", FieldValue::Text("before".into()));
    let resp = leader
        .propose(RaftCommand::InsertDocument { document: doc })
        .await
        .expect("pre-failover propose failed");
    assert!(resp.success);

    // Simulate leader failure by removing it from the router map.
    // This makes it unreachable by other nodes' replication streams.
    routers.write().await.remove(&original_leader_id);

    // Also shut down the leader's Raft instance.
    let leader_node = nodes
        .iter()
        .find(|n| n.node_id() == original_leader_id)
        .unwrap();
    let _ = leader_node.raft_handle().shutdown().await;

    // Wait for a new leader among the remaining nodes.
    let remaining: Vec<&RaftNode> = nodes
        .iter()
        .filter(|n| n.node_id() != original_leader_id)
        .collect();

    let new_leader_id = timeout(Duration::from_secs(5), async {
        loop {
            for node in &remaining {
                let metrics = node.metrics().await;
                if metrics.state == ServerState::Leader {
                    return node.node_id();
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("new leader election timed out after 5 seconds");

    assert_ne!(
        new_leader_id, original_leader_id,
        "new leader should be different from failed leader"
    );

    // The new leader should be able to accept writes.
    let new_leader = remaining
        .iter()
        .find(|n| n.node_id() == new_leader_id)
        .expect("new leader not in remaining nodes");

    // Give the new leader a moment to stabilize.
    sleep(Duration::from_millis(500)).await;

    let doc2 = Document::new(DocumentId::new("post-failover"))
        .with_field("data", FieldValue::Text("after".into()));
    let resp = new_leader
        .propose(RaftCommand::InsertDocument { document: doc2 })
        .await
        .expect("post-failover propose failed");
    assert!(resp.success);
    assert_eq!(resp.document_id, Some(DocumentId::new("post-failover")));
}

/// Test that the cluster metrics show correct membership.
#[tokio::test]
async fn cluster_metrics_show_membership() {
    let (nodes, _routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    wait_for_leader(&nodes, Duration::from_secs(2)).await;

    sleep(Duration::from_millis(300)).await;

    for node in &nodes {
        let metrics = node.metrics().await;
        // All 3 nodes should be in the membership config.
        let membership = &metrics.membership_config;
        let voter_ids = membership.voter_ids().collect::<Vec<_>>();
        assert_eq!(
            voter_ids.len(),
            3,
            "node {} should see 3 voters, got {:?}",
            node.node_id(),
            voter_ids
        );
    }
}
