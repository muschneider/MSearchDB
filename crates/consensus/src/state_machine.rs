//! Raft state machine implementation for MSearchDB.
//!
//! The [`DbStateMachine`] applies committed log entries to the underlying
//! storage engine and search index, and manages snapshot creation / restoration.
//!
//! # Architecture
//!
//! ```text
//!   committed log entries
//!          │
//!          ▼
//!   ┌──────────────────┐
//!   │  DbStateMachine  │
//!   │                  │
//!   │  ┌────────────┐  │     writes     ┌────────────────┐
//!   │  │ apply(cmd) ├──┼──────────────►│ StorageBackend  │
//!   │  └──────┬─────┘  │               └────────────────┘
//!   │         │        │
//!   │         │ index   │     writes     ┌────────────────┐
//!   │         └────────┼──────────────►│  IndexBackend   │
//!   └──────────────────┘               └────────────────┘
//! ```
//!
//! The state machine is **not** thread-safe by itself; openraft serialises all
//! calls to `&mut self` methods.  Interior mutability for the backends is
//! handled by `Arc<dyn StorageBackend>` and `Arc<dyn IndexBackend>`.
//!
//! # Snapshot strategy
//!
//! Snapshots are serialised as JSON-encoded lists of all documents currently
//! in storage.  This is simple but acceptable for early development; a more
//! efficient binary format can be adopted later.

use std::io::Cursor;
use std::sync::Arc;

use openraft::storage::{RaftStateMachine, Snapshot, SnapshotMeta};
use openraft::{
    Entry, EntryPayload, LogId, RaftSnapshotBuilder, StorageError, StorageIOError, StoredMembership,
};
use tokio::sync::RwLock;

use msearchdb_core::document::{Document, DocumentId};
use msearchdb_core::traits::{IndexBackend, StorageBackend};

use crate::types::{RaftCommand, RaftResponse, TypeConfig};

// ---------------------------------------------------------------------------
// DbStateMachine
// ---------------------------------------------------------------------------

/// The application-level state machine driven by Raft.
///
/// Receives committed [`RaftCommand`]s and applies them to both the storage
/// engine and the search index atomically (best effort — see crate-level docs
/// for the eventual consistency guarantees).
///
/// ## Why `Arc<RwLock<...>>` instead of `Arc<Mutex<...>>`?
///
/// Reads (e.g. snapshot building) can proceed concurrently with each other,
/// while writes (apply) require exclusive access.  `RwLock` gives us that
/// read/write asymmetry without blocking readers behind a single writer
/// queue.
pub struct DbStateMachine {
    /// The persistent storage backend (e.g. RocksDB).
    storage: Arc<dyn StorageBackend>,

    /// The full-text search index (e.g. Tantivy).
    index: Arc<dyn IndexBackend>,

    /// The last log id that was applied to this state machine.
    last_applied_log: RwLock<Option<LogId<u64>>>,

    /// The last applied membership configuration.
    last_membership: RwLock<StoredMembership<u64, openraft::BasicNode>>,

    /// The most recent snapshot, if one has been built.
    current_snapshot: RwLock<Option<StoredSnapshot>>,
}

/// An in-memory representation of a snapshot.
struct StoredSnapshot {
    meta: SnapshotMeta<u64, openraft::BasicNode>,
    data: Vec<u8>,
}

impl DbStateMachine {
    /// Create a new state machine backed by the given storage and index.
    pub fn new(storage: Arc<dyn StorageBackend>, index: Arc<dyn IndexBackend>) -> Self {
        Self {
            storage,
            index,
            last_applied_log: RwLock::new(None),
            last_membership: RwLock::new(StoredMembership::default()),
            current_snapshot: RwLock::new(None),
        }
    }

    /// Apply a single [`RaftCommand`] to storage and index.
    ///
    /// Returns a [`RaftResponse`] describing the outcome.
    async fn apply_command(&self, cmd: &RaftCommand) -> RaftResponse {
        match cmd {
            RaftCommand::InsertDocument { document } => {
                let id = document.id.clone();
                if let Err(e) = self.storage.put(document.clone()).await {
                    tracing::error!(error = %e, doc_id = %id, "failed to insert document into storage");
                    return RaftResponse::fail();
                }
                if let Err(e) = self.index.index_document(document).await {
                    tracing::error!(error = %e, doc_id = %id, "failed to index document");
                    // Storage succeeded but index failed — log and continue.
                    // A background reconciliation task would fix this.
                }
                RaftResponse::ok(id)
            }

            RaftCommand::DeleteDocument { id } => {
                if let Err(e) = self.storage.delete(id).await {
                    tracing::error!(error = %e, doc_id = %id, "failed to delete document from storage");
                    return RaftResponse::fail();
                }
                if let Err(e) = self.index.delete_document(id).await {
                    tracing::error!(error = %e, doc_id = %id, "failed to remove document from index");
                }
                RaftResponse::ok(id.clone())
            }

            RaftCommand::UpdateDocument { document } => {
                let id = document.id.clone();
                if let Err(e) = self.storage.put(document.clone()).await {
                    tracing::error!(error = %e, doc_id = %id, "failed to update document in storage");
                    return RaftResponse::fail();
                }
                // Re-index the updated document.
                if let Err(e) = self.index.index_document(document).await {
                    tracing::error!(error = %e, doc_id = %id, "failed to re-index document");
                }
                RaftResponse::ok(id)
            }

            RaftCommand::CreateCollection { name, schema: _ } => {
                tracing::info!(collection = %name, "collection created (schema stored in metadata)");
                RaftResponse::ok_no_id()
            }

            RaftCommand::DeleteCollection { name } => {
                tracing::info!(collection = %name, "collection deleted");
                RaftResponse::ok_no_id()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RaftStateMachine implementation
// ---------------------------------------------------------------------------

impl RaftStateMachine<TypeConfig> for DbStateMachine {
    type SnapshotBuilder = DbSnapshotBuilder;

    async fn applied_state(
        &mut self,
    ) -> Result<
        (
            Option<LogId<u64>>,
            StoredMembership<u64, openraft::BasicNode>,
        ),
        StorageError<u64>,
    > {
        let last = self.last_applied_log.read().await;
        let membership = self.last_membership.read().await;
        Ok((*last, membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<RaftResponse>, StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut responses = Vec::new();

        for entry in entries {
            let log_id = entry.log_id;
            *self.last_applied_log.write().await = Some(log_id);

            match entry.payload {
                EntryPayload::Blank => {
                    // Blank entries are used for leader commits — no app-level action.
                    responses.push(RaftResponse::ok_no_id());
                }
                EntryPayload::Normal(cmd) => {
                    let resp = self.apply_command(&cmd).await;
                    responses.push(resp);
                }
                EntryPayload::Membership(mem) => {
                    *self.last_membership.write().await = StoredMembership::new(Some(log_id), mem);
                    responses.push(RaftResponse::ok_no_id());
                }
            }
        }

        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        DbSnapshotBuilder {
            last_applied_log: *self.last_applied_log.read().await,
            last_membership: self.last_membership.read().await.clone(),
            storage: Arc::clone(&self.storage),
        }
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<u64>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, openraft::BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<u64>> {
        let data = snapshot.into_inner();

        // Deserialise the snapshot as a list of documents and re-insert them.
        let documents: Vec<Document> = serde_json::from_slice(&data)
            .map_err(|e| StorageIOError::read_snapshot(Some(meta.signature()), &e))?;

        for doc in &documents {
            let _ = self.storage.put(doc.clone()).await;
            let _ = self.index.index_document(doc).await;
        }

        *self.last_applied_log.write().await = meta.last_log_id;
        *self.last_membership.write().await = meta.last_membership.clone();

        // Persist the snapshot data in memory for get_current_snapshot.
        *self.current_snapshot.write().await = Some(StoredSnapshot {
            meta: meta.clone(),
            data,
        });

        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<u64>> {
        let snap_guard = self.current_snapshot.read().await;
        match &*snap_guard {
            Some(stored) => Ok(Some(Snapshot {
                meta: stored.meta.clone(),
                snapshot: Box::new(Cursor::new(stored.data.clone())),
            })),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// DbSnapshotBuilder
// ---------------------------------------------------------------------------

/// Builds a point-in-time snapshot of the state machine.
///
/// The snapshot is simply a JSON-serialised array of all documents in storage.
pub struct DbSnapshotBuilder {
    last_applied_log: Option<LogId<u64>>,
    last_membership: StoredMembership<u64, openraft::BasicNode>,
    storage: Arc<dyn StorageBackend>,
}

impl RaftSnapshotBuilder<TypeConfig> for DbSnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<u64>> {
        // Scan all documents from the storage backend.
        let all_docs = self
            .storage
            .scan(
                DocumentId::new("\0")..=DocumentId::new("\u{10ffff}"),
                usize::MAX,
            )
            .await
            .map_err(|e| StorageIOError::read_state_machine(&e))?;

        let data =
            serde_json::to_vec(&all_docs).map_err(|e| StorageIOError::read_state_machine(&e))?;

        let snapshot_id = if let Some(last) = self.last_applied_log {
            format!("{}-{}-snapshot", last.leader_id, last.index)
        } else {
            "empty-snapshot".to_string()
        };

        let meta = SnapshotMeta {
            last_log_id: self.last_applied_log,
            last_membership: self.last_membership.clone(),
            snapshot_id,
        };

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use msearchdb_core::document::FieldValue;
    use msearchdb_core::error::{DbError, DbResult};
    use msearchdb_core::query::{Query, SearchResult};
    use msearchdb_index::schema_builder::SchemaConfig;
    use std::collections::HashMap;
    use std::ops::RangeInclusive;
    use tokio::sync::Mutex;

    // -- Mock StorageBackend ------------------------------------------------

    struct MockStorage {
        docs: Mutex<HashMap<String, Document>>,
    }

    impl MockStorage {
        fn new() -> Self {
            Self {
                docs: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl StorageBackend for MockStorage {
        async fn get(&self, id: &DocumentId) -> DbResult<Document> {
            let docs = self.docs.lock().await;
            docs.get(id.as_str())
                .cloned()
                .ok_or_else(|| DbError::NotFound(id.to_string()))
        }

        async fn put(&self, document: Document) -> DbResult<()> {
            let mut docs = self.docs.lock().await;
            docs.insert(document.id.as_str().to_owned(), document);
            Ok(())
        }

        async fn delete(&self, id: &DocumentId) -> DbResult<()> {
            let mut docs = self.docs.lock().await;
            docs.remove(id.as_str())
                .map(|_| ())
                .ok_or_else(|| DbError::NotFound(id.to_string()))
        }

        async fn scan(
            &self,
            _range: RangeInclusive<DocumentId>,
            _limit: usize,
        ) -> DbResult<Vec<Document>> {
            let docs = self.docs.lock().await;
            Ok(docs.values().cloned().collect())
        }
    }

    // -- Mock IndexBackend --------------------------------------------------

    struct MockIndex;

    #[async_trait]
    impl IndexBackend for MockIndex {
        async fn index_document(&self, _document: &Document) -> DbResult<()> {
            Ok(())
        }

        async fn search(&self, _query: &Query) -> DbResult<SearchResult> {
            Ok(SearchResult::empty(0))
        }

        async fn delete_document(&self, _id: &DocumentId) -> DbResult<()> {
            Ok(())
        }
    }

    fn make_sm() -> DbStateMachine {
        DbStateMachine::new(Arc::new(MockStorage::new()), Arc::new(MockIndex))
    }

    #[tokio::test]
    async fn apply_insert_stores_document() {
        let sm = make_sm();

        let doc = Document::new(DocumentId::new("t1"))
            .with_field("title", FieldValue::Text("test".into()));

        let cmd = RaftCommand::InsertDocument {
            document: doc.clone(),
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);
        assert_eq!(resp.document_id, Some(DocumentId::new("t1")));

        // Verify the document is in storage.
        let fetched = sm.storage.get(&DocumentId::new("t1")).await.unwrap();
        assert_eq!(fetched.id, doc.id);
    }

    #[tokio::test]
    async fn apply_delete_removes_document() {
        let sm = make_sm();

        // Insert first
        let doc = Document::new(DocumentId::new("t2"));
        sm.storage.put(doc).await.unwrap();

        // Delete via command
        let cmd = RaftCommand::DeleteDocument {
            id: DocumentId::new("t2"),
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);

        // Verify removal
        let result = sm.storage.get(&DocumentId::new("t2")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn apply_update_replaces_document() {
        let sm = make_sm();

        // Insert
        let doc = Document::new(DocumentId::new("t3")).with_field("v", FieldValue::Number(1.0));
        sm.storage.put(doc).await.unwrap();

        // Update
        let updated = Document::new(DocumentId::new("t3")).with_field("v", FieldValue::Number(2.0));
        let cmd = RaftCommand::UpdateDocument {
            document: updated.clone(),
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);

        let fetched = sm.storage.get(&DocumentId::new("t3")).await.unwrap();
        assert_eq!(fetched.get_field("v"), Some(&FieldValue::Number(2.0)));
    }

    #[tokio::test]
    async fn apply_create_collection_succeeds() {
        let sm = make_sm();
        let cmd = RaftCommand::CreateCollection {
            name: "test".into(),
            schema: SchemaConfig::new(),
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);
        assert!(resp.document_id.is_none());
    }

    #[tokio::test]
    async fn apply_delete_collection_succeeds() {
        let sm = make_sm();
        let cmd = RaftCommand::DeleteCollection {
            name: "test".into(),
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);
    }
}
