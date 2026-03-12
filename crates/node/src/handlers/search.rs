//! Search query handlers.
//!
//! Supports both a JSON body DSL (`POST /_search`) and a simple query-string
//! search (`GET /_search?q=text`).  Both endpoints accept an optional
//! `?consistency=` query parameter whose value is echoed in the response.
//! In single-node mode the parameter is informational only — all reads go
//! to the local index.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use msearchdb_core::consistency::ConsistencyLevel;
use msearchdb_core::query::{FullTextQuery, Operator, Query as CoreQuery};

use crate::dto::{
    ErrorResponse, GetDocumentParams, SearchRequest, SearchResponse, SimpleSearchParams,
};
use crate::errors::db_error_to_response;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// POST /collections/{name}/_search — full DSL search
// ---------------------------------------------------------------------------

/// Execute a search query using the JSON query DSL.
///
/// Reads directly from the local index backend (no Raft required for reads).
/// Accepts an optional `?consistency=` query parameter (echoed in response).
pub async fn search_documents(
    State(state): State<AppState>,
    Path(collection): Path<String>,
    Query(params): Query<GetDocumentParams>,
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

    let consistency = params
        .consistency
        .as_deref()
        .map(ConsistencyLevel::from_str_param)
        .unwrap_or_default();

    let core_query = body.query.into_core_query();

    match state.index.search(&core_query).await {
        Ok(result) => {
            let mut resp = serde_json::to_value(SearchResponse::from_search_result(result))
                .unwrap_or_default();
            if let Some(obj) = resp.as_object_mut() {
                obj.insert(
                    "consistency".to_string(),
                    serde_json::Value::String(consistency.to_string()),
                );
            }
            (StatusCode::OK, Json(resp))
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
/// Accepts an optional `?consistency=` query parameter (echoed in response).
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

    let consistency = params
        .consistency
        .as_deref()
        .map(ConsistencyLevel::from_str_param)
        .unwrap_or_default();

    let core_query = CoreQuery::FullText(FullTextQuery {
        field: "_body".to_string(),
        query: params.q,
        operator: Operator::Or,
    });

    match state.index.search(&core_query).await {
        Ok(result) => {
            let mut resp = serde_json::to_value(SearchResponse::from_search_result(result))
                .unwrap_or_default();
            if let Some(obj) = resp.as_object_mut() {
                obj.insert(
                    "consistency".to_string(),
                    serde_json::Value::String(consistency.to_string()),
                );
            }
            (StatusCode::OK, Json(resp))
        }
        Err(e) => {
            let (status, resp) = db_error_to_response(e);
            (status, Json(serde_json::to_value(resp).unwrap()))
        }
    }
}
