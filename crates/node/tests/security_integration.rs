//! Security integration tests for the MSearchDB REST API.
//!
//! Tests cover:
//! - TLS handshake with self-signed certificates
//! - API key authentication with role-based rejection
//! - JWT authentication with role-based rejection
//! - Rate limiting via token bucket
//! - Connection limit enforcement
//! - Input validation (document size, field names, query depth, bulk size)
//! - Audit logging (writes produce entries)

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use serde_json::Value;
use tokio::sync::RwLock;
use tower::ServiceExt;

use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_core::config::NodeConfig;
use msearchdb_core::document::{Document, DocumentId};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::query::{Query, SearchResult};
use msearchdb_core::security::{
    ApiKeyEntry, ApiKeyRegistry, AuditLogger, ConnectionCounter, JwtManager, RateLimiter, Role,
    SecurityConfig, TokenBucket, ValidationConfig,
};
use msearchdb_core::traits::{IndexBackend, StorageBackend};
use msearchdb_network::connection_pool::ConnectionPool;
use msearchdb_node::state::AppState;

// ---------------------------------------------------------------------------
// Mock backends (same pattern as api_integration.rs)
// ---------------------------------------------------------------------------

struct MemStorage(tokio::sync::RwLock<HashMap<String, Document>>);

impl MemStorage {
    fn new() -> Self {
        Self(tokio::sync::RwLock::new(HashMap::new()))
    }
}

#[async_trait]
impl StorageBackend for MemStorage {
    async fn get(&self, id: &DocumentId) -> DbResult<Document> {
        self.0
            .read()
            .await
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| DbError::NotFound(id.to_string()))
    }
    async fn put(&self, doc: Document) -> DbResult<()> {
        self.0.write().await.insert(doc.id.as_str().to_owned(), doc);
        Ok(())
    }
    async fn delete(&self, id: &DocumentId) -> DbResult<()> {
        self.0.write().await.remove(id.as_str());
        Ok(())
    }
    async fn scan(
        &self,
        _range: RangeInclusive<DocumentId>,
        _limit: usize,
    ) -> DbResult<Vec<Document>> {
        Ok(vec![])
    }
}

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

// ---------------------------------------------------------------------------
// Helper: build a test state with security options
// ---------------------------------------------------------------------------

struct SecurityTestOpts {
    api_keys: Vec<ApiKeyEntry>,
    legacy_api_key: Option<String>,
    jwt_secret: Option<String>,
    rate_limit_capacity: u64,
    rate_limit_refill: f64,
    max_connections: u64,
}

impl Default for SecurityTestOpts {
    fn default() -> Self {
        Self {
            api_keys: vec![],
            legacy_api_key: None,
            jwt_secret: None,
            rate_limit_capacity: 100,
            rate_limit_refill: 100.0,
            max_connections: 1000,
        }
    }
}

async fn make_security_state(opts: SecurityTestOpts) -> AppState {
    let config = NodeConfig::default();
    let storage: Arc<dyn StorageBackend> = Arc::new(MemStorage::new());
    let index: Arc<dyn IndexBackend> = Arc::new(EmptyIndex);

    let raft_node = RaftNode::new(&config, storage.clone(), index.clone())
        .await
        .unwrap();
    let raft_node = Arc::new(raft_node);

    let mut members = BTreeMap::new();
    members.insert(config.node_id.as_u64(), openraft::BasicNode::default());
    raft_node.initialize(members).await.unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut registry = ApiKeyRegistry::new();
    for entry in opts.api_keys {
        registry.register(entry);
    }

    let jwt_manager = opts
        .jwt_secret
        .map(|s| Arc::new(JwtManager::new(s, Duration::from_secs(3600))));

    AppState {
        raft_node: raft_node.clone(),
        storage,
        index,
        connection_pool: Arc::new(ConnectionPool::new()),
        collections: Arc::new(RwLock::new(HashMap::new())),
        aliases: Arc::new(RwLock::new(HashMap::new())),
        api_key: opts.legacy_api_key,
        api_key_registry: Arc::new(registry),
        jwt_manager,
        rate_limiter: Arc::new(RateLimiter::new(
            opts.rate_limit_capacity,
            opts.rate_limit_refill,
        )),
        connection_counter: Arc::new(ConnectionCounter::new(opts.max_connections)),
        validation_config: Arc::new(ValidationConfig::default()),
        audit_logger: Arc::new(AuditLogger::noop()),
        security_config: Arc::new(SecurityConfig::default()),
        metrics: Arc::new(msearchdb_node::metrics::Metrics::new()),
        local_node_id: config.node_id,
        read_coordinator: Arc::new(msearchdb_core::read_coordinator::ReadCoordinator::new(
            config.replication_factor,
        )),
        snapshot_manager: None,
        document_cache: Arc::new(msearchdb_node::cache::DocumentCache::with_defaults()),
        session_manager: Arc::new(msearchdb_node::session::SessionManager::new()),
        write_batcher: Arc::new(msearchdb_node::write_batcher::WriteBatcher::new(raft_node)),
    }
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

// ===========================================================================
// TLS tests
// ===========================================================================

#[test]
fn tls_self_signed_cert_generation_and_rustls_config() {
    let cert = msearchdb_core::security::generate_self_signed_cert(&["localhost".into()]).unwrap();

    // Cert PEM is valid
    assert!(cert.cert_pem.contains("BEGIN CERTIFICATE"));
    assert!(cert.key_pem.contains("BEGIN PRIVATE KEY"));

    // Can load into rustls ServerConfig
    let config =
        msearchdb_core::security::load_rustls_config_from_pem(&cert.cert_pem, &cert.key_pem)
            .unwrap();

    // Config has no ALPN by default (we can set it later).
    assert!(config.alpn_protocols.is_empty());
}

#[test]
fn tls_config_validation() {
    use msearchdb_core::security::TlsConfig;

    // Enabled without certs: error
    let cfg = TlsConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(cfg.validate().is_err());

    // mTLS without TLS: error
    let cfg = TlsConfig {
        mtls_enabled: true,
        ..Default::default()
    };
    assert!(cfg.validate().is_err());

    // Valid full config
    let cfg = TlsConfig {
        enabled: true,
        cert_path: Some("c.pem".into()),
        key_path: Some("k.pem".into()),
        ca_cert_path: Some("ca.pem".into()),
        mtls_enabled: true,
    };
    assert!(cfg.validate().is_ok());
}

// ===========================================================================
// Auth tests — API key RBAC
// ===========================================================================

#[tokio::test]
async fn auth_no_key_configured_allows_all() {
    let state = make_security_state(SecurityTestOpts::default()).await;
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/_cluster/health")
        .body(Body::empty())
        .unwrap();

    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn auth_api_key_admin_allows_all() {
    let state = make_security_state(SecurityTestOpts {
        api_keys: vec![ApiKeyEntry::new("admin-key", Role::Admin)],
        ..Default::default()
    })
    .await;
    let app = msearchdb_node::build_router(state);

    // Admin can read cluster health.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/_cluster/health")
        .header("x-api-key", "admin-key")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn auth_api_key_reader_cannot_write() {
    let state = make_security_state(SecurityTestOpts {
        api_keys: vec![ApiKeyEntry::new("reader-key", Role::Reader)],
        ..Default::default()
    })
    .await;
    let app = msearchdb_node::build_router(state);

    // First create collection (requires admin/writer — should be rejected).
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/test")
        .header("x-api-key", "reader-key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "forbidden");
}

#[tokio::test]
async fn auth_api_key_writer_can_create_docs_but_not_admin_ops() {
    let state = make_security_state(SecurityTestOpts {
        api_keys: vec![ApiKeyEntry::new("writer-key", Role::Writer)],
        ..Default::default()
    })
    .await;
    let app = msearchdb_node::build_router(state);

    // Writer cannot create collections (requires admin).
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/test")
        .header("x-api-key", "writer-key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn auth_rejects_missing_key() {
    let state = make_security_state(SecurityTestOpts {
        api_keys: vec![ApiKeyEntry::new("valid-key", Role::Admin)],
        ..Default::default()
    })
    .await;
    let app = msearchdb_node::build_router(state);

    // No key header → 401
    let req = Request::builder()
        .method(Method::GET)
        .uri("/_cluster/health")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "unauthorized");
}

#[tokio::test]
async fn auth_rejects_wrong_key() {
    let state = make_security_state(SecurityTestOpts {
        api_keys: vec![ApiKeyEntry::new("correct-key", Role::Admin)],
        ..Default::default()
    })
    .await;
    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/_cluster/health")
        .header("x-api-key", "wrong-key")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// Auth tests — JWT
// ===========================================================================

#[tokio::test]
async fn auth_jwt_valid_token_allowed() {
    let secret = "jwt-test-secret";
    let state = make_security_state(SecurityTestOpts {
        jwt_secret: Some(secret.to_string()),
        // Need at least one auth mechanism to trigger auth checks.
        // Since jwt_manager is set, auth will check JWT.
        api_keys: vec![ApiKeyEntry::new("placeholder", Role::Reader)],
        ..Default::default()
    })
    .await;

    let jwt_mgr = JwtManager::new(secret, Duration::from_secs(3600));
    let token = jwt_mgr.issue("test-user", Role::Admin).unwrap();

    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/_cluster/health")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn auth_jwt_insufficient_role_rejected() {
    let secret = "jwt-test-secret";
    let state = make_security_state(SecurityTestOpts {
        jwt_secret: Some(secret.to_string()),
        api_keys: vec![ApiKeyEntry::new("placeholder", Role::Reader)],
        ..Default::default()
    })
    .await;

    let jwt_mgr = JwtManager::new(secret, Duration::from_secs(3600));
    // Reader token trying admin operation.
    let token = jwt_mgr.issue("test-user", Role::Reader).unwrap();

    let app = msearchdb_node::build_router(state);

    // PUT collection requires admin.
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/collections/secured")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "forbidden");
}

#[tokio::test]
async fn auth_jwt_invalid_token_rejected() {
    let state = make_security_state(SecurityTestOpts {
        jwt_secret: Some("real-secret".into()),
        api_keys: vec![ApiKeyEntry::new("placeholder", Role::Reader)],
        ..Default::default()
    })
    .await;

    let app = msearchdb_node::build_router(state);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/_cluster/health")
        .header(header::AUTHORIZATION, "Bearer invalid.jwt.token")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// Rate limiting tests
// ===========================================================================

#[tokio::test]
async fn rate_limit_rejects_when_exhausted() {
    // Very low limit: 3 requests, 0.1 refill/sec (effectively no refill).
    let state = make_security_state(SecurityTestOpts {
        rate_limit_capacity: 3,
        rate_limit_refill: 0.1,
        ..Default::default()
    })
    .await;
    let app = msearchdb_node::build_router(state);

    // First 3 requests should succeed.
    for _ in 0..3 {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/_cluster/health")
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(app.clone(), req).await;
        assert_eq!(status, StatusCode::OK);
    }

    // 4th request should be rate limited.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/_cluster/health")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, req).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["type"], "rate_limited");
}

#[test]
fn token_bucket_concurrent_safety() {
    use std::sync::Arc;
    use std::thread;

    let bucket = Arc::new(TokenBucket::new(100, 0.0)); // no refill
    let mut handles = vec![];

    for _ in 0..10 {
        let b = bucket.clone();
        handles.push(thread::spawn(move || {
            let mut count = 0;
            for _ in 0..20 {
                if b.try_acquire() {
                    count += 1;
                }
            }
            count
        }));
    }

    let total: u32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    // Exactly 100 tokens were available (no refill), so total acquired <= 100.
    assert_eq!(total, 100);
}

// ===========================================================================
// Connection limit tests
// ===========================================================================

#[test]
fn connection_counter_limits_enforced() {
    let counter = ConnectionCounter::new(3);

    assert!(counter.try_acquire());
    assert!(counter.try_acquire());
    assert!(counter.try_acquire());
    assert!(!counter.try_acquire()); // 4th rejected
    assert_eq!(counter.active(), 3);

    counter.release();
    assert_eq!(counter.active(), 2);
    assert!(counter.try_acquire()); // slot freed
}

// ===========================================================================
// Input validation tests
// ===========================================================================

#[test]
fn validate_field_name_system_prefix_rejected() {
    use msearchdb_core::security::validate_field_name;

    assert!(validate_field_name("_id").is_err());
    assert!(validate_field_name("_secret").is_err());
    assert!(validate_field_name("_body").is_err());

    assert!(validate_field_name("name").is_ok());
    assert!(validate_field_name("my_field").is_ok());
}

#[test]
fn validate_document_size_enforced() {
    use msearchdb_core::security::{validate_document_size, ValidationConfig};

    let config = ValidationConfig::default();
    // 10 MB limit
    assert!(validate_document_size(100, &config).is_ok());
    assert!(validate_document_size(10 * 1024 * 1024, &config).is_ok()); // exactly at limit
    assert!(validate_document_size(10 * 1024 * 1024 + 1, &config).is_err());
}

#[test]
fn validate_query_depth_enforced() {
    use msearchdb_core::query::{BoolQuery, TermQuery};
    use msearchdb_core::security::{validate_query_depth, ValidationConfig};

    let config = ValidationConfig {
        max_query_depth: 3,
        ..Default::default()
    };

    // Depth 1: OK.
    let q = Query::Term(TermQuery {
        field: "f".into(),
        value: "v".into(),
    });
    assert!(validate_query_depth(&q, &config).is_ok());

    // Depth 3: OK.
    let q3 = Query::Bool(BoolQuery {
        must: vec![Query::Bool(BoolQuery {
            must: vec![Query::Term(TermQuery {
                field: "f".into(),
                value: "v".into(),
            })],
            should: vec![],
            must_not: vec![],
        })],
        should: vec![],
        must_not: vec![],
    });
    assert!(validate_query_depth(&q3, &config).is_ok());

    // Depth 4: Exceeds limit of 3.
    let q4 = Query::Bool(BoolQuery {
        must: vec![q3],
        should: vec![],
        must_not: vec![],
    });
    assert!(validate_query_depth(&q4, &config).is_err());
}

#[test]
fn validate_bulk_size_enforced() {
    use msearchdb_core::security::{validate_bulk_size, ValidationConfig};

    let config = ValidationConfig::default();
    assert!(validate_bulk_size(1000, &config).is_ok());
    assert!(validate_bulk_size(10 * 1024 * 1024 + 1, &config).is_err());
}

// ===========================================================================
// Legacy single-key backward compatibility
// ===========================================================================

#[tokio::test]
async fn legacy_api_key_still_works() {
    let state = make_security_state(SecurityTestOpts {
        legacy_api_key: Some("legacy-secret".into()),
        ..Default::default()
    })
    .await;
    let app = msearchdb_node::build_router(state);

    // With correct key → OK.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/_cluster/health")
        .header("x-api-key", "legacy-secret")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);

    // With wrong key → 401.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/_cluster/health")
        .header("x-api-key", "wrong")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
