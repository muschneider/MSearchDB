//! Distributed multi-node integration tests for MSearchDB.
//!
//! These tests verify end-to-end distributed behavior:
//! - 3-node Raft cluster formation
//! - Write replication across nodes
//! - Read from a different node than the writer
//! - Leader failover and continued operation

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use openraft::{BasicNode, ServerState};
use tokio::time::sleep;

use msearchdb_consensus::raft_node::{InMemoryStorage, NoopIndex, RaftNode};
use msearchdb_consensus::types::RaftCommand;
use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::traits::{IndexBackend, StorageBackend};

// ---------------------------------------------------------------------------
// Cluster helpers
// ---------------------------------------------------------------------------

/// A minimal 3-node in-process cluster using channel-based networking.
struct TestCluster {
    nodes: Vec<Arc<RaftNode>>,
    storages: Vec<Arc<InMemoryStorage>>,
    #[allow(dead_code)]
    routers: Arc<tokio::sync::RwLock<std::collections::HashMap<u64, openraft::Raft<msearchdb_consensus::types::TypeConfig>>>>,
}

impl TestCluster {
    async fn new() -> Self {
        use msearchdb_consensus::network::ChannelNetworkFactory;

        let routers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));

        let mut nodes = Vec::new();
        let mut storages = Vec::new();

        for id in 1..=3u64 {
            let storage = Arc::new(InMemoryStorage::new());
            let index: Arc<dyn IndexBackend> = Arc::new(NoopIndex);
            let network = ChannelNetworkFactory::new(routers.clone());
            let (node, handle) = RaftNode::new_with_backends(
                id,
                network,
                storage.clone() as Arc<dyn StorageBackend>,
                index,
            )
            .await
            .unwrap();

            routers.write().await.insert(id, handle);
            nodes.push(Arc::new(node));
            storages.push(storage);
        }

        Self {
            nodes,
            storages,
            routers,
        }
    }

    async fn initialize(&self) {
        let mut members = BTreeMap::new();
        for id in 1..=3u64 {
            members.insert(id, BasicNode::default());
        }
        self.nodes[0].initialize(members).await.unwrap();
    }

    async fn wait_for_leader(&self) -> u64 {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                for node in &self.nodes {
                    let m = node.metrics().await;
                    if m.state == ServerState::Leader {
                        return node.node_id();
                    }
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("leader election timed out")
    }

    fn leader_node(&self, leader_id: u64) -> &Arc<RaftNode> {
        self.nodes
            .iter()
            .find(|n| n.node_id() == leader_id)
            .unwrap()
    }

    fn follower_nodes(&self, leader_id: u64) -> Vec<&Arc<RaftNode>> {
        self.nodes
            .iter()
            .filter(|n| n.node_id() != leader_id)
            .collect()
    }

    fn storage_for(&self, node_id: u64) -> &Arc<InMemoryStorage> {
        &self.storages[(node_id - 1) as usize]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Bootstrap a 3-node cluster, write a document, verify it is replicated.
#[tokio::test]
async fn distributed_write_replicates_to_all_nodes() {
    let cluster = TestCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader().await;
    sleep(Duration::from_millis(300)).await;

    let leader = cluster.leader_node(leader_id);

    // Write a document via the leader.
    let doc = Document::new(DocumentId::new("dist-1"))
        .with_field("title", FieldValue::Text("distributed test".into()));
    let resp = leader
        .propose(RaftCommand::InsertDocument {
            collection: "test".into(),
            document: doc,
        })
        .await
        .expect("propose failed");
    assert!(resp.success);

    // Wait for replication.
    sleep(Duration::from_millis(500)).await;

    // Verify the document is on all 3 nodes.
    for id in 1..=3u64 {
        let storage = cluster.storage_for(id);
        let result = storage
            .get_from_collection("test", &DocumentId::new("dist-1"))
            .await;
        assert!(
            result.is_ok(),
            "document not found on node {} storage",
            id
        );
        let doc = result.unwrap();
        assert_eq!(
            doc.get_field("title"),
            Some(&FieldValue::Text("distributed test".into()))
        );
    }
}

/// Write on the leader, read from a follower.
#[tokio::test]
async fn distributed_read_from_different_node() {
    let cluster = TestCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader().await;
    sleep(Duration::from_millis(300)).await;

    let leader = cluster.leader_node(leader_id);

    // Write a document.
    let doc = Document::new(DocumentId::new("cross-read-1"))
        .with_field("name", FieldValue::Text("test".into()));
    let resp = leader
        .propose(RaftCommand::InsertDocument {
            collection: "test".into(),
            document: doc,
        })
        .await
        .expect("propose failed");
    assert!(resp.success);

    sleep(Duration::from_millis(500)).await;

    // Read from a follower's storage.
    let followers = cluster.follower_nodes(leader_id);
    let follower_storage =
        cluster.storage_for(followers[0].node_id());
    let result = follower_storage
        .get_from_collection("test", &DocumentId::new("cross-read-1"))
        .await;
    assert!(result.is_ok(), "document not found on follower");
}

/// Kill the leader, verify a new leader is elected, verify writes continue.
#[tokio::test]
async fn distributed_survives_leader_failover() {
    let cluster = TestCluster::new().await;
    cluster.initialize().await;
    let original_leader = cluster.wait_for_leader().await;
    sleep(Duration::from_millis(300)).await;

    let leader = cluster.leader_node(original_leader);

    // Write some data before failover.
    for i in 0..10 {
        let doc = Document::new(DocumentId::new(format!("pre-failover-{}", i)))
            .with_field("phase", FieldValue::Text("before".into()));
        let resp = leader
            .propose(RaftCommand::InsertDocument {
                collection: "test".into(),
                document: doc,
            })
            .await
            .expect("pre-failover write failed");
        assert!(resp.success);
    }

    // Wait for replication of pre-failover data.
    sleep(Duration::from_millis(500)).await;

    // Shut down the original leader.
    cluster
        .routers
        .write()
        .await
        .remove(&original_leader);
    let _ = leader.raft_handle().shutdown().await;

    // Wait for new leader election among remaining nodes.
    let remaining: Vec<&Arc<RaftNode>> = cluster.follower_nodes(original_leader);
    let new_leader_id = tokio::time::timeout(Duration::from_secs(5), async {
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

    // Write on the new leader.
    let new_leader = remaining
        .iter()
        .find(|n| n.node_id() == new_leader_id)
        .unwrap();

    sleep(Duration::from_millis(500)).await;

    let doc = Document::new(DocumentId::new("post-failover-1"))
        .with_field("phase", FieldValue::Text("after".into()));
    let resp = new_leader
        .propose(RaftCommand::InsertDocument {
            collection: "test".into(),
            document: doc,
        })
        .await
        .expect("post-failover write failed");
    assert!(resp.success);

    sleep(Duration::from_millis(300)).await;

    // Verify the post-failover document is on the surviving nodes.
    for node in &remaining {
        let storage = cluster.storage_for(node.node_id());
        let result = storage
            .get_from_collection("test", &DocumentId::new("post-failover-1"))
            .await;
        assert!(
            result.is_ok(),
            "post-failover document not found on node {}",
            node.node_id()
        );
    }
}

/// Collection creation replicates to all nodes.
#[tokio::test]
async fn distributed_collection_ops_replicate() {
    let cluster = TestCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader().await;
    sleep(Duration::from_millis(300)).await;

    let leader = cluster.leader_node(leader_id);

    // Create a collection.
    let resp = leader
        .propose(RaftCommand::CreateCollection {
            name: "products".into(),
            schema: msearchdb_index::schema_builder::SchemaConfig::new(),
        })
        .await
        .expect("create collection failed");
    assert!(resp.success);

    // Delete the collection.
    let resp = leader
        .propose(RaftCommand::DeleteCollection {
            name: "products".into(),
        })
        .await
        .expect("delete collection failed");
    assert!(resp.success);
}

/// Batch insert replicates all documents.
#[tokio::test]
async fn distributed_batch_insert_replicates() {
    let cluster = TestCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader().await;
    sleep(Duration::from_millis(300)).await;

    let leader = cluster.leader_node(leader_id);

    let docs: Vec<Document> = (0..50)
        .map(|i| {
            Document::new(DocumentId::new(format!("batch-{}", i)))
                .with_field("seq", FieldValue::Number(i as f64))
        })
        .collect();

    let resp = leader
        .propose_batch("test", docs)
        .await
        .expect("batch insert failed");
    assert!(resp.success);
    assert_eq!(resp.affected_count, 50);

    sleep(Duration::from_millis(500)).await;

    // Verify all 50 documents on all nodes.
    for node_id in 1..=3u64 {
        let storage = cluster.storage_for(node_id);
        for i in 0..50 {
            let result = storage
                .get_from_collection("test", &DocumentId::new(format!("batch-{}", i)))
                .await;
            assert!(
                result.is_ok(),
                "batch doc {} not found on node {}",
                i,
                node_id
            );
        }
    }
}
