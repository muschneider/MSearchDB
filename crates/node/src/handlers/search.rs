//! Search query handlers.
//!
//! Supports both a JSON body DSL (`POST /_search`) and a simple query-string
//! search (`GET /_search?q=text`).  Both endpoints accept an optional
//! `?consistency=` query parameter whose value is echoed in the response.
//! In single-node mode the parameter is informational only — all reads go
//! to the local index.
//!
//! ## Alias Resolution
//!
//! If the path parameter does not match a known collection, the handler
//! checks the alias registry.  When an alias is found, the search fans out
//! across all backing collections and merges results (ordered by score).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use msearchdb_core::consistency::ConsistencyLevel;
use msearchdb_core::query::{FullTextQuery, Operator, Query as CoreQuery, SearchResult};

use crate::dto::{
    ErrorResponse, GetDocumentParams, SearchRequest, SearchResponse, SimpleSearchParams,
};
use crate::errors::db_error_to_response;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Alias resolution helpers
// ---------------------------------------------------------------------------

/// Resolve a name to one or more collection names.
///
/// Returns `Ok(vec)` with the collection names to search.
/// Returns `Err((StatusCode, Json))` if neither collection nor alias exists.
async fn resolve_search_targets(
    state: &AppState,
    name: &str,
) -> Result<Vec<String>, (StatusCode, Json<serde_json::Value>)> {
    // Check collections first.
    {
        let collections = state.collections.read().await;
        if collections.contains_key(name) {
            return Ok(vec![name.to_owned()]);
        }
    }

    // Fall back to aliases.
    {
        let aliases = state.aliases.read().await;
        if let Some(alias) = aliases.get(name) {
            let targets: Vec<String> = alias.collections().to_vec();
            if targets.is_empty() {
                let resp = ErrorResponse::bad_request(format!(
                    "alias '{}' has no backing collections",
                    name
                ));
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::to_value(resp).unwrap()),
                ));
            }
            return Ok(targets);
        }
    }

    let resp = ErrorResponse::not_found(format!("collection '{}' not found", name));
    Err((
        StatusCode::NOT_FOUND,
        Json(serde_json::to_value(resp).unwrap()),
    ))
}

/// Fan out a query across multiple collections and merge results by score.
async fn fan_out_search(
    state: &AppState,
    targets: &[String],
    query: &CoreQuery,
    options: Option<&msearchdb_core::query::SearchOptions>,
) -> Result<SearchResult, msearchdb_core::error::DbError> {
    let mut all_docs: Vec<msearchdb_core::query::ScoredDocument> = Vec::new();
    let mut total: u64 = 0;
    let mut all_aggs: std::collections::HashMap<String, msearchdb_core::query::AggregationResult> =
        std::collections::HashMap::new();
    let start = std::time::Instant::now();

    for target in targets {
        let result = if let Some(opts) = options {
            state
                .index
                .search_collection_with_options(target, query, opts)
                .await?
        } else {
            state.index.search_collection(target, query).await?
        };
        total += result.total;
        all_docs.extend(result.documents);
        // Merge aggregations (last writer wins for multi-collection fan-out).
        all_aggs.extend(result.aggregations);
    }

    // Sort by score descending (if no sort options, default behaviour).
    if options.is_none_or(|o| o.sort.is_empty()) {
        all_docs.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let took_ms = start.elapsed().as_millis() as u64;

    Ok(SearchResult {
        documents: all_docs,
        total,
        took_ms,
        aggregations: all_aggs,
    })
}

// ---------------------------------------------------------------------------
// POST /collections/{name}/_search — full DSL search
// ---------------------------------------------------------------------------

/// Execute a search query using the JSON query DSL.
///
/// Reads directly from the local index backend (no Raft required for reads).
/// If `name` is an alias, fans out across all target collections.
/// Accepts an optional `?consistency=` query parameter (echoed in response).
pub async fn search_documents(
    State(state): State<AppState>,
    Path(collection): Path<String>,
    Query(params): Query<GetDocumentParams>,
    Json(body): Json<SearchRequest>,
) -> impl IntoResponse {
    let targets = match resolve_search_targets(&state, &collection).await {
        Ok(t) => t,
        Err(err_resp) => return err_resp,
    };

    let consistency = params
        .consistency
        .as_deref()
        .map(ConsistencyLevel::from_str_param)
        .unwrap_or_default();

    let search_options = body.to_search_options();
    let core_query = body.query.into_core_query();

    let search_start = std::time::Instant::now();
    match fan_out_search(&state, &targets, &core_query, Some(&search_options)).await {
        Ok(result) => {
            let duration_secs = search_start.elapsed().as_secs_f64();
            state
                .metrics
                .search_duration_seconds
                .with_label_values(&[&collection])
                .observe(duration_secs);

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
/// If `name` is an alias, fans out across all target collections.
/// Accepts an optional `?consistency=` query parameter (echoed in response).
pub async fn simple_search(
    State(state): State<AppState>,
    Path(collection): Path<String>,
    Query(params): Query<SimpleSearchParams>,
) -> impl IntoResponse {
    let targets = match resolve_search_targets(&state, &collection).await {
        Ok(t) => t,
        Err(err_resp) => return err_resp,
    };

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

    let search_start = std::time::Instant::now();
    match fan_out_search(&state, &targets, &core_query, None).await {
        Ok(result) => {
            let duration_secs = search_start.elapsed().as_secs_f64();
            state
                .metrics
                .search_duration_seconds
                .with_label_values(&[&collection])
                .observe(duration_secs);

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
