//! Error types for MSearchDB.
//!
//! All errors across the MSearchDB workspace are represented by the unified
//! [`DbError`] enum. Each variant carries a human-readable context string to
//! aid debugging. The [`thiserror`] crate generates `Display` and `Error`
//! implementations automatically from the `#[error(...)]` attributes.
//!
//! # Examples
//!
//! ```
//! use msearchdb_core::error::{DbError, DbResult};
//!
//! fn find_document(id: &str) -> DbResult<String> {
//!     Err(DbError::NotFound(format!("document '{}' not found", id)))
//! }
//!
//! let err = find_document("missing").unwrap_err();
//! assert!(err.to_string().contains("missing"));
//! ```

use thiserror::Error;

/// Unified error type for all MSearchDB operations.
///
/// Marked `#[non_exhaustive]` to allow adding new error variants in future
/// versions without a semver-breaking change.
#[derive(Debug, Error, Clone, PartialEq)]
#[non_exhaustive]
pub enum DbError {
    /// The requested resource was not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// An error occurred in the storage engine.
    #[error("storage error: {0}")]
    StorageError(String),

    /// An error occurred in the indexing engine.
    #[error("index error: {0}")]
    IndexError(String),

    /// A networking or communication error occurred.
    #[error("network error: {0}")]
    NetworkError(String),

    /// A consensus protocol error occurred (e.g., leader election failure).
    #[error("consensus error: {0}")]
    ConsensusError(String),

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    SerializationError(String),

    /// The input provided by the caller was invalid.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A read could not satisfy the requested consistency level.
    #[error("consistency error: {0}")]
    ConsistencyError(String),

    /// A field type conflict was detected during schema evolution.
    ///
    /// Returned when a document contains a field whose type differs from the
    /// type already recorded in the collection's [`FieldMapping`](crate::collection::FieldMapping).
    #[error("schema conflict on field '{field}': existing type '{existing}', incoming type '{incoming}'")]
    SchemaConflict {
        /// The field name that caused the conflict.
        field: String,
        /// The type already recorded in the mapping.
        existing: String,
        /// The type of the incoming value that conflicts.
        incoming: String,
    },

    /// The requested collection already exists.
    #[error("collection already exists: {0}")]
    CollectionAlreadyExists(String),

    /// The requested alias already exists.
    #[error("alias already exists: {0}")]
    AliasAlreadyExists(String),

    /// The requested alias was not found.
    #[error("alias not found: {0}")]
    AliasNotFound(String),
}

/// A convenience alias used throughout MSearchDB.
pub type DbResult<T> = Result<T, DbError>;

// ---------------------------------------------------------------------------
// Conversions from common error types
// ---------------------------------------------------------------------------

impl From<serde_json::Error> for DbError {
    fn from(e: serde_json::Error) -> Self {
        DbError::SerializationError(e.to_string())
    }
}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        DbError::StorageError(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_not_found() {
        let err = DbError::NotFound("document 'abc' not found".into());
        assert_eq!(err.to_string(), "not found: document 'abc' not found");
    }

    #[test]
    fn error_display_storage() {
        let err = DbError::StorageError("disk full".into());
        assert_eq!(err.to_string(), "storage error: disk full");
    }

    #[test]
    fn error_display_index() {
        let err = DbError::IndexError("corrupt segment".into());
        assert_eq!(err.to_string(), "index error: corrupt segment");
    }

    #[test]
    fn error_display_network() {
        let err = DbError::NetworkError("connection refused".into());
        assert_eq!(err.to_string(), "network error: connection refused");
    }

    #[test]
    fn error_display_consensus() {
        let err = DbError::ConsensusError("split brain detected".into());
        assert_eq!(err.to_string(), "consensus error: split brain detected");
    }

    #[test]
    fn error_display_serialization() {
        let err = DbError::SerializationError("invalid JSON".into());
        assert_eq!(err.to_string(), "serialization error: invalid JSON");
    }

    #[test]
    fn error_display_invalid_input() {
        let err = DbError::InvalidInput("empty query".into());
        assert_eq!(err.to_string(), "invalid input: empty query");
    }

    #[test]
    fn error_from_serde_json() {
        let bad_json = "{ not valid json }";
        let serde_err = serde_json::from_str::<serde_json::Value>(bad_json).unwrap_err();
        let db_err: DbError = serde_err.into();
        assert!(matches!(db_err, DbError::SerializationError(_)));
    }

    #[test]
    fn error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let db_err: DbError = io_err.into();
        assert!(matches!(db_err, DbError::StorageError(_)));
        assert!(db_err.to_string().contains("file missing"));
    }

    #[test]
    fn error_display_consistency() {
        let err = DbError::ConsistencyError("quorum not met".into());
        assert_eq!(err.to_string(), "consistency error: quorum not met");
    }

    #[test]
    fn error_display_schema_conflict() {
        let err = DbError::SchemaConflict {
            field: "price".into(),
            existing: "number".into(),
            incoming: "text".into(),
        };
        assert_eq!(
            err.to_string(),
            "schema conflict on field 'price': existing type 'number', incoming type 'text'"
        );
    }

    #[test]
    fn error_display_collection_already_exists() {
        let err = DbError::CollectionAlreadyExists("products".into());
        assert_eq!(err.to_string(), "collection already exists: products");
    }

    #[test]
    fn error_display_alias_already_exists() {
        let err = DbError::AliasAlreadyExists("current".into());
        assert_eq!(err.to_string(), "alias already exists: current");
    }

    #[test]
    fn error_display_alias_not_found() {
        let err = DbError::AliasNotFound("missing_alias".into());
        assert_eq!(err.to_string(), "alias not found: missing_alias");
    }

    #[test]
    fn dbresult_ok_and_err() {
        let ok: DbResult<i32> = Ok(42);
        assert_eq!(ok.unwrap(), 42);

        let err: DbResult<i32> = Err(DbError::NotFound("x".into()));
        assert!(err.is_err());
    }
}
