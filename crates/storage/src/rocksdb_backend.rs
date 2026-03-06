//! RocksDB-backed persistent storage engine for MSearchDB.
//!
//! This module provides [`RocksDbStorage`], a production-grade storage backend
//! that wraps RocksDB via [`Arc<DB>`] for safe sharing across async tasks.
//! Blocking RocksDB calls are offloaded to [`tokio::task::spawn_blocking`] so
//! they do not block the async runtime.
//!
//! # Column Families
//!
//! Three column families partition the keyspace:
//!
//! - **`documents`** — serialized [`Document`] values keyed by [`DocumentId`].
//! - **`metadata`** — auxiliary metadata (e.g., schema versions, stats).
//! - **`sequence`** — monotonically increasing sequence counter for ordering.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//! use msearchdb_storage::rocksdb_backend::{RocksDbStorage, StorageOptions};
//!
//! # async fn example() -> msearchdb_core::error::DbResult<()> {
//! let storage = RocksDbStorage::new(
//!     Path::new("/tmp/msearchdb-data"),
//!     StorageOptions::default(),
//! )?;
//!
//! // Use via the StorageBackend trait or direct methods.
//! # Ok(())
//! # }
//! ```

use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rocksdb::{
    BlockBasedOptions, ColumnFamilyDescriptor, DBCompressionType, IteratorMode, Options,
    ReadOptions, WriteBatch, DB,
};
use serde::{Deserialize, Serialize};

use msearchdb_core::document::{Document, DocumentId};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::traits::StorageBackend;

// ---------------------------------------------------------------------------
// Column-family names
// ---------------------------------------------------------------------------

/// Column family for serialized documents.
pub const CF_DOCUMENTS: &str = "documents";

/// Column family for metadata entries.
pub const CF_METADATA: &str = "metadata";

/// Column family for the sequence counter.
pub const CF_SEQUENCE: &str = "sequence";

/// All column-family names used by MSearchDB.
const COLUMN_FAMILIES: [&str; 3] = [CF_DOCUMENTS, CF_METADATA, CF_SEQUENCE];

// ---------------------------------------------------------------------------
// StorageOptions
// ---------------------------------------------------------------------------

/// Tuning knobs for the RocksDB storage engine.
///
/// Sensible defaults are provided via [`Default`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StorageOptions {
    /// Write buffer size in megabytes (default: 64).
    pub write_buffer_mb: usize,
    /// Maximum number of open file descriptors (default: 1000).
    pub max_open_files: i32,
    /// Enable Snappy compression (default: true).
    pub compression: bool,
    /// Bloom filter bits per key (default: 10).
    pub bloom_filter_bits: i32,
}

impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            write_buffer_mb: 64,
            max_open_files: 1000,
            compression: true,
            bloom_filter_bits: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// RocksDbStorage
// ---------------------------------------------------------------------------

/// Persistent storage engine backed by RocksDB.
///
/// Wraps [`Arc<DB>`] so the handle can be cheaply cloned and shared across
/// async tasks. Blocking RocksDB operations are dispatched to the Tokio
/// blocking thread pool via [`tokio::task::spawn_blocking`].
pub struct RocksDbStorage {
    /// Shared RocksDB handle.
    db: Arc<DB>,
    /// Monotonically increasing sequence number for ordering writes.
    sequence: AtomicU64,
}

impl RocksDbStorage {
    /// Open (or create) a RocksDB database at `path` with the given options.
    ///
    /// Configures column families, compression, bloom filters, and write
    /// buffer sizes according to [`StorageOptions`].
    pub fn new(path: &Path, options: StorageOptions) -> DbResult<Self> {
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        db_opts.set_write_buffer_size(options.write_buffer_mb * 1024 * 1024);
        db_opts.set_max_open_files(options.max_open_files);

        if options.compression {
            db_opts.set_compression_type(DBCompressionType::Snappy);
        } else {
            db_opts.set_compression_type(DBCompressionType::None);
        }

        // Column-family descriptors — each gets bloom filters.
        let cf_descriptors: Vec<ColumnFamilyDescriptor> = COLUMN_FAMILIES
            .iter()
            .map(|name| {
                let mut cf_opts = Options::default();
                let mut block_opts = BlockBasedOptions::default();
                block_opts.set_bloom_filter(options.bloom_filter_bits as f64, false);
                cf_opts.set_block_based_table_factory(&block_opts);
                ColumnFamilyDescriptor::new(*name, cf_opts)
            })
            .collect();

        let db = DB::open_cf_descriptors(&db_opts, path, cf_descriptors)
            .map_err(|e| DbError::StorageError(e.to_string()))?;

        // Recover the sequence counter from the "sequence" CF.
        let seq = {
            let cf = db
                .cf_handle(CF_SEQUENCE)
                .ok_or_else(|| DbError::StorageError("missing sequence CF".into()))?;
            match db.get_cf(&cf, b"current_seq") {
                Ok(Some(bytes)) => {
                    let arr: [u8; 8] = bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| DbError::StorageError("corrupt sequence value".into()))?;
                    u64::from_le_bytes(arr)
                }
                Ok(None) => 0,
                Err(e) => return Err(DbError::StorageError(e.to_string())),
            }
        };

        tracing::info!(
            path = %path.display(),
            sequence = seq,
            "opened RocksDB storage"
        );

        Ok(Self {
            db: Arc::new(db),
            sequence: AtomicU64::new(seq),
        })
    }

    // -- Low-level column-family operations ----------------------------------

    /// Put a raw key-value pair into the named column family.
    ///
    /// Uses a [`WriteBatch`] for atomicity and increments the internal
    /// sequence counter.
    pub async fn put(&self, cf_name: &str, key: &[u8], value: &[u8]) -> DbResult<()> {
        let db = Arc::clone(&self.db);
        let cf_name = cf_name.to_owned();
        let key = key.to_vec();
        let value = value.to_vec();
        let seq = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(&cf_name)
                .ok_or_else(|| DbError::StorageError(format!("unknown CF: {}", cf_name)))?;
            let seq_cf = db
                .cf_handle(CF_SEQUENCE)
                .ok_or_else(|| DbError::StorageError("missing sequence CF".into()))?;

            let mut batch = WriteBatch::default();
            batch.put_cf(&cf, &key, &value);
            batch.put_cf(&seq_cf, b"current_seq", seq.to_le_bytes());
            db.write(batch)
                .map_err(|e| DbError::StorageError(e.to_string()))
        })
        .await
        .map_err(|e| DbError::StorageError(format!("spawn_blocking join error: {}", e)))?
    }

    /// Get a raw value by key from the named column family.
    ///
    /// Uses [`ReadOptions`] with `fill_cache = true` for hot-path reads.
    pub async fn get(&self, cf_name: &str, key: &[u8]) -> DbResult<Option<Vec<u8>>> {
        let db = Arc::clone(&self.db);
        let cf_name = cf_name.to_owned();
        let key = key.to_vec();

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(&cf_name)
                .ok_or_else(|| DbError::StorageError(format!("unknown CF: {}", cf_name)))?;

            let mut read_opts = ReadOptions::default();
            read_opts.set_verify_checksums(true);
            read_opts.fill_cache(true);

            db.get_cf_opt(&cf, &key, &read_opts)
                .map_err(|e| DbError::StorageError(e.to_string()))
        })
        .await
        .map_err(|e| DbError::StorageError(format!("spawn_blocking join error: {}", e)))?
    }

    /// Delete a key from the named column family.
    pub async fn delete(&self, cf_name: &str, key: &[u8]) -> DbResult<()> {
        let db = Arc::clone(&self.db);
        let cf_name = cf_name.to_owned();
        let key = key.to_vec();

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(&cf_name)
                .ok_or_else(|| DbError::StorageError(format!("unknown CF: {}", cf_name)))?;
            db.delete_cf(&cf, &key)
                .map_err(|e| DbError::StorageError(e.to_string()))
        })
        .await
        .map_err(|e| DbError::StorageError(format!("spawn_blocking join error: {}", e)))?
    }

    /// Scan keys with a given prefix in the named column family.
    ///
    /// Returns up to `limit` key-value pairs whose key starts with `prefix`.
    pub async fn scan(
        &self,
        cf_name: &str,
        prefix: &[u8],
        limit: usize,
    ) -> DbResult<Vec<(Vec<u8>, Vec<u8>)>> {
        let db = Arc::clone(&self.db);
        let cf_name = cf_name.to_owned();
        let prefix = prefix.to_vec();

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(&cf_name)
                .ok_or_else(|| DbError::StorageError(format!("unknown CF: {}", cf_name)))?;

            let iter = db.iterator_cf(
                &cf,
                IteratorMode::From(&prefix, rocksdb::Direction::Forward),
            );

            let mut results = Vec::new();
            for item in iter {
                let (k, v) = item.map_err(|e| DbError::StorageError(e.to_string()))?;
                if !k.starts_with(&prefix) {
                    break;
                }
                results.push((k.to_vec(), v.to_vec()));
                if results.len() >= limit {
                    break;
                }
            }
            Ok(results)
        })
        .await
        .map_err(|e| DbError::StorageError(format!("spawn_blocking join error: {}", e)))?
    }

    // -- Document-level convenience methods ----------------------------------

    /// Serialize and store a [`Document`] in the `documents` column family.
    ///
    /// Uses MessagePack (via `rmp-serde`) for compact binary serialization.
    pub async fn put_document(&self, doc: &Document) -> DbResult<()> {
        let value =
            rmp_serde::to_vec(doc).map_err(|e| DbError::SerializationError(e.to_string()))?;
        self.put(CF_DOCUMENTS, doc.id.as_str().as_bytes(), &value)
            .await
    }

    /// Retrieve and deserialize a [`Document`] by its [`DocumentId`].
    pub async fn get_document(&self, id: &DocumentId) -> DbResult<Option<Document>> {
        let raw = self.get(CF_DOCUMENTS, id.as_str().as_bytes()).await?;
        match raw {
            Some(bytes) => {
                let doc: Document = rmp_serde::from_slice(&bytes)
                    .map_err(|e| DbError::SerializationError(e.to_string()))?;
                Ok(Some(doc))
            }
            None => Ok(None),
        }
    }

    /// Delete a document from storage by its [`DocumentId`].
    pub async fn delete_document(&self, id: &DocumentId) -> DbResult<()> {
        self.delete(CF_DOCUMENTS, id.as_str().as_bytes()).await
    }

    /// List documents with pagination (offset / limit).
    ///
    /// Iterates over all keys in the `documents` column family in
    /// lexicographic order, skipping `offset` entries.
    pub async fn list_documents(&self, offset: usize, limit: usize) -> DbResult<Vec<Document>> {
        let db = Arc::clone(&self.db);

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(CF_DOCUMENTS)
                .ok_or_else(|| DbError::StorageError("missing documents CF".into()))?;

            let iter = db.iterator_cf(&cf, IteratorMode::Start);
            let mut docs = Vec::new();
            for (idx, item) in iter.enumerate() {
                let (_, v) = item.map_err(|e| DbError::StorageError(e.to_string()))?;
                if idx < offset {
                    continue;
                }
                if docs.len() >= limit {
                    break;
                }
                let doc: Document = rmp_serde::from_slice(&v)
                    .map_err(|e| DbError::SerializationError(e.to_string()))?;
                docs.push(doc);
            }
            Ok(docs)
        })
        .await
        .map_err(|e| DbError::StorageError(format!("spawn_blocking join error: {}", e)))?
    }

    /// Return the current sequence number.
    pub fn current_sequence(&self) -> u64 {
        self.sequence.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// StorageBackend trait implementation
// ---------------------------------------------------------------------------

/// Implements the core [`StorageBackend`] trait, mapping trait methods to the
/// document-level helpers on [`RocksDbStorage`].
#[async_trait]
impl StorageBackend for RocksDbStorage {
    async fn get(&self, id: &DocumentId) -> DbResult<Document> {
        self.get_document(id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("document '{}' not found", id)))
    }

    async fn put(&self, document: Document) -> DbResult<()> {
        self.put_document(&document).await
    }

    async fn delete(&self, id: &DocumentId) -> DbResult<()> {
        // Check existence first to return NotFound as the trait contract requires.
        let exists = self.get_document(id).await?;
        if exists.is_none() {
            return Err(DbError::NotFound(format!("document '{}' not found", id)));
        }
        self.delete_document(id).await
    }

    async fn scan(
        &self,
        range: RangeInclusive<DocumentId>,
        limit: usize,
    ) -> DbResult<Vec<Document>> {
        let db = Arc::clone(&self.db);
        let start = range.start().as_str().to_owned();
        let end = range.end().as_str().to_owned();

        tokio::task::spawn_blocking(move || {
            let cf = db
                .cf_handle(CF_DOCUMENTS)
                .ok_or_else(|| DbError::StorageError("missing documents CF".into()))?;

            let iter = db.iterator_cf(
                &cf,
                IteratorMode::From(start.as_bytes(), rocksdb::Direction::Forward),
            );

            let mut docs = Vec::new();
            for item in iter {
                let (k, v) = item.map_err(|e| DbError::StorageError(e.to_string()))?;
                let key_str = String::from_utf8_lossy(&k);
                if key_str.as_ref() > end.as_str() {
                    break;
                }
                let doc: Document = rmp_serde::from_slice(&v)
                    .map_err(|e| DbError::SerializationError(e.to_string()))?;
                docs.push(doc);
                if docs.len() >= limit {
                    break;
                }
            }
            Ok(docs)
        })
        .await
        .map_err(|e| DbError::StorageError(format!("spawn_blocking join error: {}", e)))?
    }
}

// ---------------------------------------------------------------------------
// Drop — RAII cleanup
// ---------------------------------------------------------------------------

impl Drop for RocksDbStorage {
    fn drop(&mut self) {
        tracing::info!("closing RocksDB storage");
        // DB::drop flushes memtables and closes file descriptors.
        // Arc ensures this only runs when the last reference is dropped.
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use msearchdb_core::document::FieldValue;
    use tempfile::TempDir;

    fn temp_storage() -> (TempDir, RocksDbStorage) {
        let dir = TempDir::new().expect("create temp dir");
        let storage =
            RocksDbStorage::new(dir.path(), StorageOptions::default()).expect("open storage");
        (dir, storage)
    }

    #[tokio::test]
    async fn open_and_close() {
        let (_dir, _storage) = temp_storage();
        // RAII drop handles cleanup.
    }

    #[tokio::test]
    async fn raw_put_get_delete() {
        let (_dir, storage) = temp_storage();

        storage.put(CF_DOCUMENTS, b"key1", b"value1").await.unwrap();
        let val = storage.get(CF_DOCUMENTS, b"key1").await.unwrap();
        assert_eq!(val, Some(b"value1".to_vec()));

        storage.delete(CF_DOCUMENTS, b"key1").await.unwrap();
        let val = storage.get(CF_DOCUMENTS, b"key1").await.unwrap();
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn document_roundtrip() {
        let (_dir, storage) = temp_storage();

        let doc = Document::new(DocumentId::new("test-doc"))
            .with_field("title", FieldValue::Text("Hello MSearchDB".into()))
            .with_field("score", FieldValue::Number(42.5))
            .with_field("active", FieldValue::Boolean(true))
            .with_field(
                "tags",
                FieldValue::Array(vec![
                    FieldValue::Text("rust".into()),
                    FieldValue::Text("database".into()),
                ]),
            );

        storage.put_document(&doc).await.unwrap();
        let loaded = storage
            .get_document(&DocumentId::new("test-doc"))
            .await
            .unwrap()
            .expect("document should exist");

        assert_eq!(loaded.id, doc.id);
        assert_eq!(loaded.get_field("title"), doc.get_field("title"));
        assert_eq!(loaded.get_field("score"), doc.get_field("score"));
        assert_eq!(loaded.get_field("active"), doc.get_field("active"));
        assert_eq!(loaded.get_field("tags"), doc.get_field("tags"));
    }

    #[tokio::test]
    async fn sequence_increments() {
        let (_dir, storage) = temp_storage();
        assert_eq!(storage.current_sequence(), 0);

        storage.put(CF_DOCUMENTS, b"k1", b"v1").await.unwrap();
        assert_eq!(storage.current_sequence(), 1);

        storage.put(CF_DOCUMENTS, b"k2", b"v2").await.unwrap();
        assert_eq!(storage.current_sequence(), 2);
    }

    #[tokio::test]
    async fn storage_options_defaults() {
        let opts = StorageOptions::default();
        assert_eq!(opts.write_buffer_mb, 64);
        assert_eq!(opts.max_open_files, 1000);
        assert!(opts.compression);
        assert_eq!(opts.bloom_filter_bits, 10);
    }

    #[tokio::test]
    async fn scan_with_prefix() {
        let (_dir, storage) = temp_storage();

        for i in 0..5 {
            let key = format!("prefix-{}", i);
            storage
                .put(CF_DOCUMENTS, key.as_bytes(), b"value")
                .await
                .unwrap();
        }
        storage
            .put(CF_DOCUMENTS, b"other-key", b"value")
            .await
            .unwrap();

        let results = storage.scan(CF_DOCUMENTS, b"prefix-", 100).await.unwrap();
        assert_eq!(results.len(), 5);
    }
}
