//! Search query handlers.
//!
//! Supports both a JSON body DSL (`POST /_search`) and a simple query-string
//! search (`GET /_search?q=text`).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use msearchdb_core::query::{FullTextQuery, Operator, Query as CoreQuery};

use crate::dto::{ErrorResponse, SearchRequest, SearchResponse, SimpleSearchParams};
use crate::errors::db_error_to_response;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// POST /collections/{name}/_search — full DSL search
// ---------------------------------------------------------------------------

/// Execute a search query using the JSON query DSL.
///
/// Reads directly from the local index backend (no Raft required for reads).
pub async fn search_documents(
    State(state): State<AppState>,
    Path(collection): Path<String>,
    Json(body): Json<SearchRequest>,
) -> impl IntoResponse {
    // Verify collection exists
    {
        let collections = state.collections.read().await;
        if !collections.contains_key(&collection) {
            let resp = ErrorResponse::not_found(format!("collection '{}' not found", collection));
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::to_value(resp).unwrap()),
            );
        }
    }

    let core_query = body.query.into_core_query();

    match state.index.search(&core_query).await {
        Ok(result) => {
            let resp = SearchResponse::from_search_result(result);
            (StatusCode::OK, Json(serde_json::to_value(resp).unwrap()))
        }
        Err(e) => {
            let (status, resp) = db_error_to_response(e);
            (status, Json(serde_json::to_value(resp).unwrap()))
        }
    }
}

// ---------------------------------------------------------------------------
// GET /collections/{name}/_search?q=text — simple query-string search
// ---------------------------------------------------------------------------

/// Execute a simple query-string search.
///
/// Constructs a [`FullTextQuery`] against the `_body` catch-all field.
pub async fn simple_search(
    State(state): State<AppState>,
    Path(collection): Path<String>,
    Query(params): Query<SimpleSearchParams>,
) -> impl IntoResponse {
    // Verify collection exists
    {
        let collections = state.collections.read().await;
        if !collections.contains_key(&collection) {
            let resp = ErrorResponse::not_found(format!("collection '{}' not found", collection));
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::to_value(resp).unwrap()),
            );
        }
    }

    let core_query = CoreQuery::FullText(FullTextQuery {
        field: "_body".to_string(),
        query: params.q,
        operator: Operator::Or,
    });

    match state.index.search(&core_query).await {
        Ok(result) => {
            let resp = SearchResponse::from_search_result(result);
            (StatusCode::OK, Json(serde_json::to_value(resp).unwrap()))
        }
        Err(e) => {
            let (status, resp) = db_error_to_response(e);
            (status, Json(serde_json::to_value(resp).unwrap()))
        }
    }
}
