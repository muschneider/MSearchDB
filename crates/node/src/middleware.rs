//! axum middleware for the MSearchDB REST API.
//!
//! This module provides five middleware layers composed via [`tower`]:
//!
//! 1. **Request ID** — assigns a UUID v4 to each request and sets the
//!    `X-Request-Id` header on both request and response.
//! 2. **Tracing** — structured log line for every request with method, path,
//!    status code, and latency.
//! 3. **Auth** — simple API-key authentication via the `X-API-Key` header.
//!    Skipped entirely when no key is configured.
//! 4. **Compression** — gzip response bodies larger than 1 KB via
//!    [`tower_http::compression`].
//! 5. **Timeout** — 30-second per-request deadline via [`tower::timeout`].
//!
//! # tower Middleware Composition
//!
//! All middleware is composed using tower's `Layer` / `Service` abstractions.
//! Layers wrap an inner service and return a new service, forming a pipeline:
//!
//! ```text
//! Timeout → Compression → Tracing → RequestId → Auth → Router
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
// Tracing middleware
// ---------------------------------------------------------------------------

/// Middleware that logs method, path, status code, and latency for every request.
///
/// Uses structured [`tracing`] fields so log aggregation systems (ELK, Loki)
/// can query individual fields.
pub async fn tracing_middleware(request: Request<Body>, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let start = Instant::now();

    let response = next.run(request).await;

    let latency_ms = start.elapsed().as_millis();
    let status = response.status().as_u16();

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

/// Middleware that checks the `X-API-Key` header against the configured key.
///
/// If no API key is configured in [`AppState`], all requests pass through.
/// If a key is configured but the request header is missing or wrong, a
/// `401 Unauthorized` JSON response is returned.
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // If no API key is configured, skip auth entirely.
    let expected_key = match &state.api_key {
        Some(key) => key,
        None => return next.run(request).await,
    };

    let provided_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok());

    match provided_key {
        Some(key) if key == expected_key => next.run(request).await,
        _ => {
            let body = json!({
                "error": {
                    "type": "unauthorized",
                    "reason": "invalid or missing API key"
                },
                "status": 401
            });
            (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::to_string(&body).unwrap(),
            )
                .into_response()
        }
    }
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
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn(tracing_middleware));

        let req = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let resp = send_request(app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    async fn make_test_state(api_key: Option<String>) -> AppState {
        use async_trait::async_trait;
        use msearchdb_core::document::{Document, DocumentId};
        use msearchdb_core::error::{DbError, DbResult};
        use msearchdb_core::query::{Query, SearchResult};
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

        AppState {
            raft_node: Arc::new(raft_node),
            storage,
            index,
            connection_pool: Arc::new(ConnectionPool::new()),
            collections: Arc::new(RwLock::new(HashMap::new())),
            api_key,
            metrics: Arc::new(crate::metrics::Metrics::new()),
            local_node_id: config.node_id,
            read_coordinator: Arc::new(
                msearchdb_core::read_coordinator::ReadCoordinator::new(
                    config.replication_factor,
                ),
            ),
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
