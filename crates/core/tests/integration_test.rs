//! Workspace-level integration tests for MSearchDB.
//!
//! These tests verify that the core types work correctly when used from
//! an external crate (simulating how downstream consumers will use them).

use msearchdb_core::cluster::*;
use msearchdb_core::config::NodeConfig;
use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::query::*;

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
