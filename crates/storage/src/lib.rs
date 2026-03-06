//! # msearchdb-storage
//!
//! Storage engine for MSearchDB. Provides persistent key-value storage
//! with support for the [`msearchdb_core::traits::StorageBackend`] trait.
//!
//! ## Modules
//!
//! - [`rocksdb_backend`] — RocksDB-backed persistent storage with column
//!   families, bloom filters, and Snappy compression.
//! - [`wal`] — Write-ahead log for crash recovery with CRC32 checksums.
//! - [`memtable`] — In-memory write buffer backed by a lock-free skip list.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//! use msearchdb_storage::rocksdb_backend::{RocksDbStorage, StorageOptions};
//! use msearchdb_core::traits::StorageBackend;
//! use msearchdb_core::document::{Document, DocumentId, FieldValue};
//!
//! # async fn example() -> msearchdb_core::error::DbResult<()> {
//! let storage = RocksDbStorage::new(
//!     Path::new("/tmp/msearchdb"),
//!     StorageOptions::default(),
//! )?;
//!
//! // Use the StorageBackend trait.
//! let doc = Document::new(DocumentId::new("doc-1"))
//!     .with_field("title", FieldValue::Text("hello".into()));
//! StorageBackend::put(&storage, doc).await?;
//! # Ok(())
//! # }
//! ```

pub use msearchdb_core;

pub mod memtable;
pub mod rocksdb_backend;
pub mod wal;
