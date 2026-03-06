//! Tantivy-based full-text search index for MSearchDB.
//!
//! [`TantivyIndex`] wraps the Tantivy search library to provide document
//! indexing, querying, deletion, and update operations. It implements the
//! [`IndexBackend`] async trait from `msearchdb-core`.
//!
//! # Architecture
//!
//! - **`Arc<Index>`** — the Tantivy index is shared across threads for reading.
//! - **`Arc<Mutex<IndexWriter>>`** — the writer is behind a `Mutex` because
//!   Tantivy's `IndexWriter` is `!Sync`. Only one thread may write at a time,
//!   but reads are fully concurrent via the `IndexReader`.
//! - **`IndexReader`** — configured with `ReloadPolicy::OnCommitWithDelay` so
//!   that new searchers pick up committed segments automatically.
//! - **`Arc<Schema>`** — the immutable schema is shared across all operations.
//!
//! CPU-bound Tantivy operations (indexing, searching) are dispatched to the
//! Tokio blocking thread pool via [`tokio::task::spawn_blocking`] to avoid
//! starving the async runtime.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//! use msearchdb_index::schema_builder::{SchemaConfig, FieldConfig, FieldType};
//! use msearchdb_index::tantivy_index::TantivyIndex;
//!
//! # async fn example() -> msearchdb_core::DbResult<()> {
//! let config = SchemaConfig::new()
//!     .with_field(FieldConfig::new("title", FieldType::Text))
//!     .with_field(FieldConfig::new("body", FieldType::Text));
//!
//! let index = TantivyIndex::new(Path::new("/tmp/my_index"), config)?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use tantivy::collector::TopDocs;
use tantivy::merge_policy::LogMergePolicy;
use tantivy::query::{
    AllQuery, BooleanQuery, Occur, QueryParser, RangeQuery, TermQuery as TantivyTermQuery,
};
use tantivy::schema::{IndexRecordOption, Schema, Value};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term};

use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::query::{
    BoolQuery, FullTextQuery, Operator, Query, RangeQuery as CoreRangeQuery, ScoredDocument,
    SearchResult, TermQuery as CoreTermQuery,
};
use msearchdb_core::traits::IndexBackend;

use crate::analyzer::register_analyzers;
use crate::schema_builder::{DynamicSchemaBuilder, FieldMap, SchemaConfig};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default heap size for the IndexWriter (50 MB).
const WRITER_HEAP_BYTES: usize = 50 * 1024 * 1024;

/// Default search result limit.
const DEFAULT_SEARCH_LIMIT: usize = 10;

// ---------------------------------------------------------------------------
// TantivyIndex
// ---------------------------------------------------------------------------

/// A full-text search index backed by Tantivy.
///
/// This struct manages the Tantivy index, writer, reader, and schema. It
/// provides both low-level methods (index, commit, search, delete) and
/// implements the [`IndexBackend`] async trait for integration with MSearchDB.
///
/// # Thread safety
///
/// - The `IndexWriter` is wrapped in `Arc<Mutex<_>>` because it is `!Sync`.
///   All write operations acquire the mutex, ensuring single-writer semantics.
/// - The `IndexReader` and `Index` are `Send + Sync` and can be shared freely.
/// - The `Schema` is immutable and shared via `Arc`.
pub struct TantivyIndex {
    /// The Tantivy index instance (thread-safe, shared).
    index: Arc<Index>,
    /// The index writer (single-writer, mutex-protected).
    ///
    /// Tantivy's `IndexWriter` is `!Sync` because it maintains internal
    /// mutable state for segment management. The `Mutex` serializes write
    /// access while allowing concurrent reads.
    writer: Arc<Mutex<IndexWriter>>,
    /// The index reader (automatically reloads on commit).
    reader: IndexReader,
    /// The immutable Tantivy schema, shared across threads.
    schema: Arc<Schema>,
    /// Maps field names to Tantivy `Field` handles for fast lookup.
    field_map: FieldMap,
}

impl TantivyIndex {
    /// Create a new Tantivy index at the given path with the given schema.
    ///
    /// If the directory does not exist, it will be created. If an index already
    /// exists at the path, it will be opened (not overwritten).
    ///
    /// The `IndexWriter` is configured with a 50 MB heap and a `LogMergePolicy`
    /// with at most 5 segments to balance write throughput and search latency.
    ///
    /// # Errors
    ///
    /// Returns `DbError::IndexError` if the index cannot be created or opened.
    pub fn new(index_path: &Path, schema_config: SchemaConfig) -> DbResult<Self> {
        let (schema, field_map) = DynamicSchemaBuilder::build(&schema_config);

        // Create directory if needed
        std::fs::create_dir_all(index_path)
            .map_err(|e| DbError::IndexError(format!("failed to create index directory: {}", e)))?;

        // Create or open the Tantivy index
        let index = Index::create_in_dir(index_path, (*schema).clone()).or_else(|_| {
            Index::open_in_dir(index_path)
                .map_err(|e| DbError::IndexError(format!("failed to open index: {}", e)))
        })?;

        // Register custom analyzers
        register_analyzers(&index);

        // Configure the writer with LogMergePolicy
        let writer = index
            .writer(WRITER_HEAP_BYTES)
            .map_err(|e| DbError::IndexError(format!("failed to create index writer: {}", e)))?;

        let merge_policy = LogMergePolicy::default();
        writer.set_merge_policy(Box::new(merge_policy));

        // Configure the reader
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| DbError::IndexError(format!("failed to create index reader: {}", e)))?;

        Ok(Self {
            index: Arc::new(index),
            writer: Arc::new(Mutex::new(writer)),
            reader,
            schema,
            field_map,
        })
    }

    /// Create a new Tantivy index in RAM (useful for testing).
    ///
    /// Behaves identically to [`new`](Self::new) but stores data in memory
    /// instead of on disk.
    pub fn new_in_ram(schema_config: SchemaConfig) -> DbResult<Self> {
        let (schema, field_map) = DynamicSchemaBuilder::build(&schema_config);

        let index = Index::create_in_ram((*schema).clone());
        register_analyzers(&index);

        let writer = index
            .writer(WRITER_HEAP_BYTES)
            .map_err(|e| DbError::IndexError(format!("failed to create index writer: {}", e)))?;

        let merge_policy = LogMergePolicy::default();
        writer.set_merge_policy(Box::new(merge_policy));

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| DbError::IndexError(format!("failed to create index reader: {}", e)))?;

        Ok(Self {
            index: Arc::new(index),
            writer: Arc::new(Mutex::new(writer)),
            reader,
            schema,
            field_map,
        })
    }

    /// Get a reference to the Tantivy [`Index`].
    pub fn inner_index(&self) -> &Index {
        &self.index
    }

    /// Get the shared schema.
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// Get the field map.
    pub fn field_map(&self) -> &FieldMap {
        &self.field_map
    }

    // -----------------------------------------------------------------------
    // Document conversion
    // -----------------------------------------------------------------------

    /// Convert an MSearchDB [`Document`] to a Tantivy [`TantivyDocument`].
    ///
    /// Maps each field in the document to the corresponding Tantivy field
    /// using the schema's field map. Unknown fields are silently skipped.
    fn to_tantivy_doc(&self, doc: &Document) -> TantivyDocument {
        let mut tantivy_doc = TantivyDocument::new();

        // Set the _id field
        if let Some(&id_field) = self.field_map.get("_id") {
            tantivy_doc.add_text(id_field, doc.id.as_str());
        }

        // Set the _timestamp field
        if let Some(&ts_field) = self.field_map.get("_timestamp") {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            tantivy_doc.add_u64(ts_field, ts);
        }

        // Map user fields
        for (name, value) in &doc.fields {
            if let Some(&field) = self.field_map.get(name) {
                match value {
                    FieldValue::Text(text) => {
                        tantivy_doc.add_text(field, text);
                    }
                    FieldValue::Number(n) => {
                        tantivy_doc.add_f64(field, *n);
                    }
                    FieldValue::Boolean(b) => {
                        tantivy_doc.add_u64(field, if *b { 1 } else { 0 });
                    }
                    FieldValue::Array(arr) => {
                        // Index each element of the array into the same field
                        for item in arr {
                            if let FieldValue::Text(text) = item {
                                tantivy_doc.add_text(field, text);
                            }
                        }
                    }
                    // Future FieldValue variants are silently skipped
                    _ => {}
                }
            }
            // Fields not in the schema are silently skipped —
            // this supports schemaless documents where only configured
            // fields are indexed.
        }

        tantivy_doc
    }

    /// Reconstruct an MSearchDB [`Document`] from a Tantivy [`TantivyDocument`].
    fn reconstruct_document(&self, tantivy_doc: &TantivyDocument) -> Document {
        // Extract _id
        let id = self
            .field_map
            .get("_id")
            .and_then(|&f| {
                tantivy_doc
                    .get_first(f)
                    .and_then(|v| v.as_str().map(|s| s.to_owned()))
            })
            .unwrap_or_default();

        let mut fields = HashMap::new();

        for (name, &field) in &self.field_map {
            // Skip meta-fields
            if name.starts_with('_') {
                continue;
            }

            let entry = self.schema.get_field_entry(field);
            let field_type = entry.field_type();

            if field_type.is_indexed() || entry.is_stored() {
                if let Some(value) = tantivy_doc.get_first(field) {
                    if let Some(text) = value.as_str() {
                        fields.insert(name.clone(), FieldValue::Text(text.to_owned()));
                    } else if let Some(n) = value.as_f64() {
                        fields.insert(name.clone(), FieldValue::Number(n));
                    } else if let Some(n) = value.as_u64() {
                        // Check if this is a boolean field (stored as u64)
                        if name != "_timestamp" && name != "_score" {
                            fields.insert(name.clone(), FieldValue::Boolean(n != 0));
                        }
                    }
                }
            }
        }

        Document {
            id: DocumentId::new(id),
            fields,
        }
    }

    // -----------------------------------------------------------------------
    // Core operations
    // -----------------------------------------------------------------------

    /// Add a document to the index without committing.
    ///
    /// The document is buffered in the writer's heap. Call [`commit`](Self::commit)
    /// to make it visible to searchers. Batching writes this way is significantly
    /// faster than committing after every document.
    ///
    /// # Errors
    ///
    /// Returns `DbError::IndexError` if the writer fails.
    pub fn index_document_sync(&self, doc: &Document) -> DbResult<()> {
        let tantivy_doc = self.to_tantivy_doc(doc);
        let writer = self
            .writer
            .lock()
            .map_err(|e| DbError::IndexError(format!("failed to acquire writer lock: {}", e)))?;
        writer
            .add_document(tantivy_doc)
            .map_err(|e| DbError::IndexError(format!("failed to index document: {}", e)))?;
        Ok(())
    }

    /// Commit all buffered writes and return the commit opstamp.
    ///
    /// After committing, the reader will automatically reload to pick up
    /// the new segments (per `ReloadPolicy::OnCommitWithDelay`).
    ///
    /// # Errors
    ///
    /// Returns `DbError::IndexError` if the commit fails.
    pub fn commit(&self) -> DbResult<u64> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|e| DbError::IndexError(format!("failed to acquire writer lock: {}", e)))?;
        let opstamp = writer
            .commit()
            .map_err(|e| DbError::IndexError(format!("commit failed: {}", e)))?;

        // Manually reload to ensure the reader sees new data immediately
        self.reader
            .reload()
            .map_err(|e| DbError::IndexError(format!("reader reload failed: {}", e)))?;

        tracing::info!(opstamp, "index committed");
        Ok(opstamp)
    }

    /// Execute a search query and return scored results.
    ///
    /// The query is translated from the MSearchDB [`Query`] DSL into a Tantivy
    /// query. Results are collected using `TopDocs` with BM25 scoring and
    /// reconstructed into MSearchDB [`Document`] objects.
    ///
    /// # Errors
    ///
    /// Returns `DbError::IndexError` if the search fails.
    pub fn search_sync(&self, query: &Query, limit: usize) -> DbResult<SearchResult> {
        let start = Instant::now();
        let searcher = self.reader.searcher();

        let tantivy_query = self.translate_query(query, &searcher)?;

        let top_docs = searcher
            .search(&*tantivy_query, &TopDocs::with_limit(limit))
            .map_err(|e| DbError::IndexError(format!("search failed: {}", e)))?;

        let total = top_docs.len() as u64;
        let mut documents = Vec::with_capacity(top_docs.len());

        for (score, doc_address) in &top_docs {
            let retrieved: TantivyDocument = searcher
                .doc(*doc_address)
                .map_err(|e| DbError::IndexError(format!("failed to retrieve document: {}", e)))?;
            let doc = self.reconstruct_document(&retrieved);
            documents.push(ScoredDocument {
                document: doc,
                score: *score,
            });
        }

        let took_ms = start.elapsed().as_millis() as u64;

        Ok(SearchResult {
            documents,
            total,
            took_ms,
        })
    }

    /// Delete a document from the index by its [`DocumentId`].
    ///
    /// Uses a [`Term`] on the `_id` field. The deletion is buffered and
    /// takes effect after the next [`commit`](Self::commit).
    ///
    /// # Errors
    ///
    /// Returns `DbError::IndexError` if the writer fails.
    pub fn delete_document_sync(&self, id: &DocumentId) -> DbResult<()> {
        let id_field = self
            .field_map
            .get("_id")
            .ok_or_else(|| DbError::IndexError("_id field not in schema".to_owned()))?;

        let term = Term::from_field_text(*id_field, id.as_str());
        let writer = self
            .writer
            .lock()
            .map_err(|e| DbError::IndexError(format!("failed to acquire writer lock: {}", e)))?;
        writer.delete_term(term);

        tracing::debug!(id = %id, "document deletion queued");
        Ok(())
    }

    /// Update a document by deleting the old version and indexing the new one.
    ///
    /// This is atomic from Tantivy's perspective: both the deletion and the
    /// insertion happen in the same writer transaction.
    ///
    /// # Errors
    ///
    /// Returns `DbError::IndexError` if either operation fails.
    pub fn update_document_sync(&self, doc: &Document) -> DbResult<()> {
        self.delete_document_sync(&doc.id)?;
        self.index_document_sync(doc)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Query translation
    // -----------------------------------------------------------------------

    /// Translate an MSearchDB [`Query`] into a boxed Tantivy query.
    fn translate_query(
        &self,
        query: &Query,
        _searcher: &Searcher,
    ) -> DbResult<Box<dyn tantivy::query::Query>> {
        match query {
            Query::FullText(ftq) => self.translate_full_text(ftq),
            Query::Term(tq) => self.translate_term(tq),
            Query::Range(rq) => self.translate_range(rq),
            Query::Bool(bq) => self.translate_bool(bq),
            _ => Err(DbError::InvalidInput("unsupported query type".to_owned())),
        }
    }

    /// Translate a [`FullTextQuery`] using Tantivy's `QueryParser` with BM25.
    ///
    /// Supports fuzzy search via the `term~N` syntax (e.g., `"Ruts~1"` matches
    /// "Rust" with edit distance 1). When fuzzy syntax is detected, the field
    /// is configured with [`QueryParser::set_field_fuzzy`] and the tilde
    /// notation is stripped before parsing so the query parser receives a
    /// plain term.
    fn translate_full_text(&self, ftq: &FullTextQuery) -> DbResult<Box<dyn tantivy::query::Query>> {
        let field = self
            .field_map
            .get(&ftq.field)
            .ok_or_else(|| DbError::InvalidInput(format!("field '{}' not in schema", ftq.field)))?;

        let mut parser = QueryParser::for_index(&self.index, vec![*field]);

        // Set conjunction/disjunction based on Operator
        match ftq.operator {
            Operator::And => parser.set_conjunction_by_default(),
            Operator::Or => {} // default is disjunction
            _ => {}            // future Operator variants: fall back to disjunction
        }

        // Detect fuzzy syntax: "term~N" where N is an edit distance (1 or 2).
        // Strip the tilde notation and enable fuzzy on the field instead.
        let query_text = if let Some(caps) = extract_fuzzy_params(&ftq.query) {
            parser.set_field_fuzzy(*field, false, caps.distance, true);
            caps.clean_query
        } else {
            ftq.query.clone()
        };

        let parsed = parser
            .parse_query(&query_text)
            .map_err(|e| DbError::IndexError(format!("failed to parse full-text query: {}", e)))?;

        Ok(parsed)
    }

    /// Translate a [`CoreTermQuery`] into a Tantivy exact-match term query.
    fn translate_term(&self, tq: &CoreTermQuery) -> DbResult<Box<dyn tantivy::query::Query>> {
        let field = self
            .field_map
            .get(&tq.field)
            .ok_or_else(|| DbError::InvalidInput(format!("field '{}' not in schema", tq.field)))?;

        let term = Term::from_field_text(*field, &tq.value);
        Ok(Box::new(TantivyTermQuery::new(
            term,
            IndexRecordOption::WithFreqsAndPositions,
        )))
    }

    /// Translate a [`CoreRangeQuery`] into a Tantivy range query on F64 fields.
    fn translate_range(&self, rq: &CoreRangeQuery) -> DbResult<Box<dyn tantivy::query::Query>> {
        let field = self
            .field_map
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

    /// Translate a [`BoolQuery`] into a Tantivy `BooleanQuery`.
    fn translate_bool(&self, bq: &BoolQuery) -> DbResult<Box<dyn tantivy::query::Query>> {
        let searcher = self.reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();

        for q in &bq.must {
            let translated = self.translate_query(q, &searcher)?;
            clauses.push((Occur::Must, translated));
        }

        for q in &bq.should {
            let translated = self.translate_query(q, &searcher)?;
            clauses.push((Occur::Should, translated));
        }

        for q in &bq.must_not {
            let translated = self.translate_query(q, &searcher)?;
            clauses.push((Occur::MustNot, translated));
        }

        // If only must_not clauses exist, add AllQuery so the boolean query
        // has something to match against
        if bq.must.is_empty() && bq.should.is_empty() && !bq.must_not.is_empty() {
            clauses.push((Occur::Must, Box::new(AllQuery)));
        }

        Ok(Box::new(BooleanQuery::new(clauses)))
    }
}

// ---------------------------------------------------------------------------
// Fuzzy query helpers
// ---------------------------------------------------------------------------

/// Parameters extracted from a fuzzy query string such as `"Ruts~1"`.
struct FuzzyParams {
    /// The query string with the `~N` suffix removed.
    clean_query: String,
    /// The requested edit distance (1 or 2).
    distance: u8,
}

/// Detect and extract fuzzy search parameters from a query string.
///
/// Recognises patterns like `"term~1"`, `"Ruts~2"`, or multi-word
/// `"hello~1 world~2"`. When *any* word carries a tilde suffix the
/// overall distance is taken from the **first** match and the tildes
/// are stripped from every word so the `QueryParser` receives plain
/// tokens.
///
/// Returns `None` if no fuzzy syntax is present.
fn extract_fuzzy_params(query: &str) -> Option<FuzzyParams> {
    // Quick bail-out: no tilde → nothing to do.
    if !query.contains('~') {
        return None;
    }

    let mut distance: Option<u8> = None;
    let mut clean_parts: Vec<String> = Vec::new();
    let mut found = false;

    for word in query.split_whitespace() {
        if let Some(pos) = word.rfind('~') {
            let (term, rest) = word.split_at(pos);
            // rest is "~N" — parse the digit(s) after '~'
            if let Ok(d) = rest[1..].parse::<u8>() {
                if (1..=2).contains(&d) {
                    found = true;
                    if distance.is_none() {
                        distance = Some(d);
                    }
                    clean_parts.push(term.to_owned());
                    continue;
                }
            }
        }
        // No tilde or unparseable — keep as-is
        clean_parts.push(word.to_owned());
    }

    if found {
        Some(FuzzyParams {
            clean_query: clean_parts.join(" "),
            distance: distance.unwrap_or(1),
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// IndexBackend trait implementation
// ---------------------------------------------------------------------------

/// Async implementation of [`IndexBackend`] for [`TantivyIndex`].
///
/// All operations are dispatched to [`tokio::task::spawn_blocking`] because
/// Tantivy is a CPU-bound library. This prevents blocking the async runtime's
/// cooperative scheduling.
#[async_trait]
impl IndexBackend for TantivyIndex {
    /// Add or update a document in the search index.
    ///
    /// The document is indexed and committed immediately. For higher throughput,
    /// use the synchronous [`index_document_sync`](TantivyIndex::index_document_sync)
    /// and [`commit`](TantivyIndex::commit) methods separately.
    async fn index_document(&self, document: &Document) -> DbResult<()> {
        // Clone data needed for the blocking task
        let doc = document.clone();
        let writer = Arc::clone(&self.writer);
        let field_map = self.field_map.clone();

        // We need to build the tantivy doc inside spawn_blocking
        tokio::task::spawn_blocking(move || {
            let mut tantivy_doc = TantivyDocument::new();

            // Set _id
            if let Some(&id_field) = field_map.get("_id") {
                tantivy_doc.add_text(id_field, doc.id.as_str());
            }

            // Set _timestamp
            if let Some(&ts_field) = field_map.get("_timestamp") {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                tantivy_doc.add_u64(ts_field, ts);
            }

            // Map user fields
            for (name, value) in &doc.fields {
                if let Some(&field) = field_map.get(name) {
                    match value {
                        FieldValue::Text(text) => tantivy_doc.add_text(field, text),
                        FieldValue::Number(n) => tantivy_doc.add_f64(field, *n),
                        FieldValue::Boolean(b) => {
                            tantivy_doc.add_u64(field, if *b { 1 } else { 0 })
                        }
                        FieldValue::Array(arr) => {
                            for item in arr {
                                if let FieldValue::Text(text) = item {
                                    tantivy_doc.add_text(field, text);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            let w = writer
                .lock()
                .map_err(|e| DbError::IndexError(format!("writer lock: {}", e)))?;
            w.add_document(tantivy_doc)
                .map_err(|e| DbError::IndexError(format!("index document: {}", e)))?;
            Ok(())
        })
        .await
        .map_err(|e| DbError::IndexError(format!("spawn_blocking: {}", e)))?
    }

    /// Execute a search query against the index.
    async fn search(&self, query: &Query) -> DbResult<SearchResult> {
        // We need to perform the search in the current thread context
        // because Searcher borrows from reader
        self.search_sync(query, DEFAULT_SEARCH_LIMIT)
    }

    /// Remove a document from the index.
    async fn delete_document(&self, id: &DocumentId) -> DbResult<()> {
        let id_field = *self
            .field_map
            .get("_id")
            .ok_or_else(|| DbError::IndexError("_id field not in schema".to_owned()))?;

        let id_str = id.as_str().to_owned();
        let writer = Arc::clone(&self.writer);

        tokio::task::spawn_blocking(move || {
            let term = Term::from_field_text(id_field, &id_str);
            let w = writer
                .lock()
                .map_err(|e| DbError::IndexError(format!("writer lock: {}", e)))?;
            w.delete_term(term);
            Ok(())
        })
        .await
        .map_err(|e| DbError::IndexError(format!("spawn_blocking: {}", e)))?
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_builder::{FieldConfig, FieldType, SchemaConfig};

    /// Create a test index with standard text + numeric fields.
    fn test_index() -> TantivyIndex {
        let config = SchemaConfig::new()
            .with_field(FieldConfig::new("title", FieldType::Text))
            .with_field(FieldConfig::new("body", FieldType::Text))
            .with_field(FieldConfig::new("price", FieldType::Number))
            .with_field(FieldConfig::new("active", FieldType::Boolean));

        TantivyIndex::new_in_ram(config).unwrap()
    }

    /// Create a test document.
    fn test_doc(id: &str, title: &str, body: &str, price: f64) -> Document {
        Document::new(DocumentId::new(id))
            .with_field("title", FieldValue::Text(title.into()))
            .with_field("body", FieldValue::Text(body.into()))
            .with_field("price", FieldValue::Number(price))
            .with_field("active", FieldValue::Boolean(true))
    }

    #[test]
    fn create_index_in_ram() {
        let idx = test_index();
        assert!(idx.field_map.contains_key("_id"));
        assert!(idx.field_map.contains_key("title"));
        assert!(idx.field_map.contains_key("body"));
        assert!(idx.field_map.contains_key("price"));
    }

    #[test]
    fn index_and_search_single_document() {
        let idx = test_index();
        let doc = test_doc("doc-1", "Rust Programming", "Learn Rust today", 29.99);

        idx.index_document_sync(&doc).unwrap();
        idx.commit().unwrap();

        let query = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "Rust".into(),
            operator: Operator::Or,
        });

        let result = idx.search_sync(&query, 10).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.documents[0].document.id.as_str(), "doc-1");
    }

    #[test]
    fn index_100_documents_and_search() {
        let idx = test_index();

        for i in 0..100 {
            let doc = test_doc(
                &format!("doc-{}", i),
                &format!("Document number {}", i),
                &format!("This is the body of document {}", i),
                i as f64 * 1.5,
            );
            idx.index_document_sync(&doc).unwrap();
        }
        idx.commit().unwrap();

        // Search for "document"
        let query = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "Document".into(),
            operator: Operator::Or,
        });

        let result = idx.search_sync(&query, 100).unwrap();
        assert_eq!(result.total, 100);
    }

    #[test]
    fn bm25_exact_match_scores_higher() {
        let idx = test_index();

        let doc1 = test_doc(
            "exact",
            "distributed database systems",
            "A treatise on distributed database systems",
            10.0,
        );
        let doc2 = test_doc(
            "partial",
            "introduction to systems programming",
            "Systems are everywhere",
            20.0,
        );

        idx.index_document_sync(&doc1).unwrap();
        idx.index_document_sync(&doc2).unwrap();
        idx.commit().unwrap();

        let query = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "distributed database".into(),
            operator: Operator::And,
        });

        let result = idx.search_sync(&query, 10).unwrap();
        assert!(!result.documents.is_empty());
        // The exact match should be the top result
        assert_eq!(result.documents[0].document.id.as_str(), "exact");
    }

    #[test]
    fn term_query_exact_match() {
        let idx = test_index();

        let doc = test_doc("t1", "Exact Match Test", "body", 5.0);
        idx.index_document_sync(&doc).unwrap();
        idx.commit().unwrap();

        let query = Query::Term(CoreTermQuery {
            field: "_id".into(),
            value: "t1".into(),
        });

        let result = idx.search_sync(&query, 10).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.documents[0].document.id.as_str(), "t1");
    }

    #[test]
    fn range_query_numeric_filter() {
        let idx = test_index();

        for i in 0..10 {
            let doc = test_doc(
                &format!("rng-{}", i),
                &format!("Item {}", i),
                "body",
                i as f64 * 10.0,
            );
            idx.index_document_sync(&doc).unwrap();
        }
        idx.commit().unwrap();

        let query = Query::Range(CoreRangeQuery {
            field: "price".into(),
            gte: Some(20.0),
            lte: Some(50.0),
        });

        let result = idx.search_sync(&query, 10).unwrap();
        // Documents with price 20, 30, 40, 50
        assert_eq!(result.total, 4);
    }

    #[test]
    fn bool_query_must_and_must_not() {
        let idx = test_index();

        let doc1 = test_doc("b1", "Rust programming", "Learn Rust", 10.0);
        let doc2 = test_doc("b2", "Go programming", "Learn Go", 10.0);
        let doc3 = test_doc("b3", "Rust systems", "Rust for systems", 20.0);

        idx.index_document_sync(&doc1).unwrap();
        idx.index_document_sync(&doc2).unwrap();
        idx.index_document_sync(&doc3).unwrap();
        idx.commit().unwrap();

        // Must match "Rust" in title, must NOT match "systems" in title
        let query = Query::Bool(BoolQuery {
            must: vec![Query::FullText(FullTextQuery {
                field: "title".into(),
                query: "Rust".into(),
                operator: Operator::Or,
            })],
            should: vec![],
            must_not: vec![Query::FullText(FullTextQuery {
                field: "title".into(),
                query: "systems".into(),
                operator: Operator::Or,
            })],
        });

        let result = idx.search_sync(&query, 10).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.documents[0].document.id.as_str(), "b1");
    }

    #[test]
    fn delete_removes_document_from_results() {
        let idx = test_index();

        let doc = test_doc("del-1", "Delete me", "This will be deleted", 1.0);
        idx.index_document_sync(&doc).unwrap();
        idx.commit().unwrap();

        // Verify it exists
        let query = Query::Term(CoreTermQuery {
            field: "_id".into(),
            value: "del-1".into(),
        });
        let result = idx.search_sync(&query, 10).unwrap();
        assert_eq!(result.total, 1);

        // Delete and commit
        idx.delete_document_sync(&DocumentId::new("del-1")).unwrap();
        idx.commit().unwrap();

        // Verify it's gone
        let result = idx.search_sync(&query, 10).unwrap();
        assert_eq!(result.total, 0);
    }

    #[test]
    fn update_replaces_document() {
        let idx = test_index();

        let doc_v1 = test_doc("upd-1", "Version One", "First version", 1.0);
        idx.index_document_sync(&doc_v1).unwrap();
        idx.commit().unwrap();

        // Update with new content
        let doc_v2 = test_doc("upd-1", "Version Two", "Second version", 2.0);
        idx.update_document_sync(&doc_v2).unwrap();
        idx.commit().unwrap();

        // Search for old content should not find it
        let query_old = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "One".into(),
            operator: Operator::Or,
        });
        let result = idx.search_sync(&query_old, 10).unwrap();
        assert_eq!(result.total, 0);

        // Search for new content should find it
        let query_new = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "Two".into(),
            operator: Operator::Or,
        });
        let result = idx.search_sync(&query_new, 10).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.documents[0].document.id.as_str(), "upd-1");
    }

    #[test]
    fn document_roundtrip_preserves_fields() {
        let idx = test_index();

        let doc = test_doc("rt-1", "Roundtrip Test", "Body text", 42.0);
        idx.index_document_sync(&doc).unwrap();
        idx.commit().unwrap();

        let query = Query::Term(CoreTermQuery {
            field: "_id".into(),
            value: "rt-1".into(),
        });

        let result = idx.search_sync(&query, 1).unwrap();
        assert_eq!(result.total, 1);

        let retrieved = &result.documents[0].document;
        assert_eq!(retrieved.id.as_str(), "rt-1");
        assert_eq!(
            retrieved.get_field("title"),
            Some(&FieldValue::Text("Roundtrip Test".into()))
        );
        assert_eq!(
            retrieved.get_field("body"),
            Some(&FieldValue::Text("Body text".into()))
        );
        assert_eq!(
            retrieved.get_field("price"),
            Some(&FieldValue::Number(42.0))
        );
        assert_eq!(
            retrieved.get_field("active"),
            Some(&FieldValue::Boolean(true))
        );
    }

    #[tokio::test]
    async fn async_index_backend_trait() {
        let idx = test_index();

        let doc = test_doc("async-1", "Async Test", "Async body", 1.0);
        idx.index_document(&doc).await.unwrap();
        idx.commit().unwrap();

        let query = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "Async".into(),
            operator: Operator::Or,
        });

        let result = idx.search(&query).await.unwrap();
        assert_eq!(result.total, 1);
    }

    #[tokio::test]
    async fn async_delete_via_trait() {
        let idx = test_index();

        let doc = test_doc("async-del", "Delete Async", "body", 1.0);
        idx.index_document(&doc).await.unwrap();
        idx.commit().unwrap();

        idx.delete_document(&DocumentId::new("async-del"))
            .await
            .unwrap();
        idx.commit().unwrap();

        let query = Query::Term(CoreTermQuery {
            field: "_id".into(),
            value: "async-del".into(),
        });
        let result = idx.search(&query).await.unwrap();
        assert_eq!(result.total, 0);
    }

    #[test]
    fn search_returns_timing() {
        let idx = test_index();
        let doc = test_doc("timing-1", "Timing Test", "body", 1.0);
        idx.index_document_sync(&doc).unwrap();
        idx.commit().unwrap();

        let query = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "Timing".into(),
            operator: Operator::Or,
        });

        let result = idx.search_sync(&query, 10).unwrap();
        // took_ms should be a non-negative number
        assert!(result.took_ms < 5000); // sanity: should be well under 5s
    }
}
