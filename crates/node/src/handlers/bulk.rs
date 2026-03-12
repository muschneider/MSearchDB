//! Bulk indexing handler with NDJSON parsing.
//!
//! The bulk endpoint accepts newline-delimited JSON (NDJSON) where each pair
//! of lines represents an action and its document body:
//!
//! ```text
//! {"index": {"_id": "1"}}
//! {"title": "Document One", "body": "..."}
//! {"index": {"_id": "2"}}
//! {"title": "Document Two", "body": "..."}
//! {"delete": {"_id": "3"}}
//! ```
//!
//! Delete actions are a single line (no body follows).
//!
//! Documents are batched into groups of [`BATCH_SIZE`] and submitted as single
//! [`RaftCommand::BatchInsert`] entries to amortise consensus overhead.  Delete
//! commands are still proposed individually since they cannot be batched.

use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use msearchdb_consensus::types::RaftCommand;
use msearchdb_core::document::{Document, DocumentId};

use crate::dto::{json_to_field_value, BulkAction, BulkItem, BulkResponse, ErrorResponse};
use crate::state::AppState;

/// Maximum number of documents per Raft proposal batch.
const BATCH_SIZE: usize = 100;

// ---------------------------------------------------------------------------
// POST /collections/{name}/docs/_bulk — bulk index
// ---------------------------------------------------------------------------

/// Bulk index documents from an NDJSON request body.
///
/// Parses action/document pairs, groups index actions into batches of
/// [`BATCH_SIZE`] documents, and submits each batch as a single Raft
/// [`BatchInsert`](RaftCommand::BatchInsert) entry.  Delete actions are
/// proposed individually.  Returns a per-item result list.
pub async fn bulk_index(
    State(state): State<AppState>,
    Path(collection): Path<String>,
    body: String,
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

    let start = Instant::now();
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut items: Vec<BulkItem> = Vec::new();
    let mut has_errors = false;
    let mut docs_indexed: u64 = 0;

    // Pending insert documents and their ids (for per-item reporting).
    let mut pending_docs: Vec<(Document, String)> = Vec::new();
    // Pending delete commands (proposed individually).
    let mut pending_deletes: Vec<(RaftCommand, String)> = Vec::new();

    let mut i = 0;

    while i < lines.len() {
        let action_line = lines[i];
        let action: BulkAction = match serde_json::from_str(action_line) {
            Ok(a) => a,
            Err(e) => {
                has_errors = true;
                items.push(BulkItem {
                    action: "unknown".into(),
                    id: String::new(),
                    status: 400,
                    error: Some(format!("invalid action line: {}", e)),
                });
                i += 1;
                continue;
            }
        };

        match action {
            BulkAction::Index(meta) => {
                i += 1;
                if i >= lines.len() {
                    has_errors = true;
                    items.push(BulkItem {
                        action: "index".into(),
                        id: meta.id.unwrap_or_default(),
                        status: 400,
                        error: Some("missing document body after index action".into()),
                    });
                    break;
                }

                let doc_line = lines[i];
                let fields: std::collections::HashMap<String, serde_json::Value> =
                    match serde_json::from_str(doc_line) {
                        Ok(f) => f,
                        Err(e) => {
                            has_errors = true;
                            items.push(BulkItem {
                                action: "index".into(),
                                id: meta.id.unwrap_or_default(),
                                status: 400,
                                error: Some(format!("invalid document body: {}", e)),
                            });
                            i += 1;
                            continue;
                        }
                    };

                let doc_id = meta
                    .id
                    .map(DocumentId::new)
                    .unwrap_or_else(DocumentId::generate);
                let id_str = doc_id.as_str().to_owned();

                let mut doc = Document::new(doc_id);
                for (key, val) in fields {
                    if let Some(fv) = json_to_field_value(&val) {
                        doc.set_field(key, fv);
                    }
                }

                pending_docs.push((doc, id_str));
                i += 1;
            }
            BulkAction::Delete(meta) => {
                let id_str = meta.id.clone().unwrap_or_default();
                if id_str.is_empty() {
                    has_errors = true;
                    items.push(BulkItem {
                        action: "delete".into(),
                        id: id_str,
                        status: 400,
                        error: Some("delete action requires _id".into()),
                    });
                } else {
                    pending_deletes.push((
                        RaftCommand::DeleteDocument {
                            id: DocumentId::new(&id_str),
                        },
                        id_str,
                    ));
                }
                i += 1;
            }
        }

        // Flush insert batch when full
        if pending_docs.len() >= BATCH_SIZE {
            let batch = std::mem::take(&mut pending_docs);
            let (batch_items, batch_errors, batch_count) =
                flush_insert_batch(&state, batch).await;
            items.extend(batch_items);
            if batch_errors {
                has_errors = true;
            }
            docs_indexed += batch_count;
        }
    }

    // Flush remaining insert batch
    if !pending_docs.is_empty() {
        let (batch_items, batch_errors, batch_count) =
            flush_insert_batch(&state, pending_docs).await;
        items.extend(batch_items);
        if batch_errors {
            has_errors = true;
        }
        docs_indexed += batch_count;
    }

    // Execute pending deletes individually
    for (cmd, id) in pending_deletes {
        match state.raft_node.propose(cmd).await {
            Ok(_) => {
                items.push(BulkItem {
                    action: "delete".into(),
                    id,
                    status: 200,
                    error: None,
                });
            }
            Err(e) => {
                has_errors = true;
                items.push(BulkItem {
                    action: "delete".into(),
                    id,
                    status: 500,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    // Update doc count
    {
        let mut collections = state.collections.write().await;
        if let Some(meta) = collections.get_mut(&collection) {
            meta.doc_count += docs_indexed;
        }
    }

    let took = start.elapsed().as_millis() as u64;

    let resp = BulkResponse {
        took,
        errors: has_errors,
        items,
    };

    (StatusCode::OK, Json(serde_json::to_value(resp).unwrap()))
}

/// Flush a batch of documents as a single [`RaftCommand::BatchInsert`].
///
/// Returns `(per_item_results, had_errors, success_count)`.
async fn flush_insert_batch(
    state: &AppState,
    batch: Vec<(Document, String)>,
) -> (Vec<BulkItem>, bool, u64) {
    let ids: Vec<String> = batch.iter().map(|(_, id)| id.clone()).collect();
    let documents: Vec<Document> = batch.into_iter().map(|(doc, _)| doc).collect();
    let batch_len = documents.len();

    match state.raft_node.propose_batch(documents).await {
        Ok(resp) => {
            // All documents in the batch were applied atomically.
            let count = resp.affected_count;
            let items: Vec<BulkItem> = ids
                .into_iter()
                .enumerate()
                .map(|(idx, id)| {
                    if idx < count {
                        BulkItem {
                            action: "index".into(),
                            id,
                            status: 201,
                            error: None,
                        }
                    } else {
                        // Documents beyond the success count failed inside the
                        // state machine (storage/index error).
                        BulkItem {
                            action: "index".into(),
                            id,
                            status: 500,
                            error: Some("failed during batch apply".into()),
                        }
                    }
                })
                .collect();
            let has_errors = count < batch_len;
            (items, has_errors, count as u64)
        }
        Err(e) => {
            // Entire batch rejected (e.g. not leader).
            let err_msg = e.to_string();
            let items: Vec<BulkItem> = ids
                .into_iter()
                .map(|id| BulkItem {
                    action: "index".into(),
                    id,
                    status: 500,
                    error: Some(err_msg.clone()),
                })
                .collect();
            (items, true, 0)
        }
    }
}
