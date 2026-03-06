//! Document CRUD handlers.
//!
//! All write operations (index, upsert, delete) are proposed through Raft
//! to ensure consistency across the cluster.  Read operations go directly
//! to the local storage backend for lower latency.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use msearchdb_consensus::types::RaftCommand;
use msearchdb_core::document::{Document, DocumentId};
use msearchdb_core::error::DbError;

use crate::dto::{
    fields_to_value, json_to_field_value, request_to_document, ErrorResponse, IndexDocumentRequest,
    IndexDocumentResponse,
};
use crate::errors::db_error_to_response;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// POST /collections/{name}/docs — index document
// ---------------------------------------------------------------------------

/// Index a new document in a collection.
///
/// The document is proposed through Raft as an `InsertDocument` command.
/// A UUID is generated if no `id` is supplied in the request body.
pub async fn index_document(
    State(state): State<AppState>,
    Path(collection): Path<String>,
    Json(body): Json<IndexDocumentRequest>,
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

    let doc = request_to_document(body);
    let doc_id = doc.id.as_str().to_owned();

    let cmd = RaftCommand::InsertDocument { document: doc };

    match state.raft_node.propose(cmd).await {
        Ok(_resp) => {
            // Increment doc count
            {
                let mut collections = state.collections.write().await;
                if let Some(meta) = collections.get_mut(&collection) {
                    meta.doc_count += 1;
                }
            }
            let resp = IndexDocumentResponse {
                id: doc_id,
                result: "created".into(),
                version: 1,
            };
            (
                StatusCode::CREATED,
                Json(serde_json::to_value(resp).unwrap()),
            )
        }
        Err(e) => {
            let (status, resp) = db_error_to_response(e);
            (status, Json(serde_json::to_value(resp).unwrap()))
        }
    }
}

// ---------------------------------------------------------------------------
// PUT /collections/{name}/docs/{id} — upsert document
// ---------------------------------------------------------------------------

/// Upsert (insert or replace) a document by id.
pub async fn upsert_document(
    State(state): State<AppState>,
    Path((collection, id)): Path<(String, String)>,
    Json(body): Json<IndexDocumentRequest>,
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

    let doc_id = DocumentId::new(&id);
    let mut doc = Document::new(doc_id);
    for (key, val) in body.fields {
        if let Some(fv) = json_to_field_value(&val) {
            doc.set_field(key, fv);
        }
    }

    let cmd = RaftCommand::UpdateDocument { document: doc };

    match state.raft_node.propose(cmd).await {
        Ok(_resp) => {
            let resp = IndexDocumentResponse {
                id,
                result: "updated".into(),
                version: 1,
            };
            (StatusCode::OK, Json(serde_json::to_value(resp).unwrap()))
        }
        Err(e) => {
            let (status, resp) = db_error_to_response(e);
            (status, Json(serde_json::to_value(resp).unwrap()))
        }
    }
}

// ---------------------------------------------------------------------------
// GET /collections/{name}/docs/{id} — get document
// ---------------------------------------------------------------------------

/// Retrieve a document by id directly from local storage (no Raft).
pub async fn get_document(
    State(state): State<AppState>,
    Path((collection, id)): Path<(String, String)>,
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

    let doc_id = DocumentId::new(&id);
    match state.storage.get(&doc_id).await {
        Ok(doc) => {
            let source = fields_to_value(&doc.fields);
            let body = serde_json::json!({
                "_id": doc.id.as_str(),
                "_source": source,
                "found": true,
            });
            (StatusCode::OK, Json(body))
        }
        Err(DbError::NotFound(_)) => {
            let resp = ErrorResponse::not_found(format!("document '{}' not found", id));
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::to_value(resp).unwrap()),
            )
        }
        Err(e) => {
            let (status, resp) = db_error_to_response(e);
            (status, Json(serde_json::to_value(resp).unwrap()))
        }
    }
}

// ---------------------------------------------------------------------------
// DELETE /collections/{name}/docs/{id} — delete document
// ---------------------------------------------------------------------------

/// Delete a document by id.
pub async fn delete_document(
    State(state): State<AppState>,
    Path((collection, id)): Path<(String, String)>,
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

    let doc_id = DocumentId::new(&id);
    let cmd = RaftCommand::DeleteDocument { id: doc_id };

    match state.raft_node.propose(cmd).await {
        Ok(_) => {
            // Decrement doc count
            {
                let mut collections = state.collections.write().await;
                if let Some(meta) = collections.get_mut(&collection) {
                    meta.doc_count = meta.doc_count.saturating_sub(1);
                }
            }
            let body = serde_json::json!({
                "_id": id,
                "result": "deleted",
            });
            (StatusCode::OK, Json(body))
        }
        Err(e) => {
            let (status, resp) = db_error_to_response(e);
            (status, Json(serde_json::to_value(resp).unwrap()))
        }
    }
}
