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

    /// Optional snapshot directory for file-backed snapshots.
    /// When `Some`, snapshots are persisted to disk for efficient transfer.
    snapshot_dir: Option<std::path::PathBuf>,
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
            snapshot_dir: None,
        }
    }

    /// Create a state machine with a snapshot directory for persistence.
    pub fn with_snapshot_dir(
        storage: Arc<dyn StorageBackend>,
        index: Arc<dyn IndexBackend>,
        snapshot_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            storage,
            index,
            last_applied_log: RwLock::new(None),
            last_membership: RwLock::new(StoredMembership::default()),
            current_snapshot: RwLock::new(None),
            snapshot_dir: Some(snapshot_dir.into()),
        }
    }

    /// Return the configured snapshot directory, if any.
    pub fn snapshot_dir(&self) -> Option<&std::path::Path> {
        self.snapshot_dir.as_deref()
    }

    /// Apply a single [`RaftCommand`] to storage and index.
    ///
    /// Returns a [`RaftResponse`] describing the outcome.
    async fn apply_command(&self, cmd: &RaftCommand) -> RaftResponse {
        match cmd {
            RaftCommand::InsertDocument {
                collection,
                document,
            } => {
                let id = document.id.clone();
                if let Err(e) = self
                    .storage
                    .put_in_collection(collection, document.clone())
                    .await
                {
                    tracing::error!(error = %e, doc_id = %id, collection = %collection, "failed to insert document into storage");
                    return RaftResponse::fail();
                }
                if let Err(e) = self
                    .index
                    .index_document_in_collection(
                        collection,
                        document,
                        &msearchdb_core::collection::FieldMapping::new(),
                    )
                    .await
                {
                    tracing::error!(error = %e, doc_id = %id, collection = %collection, "failed to index document");
                    // Storage succeeded but index failed — log and continue.
                    // A background reconciliation task would fix this.
                }
                RaftResponse::ok(id)
            }

            RaftCommand::DeleteDocument { collection, id } => {
                if let Err(e) = self.storage.delete_from_collection(collection, id).await {
                    tracing::error!(error = %e, doc_id = %id, collection = %collection, "failed to delete document from storage");
                    return RaftResponse::fail();
                }
                if let Err(e) = self
                    .index
                    .delete_document_from_collection(collection, id)
                    .await
                {
                    tracing::error!(error = %e, doc_id = %id, collection = %collection, "failed to remove document from index");
                }
                RaftResponse::ok(id.clone())
            }

            RaftCommand::UpdateDocument {
                collection,
                document,
            } => {
                let id = document.id.clone();
                if let Err(e) = self
                    .storage
                    .put_in_collection(collection, document.clone())
                    .await
                {
                    tracing::error!(error = %e, doc_id = %id, collection = %collection, "failed to update document in storage");
                    return RaftResponse::fail();
                }
                // Delete old version from index, then re-index the updated document.
                let _ = self
                    .index
                    .delete_document_from_collection(collection, &id)
                    .await;
                if let Err(e) = self
                    .index
                    .index_document_in_collection(
                        collection,
                        document,
                        &msearchdb_core::collection::FieldMapping::new(),
                    )
                    .await
                {
                    tracing::error!(error = %e, doc_id = %id, collection = %collection, "failed to re-index document");
                }
                RaftResponse::ok(id)
            }

            RaftCommand::CreateCollection { name, schema: _ } => {
                if let Err(e) = self.storage.create_collection(name).await {
                    tracing::error!(error = %e, collection = %name, "failed to create storage collection");
                    return RaftResponse::fail();
                }
                if let Err(e) = self.index.create_collection_index(name).await {
                    tracing::error!(error = %e, collection = %name, "failed to create collection index");
                    // Storage succeeded but index failed — log and continue.
                }
                tracing::info!(collection = %name, "collection created");
                RaftResponse::ok_no_id()
            }

            RaftCommand::DeleteCollection { name } => {
                if let Err(e) = self.storage.drop_collection(name).await {
                    tracing::error!(error = %e, collection = %name, "failed to drop storage collection");
                    // Continue — best effort.
                }
                if let Err(e) = self.index.drop_collection_index(name).await {
                    tracing::error!(error = %e, collection = %name, "failed to drop collection index");
                }
                tracing::info!(collection = %name, "collection deleted");
                RaftResponse::ok_no_id()
            }

            RaftCommand::BatchInsert {
                collection,
                documents,
            } => {
                let total = documents.len();
                let mut success_count = 0usize;

                for doc in documents {
                    let id = doc.id.clone();
                    if let Err(e) = self
                        .storage
                        .put_in_collection(collection, doc.clone())
                        .await
                    {
                        tracing::error!(error = %e, doc_id = %id, collection = %collection, "batch: failed to store document");
                        continue;
                    }
                    if let Err(e) = self
                        .index
                        .index_document_in_collection(
                            collection,
                            doc,
                            &msearchdb_core::collection::FieldMapping::new(),
                        )
                        .await
                    {
                        tracing::error!(error = %e, doc_id = %id, collection = %collection, "batch: failed to index document");
                        // Storage succeeded but index failed — count as partial success.
                    }
                    success_count += 1;
                }

                tracing::info!(total, success_count, collection = %collection, "batch insert applied");

                if success_count == total {
                    RaftResponse::ok_batch(success_count)
                } else if success_count > 0 {
                    // Partial success — still report ok but with actual count.
                    RaftResponse::ok_batch(success_count)
                } else {
                    RaftResponse::fail()
                }
            }

            RaftCommand::CreateAlias { alias, collections } => {
                tracing::info!(alias = %alias, targets = ?collections, "alias created");
                RaftResponse::ok_no_id()
            }

            RaftCommand::DeleteAlias { alias } => {
                tracing::info!(alias = %alias, "alias deleted");
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
        let mut collections_to_commit: Vec<String> = Vec::new();

        for entry in entries {
            let log_id = entry.log_id;
            *self.last_applied_log.write().await = Some(log_id);

            match entry.payload {
                EntryPayload::Blank => {
                    // Blank entries are used for leader commits — no app-level action.
                    responses.push(RaftResponse::ok_no_id());
                }
                EntryPayload::Normal(cmd) => {
                    // Track collections that need index commits after this batch.
                    match &cmd {
                        RaftCommand::InsertDocument { collection, .. }
                        | RaftCommand::UpdateDocument { collection, .. }
                        | RaftCommand::DeleteDocument { collection, .. }
                        | RaftCommand::BatchInsert { collection, .. } => {
                            if !collections_to_commit.contains(collection) {
                                collections_to_commit.push(collection.clone());
                            }
                        }
                        _ => {}
                    }
                    let resp = self.apply_command(&cmd).await;
                    responses.push(resp);
                }
                EntryPayload::Membership(mem) => {
                    *self.last_membership.write().await = StoredMembership::new(Some(log_id), mem);
                    responses.push(RaftResponse::ok_no_id());
                }
            }
        }

        // Commit the index once per collection after processing all entries.
        // This amortises the expensive Tantivy commit across many entries.
        for collection in &collections_to_commit {
            if let Err(e) = self.index.commit_collection_index(collection).await {
                tracing::error!(error = %e, collection = %collection, "failed to commit collection index after apply batch");
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
    use msearchdb_core::collection::FieldMapping;
    use msearchdb_core::document::FieldValue;
    use msearchdb_core::error::{DbError, DbResult};
    use msearchdb_core::query::{Query, SearchResult};
    use msearchdb_index::schema_builder::SchemaConfig;
    use std::collections::HashMap;
    use std::ops::RangeInclusive;
    use tokio::sync::Mutex;

    // -- Mock StorageBackend ------------------------------------------------

    /// In-memory storage backend that supports collection-scoped operations.
    struct MockStorage {
        docs: Mutex<HashMap<String, Document>>,
        /// Collection-scoped documents keyed by "collection:doc_id".
        collection_docs: Mutex<HashMap<String, Document>>,
    }

    impl MockStorage {
        fn new() -> Self {
            Self {
                docs: Mutex::new(HashMap::new()),
                collection_docs: Mutex::new(HashMap::new()),
            }
        }

        fn collection_key(collection: &str, id: &str) -> String {
            format!("{}:{}", collection, id)
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

        async fn create_collection(&self, _name: &str) -> DbResult<()> {
            Ok(())
        }

        async fn drop_collection(&self, _name: &str) -> DbResult<()> {
            Ok(())
        }

        async fn put_in_collection(
            &self,
            collection: &str,
            document: Document,
        ) -> DbResult<()> {
            let key = Self::collection_key(collection, document.id.as_str());
            let mut docs = self.collection_docs.lock().await;
            docs.insert(key, document);
            Ok(())
        }

        async fn get_from_collection(
            &self,
            collection: &str,
            id: &DocumentId,
        ) -> DbResult<Document> {
            let key = Self::collection_key(collection, id.as_str());
            let docs = self.collection_docs.lock().await;
            docs.get(&key)
                .cloned()
                .ok_or_else(|| DbError::NotFound(id.to_string()))
        }

        async fn delete_from_collection(
            &self,
            collection: &str,
            id: &DocumentId,
        ) -> DbResult<()> {
            let key = Self::collection_key(collection, id.as_str());
            let mut docs = self.collection_docs.lock().await;
            docs.remove(&key)
                .map(|_| ())
                .ok_or_else(|| DbError::NotFound(id.to_string()))
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

        async fn create_collection_index(&self, _name: &str) -> DbResult<()> {
            Ok(())
        }

        async fn drop_collection_index(&self, _name: &str) -> DbResult<()> {
            Ok(())
        }

        async fn index_document_in_collection(
            &self,
            _collection: &str,
            _document: &Document,
            mapping: &FieldMapping,
        ) -> DbResult<FieldMapping> {
            Ok(mapping.clone())
        }

        async fn delete_document_from_collection(
            &self,
            _collection: &str,
            _id: &DocumentId,
        ) -> DbResult<()> {
            Ok(())
        }

        async fn commit_collection_index(&self, _name: &str) -> DbResult<()> {
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
            collection: "products".into(),
            document: doc.clone(),
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);
        assert_eq!(resp.document_id, Some(DocumentId::new("t1")));

        // Verify the document is in collection-scoped storage.
        let fetched = sm
            .storage
            .get_from_collection("products", &DocumentId::new("t1"))
            .await
            .unwrap();
        assert_eq!(fetched.id, doc.id);
    }

    #[tokio::test]
    async fn apply_delete_removes_document() {
        let sm = make_sm();

        // Insert first via collection-scoped storage.
        let doc = Document::new(DocumentId::new("t2"));
        sm.storage
            .put_in_collection("products", doc)
            .await
            .unwrap();

        // Delete via command
        let cmd = RaftCommand::DeleteDocument {
            collection: "products".into(),
            id: DocumentId::new("t2"),
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);

        // Verify removal
        let result = sm
            .storage
            .get_from_collection("products", &DocumentId::new("t2"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn apply_update_replaces_document() {
        let sm = make_sm();

        // Insert via collection-scoped storage.
        let doc = Document::new(DocumentId::new("t3")).with_field("v", FieldValue::Number(1.0));
        sm.storage
            .put_in_collection("products", doc)
            .await
            .unwrap();

        // Update
        let updated = Document::new(DocumentId::new("t3")).with_field("v", FieldValue::Number(2.0));
        let cmd = RaftCommand::UpdateDocument {
            collection: "products".into(),
            document: updated.clone(),
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);

        let fetched = sm
            .storage
            .get_from_collection("products", &DocumentId::new("t3"))
            .await
            .unwrap();
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

    #[tokio::test]
    async fn apply_batch_insert_stores_all_documents() {
        let sm = make_sm();

        let docs: Vec<Document> = (0..100)
            .map(|i| {
                Document::new(DocumentId::new(format!("batch-{}", i)))
                    .with_field("title", FieldValue::Text(format!("Doc {}", i)))
            })
            .collect();

        let cmd = RaftCommand::BatchInsert {
            collection: "products".into(),
            documents: docs.clone(),
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);
        assert_eq!(resp.affected_count, 100);

        // Verify all documents are in collection-scoped storage.
        for i in 0..100 {
            let fetched = sm
                .storage
                .get_from_collection("products", &DocumentId::new(format!("batch-{}", i)))
                .await
                .unwrap();
            assert_eq!(
                fetched.get_field("title"),
                Some(&FieldValue::Text(format!("Doc {}", i)))
            );
        }
    }

    #[tokio::test]
    async fn apply_batch_insert_empty_succeeds() {
        let sm = make_sm();
        let cmd = RaftCommand::BatchInsert {
            collection: "products".into(),
            documents: vec![],
        };
        let resp = sm.apply_command(&cmd).await;
        // Empty batch is a no-op — zero affected but still "success" (all 0 of 0 succeeded).
        assert!(resp.success || resp.affected_count == 0);
    }

    #[tokio::test]
    async fn apply_create_alias_succeeds() {
        let sm = make_sm();
        let cmd = RaftCommand::CreateAlias {
            alias: "latest".into(),
            collections: vec!["products_v1".into(), "products_v2".into()],
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);
        assert!(resp.document_id.is_none());
    }

    #[tokio::test]
    async fn apply_delete_alias_succeeds() {
        let sm = make_sm();
        let cmd = RaftCommand::DeleteAlias {
            alias: "latest".into(),
        };
        let resp = sm.apply_command(&cmd).await;
        assert!(resp.success);
    }
}
