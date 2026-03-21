//! axum middleware for the MSearchDB REST API.
//!
//! This module provides six middleware layers composed via [`tower`]:
//!
//! 1. **Trace Context** — propagates W3C `traceparent` headers for distributed
//!    tracing across service boundaries.
//! 2. **Request ID** — assigns a UUID v4 to each request and sets the
//!    `X-Request-Id` header on both request and response.
//! 3. **Tracing** — structured log line for every request with method, path,
//!    status code, and latency.
//! 4. **Auth** — simple API-key authentication via the `X-API-Key` header.
//!    Skipped entirely when no key is configured.
//! 5. **Compression** — gzip response bodies larger than 1 KB via
//!    [`tower_http::compression`].
//! 6. **Timeout** — 30-second per-request deadline via [`tower::timeout`].
//!
//! # tower Middleware Composition
//!
//! All middleware is composed using tower's `Layer` / `Service` abstractions.
//! Layers wrap an inner service and return a new service, forming a pipeline:
//!
//! ```text
//! Timeout → Compression → TraceContext → Tracing → RequestId → Auth → Router
//! ```

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// RequestId middleware
// ---------------------------------------------------------------------------

/// Middleware that assigns a `X-Request-Id` header to each request/response.
///
/// If the client sends a `X-Request-Id` header, it is preserved.  Otherwise
/// a new UUID v4 is generated.
pub async fn request_id_middleware(mut request: Request<Body>, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Inject into request headers so downstream handlers can read it.
    request.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );

    let mut response = next.run(request).await;

    // Copy to response headers so the client can correlate.
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );

    response
}

// ---------------------------------------------------------------------------
// Trace context propagation middleware
// ---------------------------------------------------------------------------

/// Middleware that propagates W3C `traceparent` context from incoming requests.
///
/// If the request includes a `traceparent` header, the trace context is
/// extracted and set as the parent span.  Otherwise a new trace is started.
/// The response includes a `traceparent` header with the current span context
/// so that clients can correlate downstream calls.
pub async fn trace_context_middleware(request: Request<Body>, next: Next) -> Response {
    // Extract traceparent from request headers if present.
    let traceparent = request
        .headers()
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    let method = request.method().to_string();
    let path = request.uri().path().to_owned();

    // Create a tracing span with OTel-compatible fields.
    let span = tracing::info_span!(
        "http.request",
        otel.kind = "server",
        http.method = %method,
        http.route = %path,
        http.traceparent = traceparent.as_deref().unwrap_or("none"),
    );

    let _guard = span.enter();
    let mut response = next.run(request).await;

    // Propagate trace context in response (simplified — real W3C propagation
    // requires extracting the current OTel SpanContext, but this ensures the
    // header round-trips for clients that pass it in).
    if let Some(tp) = traceparent {
        if let Ok(val) = HeaderValue::from_str(&tp) {
            response.headers_mut().insert("traceparent", val);
        }
    }

    response
}

// ---------------------------------------------------------------------------
// Tracing middleware
// ---------------------------------------------------------------------------

/// Normalize a URL path to replace dynamic segments with placeholders.
///
/// This avoids high-cardinality Prometheus label values by collapsing
/// concrete resource identifiers into fixed tokens:
///
/// - `/collections/products/docs/doc1` → `/collections/:name/docs/:id`
/// - `/collections/products/_search`   → `/collections/:name/_search`
fn normalize_path(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    let mut result: Vec<&str> = Vec::with_capacity(segments.len());
    let mut i = 0;
    while i < segments.len() {
        let seg = segments[i];
        match seg {
            "collections" => {
                result.push(seg);
                // Next segment is the collection name — replace with :name.
                if i + 1 < segments.len() {
                    i += 1;
                    result.push(":name");
                }
            }
            "docs" => {
                result.push(seg);
                // Next segment is the document id — replace with :id.
                if i + 1 < segments.len() && !segments[i + 1].starts_with('_') {
                    i += 1;
                    result.push(":id");
                }
            }
            "_aliases" => {
                result.push(seg);
                // Next segment is the alias name — replace with :name.
                if i + 1 < segments.len() {
                    i += 1;
                    result.push(":name");
                }
            }
            "_nodes" => {
                result.push(seg);
                // Next segment is the node id — replace with :id.
                if i + 1 < segments.len() {
                    i += 1;
                    result.push(":id");
                }
            }
            "_snapshot" => {
                result.push(seg);
                // Next segment is the snapshot id — replace with :id.
                if i + 1 < segments.len() && !segments[i + 1].starts_with('_') {
                    i += 1;
                    result.push(":id");
                }
            }
            _ => {
                result.push(seg);
            }
        }
        i += 1;
    }
    result.join("/")
}

/// Middleware that logs method, path, status code, and latency for every request.
///
/// Uses structured [`tracing`] fields so log aggregation systems (ELK, Loki)
/// can query individual fields.  Also updates Prometheus HTTP metrics
/// (`http_requests_total` and `http_request_duration_seconds`).
pub async fn tracing_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let start = Instant::now();

    let response = next.run(request).await;

    let latency = start.elapsed();
    let latency_ms = latency.as_millis();
    let latency_secs = latency.as_secs_f64();
    let status = response.status().as_u16();

    let method_str = method.to_string();
    let normalized = normalize_path(&path);
    let status_str = status.to_string();

    state
        .metrics
        .http_requests_total
        .with_label_values(&[&method_str, &normalized, &status_str])
        .inc();
    state
        .metrics
        .http_request_duration_seconds
        .with_label_values(&[&method_str, &normalized])
        .observe(latency_secs);

    tracing::info!(
        http.method = %method,
        http.path = %path,
        http.status = status,
        http.latency_ms = latency_ms as u64,
        "request completed"
    );

    response
}

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

/// Determine the minimum [`Role`] required for the given request.
///
/// - **Admin**: cluster management, node join, restore, collection create/delete.
/// - **Writer**: document index/update/delete, bulk, refresh.
/// - **Reader**: search, get document, cluster health, stats, metrics.
fn required_role(method: &http::Method, path: &str) -> msearchdb_core::security::Role {
    use msearchdb_core::security::Role;

    // Admin-only operations
    if path.starts_with("/_nodes/") && path.ends_with("/_join") {
        return Role::Admin;
    }
    if path == "/_restore" {
        return Role::Admin;
    }
    if path == "/_snapshot" && *method == http::Method::POST {
        return Role::Admin;
    }

    // Collection management requires admin
    if path.starts_with("/collections/")
        && !path.contains("/docs")
        && !path.contains("/_search")
        && !path.contains("/_refresh")
        && (*method == http::Method::PUT || *method == http::Method::DELETE)
    {
        return Role::Admin;
    }

    // Write operations
    if *method == http::Method::POST
        || *method == http::Method::PUT
        || *method == http::Method::DELETE
    {
        // Search POST is a read
        if path.contains("/_search") {
            return Role::Reader;
        }
        return Role::Writer;
    }

    // Everything else (GET, HEAD, OPTIONS) is read
    Role::Reader
}

/// Middleware that authenticates via API key or JWT token with role-based access.
///
/// Authentication is checked in this order:
/// 1. `X-API-Key` header → looked up in the API key registry.
/// 2. `Authorization: Bearer <token>` header → verified as JWT.
/// 3. Legacy single-key mode (`state.api_key`) for backward compatibility.
///
/// If no authentication is configured at all, all requests pass through.
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    use msearchdb_core::security::Role;

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let role_needed = required_role(&method, &path);

    // Check if any auth is configured at all.
    let has_registry = !state.api_key_registry.is_empty();
    let has_jwt = state.jwt_manager.is_some();
    let has_legacy_key = state.api_key.is_some();

    if !has_registry && !has_jwt && !has_legacy_key {
        // No auth configured — allow all.
        return next.run(request).await;
    }

    // Extract credentials from headers.
    let api_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    let bearer_token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(|t| t.to_owned()));

    // 1. Try API key registry (role-based).
    if let Some(ref key) = api_key {
        if let Some(entry) = state.api_key_registry.get(key) {
            if entry.has_permission(role_needed) {
                return next.run(request).await;
            }
            return json_error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "insufficient permissions",
            );
        }
    }

    // 2. Try JWT.
    if let Some(ref token) = bearer_token {
        if let Some(ref jwt_mgr) = state.jwt_manager {
            match jwt_mgr.verify(token) {
                Ok(claims) => {
                    let token_role: Result<Role, _> = claims.role.parse();
                    match token_role {
                        Ok(r) if r.level() >= role_needed.level() => {
                            return next.run(request).await;
                        }
                        Ok(_) => {
                            return json_error(
                                StatusCode::FORBIDDEN,
                                "forbidden",
                                "insufficient permissions",
                            );
                        }
                        Err(_) => {
                            return json_error(
                                StatusCode::UNAUTHORIZED,
                                "unauthorized",
                                "invalid role in JWT",
                            );
                        }
                    }
                }
                Err(_) => {
                    return json_error(
                        StatusCode::UNAUTHORIZED,
                        "unauthorized",
                        "invalid or expired JWT token",
                    );
                }
            }
        }
    }

    // 3. Legacy single-key fallback (all-or-nothing, admin role).
    if let Some(ref expected) = state.api_key {
        if let Some(ref provided) = api_key {
            if provided == expected {
                return next.run(request).await;
            }
        }
    }

    json_error(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "invalid or missing API key",
    )
}

// ---------------------------------------------------------------------------
// Rate limiting middleware
// ---------------------------------------------------------------------------

/// Middleware that enforces per-key rate limiting using a token bucket.
///
/// The rate limit key is the API key from the `X-API-Key` header, or
/// `"anonymous"` if no key is provided.  When the rate limit is exceeded,
/// a `429 Too Many Requests` response is returned.
pub async fn rate_limit_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("anonymous")
        .to_owned();

    if !state.rate_limiter.try_acquire(&key).await {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "rate limit exceeded — try again later",
        );
    }

    next.run(request).await
}

// ---------------------------------------------------------------------------
// Connection limit middleware
// ---------------------------------------------------------------------------

/// Middleware that enforces a maximum connection limit per node.
///
/// Returns `503 Service Unavailable` when the limit is reached.
pub async fn connection_limit_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !state.connection_counter.try_acquire() {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "connection_limit",
            "maximum connection limit reached",
        );
    }

    let response = next.run(request).await;

    state.connection_counter.release();

    response
}

/// Helper to build a JSON error response.
fn json_error(status: StatusCode, error_type: &str, reason: &str) -> Response {
    let body = json!({
        "error": {
            "type": error_type,
            "reason": reason
        },
        "status": status.as_u16()
    });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&body).unwrap(),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Timeout + Compression helpers
// ---------------------------------------------------------------------------

/// Default request timeout duration (30 seconds).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Minimum response body size for compression (1 KB).
pub const COMPRESSION_MIN_SIZE: u16 = 1024;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use http::StatusCode;
    use tower::ServiceExt;

    /// Helper to build a test app and send a request.
    async fn send_request(app: Router, request: Request<Body>) -> Response {
        app.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn request_id_middleware_generates_id() {
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn(request_id_middleware));

        let req = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let resp = send_request(app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let request_id = resp.headers().get("x-request-id");
        assert!(request_id.is_some());
        let id_str = request_id.unwrap().to_str().unwrap();
        // Should be a valid UUID
        assert_eq!(id_str.len(), 36);
    }

    #[tokio::test]
    async fn request_id_middleware_preserves_existing() {
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn(request_id_middleware));

        let req = Request::builder()
            .uri("/test")
            .header("x-request-id", "my-custom-id")
            .body(Body::empty())
            .unwrap();

        let resp = send_request(app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let request_id = resp
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(request_id, "my-custom-id");
    }

    #[tokio::test]
    async fn tracing_middleware_does_not_alter_response() {
        let state = make_test_state(None).await;
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                tracing_middleware,
            ))
            .with_state(state);

        let req = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let resp = send_request(app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn trace_context_middleware_round_trips_traceparent() {
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn(trace_context_middleware));

        let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let req = Request::builder()
            .uri("/test")
            .header("traceparent", traceparent)
            .body(Body::empty())
            .unwrap();

        let resp = send_request(app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let tp = resp
            .headers()
            .get("traceparent")
            .expect("response should contain traceparent header")
            .to_str()
            .unwrap();
        assert_eq!(tp, traceparent);
    }

    #[tokio::test]
    async fn trace_context_middleware_no_traceparent_in_response_when_absent() {
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn(trace_context_middleware));

        let req = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let resp = send_request(app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("traceparent").is_none());
    }

    #[test]
    fn normalize_path_replaces_dynamic_segments() {
        assert_eq!(
            normalize_path("/collections/products/docs/doc1"),
            "/collections/:name/docs/:id"
        );
        assert_eq!(
            normalize_path("/collections/products/_search"),
            "/collections/:name/_search"
        );
        assert_eq!(
            normalize_path("/collections/products/docs/_bulk"),
            "/collections/:name/docs/_bulk"
        );
        assert_eq!(normalize_path("/_aliases/my_alias"), "/_aliases/:name");
        assert_eq!(normalize_path("/_nodes/2/_join"), "/_nodes/:id/_join");
        assert_eq!(normalize_path("/_snapshot/abc123"), "/_snapshot/:id");
        assert_eq!(normalize_path("/_cluster/health"), "/_cluster/health");
        assert_eq!(normalize_path("/_stats"), "/_stats");
    }

    async fn make_test_state(api_key: Option<String>) -> AppState {
        use async_trait::async_trait;
        use msearchdb_core::document::{Document, DocumentId};
        use msearchdb_core::error::{DbError, DbResult};
        use msearchdb_core::query::{Query, SearchResult};
        use msearchdb_core::security::{
            ApiKeyRegistry, AuditLogger, ConnectionCounter, RateLimiter, SecurityConfig,
            ValidationConfig,
        };
        use msearchdb_core::traits::{IndexBackend, StorageBackend};
        use msearchdb_network::connection_pool::ConnectionPool;
        use std::collections::HashMap;
        use std::ops::RangeInclusive;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        struct MockStorage;
        #[async_trait]
        impl StorageBackend for MockStorage {
            async fn get(&self, id: &DocumentId) -> DbResult<Document> {
                Err(DbError::NotFound(id.to_string()))
            }
            async fn put(&self, _doc: Document) -> DbResult<()> {
                Ok(())
            }
            async fn delete(&self, _id: &DocumentId) -> DbResult<()> {
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

        struct MockIndex;
        #[async_trait]
        impl IndexBackend for MockIndex {
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

        let storage: Arc<dyn StorageBackend> = Arc::new(MockStorage);
        let index: Arc<dyn IndexBackend> = Arc::new(MockIndex);

        let config = msearchdb_core::config::NodeConfig::default();
        let raft_node =
            msearchdb_consensus::raft_node::RaftNode::new(&config, storage.clone(), index.clone())
                .await
                .unwrap();

        let raft_node = Arc::new(raft_node);

        AppState {
            raft_node: raft_node.clone(),
            storage,
            index,
            connection_pool: Arc::new(ConnectionPool::new()),
            collections: Arc::new(RwLock::new(HashMap::new())),
            aliases: Arc::new(RwLock::new(HashMap::new())),
            api_key,
            api_key_registry: Arc::new(ApiKeyRegistry::new()),
            jwt_manager: None,
            rate_limiter: Arc::new(RateLimiter::new(100, 10.0)),
            connection_counter: Arc::new(ConnectionCounter::new(1000)),
            validation_config: Arc::new(ValidationConfig::default()),
            audit_logger: Arc::new(AuditLogger::noop()),
            security_config: Arc::new(SecurityConfig::default()),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            local_node_id: config.node_id,
            read_coordinator: Arc::new(msearchdb_core::read_coordinator::ReadCoordinator::new(
                config.replication_factor,
            )),
            snapshot_manager: None,
            document_cache: Arc::new(crate::cache::DocumentCache::with_defaults()),
            session_manager: Arc::new(crate::session::SessionManager::new()),
            write_batcher: Arc::new(crate::write_batcher::WriteBatcher::new(raft_node)),
        }
    }

    #[tokio::test]
    async fn auth_middleware_passes_when_no_key_configured() {
        let state = make_test_state(None).await;
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state);

        let req = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let resp = send_request(app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_middleware_rejects_missing_key() {
        let state = make_test_state(Some("secret-key".into())).await;
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state);

        let req = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let resp = send_request(app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_middleware_rejects_wrong_key() {
        let state = make_test_state(Some("secret-key".into())).await;
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state);

        let req = Request::builder()
            .uri("/test")
            .header("x-api-key", "wrong-key")
            .body(Body::empty())
            .unwrap();

        let resp = send_request(app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_middleware_passes_correct_key() {
        let state = make_test_state(Some("secret-key".into())).await;
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state);

        let req = Request::builder()
            .uri("/test")
            .header("x-api-key", "secret-key")
            .body(Body::empty())
            .unwrap();

        let resp = send_request(app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
