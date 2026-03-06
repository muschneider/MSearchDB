//! Workspace-level integration tests for MSearchDB.
//!
//! These tests verify that the core types work correctly when used from
//! an external crate (simulating how downstream consumers will use them).

use msearchdb_core::cluster::*;
use msearchdb_core::cluster_router::ClusterRouter;
use msearchdb_core::config::NodeConfig;
use msearchdb_core::consistent_hash::ConsistentHashRing;
use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::query::*;
use msearchdb_core::rebalancer::{RebalancePlan, Rebalancer};

#[test]
fn create_document_and_serialize() {
    let doc = Document::new(DocumentId::new("integration-1"))
        .with_field("title", FieldValue::Text("Integration Test".into()))
        .with_field("score", FieldValue::Number(100.0))
        .with_field("published", FieldValue::Boolean(true))
        .with_field(
            "tags",
            FieldValue::Array(vec![
                FieldValue::Text("rust".into()),
                FieldValue::Text("database".into()),
            ]),
        );

    let json = serde_json::to_string_pretty(&doc).unwrap();
    let back: Document = serde_json::from_str(&json).unwrap();

    assert_eq!(doc.id, back.id);
    assert_eq!(doc.fields.len(), back.fields.len());
}

#[test]
fn build_complex_bool_query() {
    let query = Query::Bool(BoolQuery {
        must: vec![
            Query::FullText(FullTextQuery {
                field: "body".into(),
                query: "distributed search".into(),
                operator: Operator::And,
            }),
            Query::Range(RangeQuery {
                field: "year".into(),
                gte: Some(2020.0),
                lte: None,
            }),
        ],
        should: vec![Query::Term(TermQuery {
            field: "language".into(),
            value: "rust".into(),
        })],
        must_not: vec![Query::Term(TermQuery {
            field: "status".into(),
            value: "draft".into(),
        })],
    });

    // Roundtrip through JSON
    let json = serde_json::to_string(&query).unwrap();
    let back: Query = serde_json::from_str(&json).unwrap();
    assert_eq!(query, back);
}

#[test]
fn cluster_state_with_multiple_nodes() {
    let state = ClusterState {
        nodes: vec![
            NodeInfo {
                id: NodeId::new(1),
                address: NodeAddress::new("10.0.0.1", 9200),
                status: NodeStatus::Leader,
            },
            NodeInfo {
                id: NodeId::new(2),
                address: NodeAddress::new("10.0.0.2", 9200),
                status: NodeStatus::Follower,
            },
            NodeInfo {
                id: NodeId::new(3),
                address: NodeAddress::new("10.0.0.3", 9200),
                status: NodeStatus::Follower,
            },
        ],
        leader: Some(NodeId::new(1)),
    };

    assert_eq!(state.node_count(), 3);
    assert_eq!(state.leader, Some(NodeId::new(1)));

    let leader = state.get_node(NodeId::new(1)).unwrap();
    assert_eq!(leader.status, NodeStatus::Leader);
}

#[test]
fn node_config_default_is_valid_and_serializable() {
    let cfg = NodeConfig::default();
    cfg.validate().unwrap();

    let toml_str = cfg.to_toml_string().unwrap();
    let back = NodeConfig::from_toml_str(&toml_str).unwrap();
    assert_eq!(cfg, back);
}

#[test]
fn error_propagation_with_question_mark() {
    fn inner() -> DbResult<Document> {
        Err(DbError::NotFound("test".into()))
    }

    fn outer() -> DbResult<String> {
        let _doc = inner()?;
        Ok("unreachable".into())
    }

    let err = outer().unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)));
}

// ---------------------------------------------------------------------------
// Consistent Hash Ring integration tests
// ---------------------------------------------------------------------------

#[test]
fn consistent_hash_ring_end_to_end() {
    let mut ring: ConsistentHashRing<NodeId> = ConsistentHashRing::new();
    ring.add_node("1", NodeId::new(1));
    ring.add_node("2", NodeId::new(2));
    ring.add_node("3", NodeId::new(3));

    // Should return 3 distinct NodeIds.
    let replicas = ring.get_nodes("my-document", 3);
    assert_eq!(replicas.len(), 3);

    let mut ids: Vec<u64> = replicas.iter().map(|n| n.as_u64()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "All 3 replicas must be distinct");
}

#[test]
fn consistent_hash_ring_balance_with_node_ids() {
    let mut ring: ConsistentHashRing<NodeId> = ConsistentHashRing::new();
    ring.add_node("1", NodeId::new(1));
    ring.add_node("2", NodeId::new(2));
    ring.add_node("3", NodeId::new(3));

    let balance = ring.ring_balance();
    assert_eq!(balance.len(), 3);
    for (_id, pct) in &balance {
        assert!(
            (28.0..=38.0).contains(pct),
            "Node owns {pct:.1}%, expected ~33% ± 5%"
        );
    }
}

// ---------------------------------------------------------------------------
// Cluster Router integration tests
// ---------------------------------------------------------------------------

fn make_test_nodes(count: usize) -> Vec<NodeInfo> {
    (1..=count)
        .map(|i| NodeInfo {
            id: NodeId::new(i as u64),
            address: NodeAddress::new(format!("10.0.0.{i}"), 9200),
            status: NodeStatus::Follower,
        })
        .collect()
}

#[test]
fn cluster_router_route_document_deterministic() {
    let router = ClusterRouter::new(make_test_nodes(3), 3);
    let doc_id = DocumentId::new("integration-doc-1");

    let first = router.route_document(&doc_id);
    let second = router.route_document(&doc_id);
    assert_eq!(first, second, "Same doc must always route to same nodes");
    assert_eq!(first.len(), 3);
}

#[test]
fn cluster_router_add_fourth_node_rebalance() {
    let router = ClusterRouter::new(make_test_nodes(3), 3);

    // Route 1,000 documents with 3 nodes.
    let doc_ids: Vec<DocumentId> = (0..1_000)
        .map(|i| DocumentId::new(format!("doc-{i}")))
        .collect();

    let before: Vec<Vec<NodeId>> = doc_ids.iter().map(|d| router.route_document(d)).collect();

    // Add 4th node and compute rebalance plan.
    let new_node = NodeInfo {
        id: NodeId::new(4),
        address: NodeAddress::new("10.0.0.4", 9200),
        status: NodeStatus::Follower,
    };
    let plan = router.rebalance_plan(&new_node);
    assert!(!plan.is_empty(), "Adding a node should require some moves");

    // Apply the node addition.
    let mut router2 = router.clone();
    router2.add_node(new_node);

    let after: Vec<Vec<NodeId>> = doc_ids.iter().map(|d| router2.route_document(d)).collect();

    // Some documents should now route to node-4.
    let routed_to_4 = after
        .iter()
        .filter(|nodes| nodes.contains(&NodeId::new(4)))
        .count();
    assert!(
        routed_to_4 > 0,
        "Some documents should route to the new node"
    );

    // All documents should still be routable (no loss).
    for nodes in &after {
        assert_eq!(nodes.len(), 3, "Each doc should still have 3 replicas");
    }

    // Verify balance improved: ring now has 4 nodes.
    // Check that node-4 received a non-trivial share.
    let mut count_4 = 0_usize;
    for nodes in &after {
        if nodes[0] == NodeId::new(4) {
            count_4 += 1;
        }
    }
    let pct_4 = (count_4 as f64 / doc_ids.len() as f64) * 100.0;
    assert!(
        pct_4 > 10.0,
        "Node-4 primary share {pct_4:.1}% is too low (expected ~25%)"
    );

    // Verify that most documents did NOT change primary (minimal disruption).
    let unchanged = before
        .iter()
        .zip(after.iter())
        .filter(|(b, a)| b[0] == a[0])
        .count();
    let unchanged_pct = (unchanged as f64 / doc_ids.len() as f64) * 100.0;
    assert!(
        unchanged_pct > 60.0,
        "Expected >60% unchanged primaries, got {unchanged_pct:.1}%"
    );
}

#[test]
fn cluster_router_quorum_and_health() {
    let mut router = ClusterRouter::new(make_test_nodes(3), 3);
    assert!(router.can_satisfy_quorum());
    assert_eq!(router.healthy_nodes().len(), 3);

    // Remove one node — quorum still possible.
    router.remove_node(NodeId::new(3));
    assert!(router.can_satisfy_quorum());
    assert_eq!(router.healthy_nodes().len(), 2);

    // Remove another — quorum lost.
    router.remove_node(NodeId::new(2));
    assert!(!router.can_satisfy_quorum());
    assert_eq!(router.healthy_nodes().len(), 1);
}

#[test]
fn cluster_router_query_routing_strategies() {
    let router = ClusterRouter::new(make_test_nodes(3), 3);

    // Full-text search: scatter to all.
    let ft_query = Query::FullText(FullTextQuery {
        field: "body".into(),
        query: "distributed search".into(),
        operator: Operator::And,
    });
    let ft_targets = router.route_query(&ft_query, "products");
    assert_eq!(ft_targets.len(), 3);

    // Term query on _id: route to replicas.
    let id_query = Query::Term(TermQuery {
        field: "_id".into(),
        value: "specific-doc".into(),
    });
    let id_targets = router.route_query(&id_query, "products");
    assert_eq!(id_targets.len(), 3);

    // Should match route_document for the same id.
    let doc_replicas = router.route_document(&DocumentId::new("specific-doc"));
    assert_eq!(id_targets, doc_replicas);
}

// ---------------------------------------------------------------------------
// Rebalancer integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rebalancer_execute_plan_completes() {
    let plan = RebalancePlan {
        moves: vec![
            msearchdb_core::rebalancer::DataMove {
                document_id: DocumentId::new("doc-a"),
                from_node: NodeId::new(1),
                to_node: NodeId::new(4),
            },
            msearchdb_core::rebalancer::DataMove {
                document_id: DocumentId::new("doc-b"),
                from_node: NodeId::new(2),
                to_node: NodeId::new(4),
            },
        ],
    };

    let status = Rebalancer::execute(plan).await;
    assert_eq!(
        status,
        msearchdb_core::rebalancer::RebalanceStatus::Completed
    );
}
