//! Snapshot and backup/restore HTTP handlers.
//!
//! | Method | Path | Handler | Description |
//! |--------|------|---------|-------------|
//! | `POST` | `/_snapshot` | [`create_snapshot`] | Trigger manual snapshot |
//! | `GET` | `/_snapshot` | [`list_snapshots`] | List available snapshots |
//! | `GET` | `/_snapshot/:id` | [`download_snapshot`] | Download snapshot archive |
//! | `POST` | `/_restore` | [`restore_snapshot`] | Restore from snapshot |

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// Response body for snapshot creation.
#[derive(Serialize)]
pub struct SnapshotResponse {
    pub success: bool,
    pub snapshot_id: String,
    pub term: u64,
    pub last_log_index: u64,
    pub size_bytes: u64,
    pub doc_count: u64,
}

/// Response body for listing snapshots.
#[derive(Serialize)]
pub struct SnapshotListResponse {
    pub snapshots: Vec<msearchdb_core::snapshot::SnapshotInfo>,
}

/// Request body for restore.
#[derive(Deserialize)]
pub struct RestoreRequest {
    /// Snapshot id to restore from (local snapshot).
    pub snapshot_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /_snapshot` — Trigger a manual snapshot on this node.
///
/// The snapshot captures RocksDB + Tantivy state at the current Raft
/// log position.
pub async fn create_snapshot(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot_manager = match &state.snapshot_manager {
        Some(sm) => sm,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "snapshot manager not initialised"
                })),
            )
                .into_response();
        }
    };

    // Get Raft metrics for term and log index
    let metrics = state.raft_node.metrics().await;
    let term = metrics.current_term;
    let last_log_index = metrics.last_applied.map(|l| l.index).unwrap_or(0);

    // Estimate doc count (best effort from metrics or storage)
    let doc_count = {
        let collections = state.collections.read().await;
        collections.values().map(|c| c.doc_count).sum::<u64>()
    };

    // Create snapshot (blocking I/O)
    let sm = snapshot_manager.clone();
    let result =
        tokio::task::spawn_blocking(move || sm.create_snapshot(term, last_log_index, doc_count))
            .await;

    match result {
        Ok(Ok(info)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "snapshot_id": info.id,
                "term": info.term,
                "last_log_index": info.last_log_index,
                "size_bytes": info.size_bytes,
                "doc_count": info.doc_count,
            })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("snapshot creation failed: {}", e)
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("task join error: {}", e)
            })),
        )
            .into_response(),
    }
}

/// `GET /_snapshot` — List all available snapshots.
pub async fn list_snapshots(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot_manager = match &state.snapshot_manager {
        Some(sm) => sm,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "snapshot manager not initialised"
                })),
            )
                .into_response();
        }
    };

    let sm = snapshot_manager.clone();
    let result = tokio::task::spawn_blocking(move || sm.list_snapshots()).await;

    match result {
        Ok(Ok(snapshots)) => (
            StatusCode::OK,
            Json(serde_json::json!({ "snapshots": snapshots })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /_snapshot/:id` — Download a snapshot as a compressed archive.
///
/// Returns the raw `.snap` file with appropriate Content-Type and
/// Content-Disposition headers for browser download.
pub async fn download_snapshot(
    State(state): State<AppState>,
    Path(snapshot_id): Path<String>,
) -> impl IntoResponse {
    let snapshot_manager = match &state.snapshot_manager {
        Some(sm) => sm,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "snapshot manager not initialised"
                })),
            )
                .into_response();
        }
    };

    // Check snapshot exists
    let sm = snapshot_manager.clone();
    let snap_id = snapshot_id.clone();
    let info_result = tokio::task::spawn_blocking(move || sm.get_snapshot(&snap_id)).await;

    match info_result {
        Ok(Err(e)) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
        Ok(Ok(_)) => {} // exists, continue
    }

    // Stream the file
    let snap_path = snapshot_manager.snapshot_path(&snapshot_id);
    match tokio::fs::File::open(&snap_path).await {
        Ok(file) => {
            let stream = ReaderStream::new(file);
            let body = Body::from_stream(stream);
            let filename = format!("{}.snap", snapshot_id);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", filename),
                )
                .body(body)
                .unwrap()
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to open snapshot: {}", e) })),
        )
            .into_response(),
    }
}

/// `POST /_restore` — Restore the database from a snapshot.
///
/// Accepts a JSON body with `snapshot_id` to restore from a local snapshot.
pub async fn restore_snapshot(
    State(state): State<AppState>,
    Json(req): Json<RestoreRequest>,
) -> impl IntoResponse {
    let snapshot_manager = match &state.snapshot_manager {
        Some(sm) => sm,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "snapshot manager not initialised"
                })),
            );
        }
    };

    let snapshot_id = match req.snapshot_id {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "snapshot_id is required"
                })),
            );
        }
    };

    let sm = snapshot_manager.clone();
    let result = tokio::task::spawn_blocking(move || {
        let snap_path = sm.snapshot_path(&snapshot_id);
        sm.restore_from_path(&snap_path)
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "message": "database restored from snapshot; restart node to apply"
            })),
        ),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("restore failed: {}", e)
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("task join error: {}", e)
            })),
        ),
    }
}
