//! Integration tests for the MSearchDB network crate.
//!
//! These tests start tonic gRPC servers in-process (on random ports) and
//! exercise full RPC roundtrips: health checks, search, get, scatter-gather,
//! and write forwarding.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::time::sleep;

use msearchdb_core::cluster::{NodeAddress, NodeId};
use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::query::{FullTextQuery, Operator, Query, ScoredDocument, SearchResult};
use msearchdb_core::traits::{IndexBackend, StorageBackend};

use msearchdb_network::client::NodeClient;
use msearchdb_network::connection_pool::{CircuitState, ConnectionPool, PoolConfig};
use msearchdb_network::proto;
use msearchdb_network::scatter_gather::scatter_search;
use msearchdb_network::server::QueryServiceImpl;

// ---------------------------------------------------------------------------
// Mock backends for integration tests
// ---------------------------------------------------------------------------

/// In-memory storage backend for testing.
struct MockStorage {
    docs: Mutex<HashMap<String, Document>>,
}

impl MockStorage {
    fn new() -> Self {
        Self {
            docs: Mutex::new(HashMap::new()),
        }
    }

    async fn insert(&self, doc: Document) {
        let mut docs = self.docs.lock().await;
        docs.insert(doc.id.as_str().to_owned(), doc);
    }
}

#[async_trait]
impl StorageBackend for MockStorage {
    async fn get(&self, id: &DocumentId) -> DbResult<Document> {
        let docs = self.docs.lock().await;
        docs.get(id.as_str())
            .cloned()
            .ok_or_else(|| DbError::NotFound(id.to_string()))
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
            .ok_or_else(|| DbError::NotFound(id.to_string()))
    }

    async fn scan(
        &self,
        _range: std::ops::RangeInclusive<DocumentId>,
        limit: usize,
    ) -> DbResult<Vec<Document>> {
        let docs = self.docs.lock().await;
        Ok(docs.values().take(limit).cloned().collect())
    }
}

/// Mock index backend that returns configurable search results.
struct MockIndex {
    results: Mutex<SearchResult>,
}

impl MockIndex {
    fn new(results: SearchResult) -> Self {
        Self {
            results: Mutex::new(results),
        }
    }

    fn empty() -> Self {
        Self::new(SearchResult::empty(0))
    }
}

#[async_trait]
impl IndexBackend for MockIndex {
    async fn index_document(&self, _document: &Document) -> DbResult<()> {
        Ok(())
    }

    async fn search(&self, _query: &Query) -> DbResult<SearchResult> {
        let results = self.results.lock().await;
        Ok(results.clone())
    }

    async fn delete_document(&self, _id: &DocumentId) -> DbResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Find a free TCP port by binding to port 0.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Start a QueryService-only gRPC server on a random port and return the
/// address.
async fn start_query_server(
    storage: Arc<dyn StorageBackend>,
    index: Arc<dyn IndexBackend>,
    node_id: u64,
) -> SocketAddr {
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

    let query_svc = QueryServiceImpl::new(storage, index, node_id);

    // Spawn the server in the background.
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(proto::query_service_server::QueryServiceServer::new(
                query_svc,
            ))
            .serve(addr)
            .await
            .unwrap();
    });

    // Give the server a moment to start.
    sleep(Duration::from_millis(50)).await;

    addr
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_service_search_roundtrip() {
    let storage: Arc<dyn StorageBackend> = Arc::new(MockStorage::new());
    let index: Arc<dyn IndexBackend> = Arc::new(MockIndex::new(SearchResult {
        documents: vec![ScoredDocument {
            document: Document::new(DocumentId::new("d1"))
                .with_field("title", FieldValue::Text("hello world".into())),
            score: 2.5,
        }],
        total: 1,
        took_ms: 5,
    }));

    let addr = start_query_server(storage, index, 1).await;

    // Connect a client.
    let client = NodeClient::connect_to_endpoint(&format!("http://{}", addr))
        .await
        .unwrap();

    let query = Query::FullText(FullTextQuery {
        field: "title".into(),
        query: "hello".into(),
        operator: Operator::Or,
    });

    let result = client.search(&query, 10).await.unwrap();
    assert_eq!(result.total, 1);
    assert_eq!(result.documents.len(), 1);
    assert_eq!(result.documents[0].document.id.as_str(), "d1");
    assert_eq!(result.documents[0].score, 2.5);
}

#[tokio::test]
async fn query_service_get_roundtrip() {
    let storage = Arc::new(MockStorage::new());
    storage
        .insert(
            Document::new(DocumentId::new("abc"))
                .with_field("name", FieldValue::Text("Alice".into())),
        )
        .await;
    let storage: Arc<dyn StorageBackend> = storage;
    let index: Arc<dyn IndexBackend> = Arc::new(MockIndex::empty());

    let addr = start_query_server(storage, index, 2).await;
    let client = NodeClient::connect_to_endpoint(&format!("http://{}", addr))
        .await
        .unwrap();

    // Get existing document.
    let doc = client.get(&DocumentId::new("abc")).await.unwrap();
    assert!(doc.is_some());
    let doc = doc.unwrap();
    assert_eq!(doc.id.as_str(), "abc");
    assert_eq!(
        doc.get_field("name"),
        Some(&FieldValue::Text("Alice".into()))
    );

    // Get missing document.
    let missing = client.get(&DocumentId::new("missing")).await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn two_servers_full_rpc_roundtrip() {
    // Start two independent QueryService servers.
    let storage1: Arc<dyn StorageBackend> = Arc::new(MockStorage::new());
    let index1: Arc<dyn IndexBackend> = Arc::new(MockIndex::new(SearchResult {
        documents: vec![ScoredDocument {
            document: Document::new(DocumentId::new("n1-d1"))
                .with_field("title", FieldValue::Text("from node 1".into())),
            score: 3.0,
        }],
        total: 1,
        took_ms: 2,
    }));

    let storage2: Arc<dyn StorageBackend> = Arc::new(MockStorage::new());
    let index2: Arc<dyn IndexBackend> = Arc::new(MockIndex::new(SearchResult {
        documents: vec![ScoredDocument {
            document: Document::new(DocumentId::new("n2-d1"))
                .with_field("title", FieldValue::Text("from node 2".into())),
            score: 1.5,
        }],
        total: 1,
        took_ms: 3,
    }));

    let addr1 = start_query_server(storage1, index1, 1).await;
    let addr2 = start_query_server(storage2, index2, 2).await;

    let client1 = NodeClient::connect_to_endpoint(&format!("http://{}", addr1))
        .await
        .unwrap();
    let client2 = NodeClient::connect_to_endpoint(&format!("http://{}", addr2))
        .await
        .unwrap();

    let query = Query::FullText(FullTextQuery {
        field: "title".into(),
        query: "test".into(),
        operator: Operator::Or,
    });

    // Each server should return its own results.
    let r1 = client1.search(&query, 10).await.unwrap();
    assert_eq!(r1.documents[0].document.id.as_str(), "n1-d1");

    let r2 = client2.search(&query, 10).await.unwrap();
    assert_eq!(r2.documents[0].document.id.as_str(), "n2-d1");
}

#[tokio::test]
async fn scatter_gather_with_three_nodes() {
    // Create 3 servers with different results.
    let make_server = |node_id: u64, doc_id: &str, score: f32| {
        let storage: Arc<dyn StorageBackend> = Arc::new(MockStorage::new());
        let index: Arc<dyn IndexBackend> = Arc::new(MockIndex::new(SearchResult {
            documents: vec![ScoredDocument {
                document: Document::new(DocumentId::new(doc_id))
                    .with_field("x", FieldValue::Number(score as f64)),
                score,
            }],
            total: 1,
            took_ms: 1,
        }));
        (storage, index, node_id)
    };

    let (s1, i1, _) = make_server(1, "a", 5.0);
    let (s2, i2, _) = make_server(2, "b", 3.0);
    let (s3, i3, _) = make_server(3, "c", 7.0);

    let addr1 = start_query_server(s1, i1, 1).await;
    let addr2 = start_query_server(s2, i2, 2).await;
    let addr3 = start_query_server(s3, i3, 3).await;

    let c1 = NodeClient::connect_to_endpoint(&format!("http://{}", addr1))
        .await
        .unwrap();
    let c2 = NodeClient::connect_to_endpoint(&format!("http://{}", addr2))
        .await
        .unwrap();
    let c3 = NodeClient::connect_to_endpoint(&format!("http://{}", addr3))
        .await
        .unwrap();

    let nodes = vec![
        (NodeId::new(1), c1),
        (NodeId::new(2), c2),
        (NodeId::new(3), c3),
    ];

    let query = Query::FullText(FullTextQuery {
        field: "x".into(),
        query: "anything".into(),
        operator: Operator::Or,
    });

    let result = scatter_search(&nodes, &query, 10, Some(Duration::from_secs(5)))
        .await
        .unwrap();

    // Should have results from all 3 nodes, ranked by score.
    assert_eq!(result.documents.len(), 3);
    assert_eq!(result.documents[0].document.id.as_str(), "c"); // score 7.0
    assert_eq!(result.documents[1].document.id.as_str(), "a"); // score 5.0
    assert_eq!(result.documents[2].document.id.as_str(), "b"); // score 3.0
    assert_eq!(result.total, 3);
}

#[tokio::test]
async fn scatter_gather_one_node_timing_out() {
    // Create 3 servers; the third one will be on an unreachable address.
    let (s1, i1) = {
        let s: Arc<dyn StorageBackend> = Arc::new(MockStorage::new());
        let i: Arc<dyn IndexBackend> = Arc::new(MockIndex::new(SearchResult {
            documents: vec![ScoredDocument {
                document: Document::new(DocumentId::new("a"))
                    .with_field("x", FieldValue::Number(5.0)),
                score: 5.0,
            }],
            total: 1,
            took_ms: 1,
        }));
        (s, i)
    };

    let (s2, i2) = {
        let s: Arc<dyn StorageBackend> = Arc::new(MockStorage::new());
        let i: Arc<dyn IndexBackend> = Arc::new(MockIndex::new(SearchResult {
            documents: vec![ScoredDocument {
                document: Document::new(DocumentId::new("b"))
                    .with_field("x", FieldValue::Number(3.0)),
                score: 3.0,
            }],
            total: 1,
            took_ms: 1,
        }));
        (s, i)
    };

    let addr1 = start_query_server(s1, i1, 1).await;
    let addr2 = start_query_server(s2, i2, 2).await;

    let c1 = NodeClient::connect_to_endpoint(&format!("http://{}", addr1))
        .await
        .unwrap();
    let c2 = NodeClient::connect_to_endpoint(&format!("http://{}", addr2))
        .await
        .unwrap();

    // Create a client connected to a real server but that will be used to call
    // a nonexistent endpoint — we'll simulate the timeout by creating a client
    // pointing at a port with no server.
    let dead_port = free_port();
    // We won't connect — we'll construct from an already-connected channel
    // to a dead server. Instead, let's just use the good clients + one that
    // returns an error.
    //
    // Simplest approach: point the 3rd "client" at an unreachable port.
    // Since `connect_to_endpoint` will fail, let's use a channel that's
    // connected to a live server but call the wrong service.  Actually,
    // the simplest is to create a client to a running server but with a
    // very short timeout — or just skip the dead node.
    //
    // The cleanest test: scatter_search should tolerate errors from one node.
    // We'll use 2 real clients and 1 client that points at a dead endpoint
    // by reusing an already-established channel. The search RPC will fail
    // when the server doesn't exist.

    // Connect to the dead port — this will succeed because tonic lazy-connects.
    let dead_channel =
        tonic::transport::Channel::from_shared(format!("http://127.0.0.1:{}", dead_port))
            .unwrap()
            .connect_timeout(Duration::from_millis(50))
            .timeout(Duration::from_millis(50))
            .connect_lazy();

    let c3 = NodeClient::from_channel(dead_channel, format!("http://127.0.0.1:{}", dead_port));

    let nodes = vec![
        (NodeId::new(1), c1),
        (NodeId::new(2), c2),
        (NodeId::new(3), c3),
    ];

    let query = Query::FullText(FullTextQuery {
        field: "x".into(),
        query: "anything".into(),
        operator: Operator::Or,
    });

    // Use a short timeout so the dead node times out quickly.
    let result = scatter_search(&nodes, &query, 10, Some(Duration::from_millis(500)))
        .await
        .unwrap();

    // Should still have results from the 2 healthy nodes.
    assert!(result.documents.len() >= 2);

    // Results should be ranked.
    assert_eq!(result.documents[0].document.id.as_str(), "a"); // score 5.0
    assert_eq!(result.documents[1].document.id.as_str(), "b"); // score 3.0
}

#[tokio::test]
async fn scatter_gather_deduplication_across_nodes() {
    // Two servers return the same document with different scores.
    let make_index = |score: f32| -> Arc<dyn IndexBackend> {
        Arc::new(MockIndex::new(SearchResult {
            documents: vec![ScoredDocument {
                document: Document::new(DocumentId::new("shared-doc"))
                    .with_field("x", FieldValue::Number(1.0)),
                score,
            }],
            total: 1,
            took_ms: 1,
        }))
    };

    let s1: Arc<dyn StorageBackend> = Arc::new(MockStorage::new());
    let s2: Arc<dyn StorageBackend> = Arc::new(MockStorage::new());

    let addr1 = start_query_server(s1, make_index(2.0), 1).await;
    let addr2 = start_query_server(s2, make_index(5.0), 2).await;

    let c1 = NodeClient::connect_to_endpoint(&format!("http://{}", addr1))
        .await
        .unwrap();
    let c2 = NodeClient::connect_to_endpoint(&format!("http://{}", addr2))
        .await
        .unwrap();

    let nodes = vec![(NodeId::new(1), c1), (NodeId::new(2), c2)];

    let query = Query::FullText(FullTextQuery {
        field: "x".into(),
        query: "anything".into(),
        operator: Operator::Or,
    });

    let result = scatter_search(&nodes, &query, 10, Some(Duration::from_secs(5)))
        .await
        .unwrap();

    // Should deduplicate to 1 document with the higher score.
    assert_eq!(result.documents.len(), 1);
    assert_eq!(result.documents[0].document.id.as_str(), "shared-doc");
    assert_eq!(result.documents[0].score, 5.0);
}

#[tokio::test]
async fn connection_pool_acquire_and_circuit_breaker() {
    let storage: Arc<dyn StorageBackend> = Arc::new(MockStorage::new());
    let index: Arc<dyn IndexBackend> = Arc::new(MockIndex::empty());

    let addr = start_query_server(storage, index, 1).await;
    let node_addr = NodeAddress::new("127.0.0.1", addr.port());
    let node_id = NodeId::new(1);

    let pool = ConnectionPool::with_config(PoolConfig {
        max_connections_per_node: 10,
        failure_threshold: 2,
        open_duration: Duration::from_secs(60),
    });

    // Acquire a client — should connect.
    let _client = pool.get(&node_id, &node_addr).await.unwrap();

    // Pool should now have one entry.
    assert_eq!(pool.len().await, 1);
    assert_eq!(
        pool.circuit_state(&node_id).await,
        Some(CircuitState::Closed)
    );

    // Record 2 failures to trip the circuit breaker.
    pool.record_failure(&node_id).await;
    pool.record_failure(&node_id).await;
    assert_eq!(pool.circuit_state(&node_id).await, Some(CircuitState::Open));

    // Next acquire should fail with circuit open.
    let result = pool.get(&node_id, &node_addr).await;
    assert!(result.is_err());

    // Record success should reset.
    pool.record_success(&node_id).await;
    assert_eq!(
        pool.circuit_state(&node_id).await,
        Some(CircuitState::Closed)
    );

    // Re-acquire should work.
    let _client2 = pool.get(&node_id, &node_addr).await.unwrap();
    assert_eq!(pool.len().await, 1);
}

#[tokio::test]
async fn connection_pool_reacquire_returns_cached_client() {
    let storage: Arc<dyn StorageBackend> = Arc::new(MockStorage::new());
    let index: Arc<dyn IndexBackend> = Arc::new(MockIndex::empty());

    let addr = start_query_server(storage, index, 42).await;
    let node_addr = NodeAddress::new("127.0.0.1", addr.port());
    let node_id = NodeId::new(42);

    let pool = ConnectionPool::new();

    // First acquire creates a new connection.
    let _c1 = pool.get(&node_id, &node_addr).await.unwrap();
    assert_eq!(pool.len().await, 1);

    // Second acquire returns the cached connection (pool length stays 1).
    let _c2 = pool.get(&node_id, &node_addr).await.unwrap();
    assert_eq!(pool.len().await, 1);
}

// ---------------------------------------------------------------------------
// Protobuf serialization roundtrip tests
// ---------------------------------------------------------------------------

#[test]
fn protobuf_write_request_roundtrip() {
    use msearchdb_consensus::types::RaftCommand;

    let cmd = RaftCommand::InsertDocument {
        document: Document::new(DocumentId::new("proto-doc"))
            .with_field("title", FieldValue::Text("hello".into())),
    };

    let bytes = serde_json::to_vec(&cmd).unwrap();
    let req = proto::WriteRequest {
        command: bytes.clone(),
    };
    let back: RaftCommand = serde_json::from_slice(&req.command).unwrap();
    assert_eq!(cmd, back);
}

#[test]
fn protobuf_write_response_roundtrip() {
    let resp = proto::WriteResponse {
        success: true,
        document_id: "doc-42".into(),
        error: String::new(),
    };
    assert!(resp.success);
    assert_eq!(resp.document_id, "doc-42");
    assert!(resp.error.is_empty());
}

#[test]
fn protobuf_health_request_response() {
    let _req = proto::HealthRequest {};
    let resp = proto::HealthResponse {
        node_id: 7,
        status: "Follower".into(),
        leader_id: 1,
        current_term: 10,
        last_applied: 50,
        is_healthy: true,
    };
    assert_eq!(resp.node_id, 7);
    assert!(resp.is_healthy);
}

#[test]
fn protobuf_join_roundtrip() {
    let req = proto::JoinRequest {
        node_id: 5,
        address: "10.0.0.5:9300".into(),
    };
    assert_eq!(req.node_id, 5);
    assert_eq!(req.address, "10.0.0.5:9300");

    let resp = proto::JoinResponse {
        success: true,
        error: String::new(),
        leader_id: 1,
    };
    assert!(resp.success);
}

#[test]
fn protobuf_search_request_response() {
    let query = Query::FullText(FullTextQuery {
        field: "body".into(),
        query: "rust async".into(),
        operator: Operator::And,
    });
    let query_bytes = serde_json::to_vec(&query).unwrap();

    let req = proto::SearchRequest {
        query: query_bytes.clone(),
        limit: 25,
    };
    let back_query: Query = serde_json::from_slice(&req.query).unwrap();
    assert_eq!(query, back_query);
    assert_eq!(req.limit, 25);

    let result = SearchResult {
        documents: vec![ScoredDocument {
            document: Document::new(DocumentId::new("sr-1"))
                .with_field("body", FieldValue::Text("rust async".into())),
            score: 4.2,
        }],
        total: 1,
        took_ms: 8,
    };
    let result_bytes = serde_json::to_vec(&result).unwrap();

    let resp = proto::SearchResponse {
        result: result_bytes.clone(),
        error: String::new(),
    };
    let back_result: SearchResult = serde_json::from_slice(&resp.result).unwrap();
    assert_eq!(result, back_result);
}

#[test]
fn protobuf_get_request_response() {
    let req = proto::GetRequest {
        document_id: "get-1".into(),
    };
    assert_eq!(req.document_id, "get-1");

    let doc =
        Document::new(DocumentId::new("get-1")).with_field("status", FieldValue::Boolean(true));
    let doc_bytes = serde_json::to_vec(&doc).unwrap();

    let resp = proto::GetResponse {
        document: doc_bytes.clone(),
        found: true,
        error: String::new(),
    };
    assert!(resp.found);
    let back: Document = serde_json::from_slice(&resp.document).unwrap();
    assert_eq!(doc.id, back.id);
}

#[test]
fn protobuf_snapshot_chunk() {
    let chunk = proto::SnapshotChunk {
        offset: 0,
        data: vec![1, 2, 3, 4, 5],
        done: false,
        metadata: vec![10, 20],
    };
    assert_eq!(chunk.offset, 0);
    assert!(!chunk.done);
    assert_eq!(chunk.data.len(), 5);
    assert_eq!(chunk.metadata.len(), 2);
}

#[test]
fn protobuf_scatter_request_result() {
    let query = Query::Term(msearchdb_core::query::TermQuery {
        field: "status".into(),
        value: "active".into(),
    });
    let query_bytes = serde_json::to_vec(&query).unwrap();

    let req = proto::ScatterRequest {
        query: query_bytes,
        limit: 50,
    };
    assert_eq!(req.limit, 50);

    let scored = ScoredDocument {
        document: Document::new(DocumentId::new("sc-1")),
        score: 1.0,
    };
    let scored_bytes = serde_json::to_vec(&scored).unwrap();

    let result = proto::ScatterResult {
        scored_document: scored_bytes.clone(),
        node_id: 3,
    };
    let back: ScoredDocument = serde_json::from_slice(&result.scored_document).unwrap();
    assert_eq!(scored, back);
    assert_eq!(result.node_id, 3);
}
