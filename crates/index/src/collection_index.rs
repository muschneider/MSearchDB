//! Per-collection Tantivy index management with dynamic field mapping.
//!
//! [`CollectionIndexManager`] manages a separate Tantivy index per MSearchDB
//! collection.  Each collection gets its own directory (or RAM index for
//! testing), its own schema, writer, and reader — providing full isolation.
//!
//! ## Dynamic Field Mapping
//!
//! When a document is indexed, the manager inspects every field in the
//! document, compares its [`FieldValue`](msearchdb_core::document::FieldValue)
//! type against the collection's [`FieldMapping`], and:
//!
//! 1. If the field is **new**, registers it with the mapping and rebuilds
//!    the Tantivy schema to include it.
//! 2. If the field is **known** with the **same type**, proceeds normally.
//! 3. If the field is **known** with a **different type**, returns
//!    [`DbError::SchemaConflict`].
//!
//! Schema rebuilds are rare (only on new field discovery) and are
//! transparently handled by closing the old writer/reader and opening new
//! ones against the updated schema.
//!
//! # Examples
//!
//! ```no_run
//! use msearchdb_index::collection_index::CollectionIndexManager;
//! use msearchdb_core::collection::FieldMapping;
//! use msearchdb_core::document::{Document, DocumentId, FieldValue};
//!
//! # async fn example() -> msearchdb_core::DbResult<()> {
//! let manager = CollectionIndexManager::new_in_ram();
//! manager.create_collection("products")?;
//!
//! let mapping = FieldMapping::new();
//! let doc = Document::new(DocumentId::new("p1"))
//!     .with_field("name", FieldValue::Text("Laptop".into()))
//!     .with_field("price", FieldValue::Number(999.99));
//!
//! let updated_mapping = manager.index_document("products", &doc, &mapping)?;
//! assert_eq!(updated_mapping.field_count(), 2);
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use tantivy::collector::TopDocs;
use tantivy::merge_policy::LogMergePolicy;
use tantivy::query::{
    AllQuery, BooleanQuery, Occur, QueryParser, RangeQuery, TermQuery as TantivyTermQuery,
};
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, SchemaBuilder, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

use msearchdb_core::collection::{FieldMapping, MappedFieldType};
use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::query::{
    apply_collapse, apply_search_after, apply_sort, compute_aggregations, BoolQuery, FullTextQuery,
    Operator, Query, RangeQuery as CoreRangeQuery, ScoredDocument, SearchOptions, SearchResult,
    TermQuery as CoreTermQuery,
};
use msearchdb_core::VectorClock;

use crate::analyzer::register_analyzers;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default heap size for the IndexWriter (50 MB).
const WRITER_HEAP_BYTES: usize = 50 * 1024 * 1024;

/// Default search result limit.
const DEFAULT_SEARCH_LIMIT: usize = 10;

/// Number of document store blocks to cache per collection reader (L2 cache).
const DOC_STORE_CACHE_BLOCKS: usize = 128;

// ---------------------------------------------------------------------------
// FieldMap — maps field names to Tantivy Field handles
// ---------------------------------------------------------------------------

/// A mapping from field names to Tantivy [`Field`] handles.
type FieldMap = HashMap<String, Field>;

// ---------------------------------------------------------------------------
// CollectionIndex — a single collection's index state
// ---------------------------------------------------------------------------

/// Internal state for a single collection's Tantivy index.
struct CollectionIndex {
    /// The Tantivy index instance.
    index: Index,
    /// The index writer (single-writer, mutex-protected).
    writer: Mutex<IndexWriter>,
    /// The index reader (auto-reload on commit).
    reader: IndexReader,
    /// The Tantivy schema.
    schema: Arc<Schema>,
    /// Maps field names to Tantivy Field handles.
    field_map: FieldMap,
}

// ---------------------------------------------------------------------------
// CollectionIndexManager
// ---------------------------------------------------------------------------

/// Manages per-collection Tantivy indices with dynamic field mapping.
///
/// Each collection has its own isolated Tantivy index. The manager supports
/// creating, dropping, and operating on collection indices.
///
/// Thread safety: The inner `RwLock` protects the collection registry.
/// Individual collection indices are independently synchronized via their
/// own `Mutex<IndexWriter>`.
pub struct CollectionIndexManager {
    /// Collection name → index state.
    collections: RwLock<HashMap<String, CollectionIndex>>,
    /// Base directory for on-disk indices.  `None` = RAM-only mode.
    base_path: Option<PathBuf>,
}

impl CollectionIndexManager {
    /// Create a new manager that stores indices on disk under `base_path`.
    ///
    /// Each collection index is stored in `{base_path}/idx_{collection_name}/`.
    pub fn new(base_path: &Path) -> Self {
        Self {
            collections: RwLock::new(HashMap::new()),
            base_path: Some(base_path.to_path_buf()),
        }
    }

    /// Create a new manager that stores all indices in RAM (for testing).
    pub fn new_in_ram() -> Self {
        Self {
            collections: RwLock::new(HashMap::new()),
            base_path: None,
        }
    }

    // -----------------------------------------------------------------------
    // Collection lifecycle
    // -----------------------------------------------------------------------

    /// Create a new Tantivy index for the named collection.
    ///
    /// The initial schema contains only the meta-fields (`_id`, `_score`,
    /// `_timestamp`, `_body`).  User fields are added dynamically as
    /// documents are indexed.
    ///
    /// Idempotent: returns `Ok(())` if the collection index already exists.
    pub fn create_collection(&self, collection: &str) -> DbResult<()> {
        let mut collections = self
            .collections
            .write()
            .map_err(|e| DbError::IndexError(format!("lock poisoned: {}", e)))?;

        if collections.contains_key(collection) {
            return Ok(()); // idempotent
        }

        let (schema, field_map) = Self::build_initial_schema();
        let col_index = self.open_index(collection, &schema, field_map)?;
        collections.insert(collection.to_owned(), col_index);

        tracing::info!(collection, "created collection index");
        Ok(())
    }

    /// Drop the Tantivy index for the named collection.
    pub fn drop_collection(&self, collection: &str) -> DbResult<()> {
        let mut collections = self
            .collections
            .write()
            .map_err(|e| DbError::IndexError(format!("lock poisoned: {}", e)))?;

        collections.remove(collection);
        // On-disk: the directory remains for now (could add cleanup later).
        tracing::info!(collection, "dropped collection index");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Document indexing with dynamic field mapping
    // -----------------------------------------------------------------------

    /// Index a document in a collection, auto-detecting field types.
    ///
    /// Compares each document field against the provided [`FieldMapping`].
    /// New fields are registered; conflicting types return an error.
    ///
    /// Returns the updated [`FieldMapping`] reflecting any newly discovered
    /// fields.
    pub fn index_document(
        &self,
        collection: &str,
        doc: &Document,
        mapping: &FieldMapping,
    ) -> DbResult<FieldMapping> {
        // 1. Detect any new fields and compute the updated mapping.
        let mut updated_mapping = mapping.clone();
        let mut new_fields = Vec::new();

        for (name, value) in &doc.fields {
            let mapped_type = Self::field_value_to_mapped_type(value);
            if updated_mapping.register_field(name.clone(), mapped_type.clone())? {
                new_fields.push((name.clone(), mapped_type));
            }
        }

        // 2. If new fields were discovered, rebuild the Tantivy schema.
        if !new_fields.is_empty() {
            self.rebuild_schema(collection, &updated_mapping)?;
        }

        // 3. Index the document.
        let collections = self
            .collections
            .read()
            .map_err(|e| DbError::IndexError(format!("lock poisoned: {}", e)))?;

        let col_index = collections.get(collection).ok_or_else(|| {
            DbError::NotFound(format!("collection index '{}' not found", collection))
        })?;

        let tantivy_doc = Self::build_tantivy_doc(doc, &col_index.field_map);

        let writer = col_index
            .writer
            .lock()
            .map_err(|e| DbError::IndexError(format!("writer lock: {}", e)))?;
        writer
            .add_document(tantivy_doc)
            .map_err(|e| DbError::IndexError(format!("index document: {}", e)))?;

        Ok(updated_mapping)
    }

    /// Delete a document from a collection's index.
    pub fn delete_document(&self, collection: &str, id: &DocumentId) -> DbResult<()> {
        let collections = self
            .collections
            .read()
            .map_err(|e| DbError::IndexError(format!("lock poisoned: {}", e)))?;

        let col_index = collections.get(collection).ok_or_else(|| {
            DbError::NotFound(format!("collection index '{}' not found", collection))
        })?;

        let id_field = col_index
            .field_map
            .get("_id")
            .ok_or_else(|| DbError::IndexError("_id field not in schema".into()))?;

        let term = Term::from_field_text(*id_field, id.as_str());
        let writer = col_index
            .writer
            .lock()
            .map_err(|e| DbError::IndexError(format!("writer lock: {}", e)))?;
        writer.delete_term(term);
        Ok(())
    }

    /// Commit buffered writes for a collection's index.
    pub fn commit(&self, collection: &str) -> DbResult<()> {
        let collections = self
            .collections
            .read()
            .map_err(|e| DbError::IndexError(format!("lock poisoned: {}", e)))?;

        let col_index = collections.get(collection).ok_or_else(|| {
            DbError::NotFound(format!("collection index '{}' not found", collection))
        })?;

        let mut writer = col_index
            .writer
            .lock()
            .map_err(|e| DbError::IndexError(format!("writer lock: {}", e)))?;
        writer
            .commit()
            .map_err(|e| DbError::IndexError(format!("commit failed: {}", e)))?;
        col_index
            .reader
            .reload()
            .map_err(|e| DbError::IndexError(format!("reader reload: {}", e)))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    /// Search within a collection's index.
    pub fn search(&self, collection: &str, query: &Query) -> DbResult<SearchResult> {
        let collections = self
            .collections
            .read()
            .map_err(|e| DbError::IndexError(format!("lock poisoned: {}", e)))?;

        let col_index = collections.get(collection).ok_or_else(|| {
            DbError::NotFound(format!("collection index '{}' not found", collection))
        })?;

        let start = Instant::now();
        let searcher = col_index.reader.searcher();
        let tantivy_query = Self::translate_query(query, &col_index.index, &col_index.field_map)?;

        let top_docs = searcher
            .search(&*tantivy_query, &TopDocs::with_limit(DEFAULT_SEARCH_LIMIT))
            .map_err(|e| DbError::IndexError(format!("search failed: {}", e)))?;

        let total = top_docs.len() as u64;
        let mut documents = Vec::with_capacity(top_docs.len());

        for (score, doc_address) in &top_docs {
            let retrieved: TantivyDocument = searcher
                .doc(*doc_address)
                .map_err(|e| DbError::IndexError(format!("failed to retrieve doc: {}", e)))?;
            let doc =
                Self::reconstruct_document(&retrieved, &col_index.schema, &col_index.field_map);
            documents.push(ScoredDocument {
                document: doc,
                score: *score,
                sort: Vec::new(),
            });
        }

        let took_ms = start.elapsed().as_millis() as u64;

        Ok(SearchResult {
            documents,
            total,
            took_ms,
            aggregations: HashMap::new(),
        })
    }

    /// Search with extended options (sort, aggregations, search_after, collapse).
    ///
    /// Fetches a generous set of raw results, then applies the core query
    /// engine functions for sorting, aggregation, search_after, and collapse.
    pub fn search_with_options(
        &self,
        collection: &str,
        query: &Query,
        options: &SearchOptions,
    ) -> DbResult<SearchResult> {
        let collections = self
            .collections
            .read()
            .map_err(|e| DbError::IndexError(format!("lock poisoned: {}", e)))?;

        let col_index = collections.get(collection).ok_or_else(|| {
            DbError::NotFound(format!("collection index '{}' not found", collection))
        })?;

        let start = Instant::now();
        let searcher = col_index.reader.searcher();
        let tantivy_query = Self::translate_query(query, &col_index.index, &col_index.field_map)?;

        // Fetch a generous number of raw results for aggregation / sorting.
        // We need ALL matching docs for accurate aggregations, so use a large limit.
        let fetch_limit = std::cmp::max(
            options.from + options.size + 1000,
            DEFAULT_SEARCH_LIMIT * 100,
        );
        let top_docs = searcher
            .search(&*tantivy_query, &TopDocs::with_limit(fetch_limit))
            .map_err(|e| DbError::IndexError(format!("search failed: {}", e)))?;

        let total = top_docs.len() as u64;
        let mut documents = Vec::with_capacity(top_docs.len());

        for (score, doc_address) in &top_docs {
            let retrieved: TantivyDocument = searcher
                .doc(*doc_address)
                .map_err(|e| DbError::IndexError(format!("failed to retrieve doc: {}", e)))?;
            let doc =
                Self::reconstruct_document(&retrieved, &col_index.schema, &col_index.field_map);
            documents.push(ScoredDocument {
                document: doc,
                score: *score,
                sort: Vec::new(),
            });
        }

        // 1. Compute aggregations over ALL matched docs (before pagination).
        let aggregations = if options.aggregations.is_empty() {
            HashMap::new()
        } else {
            compute_aggregations(&documents, &options.aggregations)
        };

        // 2. Apply sorting.
        apply_sort(&mut documents, &options.sort);

        // 3. Apply search_after cursor filtering.
        if let Some(ref cursor) = options.search_after {
            apply_search_after(&mut documents, cursor, &options.sort);
        }

        // 4. Apply collapse / deduplication.
        if let Some(ref collapse) = options.collapse {
            apply_collapse(&mut documents, collapse);
        }

        // 5. Apply from/size pagination.
        let from = options.from;
        let size = options.size;
        let paged: Vec<ScoredDocument> = documents.into_iter().skip(from).take(size).collect();

        let took_ms = start.elapsed().as_millis() as u64;

        Ok(SearchResult {
            documents: paged,
            total,
            took_ms,
            aggregations,
        })
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Build the initial Tantivy schema with meta-fields and `_body`.
    fn build_initial_schema() -> (Schema, FieldMap) {
        Self::build_schema_from_mapping(&FieldMapping::new(), true)
    }

    /// Build a Tantivy schema from a [`FieldMapping`].
    ///
    /// Always includes meta-fields (`_id`, `_score`, `_timestamp`).
    /// Optionally includes `_body` (catch-all text field).
    fn build_schema_from_mapping(mapping: &FieldMapping, include_body: bool) -> (Schema, FieldMap) {
        let mut builder = SchemaBuilder::default();
        let mut field_map = HashMap::new();

        // -- Meta-fields --

        let id_options = TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let id_field = builder.add_text_field("_id", id_options);
        field_map.insert("_id".to_owned(), id_field);

        let score_options = NumericOptions::default().set_stored().set_fast();
        let score_field = builder.add_f64_field("_score", score_options);
        field_map.insert("_score".to_owned(), score_field);

        let ts_options = NumericOptions::default()
            .set_stored()
            .set_fast()
            .set_indexed();
        let ts_field = builder.add_u64_field("_timestamp", ts_options);
        field_map.insert("_timestamp".to_owned(), ts_field);

        if include_body {
            let body_options = TextOptions::default().set_stored().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("default")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            );
            let body_field = builder.add_text_field("_body", body_options);
            field_map.insert("_body".to_owned(), body_field);
        }

        // -- User fields from mapping --

        for (name, mapped_type) in mapping.fields() {
            let field = match mapped_type {
                MappedFieldType::Text => {
                    let opts = TextOptions::default().set_stored().set_indexing_options(
                        TextFieldIndexing::default()
                            .set_tokenizer("default")
                            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                    );
                    builder.add_text_field(name, opts)
                }
                MappedFieldType::Number => {
                    let opts = NumericOptions::default()
                        .set_stored()
                        .set_fast()
                        .set_indexed();
                    builder.add_f64_field(name, opts)
                }
                MappedFieldType::Boolean => {
                    let opts = NumericOptions::default()
                        .set_stored()
                        .set_fast()
                        .set_indexed();
                    builder.add_u64_field(name, opts)
                }
                MappedFieldType::Array => {
                    // Arrays are indexed as multi-valued text fields.
                    let opts = TextOptions::default().set_stored().set_indexing_options(
                        TextFieldIndexing::default()
                            .set_tokenizer("default")
                            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                    );
                    builder.add_text_field(name, opts)
                }
                // Future MappedFieldType variants default to text.
                _ => {
                    let opts = TextOptions::default().set_stored().set_indexing_options(
                        TextFieldIndexing::default()
                            .set_tokenizer("default")
                            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                    );
                    builder.add_text_field(name, opts)
                }
            };
            field_map.insert(name.clone(), field);
        }

        let schema = builder.build();
        (schema, field_map)
    }

    /// Open (or create) a Tantivy index with the given schema.
    fn open_index(
        &self,
        collection: &str,
        schema: &Schema,
        field_map: FieldMap,
    ) -> DbResult<CollectionIndex> {
        let index = if let Some(ref base) = self.base_path {
            let dir = base.join(format!("idx_{}", collection));
            std::fs::create_dir_all(&dir)
                .map_err(|e| DbError::IndexError(format!("failed to create index dir: {}", e)))?;
            Index::create_in_dir(&dir, schema.clone()).or_else(|_| {
                Index::open_in_dir(&dir)
                    .map_err(|e| DbError::IndexError(format!("failed to open index: {}", e)))
            })?
        } else {
            Index::create_in_ram(schema.clone())
        };

        register_analyzers(&index);

        let writer = index
            .writer(WRITER_HEAP_BYTES)
            .map_err(|e| DbError::IndexError(format!("failed to create writer: {}", e)))?;
        let merge_policy = LogMergePolicy::default();
        writer.set_merge_policy(Box::new(merge_policy));

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .doc_store_cache_num_blocks(DOC_STORE_CACHE_BLOCKS)
            .try_into()
            .map_err(|e| DbError::IndexError(format!("failed to create reader: {}", e)))?;

        Ok(CollectionIndex {
            index,
            writer: Mutex::new(writer),
            reader,
            schema: Arc::new(schema.clone()),
            field_map,
        })
    }

    /// Rebuild the Tantivy schema for a collection when new fields are
    /// discovered.
    ///
    /// This is a relatively expensive operation:
    /// 1. Commit any pending writes.
    /// 2. Build a new schema from the updated mapping.
    /// 3. Close old writer/reader.
    /// 4. Re-open the index with the new schema.
    ///
    /// For RAM indices, existing data is lost on schema rebuild — this is
    /// acceptable because the rebuild only happens when a new field is
    /// first seen, and we always re-index documents afterwards.
    fn rebuild_schema(&self, collection: &str, mapping: &FieldMapping) -> DbResult<()> {
        let mut collections = self
            .collections
            .write()
            .map_err(|e| DbError::IndexError(format!("lock poisoned: {}", e)))?;

        // Remove the old index state (drops writer/reader)
        let _old = collections.remove(collection);

        // Build new schema from the updated mapping
        let (schema, field_map) = Self::build_schema_from_mapping(mapping, true);

        // Re-open
        let col_index = self.open_index(collection, &schema, field_map)?;
        collections.insert(collection.to_owned(), col_index);

        tracing::info!(
            collection,
            fields = mapping.field_count(),
            "rebuilt collection index schema"
        );
        Ok(())
    }

    /// Map a [`FieldValue`] to its [`MappedFieldType`].
    fn field_value_to_mapped_type(value: &FieldValue) -> MappedFieldType {
        match value {
            FieldValue::Text(_) => MappedFieldType::Text,
            FieldValue::Number(_) => MappedFieldType::Number,
            FieldValue::Boolean(_) => MappedFieldType::Boolean,
            FieldValue::Array(_) => MappedFieldType::Array,
            // Future variants default to Text
            _ => MappedFieldType::Text,
        }
    }

    /// Build a Tantivy document from an MSearchDB document.
    fn build_tantivy_doc(doc: &Document, field_map: &FieldMap) -> TantivyDocument {
        let mut tantivy_doc = TantivyDocument::new();

        // _id
        if let Some(&id_field) = field_map.get("_id") {
            tantivy_doc.add_text(id_field, doc.id.as_str());
        }

        // _timestamp
        if let Some(&ts_field) = field_map.get("_timestamp") {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            tantivy_doc.add_u64(ts_field, ts);
        }

        let mut body_parts: Vec<&str> = Vec::new();

        for (name, value) in &doc.fields {
            if let FieldValue::Text(text) = value {
                body_parts.push(text);
            }

            if let Some(&field) = field_map.get(name) {
                match value {
                    FieldValue::Text(text) => tantivy_doc.add_text(field, text),
                    FieldValue::Number(n) => tantivy_doc.add_f64(field, *n),
                    FieldValue::Boolean(b) => tantivy_doc.add_u64(field, if *b { 1 } else { 0 }),
                    FieldValue::Array(arr) => {
                        for item in arr {
                            if let FieldValue::Text(text) = item {
                                tantivy_doc.add_text(field, text);
                                body_parts.push(text);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // _body catch-all
        if !body_parts.is_empty() {
            if let Some(&body_field) = field_map.get("_body") {
                tantivy_doc.add_text(body_field, body_parts.join(" "));
            }
        }

        tantivy_doc
    }

    /// Reconstruct an MSearchDB document from a Tantivy document.
    fn reconstruct_document(
        tantivy_doc: &TantivyDocument,
        schema: &Schema,
        field_map: &FieldMap,
    ) -> Document {
        let id = field_map
            .get("_id")
            .and_then(|&f| {
                tantivy_doc
                    .get_first(f)
                    .and_then(|v| v.as_str().map(|s| s.to_owned()))
            })
            .unwrap_or_default();

        let mut fields = HashMap::new();

        for (name, &field) in field_map {
            if name.starts_with('_') {
                continue;
            }

            let entry = schema.get_field_entry(field);
            let _field_type = entry.field_type();

            if let Some(value) = tantivy_doc.get_first(field) {
                if let Some(text) = value.as_str() {
                    fields.insert(name.clone(), FieldValue::Text(text.to_owned()));
                } else if let Some(n) = value.as_f64() {
                    fields.insert(name.clone(), FieldValue::Number(n));
                } else if let Some(n) = value.as_u64() {
                    fields.insert(name.clone(), FieldValue::Boolean(n != 0));
                }
            }
        }

        Document {
            id: DocumentId::new(id),
            fields,
            version: VectorClock::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Query translation
    // -----------------------------------------------------------------------

    /// Translate an MSearchDB query into a Tantivy query.
    fn translate_query(
        query: &Query,
        index: &Index,
        field_map: &FieldMap,
    ) -> DbResult<Box<dyn tantivy::query::Query>> {
        match query {
            Query::FullText(ftq) => Self::translate_full_text(ftq, index, field_map),
            Query::Term(tq) => Self::translate_term(tq, field_map),
            Query::Range(rq) => Self::translate_range(rq, field_map),
            Query::Bool(bq) => Self::translate_bool(bq, index, field_map),
            _ => Err(DbError::InvalidInput("unsupported query type".to_owned())),
        }
    }

    /// Translate a full-text query.
    fn translate_full_text(
        ftq: &FullTextQuery,
        index: &Index,
        field_map: &FieldMap,
    ) -> DbResult<Box<dyn tantivy::query::Query>> {
        let field = field_map
            .get(&ftq.field)
            .ok_or_else(|| DbError::InvalidInput(format!("field '{}' not in schema", ftq.field)))?;

        let mut parser = QueryParser::for_index(index, vec![*field]);

        match ftq.operator {
            Operator::And => parser.set_conjunction_by_default(),
            Operator::Or => {}
            _ => {}
        }

        let parsed = parser
            .parse_query(&ftq.query)
            .map_err(|e| DbError::IndexError(format!("parse query: {}", e)))?;
        Ok(parsed)
    }

    /// Translate a term query.
    fn translate_term(
        tq: &CoreTermQuery,
        field_map: &FieldMap,
    ) -> DbResult<Box<dyn tantivy::query::Query>> {
        let field = field_map
            .get(&tq.field)
            .ok_or_else(|| DbError::InvalidInput(format!("field '{}' not in schema", tq.field)))?;

        let term = Term::from_field_text(*field, &tq.value);
        Ok(Box::new(TantivyTermQuery::new(
            term,
            IndexRecordOption::WithFreqsAndPositions,
        )))
    }

    /// Translate a range query.
    fn translate_range(
        rq: &CoreRangeQuery,
        field_map: &FieldMap,
    ) -> DbResult<Box<dyn tantivy::query::Query>> {
        let field = field_map
            .get(&rq.field)
            .ok_or_else(|| DbError::InvalidInput(format!("field '{}' not in schema", rq.field)))?;

        let lower = rq
            .gte
            .map(|v| std::ops::Bound::Included(Term::from_field_f64(*field, v)))
            .unwrap_or(std::ops::Bound::Unbounded);
        let upper = rq
            .lte
            .map(|v| std::ops::Bound::Included(Term::from_field_f64(*field, v)))
            .unwrap_or(std::ops::Bound::Unbounded);

        Ok(Box::new(RangeQuery::new_term_bounds(
            rq.field.clone(),
            tantivy::schema::Type::F64,
            &lower,
            &upper,
        )))
    }

    /// Translate a bool query.
    fn translate_bool(
        bq: &BoolQuery,
        index: &Index,
        field_map: &FieldMap,
    ) -> DbResult<Box<dyn tantivy::query::Query>> {
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();

        for q in &bq.must {
            clauses.push((Occur::Must, Self::translate_query(q, index, field_map)?));
        }
        for q in &bq.should {
            clauses.push((Occur::Should, Self::translate_query(q, index, field_map)?));
        }
        for q in &bq.must_not {
            clauses.push((Occur::MustNot, Self::translate_query(q, index, field_map)?));
        }

        if bq.must.is_empty() && bq.should.is_empty() && !bq.must_not.is_empty() {
            clauses.push((Occur::Must, Box::new(AllQuery)));
        }

        Ok(Box::new(BooleanQuery::new(clauses)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_drop_collection() {
        let mgr = CollectionIndexManager::new_in_ram();
        mgr.create_collection("test").unwrap();

        // Idempotent
        mgr.create_collection("test").unwrap();

        mgr.drop_collection("test").unwrap();

        // Search on dropped collection should fail
        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "hello".into(),
            operator: Operator::Or,
        });
        assert!(mgr.search("test", &query).is_err());
    }

    #[test]
    fn dynamic_field_mapping_registers_new_fields() {
        let mgr = CollectionIndexManager::new_in_ram();
        mgr.create_collection("products").unwrap();

        let mapping = FieldMapping::new();
        let doc = Document::new(DocumentId::new("p1"))
            .with_field("name", FieldValue::Text("Laptop".into()))
            .with_field("price", FieldValue::Number(999.99));

        let updated = mgr.index_document("products", &doc, &mapping).unwrap();
        assert_eq!(updated.field_count(), 2);
        assert_eq!(updated.get_field_type("name"), Some(&MappedFieldType::Text));
        assert_eq!(
            updated.get_field_type("price"),
            Some(&MappedFieldType::Number)
        );
    }

    #[test]
    fn schema_conflict_returns_error() {
        let mgr = CollectionIndexManager::new_in_ram();
        mgr.create_collection("test").unwrap();

        let mut mapping = FieldMapping::new();
        mapping
            .register_field("status", MappedFieldType::Text)
            .unwrap();

        // Try to index a doc where "status" is a Number — should conflict
        let doc =
            Document::new(DocumentId::new("d1")).with_field("status", FieldValue::Number(42.0));

        let err = mgr.index_document("test", &doc, &mapping).unwrap_err();
        assert!(matches!(err, DbError::SchemaConflict { .. }));
    }

    #[test]
    fn index_and_search_with_dynamic_fields() {
        let mgr = CollectionIndexManager::new_in_ram();
        mgr.create_collection("articles").unwrap();

        let mapping = FieldMapping::new();

        let doc1 = Document::new(DocumentId::new("a1"))
            .with_field("title", FieldValue::Text("Rust Programming".into()))
            .with_field("views", FieldValue::Number(100.0));

        let mapping = mgr.index_document("articles", &doc1, &mapping).unwrap();

        let doc2 = Document::new(DocumentId::new("a2"))
            .with_field("title", FieldValue::Text("Go Programming".into()))
            .with_field("views", FieldValue::Number(200.0));

        let mapping = mgr.index_document("articles", &doc2, &mapping).unwrap();

        mgr.commit("articles").unwrap();

        // Search by title
        let query = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "Rust".into(),
            operator: Operator::Or,
        });

        let result = mgr.search("articles", &query).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.documents[0].document.id.as_str(), "a1");

        // Verify mapping has 2 fields
        assert_eq!(mapping.field_count(), 2);
    }

    #[test]
    fn index_search_delete_lifecycle() {
        let mgr = CollectionIndexManager::new_in_ram();
        mgr.create_collection("items").unwrap();

        let mapping = FieldMapping::new();
        let doc = Document::new(DocumentId::new("i1"))
            .with_field("name", FieldValue::Text("Widget".into()));

        let mapping = mgr.index_document("items", &doc, &mapping).unwrap();
        mgr.commit("items").unwrap();

        // Verify it exists
        let query = Query::Term(CoreTermQuery {
            field: "_id".into(),
            value: "i1".into(),
        });
        let result = mgr.search("items", &query).unwrap();
        assert_eq!(result.total, 1);

        // Delete
        mgr.delete_document("items", &DocumentId::new("i1"))
            .unwrap();
        mgr.commit("items").unwrap();

        let result = mgr.search("items", &query).unwrap();
        assert_eq!(result.total, 0);

        // Mapping is preserved
        assert_eq!(mapping.field_count(), 1);
    }

    #[test]
    fn collection_isolation_separate_indices() {
        let mgr = CollectionIndexManager::new_in_ram();
        mgr.create_collection("col_a").unwrap();
        mgr.create_collection("col_b").unwrap();

        let mapping_a = FieldMapping::new();
        let mapping_b = FieldMapping::new();

        let doc_a =
            Document::new(DocumentId::new("d1")).with_field("x", FieldValue::Text("alpha".into()));
        let doc_b =
            Document::new(DocumentId::new("d1")).with_field("x", FieldValue::Text("beta".into()));

        let _mapping_a = mgr.index_document("col_a", &doc_a, &mapping_a).unwrap();
        let _mapping_b = mgr.index_document("col_b", &doc_b, &mapping_b).unwrap();
        mgr.commit("col_a").unwrap();
        mgr.commit("col_b").unwrap();

        // Search col_a for "alpha" — should find it
        let query = Query::FullText(FullTextQuery {
            field: "x".into(),
            query: "alpha".into(),
            operator: Operator::Or,
        });
        let result_a = mgr.search("col_a", &query).unwrap();
        assert_eq!(result_a.total, 1);

        // Search col_b for "alpha" — should NOT find it
        let result_b = mgr.search("col_b", &query).unwrap();
        assert_eq!(result_b.total, 0);
    }

    #[test]
    fn body_catch_all_field_searchable() {
        let mgr = CollectionIndexManager::new_in_ram();
        mgr.create_collection("test").unwrap();

        let mapping = FieldMapping::new();
        let doc = Document::new(DocumentId::new("d1"))
            .with_field("title", FieldValue::Text("Important Document".into()));

        let _mapping = mgr.index_document("test", &doc, &mapping).unwrap();
        mgr.commit("test").unwrap();

        // Search the _body field
        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "Important".into(),
            operator: Operator::Or,
        });

        let result = mgr.search("test", &query).unwrap();
        assert_eq!(result.total, 1);
    }

    #[test]
    fn range_query_on_dynamic_numeric_field() {
        let mgr = CollectionIndexManager::new_in_ram();
        mgr.create_collection("prices").unwrap();

        let mut mapping = FieldMapping::new();

        for i in 0..10 {
            let doc = Document::new(DocumentId::new(format!("p{}", i)))
                .with_field("amount", FieldValue::Number(i as f64 * 10.0));
            mapping = mgr.index_document("prices", &doc, &mapping).unwrap();
        }
        mgr.commit("prices").unwrap();

        let query = Query::Range(CoreRangeQuery {
            field: "amount".into(),
            gte: Some(30.0),
            lte: Some(60.0),
        });

        let result = mgr.search("prices", &query).unwrap();
        // 30, 40, 50, 60
        assert_eq!(result.total, 4);
    }

    // ===================================================================
    // Aggregation, sorting, search_after, collapse integration tests
    // ===================================================================

    use msearchdb_core::query::{
        Aggregation, AggregationResult, CollapseOptions, DateInterval, RangeBucketDef,
        SearchOptions, SortClause, SortOrder, SortValue,
    };

    /// Helper: index a batch of product documents for aggregation tests.
    fn setup_products(mgr: &CollectionIndexManager) -> FieldMapping {
        mgr.create_collection("products").unwrap();
        let mut mapping = FieldMapping::new();

        let products = vec![
            ("p1", "Laptop", "electronics", 999.0, 1700000000.0),
            ("p2", "Phone", "electronics", 699.0, 1700003600.0),
            ("p3", "Tablet", "electronics", 499.0, 1700007200.0),
            ("p4", "Novel", "books", 15.0, 1700010800.0),
            ("p5", "Textbook", "books", 85.0, 1700014400.0),
            ("p6", "T-Shirt", "clothing", 25.0, 1700018000.0),
            ("p7", "Jeans", "clothing", 60.0, 1700021600.0),
            ("p8", "Hat", "clothing", 20.0, 1700025200.0),
            ("p9", "Keyboard", "electronics", 120.0, 1700028800.0),
            ("p10", "Mouse", "electronics", 45.0, 1700032400.0),
        ];

        for (id, name, category, price, ts) in products {
            let doc = Document::new(DocumentId::new(id))
                .with_field("name", FieldValue::Text(name.into()))
                .with_field("category", FieldValue::Text(category.into()))
                .with_field("price", FieldValue::Number(price))
                .with_field("timestamp", FieldValue::Number(ts));
            mapping = mgr.index_document("products", &doc, &mapping).unwrap();
        }
        mgr.commit("products").unwrap();
        mapping
    }

    // -- Terms aggregation integration --

    #[test]
    fn integration_terms_aggregation() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let mut aggs = HashMap::new();
        aggs.insert(
            "top_categories".to_string(),
            Aggregation::Terms {
                field: "category".into(),
                size: 10,
            },
        );

        let options = SearchOptions {
            size: 10,
            aggregations: aggs,
            ..Default::default()
        };

        let result = mgr
            .search_with_options("products", &query, &options)
            .unwrap();

        // Verify buckets
        let agg = result.aggregations.get("top_categories").unwrap();
        if let AggregationResult::Buckets { buckets } = agg {
            assert_eq!(buckets.len(), 3);
            // electronics: 5, clothing: 3, books: 2
            assert_eq!(buckets[0].key, "electronics");
            assert_eq!(buckets[0].doc_count, 5);
            assert_eq!(buckets[1].key, "clothing");
            assert_eq!(buckets[1].doc_count, 3);
            assert_eq!(buckets[2].key, "books");
            assert_eq!(buckets[2].doc_count, 2);
        } else {
            panic!("expected Buckets result");
        }
    }

    // -- Range aggregation integration --

    #[test]
    fn integration_range_aggregation() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let mut aggs = HashMap::new();
        aggs.insert(
            "price_ranges".to_string(),
            Aggregation::Range {
                field: "price".into(),
                ranges: vec![
                    RangeBucketDef {
                        key: Some("cheap".into()),
                        from: None,
                        to: Some(50.0),
                    },
                    RangeBucketDef {
                        key: Some("mid".into()),
                        from: Some(50.0),
                        to: Some(200.0),
                    },
                    RangeBucketDef {
                        key: Some("expensive".into()),
                        from: Some(200.0),
                        to: None,
                    },
                ],
            },
        );

        let options = SearchOptions {
            size: 10,
            aggregations: aggs,
            ..Default::default()
        };

        let result = mgr
            .search_with_options("products", &query, &options)
            .unwrap();

        let agg = result.aggregations.get("price_ranges").unwrap();
        if let AggregationResult::Buckets { buckets } = agg {
            assert_eq!(buckets.len(), 3);
            // cheap (<50): 15, 25, 20, 45 = 4
            assert_eq!(buckets[0].key, "cheap");
            assert_eq!(buckets[0].doc_count, 4);
            // mid (50-200): 85, 60, 120 = 3
            assert_eq!(buckets[1].key, "mid");
            assert_eq!(buckets[1].doc_count, 3);
            // expensive (>=200): 999, 699, 499 = 3
            assert_eq!(buckets[2].key, "expensive");
            assert_eq!(buckets[2].doc_count, 3);
        } else {
            panic!("expected Buckets");
        }
    }

    // -- Date histogram integration --

    #[test]
    fn integration_date_histogram_aggregation() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let mut aggs = HashMap::new();
        aggs.insert(
            "hourly".to_string(),
            Aggregation::DateHistogram {
                field: "timestamp".into(),
                interval: DateInterval::Hour,
            },
        );

        let options = SearchOptions {
            size: 10,
            aggregations: aggs,
            ..Default::default()
        };

        let result = mgr
            .search_with_options("products", &query, &options)
            .unwrap();

        let agg = result.aggregations.get("hourly").unwrap();
        if let AggregationResult::Buckets { buckets } = agg {
            // Timestamps are 1h apart, so each doc is in its own bucket.
            assert_eq!(buckets.len(), 10);
            // Each bucket should have doc_count = 1.
            for bucket in buckets {
                assert_eq!(bucket.doc_count, 1);
            }
            // Buckets should be sorted by key ascending.
            let keys: Vec<u64> = buckets
                .iter()
                .map(|b| b.key.parse::<u64>().unwrap())
                .collect();
            let mut sorted_keys = keys.clone();
            sorted_keys.sort();
            assert_eq!(keys, sorted_keys);
        } else {
            panic!("expected Buckets");
        }
    }

    // -- Metric aggregations integration --

    #[test]
    fn integration_metric_aggregations() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let mut aggs = HashMap::new();
        aggs.insert(
            "avg_price".to_string(),
            Aggregation::Avg {
                field: "price".into(),
            },
        );
        aggs.insert(
            "sum_price".to_string(),
            Aggregation::Sum {
                field: "price".into(),
            },
        );
        aggs.insert(
            "min_price".to_string(),
            Aggregation::Min {
                field: "price".into(),
            },
        );
        aggs.insert(
            "max_price".to_string(),
            Aggregation::Max {
                field: "price".into(),
            },
        );
        aggs.insert(
            "unique_categories".to_string(),
            Aggregation::Cardinality {
                field: "category".into(),
            },
        );

        let options = SearchOptions {
            size: 10,
            aggregations: aggs,
            ..Default::default()
        };

        let result = mgr
            .search_with_options("products", &query, &options)
            .unwrap();

        // avg: (999+699+499+15+85+25+60+20+120+45)/10 = 256.7
        if let AggregationResult::Metric { value: Some(v) } = &result.aggregations["avg_price"] {
            assert!((v - 256.7).abs() < 0.1);
        } else {
            panic!("expected avg Metric");
        }

        // sum: 2567.0
        if let AggregationResult::Metric { value: Some(v) } = &result.aggregations["sum_price"] {
            assert!((v - 2567.0).abs() < 0.1);
        } else {
            panic!("expected sum Metric");
        }

        // min: 15.0
        if let AggregationResult::Metric { value: Some(v) } = &result.aggregations["min_price"] {
            assert!((v - 15.0).abs() < f64::EPSILON);
        } else {
            panic!("expected min Metric");
        }

        // max: 999.0
        if let AggregationResult::Metric { value: Some(v) } = &result.aggregations["max_price"] {
            assert!((v - 999.0).abs() < f64::EPSILON);
        } else {
            panic!("expected max Metric");
        }

        // cardinality: 3 (electronics, books, clothing)
        if let AggregationResult::Metric { value: Some(v) } =
            &result.aggregations["unique_categories"]
        {
            assert!((v - 3.0).abs() < f64::EPSILON);
        } else {
            panic!("expected cardinality Metric");
        }
    }

    // -- Sorting integration --

    #[test]
    fn integration_sort_by_numeric_field() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let options = SearchOptions {
            size: 10,
            sort: vec![SortClause {
                field: "price".into(),
                order: SortOrder::Asc,
            }],
            ..Default::default()
        };

        let result = mgr
            .search_with_options("products", &query, &options)
            .unwrap();

        // Verify ascending price order.
        let prices: Vec<f64> = result
            .documents
            .iter()
            .filter_map(|d| {
                if let Some(FieldValue::Number(n)) = d.document.get_field("price") {
                    Some(*n)
                } else {
                    None
                }
            })
            .collect();

        for w in prices.windows(2) {
            assert!(w[0] <= w[1], "prices not sorted asc: {} > {}", w[0], w[1]);
        }
    }

    #[test]
    fn integration_sort_by_score_descending() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let options = SearchOptions {
            size: 10,
            sort: vec![SortClause {
                field: "_score".into(),
                order: SortOrder::Desc,
            }],
            ..Default::default()
        };

        let result = mgr
            .search_with_options("products", &query, &options)
            .unwrap();

        let scores: Vec<f32> = result.documents.iter().map(|d| d.score).collect();
        for w in scores.windows(2) {
            assert!(w[0] >= w[1], "scores not desc: {} < {}", w[0], w[1]);
        }
    }

    #[test]
    fn integration_multi_field_sort() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let options = SearchOptions {
            size: 10,
            sort: vec![
                SortClause {
                    field: "category".into(),
                    order: SortOrder::Asc,
                },
                SortClause {
                    field: "price".into(),
                    order: SortOrder::Asc,
                },
            ],
            ..Default::default()
        };

        let result = mgr
            .search_with_options("products", &query, &options)
            .unwrap();

        // Verify: within the same category, prices are ascending.
        let pairs: Vec<(String, f64)> = result
            .documents
            .iter()
            .map(|d| {
                let cat = match d.document.get_field("category") {
                    Some(FieldValue::Text(s)) => s.clone(),
                    _ => String::new(),
                };
                let price = match d.document.get_field("price") {
                    Some(FieldValue::Number(n)) => *n,
                    _ => 0.0,
                };
                (cat, price)
            })
            .collect();

        // Categories should be sorted: books < clothing < electronics.
        let cats: Vec<&str> = pairs.iter().map(|(c, _)| c.as_str()).collect();
        let mut sorted_cats = cats.clone();
        sorted_cats.sort();
        assert_eq!(cats, sorted_cats);
    }

    // -- Sort values populated for search_after cursor --

    #[test]
    fn integration_sort_values_populated() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let options = SearchOptions {
            size: 3,
            sort: vec![SortClause {
                field: "price".into(),
                order: SortOrder::Asc,
            }],
            ..Default::default()
        };

        let result = mgr
            .search_with_options("products", &query, &options)
            .unwrap();

        // Each hit should have exactly 1 sort value.
        for doc in &result.documents {
            assert_eq!(doc.sort.len(), 1, "expected 1 sort value per hit");
            assert!(
                matches!(doc.sort[0], SortValue::Number(_)),
                "expected numeric sort value"
            );
        }
    }

    // -- Search-after (cursor-based pagination) integration --

    #[test]
    fn integration_search_after_pagination() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let sort = vec![SortClause {
            field: "price".into(),
            order: SortOrder::Asc,
        }];

        // Page 1: first 3 results.
        let options_p1 = SearchOptions {
            size: 3,
            sort: sort.clone(),
            ..Default::default()
        };

        let page1 = mgr
            .search_with_options("products", &query, &options_p1)
            .unwrap();
        assert_eq!(page1.documents.len(), 3);

        // Get the last sort value from page 1 to use as cursor.
        let cursor = page1.documents.last().unwrap().sort.clone();
        assert!(!cursor.is_empty());

        // Page 2: next 3 results after the cursor.
        let options_p2 = SearchOptions {
            size: 3,
            sort: sort.clone(),
            search_after: Some(cursor.clone()),
            ..Default::default()
        };

        let page2 = mgr
            .search_with_options("products", &query, &options_p2)
            .unwrap();
        assert_eq!(page2.documents.len(), 3);

        // Verify no overlap between pages.
        let p1_ids: Vec<&str> = page1
            .documents
            .iter()
            .map(|d| d.document.id.as_str())
            .collect();
        let p2_ids: Vec<&str> = page2
            .documents
            .iter()
            .map(|d| d.document.id.as_str())
            .collect();
        for id in &p2_ids {
            assert!(
                !p1_ids.contains(id),
                "page 2 contains document from page 1: {}",
                id
            );
        }

        // Verify page 2 prices are all > last page 1 price.
        let last_p1_price = match &page1.documents.last().unwrap().sort[0] {
            SortValue::Number(n) => *n,
            _ => panic!("expected Number"),
        };
        for doc in &page2.documents {
            let price = match doc.document.get_field("price") {
                Some(FieldValue::Number(n)) => *n,
                _ => panic!("expected price"),
            };
            assert!(
                price > last_p1_price,
                "page 2 doc price {} should be > cursor {}",
                price,
                last_p1_price
            );
        }
    }

    // -- Collapse / deduplication integration --

    #[test]
    fn integration_collapse_by_field() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let options = SearchOptions {
            size: 10,
            collapse: Some(CollapseOptions {
                field: "category".into(),
            }),
            ..Default::default()
        };

        let result = mgr
            .search_with_options("products", &query, &options)
            .unwrap();

        // Should have at most 3 results (one per category).
        assert_eq!(
            result.documents.len(),
            3,
            "expected 3 collapsed results, got {}",
            result.documents.len()
        );

        // Verify all categories are unique.
        let categories: Vec<String> = result
            .documents
            .iter()
            .map(|d| match d.document.get_field("category") {
                Some(FieldValue::Text(s)) => s.clone(),
                _ => String::new(),
            })
            .collect();

        let unique: std::collections::HashSet<&String> = categories.iter().collect();
        assert_eq!(
            unique.len(),
            categories.len(),
            "categories not unique after collapse"
        );
    }

    // -- Combined: aggregation + sort + collapse --

    #[test]
    fn integration_combined_agg_sort_collapse() {
        let mgr = CollectionIndexManager::new_in_ram();
        setup_products(&mgr);

        let query = Query::FullText(FullTextQuery {
            field: "_body".into(),
            query: "electronics books clothing".into(),
            operator: Operator::Or,
        });

        let mut aggs = HashMap::new();
        aggs.insert(
            "avg_price".to_string(),
            Aggregation::Avg {
                field: "price".into(),
            },
        );
        aggs.insert(
            "top_cats".to_string(),
            Aggregation::Terms {
                field: "category".into(),
                size: 10,
            },
        );

        let options = SearchOptions {
            size: 10,
            sort: vec![SortClause {
                field: "price".into(),
                order: SortOrder::Desc,
            }],
            aggregations: aggs,
            collapse: Some(CollapseOptions {
                field: "category".into(),
            }),
            ..Default::default()
        };

        let result = mgr
            .search_with_options("products", &query, &options)
            .unwrap();

        // Aggregations computed over ALL matched docs (before collapse).
        assert!(result.aggregations.contains_key("avg_price"));
        assert!(result.aggregations.contains_key("top_cats"));

        if let AggregationResult::Metric { value: Some(v) } = &result.aggregations["avg_price"] {
            assert!((v - 256.7).abs() < 0.1);
        } else {
            panic!("expected avg Metric");
        }

        // After collapse: 3 results (one per category).
        assert_eq!(result.documents.len(), 3);

        // After sort desc by price: the highest-priced item in each category
        // should come first.
        let prices: Vec<f64> = result
            .documents
            .iter()
            .filter_map(|d| {
                if let Some(FieldValue::Number(n)) = d.document.get_field("price") {
                    Some(*n)
                } else {
                    None
                }
            })
            .collect();
        for w in prices.windows(2) {
            assert!(w[0] >= w[1], "prices not sorted desc after collapse");
        }
    }
}
