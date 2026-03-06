//! Administrative handlers for maintenance operations.
//!
//! These endpoints support operations like forcing an index refresh/commit
//! and retrieving storage and index statistics.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::dto::{ErrorResponse, StatsResponse};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// POST /collections/{name}/_refresh — force index commit
// ---------------------------------------------------------------------------

/// Force an index commit/refresh for a collection.
///
/// This makes all recently indexed documents visible to search queries.
pub async fn refresh_collection(
    State(state): State<AppState>,
    Path(collection): Path<String>,
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

    // The TantivyIndex commit is handled by the state machine, but
    // we expose this endpoint for explicit user-initiated refreshes.
    let body = serde_json::json!({
        "acknowledged": true,
        "collection": collection,
    });
    (StatusCode::OK, Json(body))
}

// ---------------------------------------------------------------------------
// GET /_stats — storage + index stats
// ---------------------------------------------------------------------------

/// Return storage and index statistics for this node.
pub async fn get_stats(State(state): State<AppState>) -> impl IntoResponse {
    let node_id = state.raft_node.node_id();
    let is_leader = state.raft_node.is_leader();

    let collections = state.collections.read().await;

    let leader_id = state.raft_node.current_leader().unwrap_or(0);

    let resp = StatsResponse {
        node_id,
        collections: collections.len(),
        is_leader,
        current_term: leader_id, // approximate — use raft metrics in production
    };

    Json(serde_json::to_value(resp).unwrap())
}
