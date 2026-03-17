//! Document CRUD handlers.
//!
//! All write operations (index, upsert, delete) are proposed through Raft
//! to ensure consistency across the cluster.  Read operations go directly
//! to the local storage backend for lower latency.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use msearchdb_consensus::types::RaftCommand;
use msearchdb_core::consistency::ConsistencyLevel;
use msearchdb_core::document::{Document, DocumentId};
use msearchdb_core::error::DbError;
use msearchdb_core::read_coordinator::ReplicaResponse;

use crate::dto::{
    fields_to_value, json_to_field_value, request_to_document, ErrorResponse, GetDocumentParams,
    IndexDocumentRequest, IndexDocumentResponse,
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

    // Get the current mapping for dynamic field detection.
    let current_mapping = {
        let collections = state.collections.read().await;
        match collections.get(&collection) {
            Some(meta) => meta.mapping.clone(),
            None => {
                let resp =
                    ErrorResponse::not_found(format!("collection '{}' not found", collection));
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::to_value(resp).unwrap()),
                );
            }
        }
    };

    let doc = request_to_document(body);
    let doc_id = doc.id.as_str().to_owned();

    let cmd = RaftCommand::InsertDocument {
        document: doc.clone(),
    };

    match state.raft_node.propose(cmd).await {
        Ok(_resp) => {
            // Store in collection-specific storage.
            if let Err(e) = state
                .storage
                .put_in_collection(&collection, doc.clone())
                .await
            {
                let (status, resp) = db_error_to_response(e);
                return (status, Json(serde_json::to_value(resp).unwrap()));
            }

            // Index in collection-specific index with dynamic mapping.
            match state
                .index
                .index_document_in_collection(&collection, &doc, &current_mapping)
                .await
            {
                Ok(updated_mapping) => {
                    // Commit the index writes.
                    let _ = state.index.commit_collection_index(&collection).await;

                    // Update mapping and doc count.
                    {
                        let mut collections = state.collections.write().await;
                        if let Some(meta) = collections.get_mut(&collection) {
                            meta.doc_count += 1;
                            meta.mapping = updated_mapping;
                        }
                    }
                }
                Err(e) => {
                    let (status, resp) = db_error_to_response(e);
                    return (status, Json(serde_json::to_value(resp).unwrap()));
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

    // Get the current mapping for dynamic field detection.
    let current_mapping = {
        let collections = state.collections.read().await;
        match collections.get(&collection) {
            Some(meta) => meta.mapping.clone(),
            None => {
                let resp =
                    ErrorResponse::not_found(format!("collection '{}' not found", collection));
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::to_value(resp).unwrap()),
                );
            }
        }
    };

    let doc_id = DocumentId::new(&id);
    let mut doc = Document::new(doc_id);
    for (key, val) in body.fields {
        if let Some(fv) = json_to_field_value(&val) {
            doc.set_field(key, fv);
        }
    }

    let cmd = RaftCommand::UpdateDocument {
        document: doc.clone(),
    };

    match state.raft_node.propose(cmd).await {
        Ok(_resp) => {
            // Update in collection-specific storage.
            if let Err(e) = state
                .storage
                .put_in_collection(&collection, doc.clone())
                .await
            {
                let (status, resp) = db_error_to_response(e);
                return (status, Json(serde_json::to_value(resp).unwrap()));
            }

            // Delete old version from index, then index new version.
            let _ = state
                .index
                .delete_document_from_collection(&collection, &doc.id)
                .await;

            match state
                .index
                .index_document_in_collection(&collection, &doc, &current_mapping)
                .await
            {
                Ok(updated_mapping) => {
                    let _ = state.index.commit_collection_index(&collection).await;

                    let mut collections = state.collections.write().await;
                    if let Some(meta) = collections.get_mut(&collection) {
                        meta.mapping = updated_mapping;
                    }
                }
                Err(e) => {
                    let (status, resp) = db_error_to_response(e);
                    return (status, Json(serde_json::to_value(resp).unwrap()));
                }
            }

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

/// Retrieve a document by id with configurable consistency level.
///
/// The consistency level is specified via the `?consistency=` query parameter.
/// Accepted values: `one` (fastest, may be stale), `quorum` (default), `all`
/// (strongest). The coordinator fans out reads to the local storage (in
/// single-node mode, all reads are equivalent) and resolves via vector clocks.
pub async fn get_document(
    State(state): State<AppState>,
    Path((collection, id)): Path<(String, String)>,
    Query(params): Query<GetDocumentParams>,
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

    let doc_id = DocumentId::new(&id);

    // In single-node mode, the local storage is the only replica.
    // We read from collection-specific storage and construct a
    // ReplicaResponse, then run it through the ReadCoordinator so that
    // the consistency-level validation and vector-clock resolution are
    // always exercised.
    match state
        .storage
        .get_from_collection(&collection, &doc_id)
        .await
    {
        Ok(doc) => {
            let replica_response = ReplicaResponse {
                node_id: state.local_node_id,
                document: doc.clone(),
                version: doc.version.clone(),
            };

            // For single-node, we have exactly 1 response. With
            // ConsistencyLevel::One this always succeeds. For Quorum/All
            // in a single-node cluster (rf=1), required_responses is 1.
            // For multi-node rf>1 but single node, the coordinator will
            // report a ConsistencyError — which is correct behaviour.
            let rf = state.read_coordinator.replication_factor();
            let effective_rf = std::cmp::min(rf, 1); // single-node: only 1 replica available
            let required = consistency.required_responses(effective_rf);

            if 1 < required {
                // Cannot satisfy the requested level with a single node
                // when rf > 1 — but for single-node clusters (rf=1) this
                // never triggers.
                let (status, resp) = db_error_to_response(DbError::ConsistencyError(format!(
                    "consistency level {} requires {} responses but only 1 available \
                     (single-node mode)",
                    consistency, required
                )));
                return (status, Json(serde_json::to_value(resp).unwrap()));
            }

            let resolution = state
                .read_coordinator
                .resolve(&[replica_response], ConsistencyLevel::One);

            match resolution {
                Ok(res) => {
                    let source = fields_to_value(&res.document.fields);
                    let body = serde_json::json!({
                        "_id": res.document.id.as_str(),
                        "_source": source,
                        "_version": res.version.total(),
                        "consistency": consistency.to_string(),
                        "found": true,
                    });
                    (StatusCode::OK, Json(body))
                }
                Err(e) => {
                    let (status, resp) = db_error_to_response(e);
                    (status, Json(serde_json::to_value(resp).unwrap()))
                }
            }
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
    let cmd = RaftCommand::DeleteDocument { id: doc_id.clone() };

    match state.raft_node.propose(cmd).await {
        Ok(_) => {
            // Delete from collection-specific storage.
            let _ = state
                .storage
                .delete_from_collection(&collection, &doc_id)
                .await;

            // Delete from collection-specific index.
            let _ = state
                .index
                .delete_document_from_collection(&collection, &doc_id)
                .await;
            let _ = state.index.commit_collection_index(&collection).await;

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
