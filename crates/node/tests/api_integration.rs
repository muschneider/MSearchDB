//! Integration tests for the MSearchDB REST API.
//!
//! These tests exercise the full HTTP stack using axum's test utilities.
//! A single-node Raft cluster is initialised in-memory for each test.

use std::collections::{BTreeMap, HashMap};
use std::ops::RangeInclusive;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http::Method;
use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};
use tower::ServiceExt;

use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_core::config::NodeConfig;
use msearchdb_core::document::{Document, DocumentId};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::query::{Query, SearchResult};
use msearchdb_core::traits::{IndexBackend, StorageBackend};
use msearchdb_network::connection_pool::ConnectionPool;
use msearchdb_node::state::AppState;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// In-memory storage backend for tests.
///
/// Supports both global and per-collection document storage.
struct MemStorage {
    docs: Mutex<HashMap<String, Document>>,
    /// Per-collection document storage: collection name → (doc id → Document).
    collections: Mutex<HashMap<String, HashMap<String, Document>>>,
}

impl MemStorage {
    fn new() -> Self {
        Self {
            docs: Mutex::new(HashMap::new()),
            collections: Mutex::new(HashMap::new()),
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

    // -- Collection-scoped operations ----------------------------------------

    async fn create_collection(&self, collection: &str) -> DbResult<()> {
        let mut cols = self.collections.lock().await;
        cols.entry(collection.to_owned())
            .or_insert_with(HashMap::new);
        Ok(())
    }

    async fn drop_collection(&self, collection: &str) -> DbResult<()> {
        let mut cols = self.collections.lock().await;
        cols.remove(collection);
        Ok(())
    }

    async fn get_from_collection(
        &self,
        collection: &str,
        id: &DocumentId,
    ) -> DbResult<Document> {
        let cols = self.collections.lock().await;
        let col = cols
            .get(collection)
            .ok_or_else(|| DbError::NotFound(format!("collection '{}' not found", collection)))?;
        col.get(id.as_str())
            .cloned()
            .ok_or_else(|| DbError::NotFound(format!("document '{}' not found", id)))
    }

    async fn put_in_collection(
        &self,
        collection: &str,
        document: Document,
    ) -> DbResult<()> {
        let mut cols = self.collections.lock().await;
        let col = cols
            .entry(collection.to_owned())
            .or_insert_with(HashMap::new);
        col.insert(document.id.as_str().to_owned(), document);
        Ok(())
    }

    async fn delete_from_collection(
        &self,
        collection: &str,
        id: &DocumentId,
    ) -> DbResult<()> {
        let mut cols = self.collections.lock().await;
        if let Some(col) = cols.get_mut(collection) {
            col.remove(id.as_str());
        }
        Ok(())
    }
}

/// No-op index that returns empty results.
///
/// Supports collection-scoped operations as no-ops / empty results.
struct EmptyIndex;

#[async_trait]
impl IndexBackend for EmptyIndex {
    async fn index_document(&self, _doc: &Document) -> DbResult<()> {
        Ok(())
    }

    async fn search(&self, _query: &Query) -> DbResult<SearchResult> {
        Ok(SearchResult::empty(1))
    }

    async fn delete_document(&self, _id: &DocumentId) -> DbResult<()> {
        Ok(())
    }

    // -- Collection-scoped operations ----------------------------------------

    async fn create_collection_index(&self, _collection: &str) -> DbResult<()> {
        Ok(())
    }

    async fn drop_collection_index(&self, _collection: &str) -> DbResult<()> {
        Ok(())
    }

    async fn index_document_in_collection(
        &self,
        _collection: &str,
        _document: &Document,
        mapping: &msearchdb_core::collection::FieldMapping,
    ) -> DbResult<msearchdb_core::collection::FieldMapping> {
        // Return the mapping unchanged — no dynamic field detection in mock.
        Ok(mapping.clone())
    }

    async fn search_collection(
        &self,
        _collection: &str,
        _query: &Query,
    ) -> DbResult<SearchResult> {
        Ok(SearchResult::empty(1))
    }

    async fn delete_document_from_collection(
        &self,
        _collection: &str,
        _id: &DocumentId,
    ) -> DbResult<()> {
        Ok(())
    }

    async fn commit_collection_index(&self, _collection: &str) -> DbResult<()> {
        Ok(())
    }
}

/// Create a test `AppState` with a single-node Raft cluster initialised.
async fn make_test_state() -> AppState {
    let config = NodeConfig::default();
    let storage: Arc<dyn StorageBackend> = Arc::new(MemStorage::new());
    let index: Arc<dyn IndexBackend> = Arc::new(EmptyIndex);

    let raft_node = RaftNode::new(&config, storage.clone(), index.clone())
        .await
        .unwrap();
    let raft_node = Arc::new(raft_node);

    // Initialise as single-node cluster so proposals succeed.
    let mut members = BTreeMap::new();
    members.insert(config.node_id.as_u64(), openraft::BasicNode::default());
    raft_node.initialize(members).await.unwrap();

    // Wait briefly for leader election.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    AppState {
        raft_node,
        storage,
        index,
        connection_pool: Arc::new(ConnectionPool::new()),
        collections: Arc::new(RwLock::new(HashMap::new())),
        aliases: Arc::new(RwLock::new(HashMap::new())),
        api_key: None,
        metrics: Arc::new(msearchdb_node::metrics::Metrics::new()),
        local_node_id: config.node_id,
        read_coordinator: Arc::new(msearchdb_core::read_coordinator::ReadCoordinator::new(
            config.replication_factor,
        )),
    }
}

/// Send a request to the app and return the response.
async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

// ---------------------------------------------------------------------------
// Collection CRUD tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_and_get_collection() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/articles")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "articles");

    // Get collection info
    let req = Request::builder()
        .uri("/collections/articles")
        .body(Body::empty())
        .unwrap();

    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "articles");
    assert_eq!(body["doc_count"], 0);
}

#[tokio::test]
async fn test_list_collections() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create two collections
    for name in &["col_a", "col_b"] {
        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/collections/{}", name))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let (status, _) = send(app.clone(), req).await;
        assert_eq!(status, StatusCode::OK);
    }

    // List
    let req = Request::builder()
        .uri("/collections")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

#[tokio::test]
async fn test_delete_collection() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/temp")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Delete
    let req = Request::builder()
        .method(Method::DELETE)
        .uri("/collections/temp")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["acknowledged"], true);

    // Verify gone
    let req = Request::builder()
        .uri("/collections/temp")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_missing_collection_returns_404() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .uri("/collections/nonexistent")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "not_found");
}

// ---------------------------------------------------------------------------
// Document CRUD tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_document_crud_lifecycle() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection first
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/docs_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Index a document
    let doc_body = json!({
        "id": "doc-1",
        "fields": {
            "title": "Hello World",
            "count": 42
        }
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/docs_test/docs")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&doc_body).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["_id"], "doc-1");
    assert_eq!(body["result"], "created");

    // Get the document
    let req = Request::builder()
        .uri("/collections/docs_test/docs/doc-1")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["_id"], "doc-1");
    assert_eq!(body["found"], true);

    // Upsert the document
    let upsert_body = json!({
        "fields": {
            "title": "Updated Title",
            "count": 100
        }
    });
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/docs_test/docs/doc-1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&upsert_body).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], "updated");

    // Delete the document
    let req = Request::builder()
        .method(Method::DELETE)
        .uri("/collections/docs_test/docs/doc-1")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], "deleted");
}

#[tokio::test]
async fn test_get_missing_document_returns_404() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/miss_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Get non-existent document
    let req = Request::builder()
        .uri("/collections/miss_test/docs/nonexistent")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "not_found");
}

#[tokio::test]
async fn test_index_document_to_missing_collection_returns_404() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    let doc_body = json!({
        "fields": {"title": "test"}
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/nonexistent/docs")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&doc_body).unwrap()))
        .unwrap();
    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Search tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_search_returns_results() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/search_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Search (returns empty from our mock index)
    let search_body = json!({
        "query": {"match": {"title": {"query": "hello"}}}
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/search_test/_search")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&search_body).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["hits"]["total"]["value"].is_number());
    assert!(!body["timed_out"].as_bool().unwrap());
}

#[tokio::test]
async fn test_simple_search_query_string() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/qs_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Simple query-string search
    let req = Request::builder()
        .uri("/collections/qs_test/_search?q=hello")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["hits"].is_object());
}

// ---------------------------------------------------------------------------
// Bulk indexing tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_bulk_index() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/bulk_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Bulk index 5 documents
    let ndjson = r#"{"index": {"_id": "1"}}
{"title": "doc one"}
{"index": {"_id": "2"}}
{"title": "doc two"}
{"index": {"_id": "3"}}
{"title": "doc three"}
{"index": {"_id": "4"}}
{"title": "doc four"}
{"index": {"_id": "5"}}
{"title": "doc five"}
"#;

    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/bulk_test/docs/_bulk")
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from(ndjson))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body["errors"].as_bool().unwrap());
    assert_eq!(body["items"].as_array().unwrap().len(), 5);
}

// ---------------------------------------------------------------------------
// Cluster endpoint tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cluster_health() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .uri("/_cluster/health")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["status"].is_string());
    assert!(body["number_of_nodes"].is_number());
}

#[tokio::test]
async fn test_cluster_state() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .uri("/_cluster/state")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["nodes"].is_array());
}

#[tokio::test]
async fn test_list_nodes() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .uri("/_nodes")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().len() >= 1);
}

// ---------------------------------------------------------------------------
// Admin endpoint tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_stats() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .uri("/_stats")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["node_id"].is_number());
}

#[tokio::test]
async fn test_refresh_collection() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/refresh_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Refresh
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/refresh_test/_refresh")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["acknowledged"], true);
}

// ---------------------------------------------------------------------------
// Error handling tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_invalid_json_returns_400() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/bad_json_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Send invalid JSON to document endpoint
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/bad_json_test/docs")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("not valid json {{{"))
        .unwrap();
    let (status, _) = send(app, req).await;
    // axum returns 422 for JSON parse errors by default
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------
// Request ID middleware test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_request_id_header_present() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .uri("/_stats")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // X-Request-Id should be present in the response
    let request_id = resp.headers().get("x-request-id");
    assert!(request_id.is_some());
}

// ---------------------------------------------------------------------------
// Auth middleware test
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Consistency-level read tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_read_with_all_consistency_returns_latest_value() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/consist_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Index a document
    let doc_body = json!({
        "id": "c-doc-1",
        "fields": {
            "title": "Original Title",
            "count": 1
        }
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/consist_test/docs")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&doc_body).unwrap()))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Read with consistency=all — should return the document
    let req = Request::builder()
        .uri("/collections/consist_test/docs/c-doc-1?consistency=all")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["_id"], "c-doc-1");
    assert_eq!(body["found"], true);
    assert_eq!(body["consistency"], "ALL");
    assert!(body["_version"].is_number());
    assert_eq!(body["_source"]["title"], "Original Title");

    // Read with consistency=one
    let req = Request::builder()
        .uri("/collections/consist_test/docs/c-doc-1?consistency=one")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["consistency"], "ONE");
    assert_eq!(body["_source"]["title"], "Original Title");

    // Read with default consistency (quorum)
    let req = Request::builder()
        .uri("/collections/consist_test/docs/c-doc-1")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["consistency"], "QUORUM");
    assert_eq!(body["_source"]["title"], "Original Title");

    // Update the document
    let upsert_body = json!({
        "fields": {
            "title": "Updated Title",
            "count": 42
        }
    });
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/consist_test/docs/c-doc-1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&upsert_body).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], "updated");

    // Read with consistency=all after update — must see the latest value
    let req = Request::builder()
        .uri("/collections/consist_test/docs/c-doc-1?consistency=all")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["_id"], "c-doc-1");
    assert_eq!(body["found"], true);
    assert_eq!(body["consistency"], "ALL");
    assert_eq!(body["_source"]["title"], "Updated Title");
    assert_eq!(body["_source"]["count"], 42.0);
}

#[tokio::test]
async fn test_read_missing_doc_with_consistency_returns_404() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/consist_miss")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Read non-existent document with consistency=all
    let req = Request::builder()
        .uri("/collections/consist_miss/docs/no-such-doc?consistency=all")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "not_found");
}

// ---------------------------------------------------------------------------
// Auth middleware test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_rejects_without_key() {
    let mut state = make_test_state().await;
    state.api_key = Some("test-secret".into());
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .uri("/_stats")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_auth_passes_with_correct_key() {
    let mut state = make_test_state().await;
    state.api_key = Some("test-secret".into());
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .uri("/_stats")
        .header("x-api-key", "test-secret")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Schema conflict test
// ---------------------------------------------------------------------------

/// Index mock that performs dynamic field mapping and detects schema conflicts.
///
/// Unlike [`EmptyIndex`], this mock actually registers field types via
/// [`FieldMapping::register_field`] so that schema conflicts surface when a
/// field's type changes.
struct MappingAwareIndex;

#[async_trait]
impl IndexBackend for MappingAwareIndex {
    async fn index_document(&self, _doc: &Document) -> DbResult<()> {
        Ok(())
    }

    async fn search(&self, _query: &Query) -> DbResult<SearchResult> {
        Ok(SearchResult::empty(1))
    }

    async fn delete_document(&self, _id: &DocumentId) -> DbResult<()> {
        Ok(())
    }

    async fn create_collection_index(&self, _collection: &str) -> DbResult<()> {
        Ok(())
    }

    async fn drop_collection_index(&self, _collection: &str) -> DbResult<()> {
        Ok(())
    }

    async fn index_document_in_collection(
        &self,
        _collection: &str,
        document: &Document,
        mapping: &msearchdb_core::collection::FieldMapping,
    ) -> DbResult<msearchdb_core::collection::FieldMapping> {
        use msearchdb_core::collection::MappedFieldType;
        use msearchdb_core::document::FieldValue;

        let mut updated = mapping.clone();
        for (name, value) in &document.fields {
            let mapped_type = match value {
                FieldValue::Text(_) => MappedFieldType::Text,
                FieldValue::Number(_) => MappedFieldType::Number,
                FieldValue::Boolean(_) => MappedFieldType::Boolean,
                FieldValue::Array(_) => MappedFieldType::Array,
                _ => MappedFieldType::Text,
            };
            updated.register_field(name.clone(), mapped_type)?;
        }
        Ok(updated)
    }

    async fn search_collection(
        &self,
        _collection: &str,
        _query: &Query,
    ) -> DbResult<SearchResult> {
        Ok(SearchResult::empty(1))
    }

    async fn delete_document_from_collection(
        &self,
        _collection: &str,
        _id: &DocumentId,
    ) -> DbResult<()> {
        Ok(())
    }

    async fn commit_collection_index(&self, _collection: &str) -> DbResult<()> {
        Ok(())
    }
}

/// Helper that builds test state with the mapping-aware index.
async fn make_schema_test_state() -> AppState {
    let config = NodeConfig::default();
    let storage: Arc<dyn StorageBackend> = Arc::new(MemStorage::new());
    let index: Arc<dyn IndexBackend> = Arc::new(MappingAwareIndex);

    let raft_node = RaftNode::new(&config, storage.clone(), index.clone())
        .await
        .unwrap();
    let raft_node = Arc::new(raft_node);

    let mut members = BTreeMap::new();
    members.insert(config.node_id.as_u64(), openraft::BasicNode::default());
    raft_node.initialize(members).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    AppState {
        raft_node,
        storage,
        index,
        connection_pool: Arc::new(ConnectionPool::new()),
        collections: Arc::new(RwLock::new(HashMap::new())),
        aliases: Arc::new(RwLock::new(HashMap::new())),
        api_key: None,
        metrics: Arc::new(msearchdb_node::metrics::Metrics::new()),
        local_node_id: config.node_id,
        read_coordinator: Arc::new(msearchdb_core::read_coordinator::ReadCoordinator::new(
            config.replication_factor,
        )),
    }
}

#[tokio::test]
async fn test_schema_conflict_returns_409() {
    let state = make_schema_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/schema_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Index a document with "price" as text
    let doc1 = json!({
        "id": "doc-text",
        "fields": {
            "title": "Widget",
            "price": "cheap"
        }
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/schema_test/docs")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&doc1).unwrap()))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Index a document with "price" as number — should conflict
    let doc2 = json!({
        "id": "doc-num",
        "fields": {
            "title": "Gadget",
            "price": 42.99
        }
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/schema_test/docs")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&doc2).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["type"], "schema_conflict");
    assert!(body["error"]["reason"].as_str().unwrap().contains("price"));
}

#[tokio::test]
async fn test_schema_evolution_new_fields_succeed() {
    let state = make_schema_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/evolve_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Index with field "title"
    let doc1 = json!({
        "id": "evo-1",
        "fields": { "title": "First" }
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/evolve_test/docs")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&doc1).unwrap()))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::CREATED);

    // Index with a NEW field "category" — schema evolution, should succeed
    let doc2 = json!({
        "id": "evo-2",
        "fields": { "title": "Second", "category": "widgets" }
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/evolve_test/docs")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&doc2).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["result"], "created");
}

// ---------------------------------------------------------------------------
// Alias CRUD tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_and_get_alias() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create a backing collection first
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/products_v1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Create alias
    let alias_body = json!({ "collections": ["products_v1"] });
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/_aliases/products")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&alias_body).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "products");
    assert_eq!(body["collections"][0], "products_v1");

    // Get alias
    let req = Request::builder()
        .uri("/_aliases/products")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "products");
    assert_eq!(body["collections"][0], "products_v1");
}

#[tokio::test]
async fn test_delete_alias() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create backing collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/temp_col")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Create alias
    let alias_body = json!({ "collections": ["temp_col"] });
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/_aliases/temp_alias")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&alias_body).unwrap()))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Delete alias
    let req = Request::builder()
        .method(Method::DELETE)
        .uri("/_aliases/temp_alias")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["acknowledged"], true);

    // Verify gone
    let req = Request::builder()
        .uri("/_aliases/temp_alias")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_list_aliases() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create two backing collections
    for name in &["col_x", "col_y"] {
        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/collections/{}", name))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let (status, _) = send(app.clone(), req).await;
        assert_eq!(status, StatusCode::OK);
    }

    // Create two aliases
    for (alias, col) in &[("alias_a", "col_x"), ("alias_b", "col_y")] {
        let body = json!({ "collections": [col] });
        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!("/_aliases/{}", alias))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let (status, _) = send(app.clone(), req).await;
        assert_eq!(status, StatusCode::OK);
    }

    // List
    let req = Request::builder()
        .uri("/_aliases")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

#[tokio::test]
async fn test_alias_not_found_returns_404() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .uri("/_aliases/nonexistent")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"]["reason"]
        .as_str()
        .unwrap()
        .contains("not found"));
}

#[tokio::test]
async fn test_alias_duplicate_returns_409() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create backing collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/dup_col")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Create alias
    let body = json!({ "collections": ["dup_col"] });
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/_aliases/dup_alias")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Try to create again — should conflict
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/_aliases/dup_alias")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_alias_with_missing_target_returns_404() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Try to create alias pointing to non-existent collection
    let body = json!({ "collections": ["no_such_collection"] });
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/_aliases/bad_alias")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"]["reason"]
        .as_str()
        .unwrap()
        .contains("not found"));
}

#[tokio::test]
async fn test_search_via_alias() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/alias_search_col")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Create alias pointing to that collection
    let alias_body = json!({ "collections": ["alias_search_col"] });
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/_aliases/search_alias")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&alias_body).unwrap()))
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // Search via alias (POST)
    let search_body = json!({
        "query": {"match": {"title": {"query": "hello"}}}
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/collections/search_alias/_search")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&search_body).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["hits"]["total"]["value"].is_number());

    // Search via alias (GET simple query)
    let req = Request::builder()
        .uri("/collections/search_alias/_search?q=hello")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["hits"].is_object());
}

// ---------------------------------------------------------------------------
// Per-collection settings tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_collection_with_custom_settings() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection with custom settings
    let create_body = json!({
        "settings": {
            "number_of_shards": 3,
            "replication_factor": 2,
            "default_analyzer": "whitespace"
        }
    });
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/settings_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&create_body).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "settings_test");
    assert_eq!(body["settings"]["number_of_shards"], 3);
    assert_eq!(body["settings"]["replication_factor"], 2);
    assert_eq!(body["settings"]["default_analyzer"], "whitespace");

    // GET the collection and verify settings are persisted
    let req = Request::builder()
        .uri("/collections/settings_test")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["settings"]["number_of_shards"], 3);
    assert_eq!(body["settings"]["replication_factor"], 2);
    assert_eq!(body["settings"]["default_analyzer"], "whitespace");
}

#[tokio::test]
async fn test_create_collection_default_settings() {
    let state = make_test_state().await;
    let app = msearchdb_node::build_router(state);

    // Create collection without settings — should get defaults
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/default_settings_test")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, body) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "default_settings_test");
    // Default settings should be present
    assert_eq!(body["settings"]["number_of_shards"], 1);
    assert_eq!(body["settings"]["replication_factor"], 1);
    assert_eq!(body["settings"]["default_analyzer"], "standard");
}
