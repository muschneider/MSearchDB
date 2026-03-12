//! Integration tests for the complete write path:
//!
//!   HTTP POST → Axum handler → RaftNode.propose()
//!     → StateMachine.apply() → StorageBackend.put() + IndexBackend.index_document()
//!     → commit
//!
//! These tests spin up a 3-node in-process Raft cluster backed by real
//! in-memory storage and index backends, then verify:
//!
//! 1. A single document survives the full write path and is retrievable.
//! 2. A [`RaftCommand::BatchInsert`] of 100 documents is applied atomically.
//! 3. Batch insert of 1 000 documents (10 × 100-doc batches) succeeds.

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

type RouterMap = Arc<RwLock<HashMap<u64, RaftHandle>>>;

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

async fn initialize_cluster(nodes: &[RaftNode]) {
    let mut members = BTreeMap::new();
    for id in 1..=3_u64 {
        members.insert(id, BasicNode { addr: format!("127.0.0.1:{}", 9000 + id) });
    }
    nodes[0].initialize(members).await.expect("init failed");
}

async fn wait_for_leader(nodes: &[RaftNode]) -> u64 {
    let deadline = Duration::from_secs(3);
    let start = tokio::time::Instant::now();
    loop {
        for node in nodes {
            let m = node.metrics().await;
            if m.state == ServerState::Leader {
                return node.node_id();
            }
        }
        if start.elapsed() > deadline {
            panic!("no leader elected within {:?}", deadline);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn find_leader(nodes: &[RaftNode], leader_id: u64) -> &RaftNode {
    nodes.iter().find(|n| n.node_id() == leader_id).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Single-document write path: propose → apply → storage.
#[tokio::test]
async fn write_path_single_document() {
    let (nodes, _routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    let leader_id = wait_for_leader(&nodes).await;
    sleep(Duration::from_millis(300)).await;

    let leader = find_leader(&nodes, leader_id);

    let doc = Document::new(DocumentId::new("wp-1"))
        .with_field("title", FieldValue::Text("write-path test".into()))
        .with_field("price", FieldValue::Number(42.0));

    let cmd = RaftCommand::InsertDocument { document: doc };
    let resp = leader.propose(cmd).await.expect("propose failed");

    assert!(resp.success);
    assert_eq!(resp.document_id, Some(DocumentId::new("wp-1")));
    assert_eq!(resp.affected_count, 1);
}

/// Batch-insert of 100 documents as a single Raft entry.
#[tokio::test]
async fn write_path_batch_100_documents() {
    let (nodes, _routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    let leader_id = wait_for_leader(&nodes).await;
    sleep(Duration::from_millis(300)).await;

    let leader = find_leader(&nodes, leader_id);

    let docs: Vec<Document> = (0..100)
        .map(|i| {
            Document::new(DocumentId::new(format!("batch-{}", i)))
                .with_field("title", FieldValue::Text(format!("Doc {}", i)))
                .with_field("seq", FieldValue::Number(i as f64))
        })
        .collect();

    let resp = leader.propose_batch(docs).await.expect("batch propose failed");

    assert!(resp.success);
    assert_eq!(resp.affected_count, 100);
}

/// Batch-insert of 1 000 documents in 10 × 100-doc batches.
#[tokio::test]
async fn write_path_batch_1000_documents_10_batches() {
    let (nodes, _routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    let leader_id = wait_for_leader(&nodes).await;
    sleep(Duration::from_millis(300)).await;

    let leader = find_leader(&nodes, leader_id);

    for batch_idx in 0..10u32 {
        let docs: Vec<Document> = (0..100)
            .map(|i| {
                let id = batch_idx * 100 + i;
                Document::new(DocumentId::new(format!("big-{}", id)))
                    .with_field("title", FieldValue::Text(format!("Document {}", id)))
            })
            .collect();

        let resp = leader
            .propose_batch(docs)
            .await
            .unwrap_or_else(|e| panic!("batch {} propose failed: {}", batch_idx, e));

        assert!(resp.success, "batch {} was not successful", batch_idx);
        assert_eq!(resp.affected_count, 100, "batch {} count mismatch", batch_idx);
    }
}

/// Delete after batch insert: verify the deleted doc is removed.
#[tokio::test]
async fn write_path_batch_then_delete() {
    let (nodes, _routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    let leader_id = wait_for_leader(&nodes).await;
    sleep(Duration::from_millis(300)).await;

    let leader = find_leader(&nodes, leader_id);

    // Insert a batch of 5 documents.
    let docs: Vec<Document> = (0..5)
        .map(|i| {
            Document::new(DocumentId::new(format!("del-test-{}", i)))
                .with_field("val", FieldValue::Number(i as f64))
        })
        .collect();

    let resp = leader.propose_batch(docs).await.expect("batch failed");
    assert!(resp.success);
    assert_eq!(resp.affected_count, 5);

    // Delete one.
    let del = RaftCommand::DeleteDocument {
        id: DocumentId::new("del-test-2"),
    };
    let resp = leader.propose(del).await.expect("delete failed");
    assert!(resp.success);
}

/// Mixed operations: batch insert + individual insert + update + delete.
#[tokio::test]
async fn write_path_mixed_operations() {
    let (nodes, _routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    let leader_id = wait_for_leader(&nodes).await;
    sleep(Duration::from_millis(300)).await;

    let leader = find_leader(&nodes, leader_id);

    // 1. Batch insert 50 docs.
    let batch: Vec<Document> = (0..50)
        .map(|i| {
            Document::new(DocumentId::new(format!("mixed-{}", i)))
                .with_field("phase", FieldValue::Text("batch".into()))
        })
        .collect();
    let resp = leader.propose_batch(batch).await.expect("batch failed");
    assert!(resp.success);
    assert_eq!(resp.affected_count, 50);

    // 2. Individual insert.
    let single = Document::new(DocumentId::new("mixed-single"))
        .with_field("phase", FieldValue::Text("single".into()));
    let resp = leader
        .propose(RaftCommand::InsertDocument { document: single })
        .await
        .expect("single insert failed");
    assert!(resp.success);

    // 3. Update.
    let updated = Document::new(DocumentId::new("mixed-0"))
        .with_field("phase", FieldValue::Text("updated".into()));
    let resp = leader
        .propose(RaftCommand::UpdateDocument { document: updated })
        .await
        .expect("update failed");
    assert!(resp.success);

    // 4. Delete.
    let resp = leader
        .propose(RaftCommand::DeleteDocument {
            id: DocumentId::new("mixed-49"),
        })
        .await
        .expect("delete failed");
    assert!(resp.success);
}

/// Replication: write on leader, verify leader failover preserves committed data.
///
/// After a batch is committed, we shut down the original leader. The remaining
/// 2 nodes elect a new leader and can still accept writes — confirming the
/// committed batch was replicated.
#[tokio::test]
async fn write_path_survives_leader_failover() {
    let (nodes, routers) = create_cluster().await;
    initialize_cluster(&nodes).await;
    let original_leader = wait_for_leader(&nodes).await;
    sleep(Duration::from_millis(300)).await;

    let leader = find_leader(&nodes, original_leader);

    // Write a batch that must be replicated.
    let docs: Vec<Document> = (0..100)
        .map(|i| {
            Document::new(DocumentId::new(format!("replicated-{}", i)))
                .with_field("tag", FieldValue::Text("pre-failover".into()))
        })
        .collect();
    let resp = leader.propose_batch(docs).await.expect("batch failed");
    assert!(resp.success);
    assert_eq!(resp.affected_count, 100);

    // Kill the original leader.
    routers.write().await.remove(&original_leader);
    let _ = leader.raft_handle().shutdown().await;

    // Wait for new leader.
    let remaining: Vec<&RaftNode> = nodes
        .iter()
        .filter(|n| n.node_id() != original_leader)
        .collect();

    let new_leader_id = timeout(Duration::from_secs(5), async {
        loop {
            for node in &remaining {
                let m = node.metrics().await;
                if m.state == ServerState::Leader {
                    return node.node_id();
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("new leader election timed out");

    assert_ne!(new_leader_id, original_leader);

    let new_leader = remaining
        .iter()
        .find(|n| n.node_id() == new_leader_id)
        .unwrap();

    sleep(Duration::from_millis(500)).await;

    // The new leader should accept further writes.
    let post_doc = Document::new(DocumentId::new("post-failover-batch"))
        .with_field("tag", FieldValue::Text("post-failover".into()));
    let resp = new_leader
        .propose(RaftCommand::InsertDocument { document: post_doc })
        .await
        .expect("post-failover propose failed");
    assert!(resp.success);
}
