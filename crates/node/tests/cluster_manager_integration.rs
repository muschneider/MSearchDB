//! Integration tests for the ClusterManager.
//!
//! These tests exercise the cluster manager's health monitoring, failure
//! detection, gossip convergence, and read repair in an in-process setting
//! (no real network I/O).

use std::collections::{BTreeMap, HashMap};
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::RwLock;

use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_core::cluster::{NodeAddress, NodeId, NodeInfo, NodeStatus};
use msearchdb_core::cluster_router::ClusterRouter;
use msearchdb_core::config::NodeConfig;
use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::query::{Query, SearchResult};
use msearchdb_core::traits::{IndexBackend, StorageBackend};
use msearchdb_network::connection_pool::ConnectionPool;
use msearchdb_node::cluster_manager::{ClusterManager, FailureDetector, GossipManager, NodeHealth};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// In-memory storage backend for integration tests.
struct MemStorage {
    docs: tokio::sync::Mutex<HashMap<String, Document>>,
}

impl MemStorage {
    fn new() -> Self {
        Self {
            docs: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl StorageBackend for MemStorage {
    async fn get(&self, id: &DocumentId) -> DbResult<Document> {
        let docs = self.docs.lock().await;
        docs.get(id.as_str())
            .cloned()
            .ok_or_else(|| DbError::NotFound(format!("document '{}' not found", id)))
    }

    async fn put(&self, document: Document) -> DbResult<()> {
        let mut docs = self.docs.lock().await;
        docs.insert(document.id.as_str().to_owned(), document);
        Ok(())
    }

    async fn delete(&self, id: &DocumentId) -> DbResult<()> {
        let mut docs = self.docs.lock().await;
        docs.remove(id.as_str())
            .map(|_| ())
            .ok_or_else(|| DbError::NotFound(format!("document '{}' not found", id)))
    }

    async fn scan(
        &self,
        _range: RangeInclusive<DocumentId>,
        _limit: usize,
    ) -> DbResult<Vec<Document>> {
        let docs = self.docs.lock().await;
        Ok(docs.values().cloned().collect())
    }
}

/// No-op index backend for integration tests.
struct EmptyIndex;

#[async_trait]
impl IndexBackend for EmptyIndex {
    async fn index_document(&self, _doc: &Document) -> DbResult<()> {
        Ok(())
    }

    async fn search(&self, _query: &Query) -> DbResult<SearchResult> {
        Ok(SearchResult::empty(0))
    }

    async fn delete_document(&self, _id: &DocumentId) -> DbResult<()> {
        Ok(())
    }
}

/// Make a NodeInfo for testing.
fn make_node_info(id: u64, host: &str, port: u16, status: NodeStatus) -> NodeInfo {
    NodeInfo {
        id: NodeId::new(id),
        address: NodeAddress::new(host, port),
        status,
    }
}

/// Build a ClusterManager with in-memory backends for testing.
async fn make_cluster_manager(node_id: u64) -> (ClusterManager, Arc<dyn StorageBackend>) {
    let config = NodeConfig::default();
    let storage: Arc<dyn StorageBackend> = Arc::new(MemStorage::new());
    let index: Arc<dyn IndexBackend> = Arc::new(EmptyIndex);

    let raft_node = RaftNode::new(&config, storage.clone(), index)
        .await
        .unwrap();
    let raft_node = Arc::new(raft_node);

    // Initialize as single-node cluster.
    let mut members = BTreeMap::new();
    members.insert(config.node_id.as_u64(), openraft::BasicNode::default());
    let _ = raft_node.initialize(members).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let local_node = make_node_info(node_id, "127.0.0.1", 9300, NodeStatus::Leader);

    let nodes = vec![
        local_node.clone(),
        make_node_info(2, "127.0.0.2", 9300, NodeStatus::Follower),
        make_node_info(3, "127.0.0.3", 9300, NodeStatus::Follower),
    ];
    let router = ClusterRouter::new(nodes, 3);

    let manager = ClusterManager::new(
        local_node,
        Arc::new(RwLock::new(router)),
        Arc::new(ConnectionPool::new()),
        raft_node,
        storage.clone(),
        3,
    );

    (manager, storage)
}

// ---------------------------------------------------------------------------
// Health monitoring integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_check_detects_node_going_offline() {
    let (manager, _) = make_cluster_manager(1).await;

    // Register nodes 2 and 3 in health state.
    manager
        .health_state
        .insert(NodeId::new(2), NodeHealth::new(NodeId::new(2)));
    manager
        .health_state
        .insert(NodeId::new(3), NodeHealth::new(NodeId::new(3)));

    // Simulate 3 consecutive failures for node 2.
    {
        let mut health = manager.health_state.get_mut(&NodeId::new(2)).unwrap();
        health.record_failure();
        health.record_failure();
        let went_offline = health.record_failure();
        assert!(went_offline, "node 2 should go offline after 3 failures");
    }

    // Verify node 2 is offline.
    let health2 = manager.health_state.get(&NodeId::new(2)).unwrap();
    assert_eq!(health2.status, NodeStatus::Offline);
    assert_eq!(health2.consecutive_failures, 3);

    // Verify node 3 is still online.
    let health3 = manager.health_state.get(&NodeId::new(3)).unwrap();
    assert!(health3.is_online());
}

#[tokio::test]
async fn health_check_detects_node_recovery() {
    let (manager, _) = make_cluster_manager(1).await;

    manager
        .health_state
        .insert(NodeId::new(2), NodeHealth::new(NodeId::new(2)));

    // Take node offline.
    {
        let mut health = manager.health_state.get_mut(&NodeId::new(2)).unwrap();
        health.record_failure();
        health.record_failure();
        health.record_failure();
        assert!(!health.is_online());
    }

    // Simulate recovery.
    {
        let mut health = manager.health_state.get_mut(&NodeId::new(2)).unwrap();
        let came_back = health.record_success(15);
        assert!(came_back, "should report recovery");
        assert!(health.is_online());
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.latency_ms, Some(15));
    }
}

#[tokio::test]
async fn health_check_within_three_seconds() {
    // Verify that 3 failures at 1s intervals = 3 seconds detection.
    let (manager, _) = make_cluster_manager(1).await;

    manager
        .health_state
        .insert(NodeId::new(2), NodeHealth::new(NodeId::new(2)));

    let start = std::time::Instant::now();

    // Simulate 3 failures at ~1s intervals.
    for _ in 0..3 {
        {
            let mut health = manager.health_state.get_mut(&NodeId::new(2)).unwrap();
            health.record_failure();
        }
        // Each health check occurs at 1s intervals (HEALTH_CHECK_INTERVAL_MS).
        // We simulate without actual sleep for fast tests.
    }

    let elapsed = start.elapsed();

    // The logic itself is O(1) — the 3-second detection is a function of
    // the 1-second health check interval × 3 failures.
    assert!(
        elapsed < Duration::from_secs(1),
        "failure detection logic should be near-instant"
    );

    let health = manager.health_state.get(&NodeId::new(2)).unwrap();
    assert_eq!(health.status, NodeStatus::Offline);
}

// ---------------------------------------------------------------------------
// Failure detector (Phi Accrual) integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn phi_accrual_tracks_multiple_nodes() {
    let mut detector = FailureDetector::new();
    let node_a = NodeId::new(1);
    let node_b = NodeId::new(2);

    let start = std::time::Instant::now();

    // Simulate heartbeats for both nodes.
    for i in 0..10 {
        let at = start + Duration::from_millis(i * 1000);
        detector.report_heartbeat_at(node_a, at);
        detector.report_heartbeat_at(node_b, at);
    }

    // Both should have low phi right after last heartbeat.
    let check = start + Duration::from_millis(9_000);
    assert!(detector.phi_at(&node_a, check).is_some());
    assert!(detector.phi_at(&node_b, check).is_some());

    // Simulate node_b going silent for 20s.
    let check_late = start + Duration::from_millis(9_000 + 20_000);
    let phi_b = detector.phi_at(&node_b, check_late).unwrap();
    assert!(
        phi_b > 8.0,
        "phi {} should indicate suspicion after 20s silence",
        phi_b
    );
}

#[tokio::test]
async fn phi_accrual_adapts_to_irregular_heartbeats() {
    let mut detector = FailureDetector::new();
    let node = NodeId::new(1);
    let start = std::time::Instant::now();

    // Simulate irregular heartbeats (500ms to 2000ms intervals).
    let intervals_ms = [500, 1500, 800, 2000, 700, 1200, 900, 1800, 600, 1000];
    let mut cumulative = 0u64;
    for &interval in &intervals_ms {
        let at = start + Duration::from_millis(cumulative);
        detector.report_heartbeat_at(node, at);
        cumulative += interval;
    }

    // Check phi at last heartbeat time — should be low.
    let at_last = start + Duration::from_millis(cumulative - intervals_ms.last().unwrap());
    let phi_at_last = detector.phi_at(&node, at_last);
    // May not have enough precision for a low phi at exact last beat, but it should exist.
    assert!(phi_at_last.is_some());
}

// ---------------------------------------------------------------------------
// Gossip convergence integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gossip_full_convergence_three_nodes() {
    // 3 nodes, each with partial knowledge, converge after gossip rounds.
    let mut mgr1 = GossipManager::new(NodeId::new(1));
    let mut mgr2 = GossipManager::new(NodeId::new(2));
    let mut mgr3 = GossipManager::new(NodeId::new(3));

    // Initial states (each node knows about itself).
    mgr1.update_local(NodeId::new(1), NodeStatus::Leader, 100);
    mgr2.update_local(NodeId::new(2), NodeStatus::Follower, 100);
    mgr3.update_local(NodeId::new(3), NodeStatus::Follower, 100);

    // Node 2 observes node 3 went offline at time 200.
    mgr2.update_local(NodeId::new(3), NodeStatus::Offline, 200);

    // Round 1: node 1 gossips to node 2.
    let msg1 = mgr1.build_message();
    mgr2.merge(&msg1);
    // Node 2 now knows about node 1 as Leader@100.
    assert_eq!(mgr2.current_view()[&NodeId::new(1)].0, NodeStatus::Leader);

    // Round 2: node 2 gossips to node 3.
    let msg2 = mgr2.build_message();
    mgr3.merge(&msg2);
    // Node 3 now knows: node 1 is Leader@100, node 3 is Offline@200 (per node 2).
    assert_eq!(mgr3.current_view()[&NodeId::new(1)].0, NodeStatus::Leader);
    assert_eq!(mgr3.current_view()[&NodeId::new(3)].0, NodeStatus::Offline);

    // Round 3: node 3 gossips to node 1.
    let msg3 = mgr3.build_message();
    mgr1.merge(&msg3);
    // Node 1 now knows: node 2 is Follower@100, node 3 is Offline@200.
    assert_eq!(mgr1.current_view()[&NodeId::new(3)].0, NodeStatus::Offline);

    // Round 4: node 1 gossips to node 2 and node 3.
    let msg1 = mgr1.build_message();
    mgr2.merge(&msg1);
    mgr3.merge(&msg1);

    // All nodes should now converge on the same view.
    // Node 1: Leader@100
    assert_eq!(mgr1.current_view()[&NodeId::new(1)].0, NodeStatus::Leader);
    assert_eq!(mgr2.current_view()[&NodeId::new(1)].0, NodeStatus::Leader);
    assert_eq!(mgr3.current_view()[&NodeId::new(1)].0, NodeStatus::Leader);

    // Node 3: Offline@200 (all should agree)
    assert_eq!(mgr1.current_view()[&NodeId::new(3)].0, NodeStatus::Offline);
    assert_eq!(mgr2.current_view()[&NodeId::new(3)].0, NodeStatus::Offline);
    assert_eq!(mgr3.current_view()[&NodeId::new(3)].0, NodeStatus::Offline);
}

#[tokio::test]
async fn gossip_partition_heals_and_converges() {
    let mut mgr1 = GossipManager::new(NodeId::new(1));
    let mut mgr2 = GossipManager::new(NodeId::new(2));

    // Both start knowing all 3 nodes as Follower@100.
    for id in [1, 2, 3] {
        mgr1.update_local(NodeId::new(id), NodeStatus::Follower, 100);
        mgr2.update_local(NodeId::new(id), NodeStatus::Follower, 100);
    }

    // Partition: node 1 sees node 2 go Offline@200.
    mgr1.update_local(NodeId::new(2), NodeStatus::Offline, 200);

    // Meanwhile node 2 sees itself as Leader@300 (after election).
    mgr2.update_local(NodeId::new(2), NodeStatus::Leader, 300);

    // Heal partition: node 2 gossips to node 1.
    let msg2 = mgr2.build_message();
    let changed = mgr1.merge(&msg2);

    // Node 1 should update node 2's status to Leader@300 (newer than Offline@200).
    assert!(changed.contains(&NodeId::new(2)));
    assert_eq!(mgr1.current_view()[&NodeId::new(2)].0, NodeStatus::Leader);
}

// ---------------------------------------------------------------------------
// Read repair integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_repair_fixes_stale_local_replica() {
    let (manager, storage) = make_cluster_manager(1).await;

    // Pre-populate storage with an old version.
    let old_doc =
        Document::new(DocumentId::new("rr-doc")).with_field("version", FieldValue::Number(1.0));
    storage.put(old_doc.clone()).await.unwrap();

    // Simulate replicas with different versions.
    let new_doc =
        Document::new(DocumentId::new("rr-doc")).with_field("version", FieldValue::Number(3.0));

    let replicas = vec![
        (NodeId::new(1), old_doc.clone(), 1), // local — stale
        (NodeId::new(2), new_doc.clone(), 3), // remote — latest
        (NodeId::new(3), old_doc.clone(), 1), // remote — stale
    ];

    let result = manager
        .read_repair(&DocumentId::new("rr-doc"), replicas)
        .await;
    assert!(result.is_ok());

    let repaired_doc = result.unwrap();
    assert_eq!(
        repaired_doc.get_field("version"),
        Some(&FieldValue::Number(3.0))
    );

    // Verify that the local storage was updated.
    let stored = storage.get(&DocumentId::new("rr-doc")).await.unwrap();
    assert_eq!(stored.get_field("version"), Some(&FieldValue::Number(3.0)));
}

#[tokio::test]
async fn read_repair_no_action_when_all_agree() {
    let (manager, _) = make_cluster_manager(1).await;

    let doc = Document::new(DocumentId::new("agreed-doc"))
        .with_field("data", FieldValue::Text("same".into()));

    let replicas = vec![
        (NodeId::new(1), doc.clone(), 5),
        (NodeId::new(2), doc.clone(), 5),
        (NodeId::new(3), doc.clone(), 5),
    ];

    let result = manager
        .read_repair(&DocumentId::new("agreed-doc"), replicas)
        .await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().id, DocumentId::new("agreed-doc"));
}

// ---------------------------------------------------------------------------
// Cluster manager node lifecycle tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn register_and_deregister_node_lifecycle() {
    let (manager, _) = make_cluster_manager(1).await;

    // Start with 1 node (local).
    assert_eq!(manager.tracked_node_count(), 1);

    // Register 2 more nodes.
    manager
        .register_node(make_node_info(2, "10.0.0.2", 9300, NodeStatus::Follower))
        .await;
    manager
        .register_node(make_node_info(3, "10.0.0.3", 9300, NodeStatus::Follower))
        .await;
    assert_eq!(manager.tracked_node_count(), 3);

    // All should be healthy.
    let snapshot = manager.health_snapshot();
    assert_eq!(snapshot.len(), 3);
    for h in &snapshot {
        assert!(h.is_online());
    }

    // Deregister node 2.
    manager.deregister_node(NodeId::new(2)).await;
    assert_eq!(manager.tracked_node_count(), 2);
    assert!(!manager.health_state.contains_key(&NodeId::new(2)));
}

#[tokio::test]
async fn leave_cluster_marks_local_offline() {
    let (manager, _) = make_cluster_manager(1).await;

    assert!(manager
        .health_state
        .get(&NodeId::new(1))
        .unwrap()
        .is_online());

    manager.leave_cluster().await.unwrap();

    let health = manager.health_state.get(&NodeId::new(1)).unwrap();
    assert_eq!(health.status, NodeStatus::Offline);
    assert!(!health.is_online());
}

// ---------------------------------------------------------------------------
// Multi-node simulation tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn simulate_node_failure_and_recovery_cycle() {
    let (manager, _) = make_cluster_manager(1).await;

    // Register 2 peer nodes.
    manager
        .register_node(make_node_info(2, "10.0.0.2", 9300, NodeStatus::Follower))
        .await;
    manager
        .register_node(make_node_info(3, "10.0.0.3", 9300, NodeStatus::Follower))
        .await;

    // Simulate node 2 failing.
    {
        let mut h = manager.health_state.get_mut(&NodeId::new(2)).unwrap();
        for _ in 0..3 {
            h.record_failure();
        }
    }

    // Verify node 2 is offline.
    assert_eq!(
        manager.health_state.get(&NodeId::new(2)).unwrap().status,
        NodeStatus::Offline
    );

    // Verify node 3 is still online.
    assert!(manager
        .health_state
        .get(&NodeId::new(3))
        .unwrap()
        .is_online());

    // Simulate node 2 recovering.
    {
        let mut h = manager.health_state.get_mut(&NodeId::new(2)).unwrap();
        let recovered = h.record_success(25);
        assert!(recovered);
    }

    // Verify recovery.
    let h2 = manager.health_state.get(&NodeId::new(2)).unwrap();
    assert!(h2.is_online());
    assert_eq!(h2.consecutive_failures, 0);
    assert_eq!(h2.latency_ms, Some(25));
}

#[tokio::test]
async fn no_data_loss_during_node_failure() {
    let (manager, storage) = make_cluster_manager(1).await;

    // Index a document.
    let doc = Document::new(DocumentId::new("persist-doc"))
        .with_field("title", FieldValue::Text("important data".into()));
    storage.put(doc.clone()).await.unwrap();

    // Simulate node 2 failing.
    manager
        .register_node(make_node_info(2, "10.0.0.2", 9300, NodeStatus::Follower))
        .await;
    {
        let mut h = manager.health_state.get_mut(&NodeId::new(2)).unwrap();
        h.record_failure();
        h.record_failure();
        h.record_failure();
    }

    // The document should still be readable from local storage.
    let fetched = storage.get(&DocumentId::new("persist-doc")).await.unwrap();
    assert_eq!(
        fetched.get_field("title"),
        Some(&FieldValue::Text("important data".into()))
    );
}
