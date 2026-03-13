//! Core type definitions for the Raft consensus layer.
//!
//! This module defines the **type-state configuration** for openraft via
//! [`declare_raft_types!`], along with the application-specific command
//! ([`RaftCommand`]) and response ([`RaftResponse`]) types.
//!
//! # Type-State Pattern
//!
//! openraft is generic over a *type configuration* trait
//! ([`openraft::RaftTypeConfig`]).  Rather than carrying a dozen generic
//! parameters through every function, the `declare_raft_types!` macro
//! generates a zero-sized struct (`TypeConfig`) that bundles all the
//! associated types in one place.  Every other generic in this crate is
//! parameterized by `TypeConfig` alone.
//!
//! # Examples
//!
//! ```
//! use msearchdb_consensus::types::{RaftCommand, RaftResponse};
//! use msearchdb_core::document::{Document, DocumentId, FieldValue};
//!
//! let cmd = RaftCommand::InsertDocument {
//!     document: Document::new(DocumentId::new("d1"))
//!         .with_field("title", FieldValue::Text("hello".into())),
//! };
//!
//! let json = serde_json::to_string(&cmd).unwrap();
//! let back: RaftCommand = serde_json::from_str(&json).unwrap();
//! assert_eq!(cmd, back);
//! ```

use std::io::Cursor;

use serde::{Deserialize, Serialize};

use msearchdb_core::document::{Document, DocumentId};
use msearchdb_index::schema_builder::SchemaConfig;

// ---------------------------------------------------------------------------
// RaftCommand — application log entry payload
// ---------------------------------------------------------------------------

/// A command that can be proposed through Raft consensus.
///
/// Each variant represents a write operation that must be replicated to a
/// quorum before being applied to the local storage and index.
///
/// Serialized with serde so it can travel inside openraft log entries and
/// across the network during `AppendEntries` RPCs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RaftCommand {
    /// Insert a new document into the database.
    InsertDocument {
        /// The document to insert.
        document: Document,
    },

    /// Delete an existing document by its id.
    DeleteDocument {
        /// The id of the document to remove.
        id: DocumentId,
    },

    /// Update an existing document (full replacement).
    UpdateDocument {
        /// The replacement document — its id determines which document is updated.
        document: Document,
    },

    /// Create a new collection with the given schema.
    CreateCollection {
        /// The name of the collection.
        name: String,
        /// Schema configuration for the collection.
        schema: SchemaConfig,
    },

    /// Delete a collection and all its documents.
    DeleteCollection {
        /// The name of the collection to delete.
        name: String,
    },

    /// Insert a batch of documents atomically.
    ///
    /// Groups up to [`BATCH_SIZE`](crate) documents into a single Raft log
    /// entry to amortise consensus overhead. The state machine applies all
    /// documents to storage and index in one pass, then commits the index.
    BatchInsert {
        /// The documents to insert.
        documents: Vec<Document>,
    },

    /// Create a collection alias that points to one or more collections.
    CreateAlias {
        /// The alias name.
        alias: String,
        /// The collections the alias points to.
        collections: Vec<String>,
    },

    /// Delete a collection alias.
    DeleteAlias {
        /// The alias name to remove.
        alias: String,
    },
}

// ---------------------------------------------------------------------------
// RaftResponse — application response from state machine
// ---------------------------------------------------------------------------

/// The response returned by the state machine after applying a [`RaftCommand`].
///
/// Implements [`openraft::AppDataResponse`] (via the blanket bound
/// `OptionalSend + OptionalSync + Debug + 'static`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RaftResponse {
    /// Whether the command was successfully applied.
    pub success: bool,
    /// The id of the affected document, if applicable.
    pub document_id: Option<DocumentId>,
    /// Number of documents affected (used by batch operations).
    #[serde(default)]
    pub affected_count: usize,
}

impl RaftResponse {
    /// Create a successful response with an associated document id.
    pub fn ok(id: DocumentId) -> Self {
        Self {
            success: true,
            document_id: Some(id),
            affected_count: 1,
        }
    }

    /// Create a successful response without a document id (e.g. for collection ops).
    pub fn ok_no_id() -> Self {
        Self {
            success: true,
            document_id: None,
            affected_count: 0,
        }
    }

    /// Create a failure response.
    pub fn fail() -> Self {
        Self {
            success: false,
            document_id: None,
            affected_count: 0,
        }
    }

    /// Create a successful batch response with the count of inserted documents.
    pub fn ok_batch(count: usize) -> Self {
        Self {
            success: true,
            document_id: None,
            affected_count: count,
        }
    }
}

// ---------------------------------------------------------------------------
// TypeConfig — openraft type-state configuration
// ---------------------------------------------------------------------------

// openraft type-state configuration for MSearchDB.
//
// This zero-sized struct bundles all the associated types required by
// `openraft::RaftTypeConfig` into a single concrete type.
//
// | Associated type | Concrete type                          |
// |-----------------|----------------------------------------|
// | `D`             | `RaftCommand`                          |
// | `R`             | `RaftResponse`                         |
// | `NodeId`        | `u64`                                  |
// | `Node`          | `openraft::BasicNode`                  |
// | `Entry`         | `openraft::Entry<TypeConfig>`          |
// | `SnapshotData`  | `Cursor<Vec<u8>>`                      |
// | `AsyncRuntime`  | `openraft::TokioRuntime`               |
// | `Responder`     | `openraft::impls::OneshotResponder`    |
openraft::declare_raft_types!(
    pub TypeConfig:
        D            = RaftCommand,
        R            = RaftResponse,
        NodeId       = u64,
        Node         = openraft::BasicNode,
        Entry        = openraft::Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
        AsyncRuntime = openraft::TokioRuntime,
        Responder    = openraft::impls::OneshotResponder<TypeConfig>,
);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use msearchdb_core::document::FieldValue;

    #[test]
    fn raft_command_insert_serde_roundtrip() {
        let cmd = RaftCommand::InsertDocument {
            document: Document::new(DocumentId::new("doc-1"))
                .with_field("title", FieldValue::Text("hello".into())),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let back: RaftCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn raft_command_delete_serde_roundtrip() {
        let cmd = RaftCommand::DeleteDocument {
            id: DocumentId::new("doc-2"),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let back: RaftCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn raft_command_update_serde_roundtrip() {
        let cmd = RaftCommand::UpdateDocument {
            document: Document::new(DocumentId::new("doc-3"))
                .with_field("score", FieldValue::Number(9.5)),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let back: RaftCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn raft_command_create_collection_serde_roundtrip() {
        let cmd = RaftCommand::CreateCollection {
            name: "articles".into(),
            schema: SchemaConfig::new(),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let back: RaftCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn raft_command_delete_collection_serde_roundtrip() {
        let cmd = RaftCommand::DeleteCollection {
            name: "articles".into(),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let back: RaftCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn raft_response_constructors() {
        let ok = RaftResponse::ok(DocumentId::new("d1"));
        assert!(ok.success);
        assert_eq!(ok.document_id, Some(DocumentId::new("d1")));

        let ok_no = RaftResponse::ok_no_id();
        assert!(ok_no.success);
        assert!(ok_no.document_id.is_none());

        let fail = RaftResponse::fail();
        assert!(!fail.success);
        assert!(fail.document_id.is_none());
    }

    #[test]
    fn raft_response_serde_roundtrip() {
        let resp = RaftResponse::ok(DocumentId::new("x"));
        let json = serde_json::to_string(&resp).unwrap();
        let back: RaftResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, back);
    }

    #[test]
    fn raft_command_batch_insert_serde_roundtrip() {
        let docs = vec![
            Document::new(DocumentId::new("b1"))
                .with_field("title", FieldValue::Text("first".into())),
            Document::new(DocumentId::new("b2"))
                .with_field("title", FieldValue::Text("second".into())),
        ];
        let cmd = RaftCommand::BatchInsert {
            documents: docs.clone(),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let back: RaftCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn raft_response_batch_constructors() {
        let batch = RaftResponse::ok_batch(100);
        assert!(batch.success);
        assert!(batch.document_id.is_none());
        assert_eq!(batch.affected_count, 100);
    }

    #[test]
    fn raft_command_create_alias_serde_roundtrip() {
        let cmd = RaftCommand::CreateAlias {
            alias: "latest".into(),
            collections: vec!["products_v1".into(), "products_v2".into()],
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let back: RaftCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn raft_command_delete_alias_serde_roundtrip() {
        let cmd = RaftCommand::DeleteAlias {
            alias: "latest".into(),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        let back: RaftCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }
}
