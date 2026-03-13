//! Collection management handlers.
//!
//! Collections are logical groupings of documents, similar to tables in a
//! relational database or indices in Elasticsearch.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use msearchdb_consensus::types::RaftCommand;
use msearchdb_index::schema_builder::SchemaConfig;

use msearchdb_core::collection::{CollectionSettings, FieldMapping};

use crate::dto::{CollectionInfoResponse, CreateCollectionRequest, ErrorResponse};
use crate::errors::db_error_to_response;
use crate::state::{AppState, CollectionMeta};

// ---------------------------------------------------------------------------
// PUT /collections/{name} — create collection
// ---------------------------------------------------------------------------

/// Create a new collection.
///
/// Proposes a `CreateCollection` command through Raft so that all nodes in
/// the cluster create the collection atomically.
pub async fn create_collection(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<CreateCollectionRequest>,
) -> impl IntoResponse {
    // Check if collection already exists
    {
        let collections = state.collections.read().await;
        if collections.contains_key(&name) {
            return (
                StatusCode::CONFLICT,
                Json(
                    serde_json::to_value(ErrorResponse::new(
                        409,
                        "resource_already_exists",
                        format!("collection '{}' already exists", name),
                    ))
                    .unwrap(),
                ),
            );
        }
    }

    let schema = body
        .schema
        .and_then(|v| serde_json::from_value::<SchemaConfig>(v).ok())
        .unwrap_or_default();

    let settings = body
        .settings
        .and_then(|v| serde_json::from_value::<CollectionSettings>(v).ok())
        .unwrap_or_default();

    let cmd = RaftCommand::CreateCollection {
        name: name.clone(),
        schema,
    };

    match state.raft_node.propose(cmd).await {
        Ok(_resp) => {
            // Create the per-collection storage column family.
            if let Err(e) = state.storage.create_collection(&name).await {
                tracing::error!(collection = %name, error = %e, "failed to create storage CF");
                return db_error_to_status_json(e);
            }

            // Create the per-collection Tantivy index.
            if let Err(e) = state.index.create_collection_index(&name).await {
                tracing::error!(collection = %name, error = %e, "failed to create collection index");
                return db_error_to_status_json(e);
            }

            let mut collections = state.collections.write().await;
            collections.insert(
                name.clone(),
                CollectionMeta {
                    name: name.clone(),
                    doc_count: 0,
                    mapping: FieldMapping::new(),
                    settings: settings.clone(),
                },
            );

            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(CollectionInfoResponse {
                        name,
                        doc_count: 0,
                        mapping: None,
                        settings: serde_json::to_value(&settings).ok(),
                    })
                    .unwrap(),
                ),
            )
        }
        Err(e) => db_error_to_status_json(e),
    }
}

// ---------------------------------------------------------------------------
// DELETE /collections/{name} — delete collection
// ---------------------------------------------------------------------------

/// Delete a collection and all its documents.
pub async fn delete_collection(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    {
        let collections = state.collections.read().await;
        if !collections.contains_key(&name) {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    serde_json::to_value(ErrorResponse::not_found(format!(
                        "collection '{}' not found",
                        name
                    )))
                    .unwrap(),
                ),
            );
        }
    }

    let cmd = RaftCommand::DeleteCollection { name: name.clone() };

    match state.raft_node.propose(cmd).await {
        Ok(_) => {
            // Drop the per-collection storage column family.
            if let Err(e) = state.storage.drop_collection(&name).await {
                tracing::warn!(collection = %name, error = %e, "failed to drop storage CF");
            }

            // Drop the per-collection Tantivy index.
            if let Err(e) = state.index.drop_collection_index(&name).await {
                tracing::warn!(collection = %name, error = %e, "failed to drop collection index");
            }

            let mut collections = state.collections.write().await;
            collections.remove(&name);
            (
                StatusCode::OK,
                Json(serde_json::json!({"acknowledged": true})),
            )
        }
        Err(e) => db_error_to_status_json(e),
    }
}

// ---------------------------------------------------------------------------
// GET /collections — list collections
// ---------------------------------------------------------------------------

/// List all collections.
pub async fn list_collections(State(state): State<AppState>) -> impl IntoResponse {
    let collections = state.collections.read().await;
    let list: Vec<CollectionInfoResponse> = collections
        .values()
        .map(|meta| CollectionInfoResponse {
            name: meta.name.clone(),
            doc_count: meta.doc_count,
            mapping: serde_json::to_value(&meta.mapping).ok(),
            settings: serde_json::to_value(&meta.settings).ok(),
        })
        .collect();
    Json(list)
}

// ---------------------------------------------------------------------------
// GET /collections/{name} — get collection info
// ---------------------------------------------------------------------------

/// Get information about a single collection.
pub async fn get_collection(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let collections = state.collections.read().await;
    match collections.get(&name) {
        Some(meta) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(CollectionInfoResponse {
                    name: meta.name.clone(),
                    doc_count: meta.doc_count,
                    mapping: serde_json::to_value(&meta.mapping).ok(),
                    settings: serde_json::to_value(&meta.settings).ok(),
                })
                .unwrap(),
            ),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(
                serde_json::to_value(ErrorResponse::not_found(format!(
                    "collection '{}' not found",
                    name
                )))
                .unwrap(),
            ),
        ),
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

/// Convert a [`DbError`] to an axum `(StatusCode, Json)` response.
fn db_error_to_status_json(
    e: msearchdb_core::error::DbError,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, resp) = db_error_to_response(e);
    (status, Json(serde_json::to_value(resp).unwrap()))
}
