//! Core trait abstractions for MSearchDB.
//!
//! These traits define the contracts that concrete implementations must fulfill.
//! Using traits rather than concrete types gives us:
//!
//! - **Testability**: Mock implementations can be injected in unit tests.
//! - **Modularity**: Each crate depends on the trait, not on another crate's internals.
//! - **Flexibility**: Swap storage backends (RocksDB vs. in-memory) without
//!   changing consuming code.
//!
//! All async traits use the [`async_trait`] macro, which desugars to
//! `Pin<Box<dyn Future>>` at trait boundaries. When Rust stabilizes native
//! async traits, this can be removed for zero-cost dispatch.

use async_trait::async_trait;
use std::ops::RangeInclusive;

use crate::cluster::NodeStatus;
use crate::collection::FieldMapping;
use crate::document::{Document, DocumentId};
use crate::error::{DbError, DbResult};
use crate::query::{Query, SearchResult};

// ---------------------------------------------------------------------------
// StorageBackend
// ---------------------------------------------------------------------------

/// Persistent key-value storage for documents.
///
/// Implementations might be backed by RocksDB, sled, an in-memory HashMap,
/// or a distributed object store.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Retrieve a document by its unique id.
    ///
    /// Returns `DbError::NotFound` if the document does not exist.
    async fn get(&self, id: &DocumentId) -> DbResult<Document>;

    /// Insert or update a document.
    ///
    /// If a document with the same id already exists, it is replaced.
    async fn put(&self, document: Document) -> DbResult<()>;

    /// Delete a document by its unique id.
    ///
    /// Returns `DbError::NotFound` if the document does not exist.
    async fn delete(&self, id: &DocumentId) -> DbResult<()>;

    /// Scan a range of document ids, returning all matching documents.
    ///
    /// The range is over the string representation of [`DocumentId`].
    /// Useful for pagination, bulk export, and replication catch-up.
    async fn scan(
        &self,
        range: RangeInclusive<DocumentId>,
        limit: usize,
    ) -> DbResult<Vec<Document>>;

    // -- Collection-scoped operations ----------------------------------------

    /// Create storage resources (e.g. a RocksDB column family) for a new
    /// collection.
    ///
    /// The default implementation returns an error indicating the backend
    /// does not support collection isolation.
    async fn create_collection(&self, _collection: &str) -> DbResult<()> {
        Err(DbError::InvalidInput(
            "collection-scoped storage not supported by this backend".into(),
        ))
    }

    /// Drop storage resources for a collection.
    async fn drop_collection(&self, _collection: &str) -> DbResult<()> {
        Err(DbError::InvalidInput(
            "collection-scoped storage not supported by this backend".into(),
        ))
    }

    /// Check whether storage resources exist for the named collection.
    async fn collection_exists(&self, _collection: &str) -> DbResult<bool> {
        Err(DbError::InvalidInput(
            "collection-scoped storage not supported by this backend".into(),
        ))
    }

    /// Retrieve a document by id from a specific collection's storage.
    async fn get_from_collection(
        &self,
        _collection: &str,
        _id: &DocumentId,
    ) -> DbResult<Document> {
        Err(DbError::InvalidInput(
            "collection-scoped storage not supported by this backend".into(),
        ))
    }

    /// Insert or update a document in a specific collection's storage.
    async fn put_in_collection(
        &self,
        _collection: &str,
        _document: Document,
    ) -> DbResult<()> {
        Err(DbError::InvalidInput(
            "collection-scoped storage not supported by this backend".into(),
        ))
    }

    /// Delete a document by id from a specific collection's storage.
    async fn delete_from_collection(
        &self,
        _collection: &str,
        _id: &DocumentId,
    ) -> DbResult<()> {
        Err(DbError::InvalidInput(
            "collection-scoped storage not supported by this backend".into(),
        ))
    }

    /// Scan documents within a specific collection's storage.
    async fn scan_collection(
        &self,
        _collection: &str,
        _range: RangeInclusive<DocumentId>,
        _limit: usize,
    ) -> DbResult<Vec<Document>> {
        Err(DbError::InvalidInput(
            "collection-scoped storage not supported by this backend".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// IndexBackend
// ---------------------------------------------------------------------------

/// Full-text search index.
///
/// Implementations handle tokenization, inverted index maintenance, and
/// query scoring (e.g., TF-IDF, BM25).
#[async_trait]
pub trait IndexBackend: Send + Sync {
    /// Add or update a document in the search index.
    ///
    /// The implementation decides which fields to index and how to tokenize them.
    async fn index_document(&self, document: &Document) -> DbResult<()>;

    /// Execute a search query and return scored results.
    async fn search(&self, query: &Query) -> DbResult<SearchResult>;

    /// Remove a document from the search index.
    async fn delete_document(&self, id: &DocumentId) -> DbResult<()>;

    /// Commit buffered writes to make them visible to searchers.
    ///
    /// Not all implementations buffer writes; the default is a no-op.
    /// Tantivy batches writes in memory until `commit()` flushes them to
    /// disk segments and reloads the reader.
    async fn commit_index(&self) -> DbResult<()> {
        Ok(())
    }

    // -- Collection-scoped operations ----------------------------------------

    /// Create a new search index for the named collection.
    ///
    /// The default implementation returns an error indicating the backend
    /// does not support per-collection indices.
    async fn create_collection_index(&self, _collection: &str) -> DbResult<()> {
        Err(DbError::InvalidInput(
            "collection-scoped index not supported by this backend".into(),
        ))
    }

    /// Drop the search index for the named collection.
    async fn drop_collection_index(&self, _collection: &str) -> DbResult<()> {
        Err(DbError::InvalidInput(
            "collection-scoped index not supported by this backend".into(),
        ))
    }

    /// Index a document within a specific collection's index.
    ///
    /// The implementation should use the collection's [`FieldMapping`] to
    /// auto-detect and register field types (dynamic mapping).  Returns
    /// an updated mapping if new fields were discovered.
    async fn index_document_in_collection(
        &self,
        _collection: &str,
        _document: &Document,
        _mapping: &FieldMapping,
    ) -> DbResult<FieldMapping> {
        Err(DbError::InvalidInput(
            "collection-scoped index not supported by this backend".into(),
        ))
    }

    /// Search within a specific collection's index.
    async fn search_collection(
        &self,
        _collection: &str,
        _query: &Query,
    ) -> DbResult<SearchResult> {
        Err(DbError::InvalidInput(
            "collection-scoped index not supported by this backend".into(),
        ))
    }

    /// Delete a document from a specific collection's index.
    async fn delete_document_from_collection(
        &self,
        _collection: &str,
        _id: &DocumentId,
    ) -> DbResult<()> {
        Err(DbError::InvalidInput(
            "collection-scoped index not supported by this backend".into(),
        ))
    }

    /// Commit buffered writes for a specific collection's index.
    async fn commit_collection_index(&self, _collection: &str) -> DbResult<()> {
        Err(DbError::InvalidInput(
            "collection-scoped index not supported by this backend".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// ReplicationLog
// ---------------------------------------------------------------------------

/// An append-only log used for Raft consensus and replication.
///
/// Each entry in the log is a serialized command (e.g., "put document X").
/// The log supports appending new entries, reading a range, and committing
/// entries that have been replicated to a quorum.
#[async_trait]
pub trait ReplicationLog: Send + Sync {
    /// Append a new entry to the log. Returns the index of the appended entry.
    async fn append(&self, entry: Vec<u8>) -> DbResult<u64>;

    /// Read log entries in the range `[start_index, end_index]` inclusive.
    async fn read(&self, start_index: u64, end_index: u64) -> DbResult<Vec<Vec<u8>>>;

    /// Mark all entries up to and including `index` as committed.
    ///
    /// Committed entries are safe to apply to the state machine.
    async fn commit(&self, index: u64) -> DbResult<()>;
}

// ---------------------------------------------------------------------------
// HealthCheck
// ---------------------------------------------------------------------------

/// Health and status reporting for a node or subsystem.
///
/// This is a synchronous trait (no async) because health checks should be
/// fast, non-blocking operations that inspect cached state.
pub trait HealthCheck: Send + Sync {
    /// Returns `true` if the component is healthy and ready to serve requests.
    fn is_healthy(&self) -> bool;

    /// Returns the current operational status of this node.
    fn status(&self) -> NodeStatus;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::FieldValue;
    use crate::error::DbError;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // -- Mock implementations to verify trait ergonomics --

    /// In-memory storage backend for testing.
    struct MemoryStorage {
        docs: Mutex<HashMap<String, Document>>,
    }

    impl MemoryStorage {
        fn new() -> Self {
            Self {
                docs: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl StorageBackend for MemoryStorage {
        async fn get(&self, id: &DocumentId) -> DbResult<Document> {
            let docs = self.docs.lock().unwrap();
            docs.get(id.as_str())
                .cloned()
                .ok_or_else(|| DbError::NotFound(format!("document '{}' not found", id)))
        }

        async fn put(&self, document: Document) -> DbResult<()> {
            let mut docs = self.docs.lock().unwrap();
            docs.insert(document.id.as_str().to_owned(), document);
            Ok(())
        }

        async fn delete(&self, id: &DocumentId) -> DbResult<()> {
            let mut docs = self.docs.lock().unwrap();
            docs.remove(id.as_str())
                .map(|_| ())
                .ok_or_else(|| DbError::NotFound(format!("document '{}' not found", id)))
        }

        async fn scan(
            &self,
            range: RangeInclusive<DocumentId>,
            limit: usize,
        ) -> DbResult<Vec<Document>> {
            let docs = self.docs.lock().unwrap();
            let start = range.start().as_str();
            let end = range.end().as_str();
            let mut results: Vec<Document> = docs
                .values()
                .filter(|d| {
                    let key = d.id.as_str();
                    key >= start && key <= end
                })
                .cloned()
                .collect();
            results.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
            results.truncate(limit);
            Ok(results)
        }
    }

    /// Trivial health check implementation.
    struct AlwaysHealthy;

    impl HealthCheck for AlwaysHealthy {
        fn is_healthy(&self) -> bool {
            true
        }

        fn status(&self) -> NodeStatus {
            NodeStatus::Leader
        }
    }

    #[tokio::test]
    async fn storage_backend_put_get_delete() {
        let store = MemoryStorage::new();

        let doc = Document::new(DocumentId::new("test-1"))
            .with_field("title", FieldValue::Text("hello".into()));

        // Put
        store.put(doc.clone()).await.unwrap();

        // Get
        let fetched = store.get(&DocumentId::new("test-1")).await.unwrap();
        assert_eq!(fetched.id, doc.id);
        assert_eq!(fetched.get_field("title"), doc.get_field("title"));

        // Delete
        store.delete(&DocumentId::new("test-1")).await.unwrap();
        let result = store.get(&DocumentId::new("test-1")).await;
        assert!(matches!(result, Err(DbError::NotFound(_))));
    }

    #[tokio::test]
    async fn storage_backend_get_missing_returns_not_found() {
        let store = MemoryStorage::new();
        let result = store.get(&DocumentId::new("missing")).await;
        assert!(matches!(result, Err(DbError::NotFound(_))));
    }

    #[tokio::test]
    async fn storage_backend_scan() {
        let store = MemoryStorage::new();

        for i in 0..5 {
            let doc = Document::new(DocumentId::new(format!("doc-{}", i)));
            store.put(doc).await.unwrap();
        }

        let range = DocumentId::new("doc-1")..=DocumentId::new("doc-3");
        let results = store.scan(range, 10).await.unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].id.as_str(), "doc-1");
        assert_eq!(results[2].id.as_str(), "doc-3");
    }

    #[tokio::test]
    async fn storage_backend_scan_with_limit() {
        let store = MemoryStorage::new();

        for i in 0..10 {
            let doc = Document::new(DocumentId::new(format!("doc-{:02}", i)));
            store.put(doc).await.unwrap();
        }

        let range = DocumentId::new("doc-00")..=DocumentId::new("doc-99");
        let results = store.scan(range, 3).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn health_check_trait() {
        let hc = AlwaysHealthy;
        assert!(hc.is_healthy());
        assert_eq!(hc.status(), NodeStatus::Leader);
    }

    // Verify that trait objects can be constructed (object safety).
    #[test]
    fn traits_are_object_safe() {
        fn _takes_storage(_: &dyn StorageBackend) {}
        fn _takes_index(_: &dyn IndexBackend) {}
        fn _takes_repl(_: &dyn ReplicationLog) {}
        fn _takes_health(_: &dyn HealthCheck) {}
    }
}
