//! # msearchdb-node
//!
//! HTTP REST API server for MSearchDB, built with [axum](https://docs.rs/axum).
//!
//! This crate provides the external-facing REST API that clients (Python SDK,
//! curl, etc.) use to interact with MSearchDB.  It composes the domain-layer
//! crates (`core`, `storage`, `index`, `consensus`, `network`) into a
//! production-ready HTTP server with middleware, routing, and JSON DTOs.
//!
//! ## Architecture
//!
//! ```text
//!  Client (Python SDK / curl)
//!    │
//!    │  HTTP/REST
//!    ▼
//! ┌──────────────────────────────────────────────────┐
//! │  axum Router                                      │
//! │  ├─ Timeout layer (30 s)                          │
//! │  ├─ Compression layer (gzip > 1 KB)               │
//! │  ├─ Tracing middleware (method, path, latency)     │
//! │  ├─ RequestId middleware (X-Request-Id)            │
//! │  └─ Auth middleware (X-API-Key)                    │
//! │                                                    │
//! │  Routes:                                           │
//! │    /collections/*        → collection handlers     │
//! │    /collections/*/docs/* → document handlers       │
//! │    /collections/*/_search → search handlers        │
//! │    /_cluster/*           → cluster handlers         │
//! │    /_stats, /_refresh    → admin handlers           │
//! └──────────────────────────────────────────────────┘
//!    │             │              │
//!    │ writes      │ reads        │ reads
//!    ▼             ▼              ▼
//!  RaftNode    StorageBackend  IndexBackend
//! ```
//!
//! ## Key Design Choices
//!
//! - **`State<AppState>`** for compile-time typed access to backend services.
//! - **`Extension<T>`** for per-request values injected by middleware.
//! - **tower middleware composition** for cross-cutting concerns.
//! - **`From` impls** between API DTOs and domain types for clean conversion.

pub mod admin_tui;
pub mod cache;
pub mod cluster_manager;
pub mod dto;
pub mod errors;
pub mod handlers;
pub mod metrics;
pub mod middleware;
pub mod observability;
pub mod session;
pub mod snapshot_manager;
pub mod state;
pub mod write_batcher;

use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::timeout::TimeoutLayer;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Router construction
// ---------------------------------------------------------------------------

/// Build the complete axum [`Router`] with all routes and middleware.
///
/// # Arguments
///
/// * `state` — shared application state containing backend services.
///
/// # Middleware Stack
///
/// Applied outermost-first (request flows top → bottom):
///
/// 1. **Timeout**: 30-second per-request deadline.
/// 2. **Compression**: gzip responses (via `Accept-Encoding: gzip`).
/// 3. **Tracing**: structured log of method, path, status, latency.
/// 4. **RequestId**: assigns/preserves `X-Request-Id` header.
/// 5. **Auth**: optional API key check via `X-API-Key`.
pub fn build_router(state: AppState) -> Router {
    let collection_routes = Router::new()
        // Collection CRUD
        .route("/collections", get(handlers::collections::list_collections))
        .route(
            "/collections/:name",
            put(handlers::collections::create_collection)
                .delete(handlers::collections::delete_collection)
                .get(handlers::collections::get_collection),
        )
        // Document CRUD
        .route(
            "/collections/:name/docs",
            post(handlers::documents::index_document),
        )
        .route(
            "/collections/:name/docs/_bulk",
            post(handlers::bulk::bulk_index),
        )
        .route(
            "/collections/:name/docs/:id",
            put(handlers::documents::upsert_document)
                .get(handlers::documents::get_document)
                .delete(handlers::documents::delete_document),
        )
        // Search
        .route(
            "/collections/:name/_search",
            post(handlers::search::search_documents).get(handlers::search::simple_search),
        )
        // Refresh
        .route(
            "/collections/:name/_refresh",
            post(handlers::admin::refresh_collection),
        );

    let alias_routes = Router::new()
        .route("/_aliases", get(handlers::aliases::list_aliases))
        .route(
            "/_aliases/:name",
            put(handlers::aliases::create_alias)
                .get(handlers::aliases::get_alias)
                .delete(handlers::aliases::delete_alias),
        );

    let cluster_routes = Router::new()
        .route("/_cluster/health", get(handlers::cluster::cluster_health))
        .route("/_cluster/state", get(handlers::cluster::cluster_state))
        .route("/_nodes", get(handlers::cluster::list_nodes))
        .route("/_nodes/:id/_join", post(handlers::cluster::join_node));

    let snapshot_routes = Router::new()
        .route(
            "/_snapshot",
            post(handlers::snapshot::create_snapshot).get(handlers::snapshot::list_snapshots),
        )
        .route("/_snapshot/:id", get(handlers::snapshot::download_snapshot))
        .route("/_restore", post(handlers::snapshot::restore_snapshot));

    let admin_routes = Router::new()
        .route("/_stats", get(handlers::admin::get_stats))
        .route("/metrics", get(metrics_handler));

    // Merge all route groups
    let app = Router::new()
        .merge(collection_routes)
        .merge(alias_routes)
        .merge(cluster_routes)
        .merge(snapshot_routes)
        .merge(admin_routes)
        .with_state(state.clone());

    // Apply middleware layers (outermost first — request flows top → bottom)
    //
    // 1. Timeout: 30 s deadline.
    // 2. Compression: gzip for responses > 1 KB.
    // 3. Connection limit: enforce max concurrent connections.
    // 4. Rate limit: per-key token bucket.
    // 5. Auth: API key / JWT with RBAC.
    // 6. Tracing: method, path, status, latency.
    // 7. RequestId: assign / preserve X-Request-Id.
    // 8. TraceContext: W3C traceparent propagation.
    app.layer(axum::middleware::from_fn(
        middleware::trace_context_middleware,
    ))
    .layer(axum::middleware::from_fn(middleware::request_id_middleware))
    .layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::tracing_middleware,
    ))
    .layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::auth_middleware,
    ))
    .layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::rate_limit_middleware,
    ))
    .layer(axum::middleware::from_fn_with_state(
        state,
        middleware::connection_limit_middleware,
    ))
    .layer(CompressionLayer::new())
    .layer(TimeoutLayer::new(Duration::from_secs(30)))
}

// ---------------------------------------------------------------------------
// Metrics handler
// ---------------------------------------------------------------------------

/// Handler for `GET /metrics` — returns Prometheus text format.
async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.metrics.gather();
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}
