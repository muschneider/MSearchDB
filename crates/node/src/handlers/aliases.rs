//! Alias management handlers.
//!
//! Collection aliases map a single name to one or more backing collections.
//! Queries against an alias fan out across all targets and merge results.
//! This enables zero-downtime reindexing.
//!
//! | Endpoint | Handler |
//! |----------|---------|
//! | `PUT /_aliases/:name` | [`create_alias`] |
//! | `GET /_aliases/:name` | [`get_alias`] |
//! | `DELETE /_aliases/:name` | [`delete_alias`] |
//! | `GET /_aliases` | [`list_aliases`] |

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use msearchdb_consensus::types::RaftCommand;
use msearchdb_core::collection::CollectionAlias;

use crate::dto::{AliasInfoResponse, CreateAliasRequest, ErrorResponse};
use crate::errors::db_error_to_response;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// PUT /_aliases/{name} — create alias
// ---------------------------------------------------------------------------

/// Create a new collection alias.
///
/// Verifies that the alias name does not conflict with an existing alias and
/// that all target collections exist.  Proposes a `CreateAlias` command
/// through Raft for cluster-wide replication.
pub async fn create_alias(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<CreateAliasRequest>,
) -> impl IntoResponse {
    // Check alias doesn't already exist.
    {
        let aliases = state.aliases.read().await;
        if aliases.contains_key(&name) {
            return (
                StatusCode::CONFLICT,
                Json(
                    serde_json::to_value(ErrorResponse::new(
                        409,
                        "resource_already_exists",
                        format!("alias '{}' already exists", name),
                    ))
                    .unwrap(),
                ),
            );
        }
    }

    // Verify all target collections exist.
    {
        let collections = state.collections.read().await;
        for col_name in &body.collections {
            if !collections.contains_key(col_name) {
                return (
                    StatusCode::NOT_FOUND,
                    Json(
                        serde_json::to_value(ErrorResponse::not_found(format!(
                            "target collection '{}' not found",
                            col_name
                        )))
                        .unwrap(),
                    ),
                );
            }
        }
    }

    if body.collections.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::to_value(ErrorResponse::bad_request(
                    "alias must point to at least one collection",
                ))
                .unwrap(),
            ),
        );
    }

    let cmd = RaftCommand::CreateAlias {
        alias: name.clone(),
        collections: body.collections.clone(),
    };

    match state.raft_node.propose(cmd).await {
        Ok(_) => {
            let alias = CollectionAlias::new_multi(name.clone(), body.collections.clone());
            let mut aliases = state.aliases.write().await;
            aliases.insert(name.clone(), alias);

            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(AliasInfoResponse {
                        name,
                        collections: body.collections,
                    })
                    .unwrap(),
                ),
            )
        }
        Err(e) => {
            let (status, resp) = db_error_to_response(e);
            (status, Json(serde_json::to_value(resp).unwrap()))
        }
    }
}

// ---------------------------------------------------------------------------
// DELETE /_aliases/{name} — delete alias
// ---------------------------------------------------------------------------

/// Delete a collection alias.
pub async fn delete_alias(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    {
        let aliases = state.aliases.read().await;
        if !aliases.contains_key(&name) {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    serde_json::to_value(ErrorResponse::not_found(format!(
                        "alias '{}' not found",
                        name
                    )))
                    .unwrap(),
                ),
            );
        }
    }

    let cmd = RaftCommand::DeleteAlias {
        alias: name.clone(),
    };

    match state.raft_node.propose(cmd).await {
        Ok(_) => {
            let mut aliases = state.aliases.write().await;
            aliases.remove(&name);
            (
                StatusCode::OK,
                Json(serde_json::json!({"acknowledged": true})),
            )
        }
        Err(e) => {
            let (status, resp) = db_error_to_response(e);
            (status, Json(serde_json::to_value(resp).unwrap()))
        }
    }
}

// ---------------------------------------------------------------------------
// GET /_aliases/{name} — get alias info
// ---------------------------------------------------------------------------

/// Get information about a single alias.
pub async fn get_alias(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let aliases = state.aliases.read().await;
    match aliases.get(&name) {
        Some(alias) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(AliasInfoResponse {
                    name: alias.name().to_owned(),
                    collections: alias.collections().to_vec(),
                })
                .unwrap(),
            ),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(
                serde_json::to_value(ErrorResponse::not_found(format!(
                    "alias '{}' not found",
                    name
                )))
                .unwrap(),
            ),
        ),
    }
}

// ---------------------------------------------------------------------------
// GET /_aliases — list all aliases
// ---------------------------------------------------------------------------

/// List all collection aliases.
pub async fn list_aliases(State(state): State<AppState>) -> impl IntoResponse {
    let aliases = state.aliases.read().await;
    let list: Vec<AliasInfoResponse> = aliases
        .values()
        .map(|alias| AliasInfoResponse {
            name: alias.name().to_owned(),
            collections: alias.collections().to_vec(),
        })
        .collect();
    Json(list)
}
