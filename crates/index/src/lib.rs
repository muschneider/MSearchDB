//! # msearchdb-index
//!
//! Full-text search indexing engine for MSearchDB, powered by [Tantivy](https://github.com/quickwit-oss/tantivy).
//!
//! This crate provides the core search functionality that differentiates MSearchDB
//! from simple key-value stores. It implements the [`IndexBackend`] trait from
//! `msearchdb-core` using Tantivy's inverted index, BM25 scoring, and pluggable
//! tokenization pipeline.
//!
//! ## Modules
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`schema_builder`] | Dynamic Tantivy schema construction from field configs |
//! | [`analyzer`] | Custom text analyzers (standard, keyword, ngram, CJK) |
//! | [`tantivy_index`] | Core search index with CRUD operations + `IndexBackend` |
//! | [`query_builder`] | Fluent API for composing complex search queries |
//! | [`highlighting`] | Search result highlighting with `<em>` tags |
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │                 TantivyIndex                     │
//! │                                                  │
//! │  index: Arc<Index>          (shared, read-only)  │
//! │  writer: Arc<Mutex<Writer>> (single-writer)      │
//! │  reader: IndexReader        (auto-reload)        │
//! │  schema: Arc<Schema>        (immutable)          │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! - **`Arc<Mutex<IndexWriter>>`** — Tantivy's `IndexWriter` is `!Sync`, so
//!   a `Mutex` serializes write access. Only one thread may add/delete documents
//!   at a time, but reads via `IndexReader` are fully concurrent.
//! - **`Arc<Schema>`** — the schema is immutable once built, so sharing via `Arc`
//!   incurs zero synchronization cost.
//! - **`tokio::task::spawn_blocking`** — CPU-bound Tantivy operations are
//!   dispatched to the blocking thread pool to avoid starving async tasks.
//!
//! ## Quick Start
//!
//! ```no_run
//! use msearchdb_index::schema_builder::{SchemaConfig, FieldConfig, FieldType};
//! use msearchdb_index::tantivy_index::TantivyIndex;
//! use msearchdb_index::query_builder::QueryBuilder;
//! use msearchdb_core::document::{Document, DocumentId, FieldValue};
//! use msearchdb_core::traits::IndexBackend;
//!
//! # async fn example() -> msearchdb_core::DbResult<()> {
//! // 1. Define schema
//! let config = SchemaConfig::new()
//!     .with_field(FieldConfig::new("title", FieldType::Text))
//!     .with_field(FieldConfig::new("body", FieldType::Text));
//!
//! // 2. Create index
//! let index = TantivyIndex::new_in_ram(config)?;
//!
//! // 3. Index a document
//! let doc = Document::new(DocumentId::new("doc-1"))
//!     .with_field("title", FieldValue::Text("Hello World".into()));
//! index.index_document(&doc).await?;
//! index.commit()?;
//!
//! // 4. Search
//! let query = QueryBuilder::new()
//!     .must_match("title", "hello")
//!     .build();
//! let results = index.search(&query).await?;
//! # Ok(())
//! # }
//! ```
//!
//! [`IndexBackend`]: msearchdb_core::traits::IndexBackend

pub use msearchdb_core;

pub mod analyzer;
pub mod collection_index;
pub mod highlighting;
pub mod query_builder;
pub mod schema_builder;
pub mod tantivy_index;
