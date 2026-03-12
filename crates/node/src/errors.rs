//! Error conversion utilities for the REST API.
//!
//! Maps [`DbError`] variants to appropriate HTTP status codes and
//! [`ErrorResponse`] bodies.
//!
//! # Mapping
//!
//! | `DbError` variant       | HTTP status |
//! |--------------------------|-------------|
//! | `NotFound`               | 404         |
//! | `InvalidInput`           | 400         |
//! | `SerializationError`     | 400         |
//! | `ConsensusError` (fwd)   | 307         |
//! | `ConsensusError` (other) | 503         |
//! | `NetworkError`           | 502         |
//! | `StorageError`           | 500         |
//! | `IndexError`             | 500         |

use axum::http::StatusCode;

use msearchdb_core::error::DbError;

use crate::dto::ErrorResponse;

/// Convert a [`DbError`] into an HTTP status code and [`ErrorResponse`].
///
/// The `From` impl between API DTOs and domain error types ensures a
/// consistent error format across all endpoints.
pub fn db_error_to_response(err: DbError) -> (StatusCode, ErrorResponse) {
    match &err {
        DbError::NotFound(msg) => (StatusCode::NOT_FOUND, ErrorResponse::not_found(msg.clone())),
        DbError::InvalidInput(msg) => (
            StatusCode::BAD_REQUEST,
            ErrorResponse::bad_request(msg.clone()),
        ),
        DbError::SerializationError(msg) => (
            StatusCode::BAD_REQUEST,
            ErrorResponse::bad_request(msg.clone()),
        ),
        DbError::ConsensusError(msg) if msg.contains("not leader") || msg.contains("forward") => {
            // The node is not the leader — return 307 with a hint.
            (
                StatusCode::TEMPORARY_REDIRECT,
                ErrorResponse::redirect(msg.clone()),
            )
        }
        DbError::ConsensusError(msg) => (
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorResponse::service_unavailable(msg.clone()),
        ),
        DbError::ConsistencyError(msg) => (
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorResponse::service_unavailable(msg.clone()),
        ),
        DbError::NetworkError(msg) => (
            StatusCode::BAD_GATEWAY,
            ErrorResponse::new(502, "network_error", msg.clone()),
        ),
        DbError::StorageError(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorResponse::internal(msg.clone()),
        ),
        DbError::IndexError(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorResponse::internal(msg.clone()),
        ),
        // Catch-all for future non_exhaustive variants.
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorResponse::internal(err.to_string()),
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_maps_to_404() {
        let (status, resp) = db_error_to_response(DbError::NotFound("gone".into()));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(resp.status, 404);
        assert_eq!(resp.error.error_type, "not_found");
    }

    #[test]
    fn invalid_input_maps_to_400() {
        let (status, resp) = db_error_to_response(DbError::InvalidInput("bad".into()));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn serialization_error_maps_to_400() {
        let (status, resp) = db_error_to_response(DbError::SerializationError("parse fail".into()));
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn consensus_forward_maps_to_307() {
        let (status, resp) = db_error_to_response(DbError::ConsensusError(
            "not leader; forward to node-2".into(),
        ));
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(resp.status, 307);
    }

    #[test]
    fn consensus_other_maps_to_503() {
        let (status, resp) = db_error_to_response(DbError::ConsensusError("split brain".into()));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.status, 503);
    }

    #[test]
    fn network_error_maps_to_502() {
        let (status, resp) = db_error_to_response(DbError::NetworkError("timeout".into()));
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(resp.status, 502);
    }

    #[test]
    fn storage_error_maps_to_500() {
        let (status, resp) = db_error_to_response(DbError::StorageError("disk full".into()));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(resp.status, 500);
    }

    #[test]
    fn index_error_maps_to_500() {
        let (status, resp) = db_error_to_response(DbError::IndexError("corrupt".into()));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(resp.status, 500);
    }
}
