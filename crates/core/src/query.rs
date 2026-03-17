//! Query DSL, aggregation framework, sorting, pagination, and search result
//! types for MSearchDB.
//!
//! MSearchDB supports a composable query language including full-text search,
//! exact term matching, numeric range filters, and boolean combinations.  On
//! top of the query layer this module provides:
//!
//! - **Aggregations** — terms, range, date histogram (bucket), and avg / sum /
//!   min / max / cardinality (metric).
//! - **Sorting** — multi-field sort, sort-by-score, sort-by-numeric-field.
//! - **Pagination** — `search_after` cursor-based deep pagination.
//! - **Collapse** — field-level deduplication / grouping.
//!
//! # Examples
//!
//! ```
//! use msearchdb_core::query::*;
//!
//! let query = Query::FullText(FullTextQuery {
//!     field: "title".into(),
//!     query: "distributed database".into(),
//!     operator: Operator::And,
//! });
//!
//! let json = serde_json::to_string(&query).unwrap();
//! let back: Query = serde_json::from_str(&json).unwrap();
//! assert_eq!(query, back);
//! ```

use serde::{Deserialize, Serialize};

use std::collections::HashMap;

use crate::document::{Document, FieldValue};

// ---------------------------------------------------------------------------
// Operator
// ---------------------------------------------------------------------------

/// Boolean operator for combining terms within a full-text query.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Operator {
    /// All terms must match.
    And,
    /// At least one term must match.
    #[default]
    Or,
}

// ---------------------------------------------------------------------------
// Query types
// ---------------------------------------------------------------------------

/// A full-text search query against a single field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FullTextQuery {
    /// The document field to search.
    pub field: String,
    /// The search text (will be tokenized).
    pub query: String,
    /// How to combine the individual tokens.
    pub operator: Operator,
}

/// An exact-match term query against a single field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TermQuery {
    /// The document field to match.
    pub field: String,
    /// The exact value to match.
    pub value: String,
}

/// A numeric range query against a single field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RangeQuery {
    /// The document field to filter.
    pub field: String,
    /// Inclusive lower bound (if any).
    pub gte: Option<f64>,
    /// Inclusive upper bound (if any).
    pub lte: Option<f64>,
}

/// A boolean combination of sub-queries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoolQuery {
    /// All of these queries must match.
    pub must: Vec<Query>,
    /// At least one of these queries should match.
    pub should: Vec<Query>,
    /// None of these queries may match.
    pub must_not: Vec<Query>,
}

/// Top-level query DSL enum.
///
/// Marked `#[non_exhaustive]` to allow adding new query types (e.g., fuzzy,
/// prefix, geo) in future releases.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Query {
    /// Full-text search with tokenization and scoring.
    FullText(FullTextQuery),
    /// Exact term matching (not analyzed).
    Term(TermQuery),
    /// Numeric range filtering.
    Range(RangeQuery),
    /// Boolean combination of sub-queries.
    Bool(BoolQuery),
}

// ---------------------------------------------------------------------------
// Sort
// ---------------------------------------------------------------------------

/// Sort direction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SortOrder {
    /// Ascending (smallest first).
    Asc,
    /// Descending (largest first).
    #[default]
    Desc,
}

/// A single sort clause.
///
/// Use `field = "_score"` to sort by relevance score.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SortClause {
    /// The field to sort on.  `"_score"` sorts by relevance.
    pub field: String,
    /// Sort direction.
    pub order: SortOrder,
}

// ---------------------------------------------------------------------------
// Search-after (cursor-based deep pagination)
// ---------------------------------------------------------------------------

/// A single sort-key value used as a cursor for `search_after` pagination.
///
/// After each page, the client sends back the last document's sort values
/// as the `search_after` array for the next page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SortValue {
    /// Numeric sort value.
    Number(f64),
    /// String sort value (for text / keyword fields).
    Text(String),
}

// ---------------------------------------------------------------------------
// Collapse (field-level deduplication)
// ---------------------------------------------------------------------------

/// Collapse (deduplicate) search results by a specific field.
///
/// Only the top-scoring document per unique field value is returned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollapseOptions {
    /// The field to deduplicate on.
    pub field: String,
}

// ---------------------------------------------------------------------------
// Aggregations
// ---------------------------------------------------------------------------

/// A single range bucket definition for a range aggregation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RangeBucketDef {
    /// Optional human-readable key for this bucket.
    #[serde(default)]
    pub key: Option<String>,
    /// Inclusive lower bound (if any).
    #[serde(default)]
    pub from: Option<f64>,
    /// Exclusive upper bound (if any).
    #[serde(default)]
    pub to: Option<f64>,
}

/// Date-histogram calendar interval.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DateInterval {
    /// One second (1 s).
    Second,
    /// One minute (60 s).
    Minute,
    /// One hour (3 600 s).
    Hour,
    /// One day (86 400 s).
    Day,
    /// One week (604 800 s).
    Week,
    /// One calendar month (variable).
    Month,
}

impl DateInterval {
    /// Return the fixed number of seconds for fixed-width intervals.
    ///
    /// `Month` is variable-width; this returns 30 days as a reasonable
    /// approximation.
    pub fn as_secs(&self) -> u64 {
        match self {
            DateInterval::Second => 1,
            DateInterval::Minute => 60,
            DateInterval::Hour => 3_600,
            DateInterval::Day => 86_400,
            DateInterval::Week => 604_800,
            DateInterval::Month => 2_592_000, // 30 days
        }
    }
}

/// Top-level aggregation definition.
///
/// Aggregations are named (keyed by a user-chosen string in the request).
/// Each variant describes a different aggregation type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Aggregation {
    /// Terms aggregation — top N values of a field with doc counts.
    Terms {
        /// The field to aggregate on.
        field: String,
        /// Maximum number of buckets to return.
        #[serde(default = "default_terms_size")]
        size: usize,
    },

    /// Range aggregation — bucket documents by user-defined numeric ranges.
    Range {
        /// The numeric field to aggregate on.
        field: String,
        /// The range definitions.
        ranges: Vec<RangeBucketDef>,
    },

    /// Date histogram — bucket documents by time intervals.
    DateHistogram {
        /// The numeric timestamp field (epoch seconds).
        field: String,
        /// The calendar interval.
        interval: DateInterval,
    },

    /// Average metric aggregation.
    Avg {
        /// The numeric field.
        field: String,
    },

    /// Sum metric aggregation.
    Sum {
        /// The numeric field.
        field: String,
    },

    /// Min metric aggregation.
    Min {
        /// The numeric field.
        field: String,
    },

    /// Max metric aggregation.
    Max {
        /// The numeric field.
        field: String,
    },

    /// Cardinality (approximate distinct count) metric aggregation.
    Cardinality {
        /// The field to count distinct values of.
        field: String,
    },
}

fn default_terms_size() -> usize {
    10
}

// ---------------------------------------------------------------------------
// Aggregation results
// ---------------------------------------------------------------------------

/// A single bucket in a bucket aggregation result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AggregationBucket {
    /// Bucket key (the field value or range label).
    pub key: String,
    /// Number of documents in this bucket.
    pub doc_count: u64,
}

/// The result of a single named aggregation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AggregationResult {
    /// Bucket aggregation result (terms, range, date_histogram).
    Buckets {
        /// The buckets in order.
        buckets: Vec<AggregationBucket>,
    },
    /// Single-value metric result (avg, sum, min, max, cardinality).
    Metric {
        /// The computed value.  `None` when there are zero matching docs.
        value: Option<f64>,
    },
}

// ---------------------------------------------------------------------------
// SearchOptions — everything beyond the query itself
// ---------------------------------------------------------------------------

/// Extended search options beyond the core query.
///
/// Carries sorting, pagination, aggregations, and collapse configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchOptions {
    /// Maximum number of hits to return.
    #[serde(default = "default_search_size")]
    pub size: usize,

    /// Offset into the result set (classic from/size pagination).
    #[serde(default)]
    pub from: usize,

    /// Sort specification (evaluated left-to-right).
    #[serde(default)]
    pub sort: Vec<SortClause>,

    /// Cursor for deep pagination.  Must correspond 1:1 with [`sort`].
    #[serde(default)]
    pub search_after: Option<Vec<SortValue>>,

    /// Named aggregations to compute over the matching documents.
    #[serde(default)]
    pub aggregations: HashMap<String, Aggregation>,

    /// Field-level deduplication / collapse.
    #[serde(default)]
    pub collapse: Option<CollapseOptions>,
}

fn default_search_size() -> usize {
    10
}

// ---------------------------------------------------------------------------
// Search results
// ---------------------------------------------------------------------------

/// A document paired with its relevance score and sort values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoredDocument {
    /// The matched document.
    pub document: Document,
    /// TF-IDF or BM25 relevance score.
    pub score: f32,
    /// Sort values for this document (used as cursor for `search_after`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<SortValue>,
}

/// The result of a search operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    /// The top-scoring documents for this query.
    pub documents: Vec<ScoredDocument>,
    /// Total number of matching documents (may exceed `documents.len()`).
    pub total: u64,
    /// Wall-clock time in milliseconds taken to execute the search.
    pub took_ms: u64,
    /// Named aggregation results (empty map if no aggregations requested).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub aggregations: HashMap<String, AggregationResult>,
}

impl SearchResult {
    /// Create an empty search result (useful for no-match cases).
    pub fn empty(took_ms: u64) -> Self {
        Self {
            documents: Vec::new(),
            total: 0,
            took_ms,
            aggregations: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregation engine — compute aggregations over a set of documents
// ---------------------------------------------------------------------------

/// Compute all requested aggregations over the given documents.
///
/// This is the core aggregation engine.  It iterates over the document set
/// once and accumulates state for each named aggregation.
pub fn compute_aggregations(
    docs: &[ScoredDocument],
    aggregations: &HashMap<String, Aggregation>,
) -> HashMap<String, AggregationResult> {
    let mut results = HashMap::new();

    for (name, agg) in aggregations {
        let result = match agg {
            Aggregation::Terms { field, size } => compute_terms(docs, field, *size),
            Aggregation::Range { field, ranges } => compute_range(docs, field, ranges),
            Aggregation::DateHistogram { field, interval } => {
                compute_date_histogram(docs, field, interval)
            }
            Aggregation::Avg { field } => compute_avg(docs, field),
            Aggregation::Sum { field } => compute_sum(docs, field),
            Aggregation::Min { field } => compute_min(docs, field),
            Aggregation::Max { field } => compute_max(docs, field),
            Aggregation::Cardinality { field } => compute_cardinality(docs, field),
        };
        results.insert(name.clone(), result);
    }

    results
}

/// Extract a numeric value from a document field.
fn extract_number(doc: &Document, field: &str) -> Option<f64> {
    match doc.get_field(field)? {
        FieldValue::Number(n) => Some(*n),
        _ => None,
    }
}

/// Extract a string representation of a field value for bucketing.
fn extract_string(doc: &Document, field: &str) -> Option<String> {
    match doc.get_field(field)? {
        FieldValue::Text(s) => Some(s.clone()),
        FieldValue::Number(n) => Some(n.to_string()),
        FieldValue::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Terms aggregation: top N values of a field with doc counts.
fn compute_terms(docs: &[ScoredDocument], field: &str, size: usize) -> AggregationResult {
    let mut counts: HashMap<String, u64> = HashMap::new();

    for scored in docs {
        if let Some(val) = extract_string(&scored.document, field) {
            *counts.entry(val).or_insert(0) += 1;
        }
    }

    let mut buckets: Vec<AggregationBucket> = counts
        .into_iter()
        .map(|(key, doc_count)| AggregationBucket { key, doc_count })
        .collect();

    // Sort by doc_count descending, then key ascending for stability.
    buckets.sort_by(|a, b| b.doc_count.cmp(&a.doc_count).then(a.key.cmp(&b.key)));
    buckets.truncate(size);

    AggregationResult::Buckets { buckets }
}

/// Range aggregation: bucket documents by user-defined numeric ranges.
fn compute_range(
    docs: &[ScoredDocument],
    field: &str,
    ranges: &[RangeBucketDef],
) -> AggregationResult {
    let mut buckets: Vec<AggregationBucket> = ranges
        .iter()
        .map(|r| {
            let key = r.key.clone().unwrap_or_else(|| {
                let from_str = r
                    .from
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "*".to_string());
                let to_str =
                    r.to.map(|v| v.to_string())
                        .unwrap_or_else(|| "*".to_string());
                format!("{}-{}", from_str, to_str)
            });
            AggregationBucket { key, doc_count: 0 }
        })
        .collect();

    for scored in docs {
        if let Some(val) = extract_number(&scored.document, field) {
            for (i, range_def) in ranges.iter().enumerate() {
                let above_from = range_def.from.is_none_or(|f| val >= f);
                let below_to = range_def.to.is_none_or(|t| val < t);
                if above_from && below_to {
                    buckets[i].doc_count += 1;
                }
            }
        }
    }

    AggregationResult::Buckets { buckets }
}

/// Date histogram: bucket documents by time intervals.
fn compute_date_histogram(
    docs: &[ScoredDocument],
    field: &str,
    interval: &DateInterval,
) -> AggregationResult {
    let interval_secs = interval.as_secs();
    let mut bucket_counts: HashMap<u64, u64> = HashMap::new();

    for scored in docs {
        if let Some(val) = extract_number(&scored.document, field) {
            let ts = val as u64;
            let bucket_key = (ts / interval_secs) * interval_secs;
            *bucket_counts.entry(bucket_key).or_insert(0) += 1;
        }
    }

    let mut buckets: Vec<AggregationBucket> = bucket_counts
        .into_iter()
        .map(|(key, doc_count)| AggregationBucket {
            key: key.to_string(),
            doc_count,
        })
        .collect();

    // Sort by bucket key ascending.
    buckets.sort_by(|a, b| {
        a.key
            .parse::<u64>()
            .unwrap_or(0)
            .cmp(&b.key.parse::<u64>().unwrap_or(0))
    });

    AggregationResult::Buckets { buckets }
}

/// Average metric aggregation.
fn compute_avg(docs: &[ScoredDocument], field: &str) -> AggregationResult {
    let mut sum = 0.0_f64;
    let mut count = 0_u64;

    for scored in docs {
        if let Some(val) = extract_number(&scored.document, field) {
            sum += val;
            count += 1;
        }
    }

    let value = if count > 0 {
        Some(sum / count as f64)
    } else {
        None
    };

    AggregationResult::Metric { value }
}

/// Sum metric aggregation.
fn compute_sum(docs: &[ScoredDocument], field: &str) -> AggregationResult {
    let mut sum = 0.0_f64;
    let mut has_value = false;

    for scored in docs {
        if let Some(val) = extract_number(&scored.document, field) {
            sum += val;
            has_value = true;
        }
    }

    AggregationResult::Metric {
        value: if has_value { Some(sum) } else { None },
    }
}

/// Min metric aggregation.
fn compute_min(docs: &[ScoredDocument], field: &str) -> AggregationResult {
    let mut min: Option<f64> = None;

    for scored in docs {
        if let Some(val) = extract_number(&scored.document, field) {
            min = Some(min.map_or(val, |m: f64| m.min(val)));
        }
    }

    AggregationResult::Metric { value: min }
}

/// Max metric aggregation.
fn compute_max(docs: &[ScoredDocument], field: &str) -> AggregationResult {
    let mut max: Option<f64> = None;

    for scored in docs {
        if let Some(val) = extract_number(&scored.document, field) {
            max = Some(max.map_or(val, |m: f64| m.max(val)));
        }
    }

    AggregationResult::Metric { value: max }
}

/// Cardinality (distinct count) metric aggregation.
fn compute_cardinality(docs: &[ScoredDocument], field: &str) -> AggregationResult {
    let mut seen = std::collections::HashSet::new();

    for scored in docs {
        if let Some(val) = extract_string(&scored.document, field) {
            seen.insert(val);
        }
    }

    let value = if seen.is_empty() {
        None
    } else {
        Some(seen.len() as f64)
    };

    AggregationResult::Metric { value }
}

// ---------------------------------------------------------------------------
// Sorting engine — apply sort clauses to scored documents
// ---------------------------------------------------------------------------

/// Sort a mutable slice of [`ScoredDocument`] according to the given sort
/// clauses.  Also populates the `sort` field on each document with its sort
/// key values (needed for `search_after` cursor).
pub fn apply_sort(docs: &mut [ScoredDocument], sort_clauses: &[SortClause]) {
    if sort_clauses.is_empty() {
        // Default: sort by score descending.
        docs.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        return;
    }

    docs.sort_by(|a, b| {
        for clause in sort_clauses {
            let ord = compare_by_clause(a, b, clause);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });

    // Populate sort values on each document.
    for doc in docs.iter_mut() {
        doc.sort = sort_clauses
            .iter()
            .map(|clause| extract_sort_value(&doc.document, &clause.field, doc.score))
            .collect();
    }
}

/// Compare two scored documents by a single sort clause.
fn compare_by_clause(
    a: &ScoredDocument,
    b: &ScoredDocument,
    clause: &SortClause,
) -> std::cmp::Ordering {
    let val_a = extract_sort_value_raw(&a.document, &clause.field, a.score);
    let val_b = extract_sort_value_raw(&b.document, &clause.field, b.score);

    let ord = compare_sort_raw(&val_a, &val_b);

    match clause.order {
        SortOrder::Asc => ord,
        SortOrder::Desc => ord.reverse(),
    }
}

/// Intermediate sort value that can be compared.
#[derive(PartialEq, PartialOrd)]
enum SortRaw {
    Number(f64),
    Text(String),
    Missing,
}

fn extract_sort_value_raw(doc: &Document, field: &str, score: f32) -> SortRaw {
    if field == "_score" {
        return SortRaw::Number(score as f64);
    }
    if field == "_id" {
        return SortRaw::Text(doc.id.as_str().to_owned());
    }
    match doc.get_field(field) {
        Some(FieldValue::Number(n)) => SortRaw::Number(*n),
        Some(FieldValue::Text(s)) => SortRaw::Text(s.clone()),
        Some(FieldValue::Boolean(b)) => SortRaw::Number(if *b { 1.0 } else { 0.0 }),
        _ => SortRaw::Missing,
    }
}

fn compare_sort_raw(a: &SortRaw, b: &SortRaw) -> std::cmp::Ordering {
    match (a, b) {
        (SortRaw::Number(x), SortRaw::Number(y)) => {
            x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
        }
        (SortRaw::Text(x), SortRaw::Text(y)) => x.cmp(y),
        (SortRaw::Missing, SortRaw::Missing) => std::cmp::Ordering::Equal,
        (SortRaw::Missing, _) => std::cmp::Ordering::Greater,
        (_, SortRaw::Missing) => std::cmp::Ordering::Less,
        // Mixed types: numbers first.
        (SortRaw::Number(_), _) => std::cmp::Ordering::Less,
        (_, SortRaw::Number(_)) => std::cmp::Ordering::Greater,
    }
}

fn extract_sort_value(doc: &Document, field: &str, score: f32) -> SortValue {
    if field == "_score" {
        return SortValue::Number(score as f64);
    }
    if field == "_id" {
        return SortValue::Text(doc.id.as_str().to_owned());
    }
    match doc.get_field(field) {
        Some(FieldValue::Number(n)) => SortValue::Number(*n),
        Some(FieldValue::Text(s)) => SortValue::Text(s.clone()),
        Some(FieldValue::Boolean(b)) => SortValue::Number(if *b { 1.0 } else { 0.0 }),
        _ => SortValue::Text(String::new()),
    }
}

// ---------------------------------------------------------------------------
// Search-after filtering
// ---------------------------------------------------------------------------

/// Filter documents that come *after* the given cursor according to the
/// sort clauses.  Removes documents whose sort values are ≤ the cursor.
pub fn apply_search_after(
    docs: &mut Vec<ScoredDocument>,
    search_after: &[SortValue],
    sort_clauses: &[SortClause],
) {
    if search_after.is_empty() || sort_clauses.is_empty() {
        return;
    }

    docs.retain(|doc| {
        let cmp = compare_doc_to_cursor(doc, search_after, sort_clauses);
        // Keep documents that come strictly after the cursor.
        cmp == std::cmp::Ordering::Greater
    });
}

/// Compare a single document's sort values against the cursor.
fn compare_doc_to_cursor(
    doc: &ScoredDocument,
    cursor: &[SortValue],
    sort_clauses: &[SortClause],
) -> std::cmp::Ordering {
    for (i, clause) in sort_clauses.iter().enumerate() {
        let doc_val = extract_sort_value(&doc.document, &clause.field, doc.score);
        let cursor_val = cursor
            .get(i)
            .cloned()
            .unwrap_or(SortValue::Text(String::new()));

        let ord = compare_sort_values(&doc_val, &cursor_val);
        let ord = match clause.order {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        };

        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

fn compare_sort_values(a: &SortValue, b: &SortValue) -> std::cmp::Ordering {
    match (a, b) {
        (SortValue::Number(x), SortValue::Number(y)) => {
            x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
        }
        (SortValue::Text(x), SortValue::Text(y)) => x.cmp(y),
        (SortValue::Number(_), SortValue::Text(_)) => std::cmp::Ordering::Less,
        (SortValue::Text(_), SortValue::Number(_)) => std::cmp::Ordering::Greater,
    }
}

// ---------------------------------------------------------------------------
// Collapse (field-level deduplication)
// ---------------------------------------------------------------------------

/// Collapse / deduplicate documents by a field, keeping only the first
/// (best-scoring after sort) document for each unique field value.
pub fn apply_collapse(docs: &mut Vec<ScoredDocument>, collapse: &CollapseOptions) {
    let mut seen = std::collections::HashSet::new();
    docs.retain(|doc| {
        let key = extract_string(&doc.document, &collapse.field)
            .unwrap_or_else(|| doc.document.id.as_str().to_owned());
        seen.insert(key)
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{DocumentId, FieldValue};

    // -- Helper: build a scored doc quickly --

    fn scored(id: &str, score: f32, fields: Vec<(&str, FieldValue)>) -> ScoredDocument {
        let mut doc = Document::new(DocumentId::new(id));
        for (k, v) in fields {
            doc.set_field(k, v);
        }
        ScoredDocument {
            document: doc,
            score,
            sort: Vec::new(),
        }
    }

    // -- Operator tests --

    #[test]
    fn operator_default_is_or() {
        assert_eq!(Operator::default(), Operator::Or);
    }

    // -- Query serde roundtrips --

    #[test]
    fn full_text_query_serde_roundtrip() {
        let q = Query::FullText(FullTextQuery {
            field: "body".into(),
            query: "distributed systems".into(),
            operator: Operator::And,
        });

        let json = serde_json::to_string(&q).unwrap();
        let back: Query = serde_json::from_str(&json).unwrap();
        assert_eq!(q, back);
    }

    #[test]
    fn term_query_serde_roundtrip() {
        let q = Query::Term(TermQuery {
            field: "status".into(),
            value: "published".into(),
        });

        let json = serde_json::to_string(&q).unwrap();
        let back: Query = serde_json::from_str(&json).unwrap();
        assert_eq!(q, back);
    }

    #[test]
    fn range_query_serde_roundtrip() {
        let q = Query::Range(RangeQuery {
            field: "price".into(),
            gte: Some(10.0),
            lte: Some(100.0),
        });

        let json = serde_json::to_string(&q).unwrap();
        let back: Query = serde_json::from_str(&json).unwrap();
        assert_eq!(q, back);
    }

    #[test]
    fn bool_query_serde_roundtrip() {
        let q = Query::Bool(BoolQuery {
            must: vec![Query::Term(TermQuery {
                field: "status".into(),
                value: "active".into(),
            })],
            should: vec![Query::FullText(FullTextQuery {
                field: "title".into(),
                query: "rust".into(),
                operator: Operator::Or,
            })],
            must_not: vec![],
        });

        let json = serde_json::to_string(&q).unwrap();
        let back: Query = serde_json::from_str(&json).unwrap();
        assert_eq!(q, back);
    }

    #[test]
    fn search_result_empty() {
        let result = SearchResult::empty(5);
        assert_eq!(result.total, 0);
        assert_eq!(result.took_ms, 5);
        assert!(result.documents.is_empty());
        assert!(result.aggregations.is_empty());
    }

    #[test]
    fn scored_document_serde_roundtrip() {
        let doc = Document::new(DocumentId::new("scored-1"))
            .with_field("title", FieldValue::Text("test".into()));

        let scored = ScoredDocument {
            document: doc,
            score: 1.5,
            sort: Vec::new(),
        };

        let json = serde_json::to_string(&scored).unwrap();
        let back: ScoredDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(scored, back);
    }

    #[test]
    fn search_result_serde_roundtrip() {
        let result = SearchResult {
            documents: vec![ScoredDocument {
                document: Document::new(DocumentId::new("r1"))
                    .with_field("x", FieldValue::Number(1.0)),
                score: 0.9,
                sort: Vec::new(),
            }],
            total: 1,
            took_ms: 12,
            aggregations: HashMap::new(),
        };

        let json = serde_json::to_string(&result).unwrap();
        let back: SearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, back);
    }

    // -- Aggregation serde --

    #[test]
    fn aggregation_serde_roundtrip() {
        let aggs: HashMap<String, Aggregation> = [
            (
                "top_categories".to_string(),
                Aggregation::Terms {
                    field: "category".into(),
                    size: 5,
                },
            ),
            (
                "avg_price".to_string(),
                Aggregation::Avg {
                    field: "price".into(),
                },
            ),
        ]
        .into_iter()
        .collect();

        let json = serde_json::to_string(&aggs).unwrap();
        let back: HashMap<String, Aggregation> = serde_json::from_str(&json).unwrap();
        assert_eq!(aggs, back);
    }

    // -- Terms aggregation --

    #[test]
    fn terms_aggregation_basic() {
        let docs = vec![
            scored("d1", 1.0, vec![("color", FieldValue::Text("red".into()))]),
            scored("d2", 1.0, vec![("color", FieldValue::Text("blue".into()))]),
            scored("d3", 1.0, vec![("color", FieldValue::Text("red".into()))]),
            scored("d4", 1.0, vec![("color", FieldValue::Text("red".into()))]),
            scored("d5", 1.0, vec![("color", FieldValue::Text("green".into()))]),
        ];

        let mut aggs = HashMap::new();
        aggs.insert(
            "colors".to_string(),
            Aggregation::Terms {
                field: "color".into(),
                size: 10,
            },
        );

        let results = compute_aggregations(&docs, &aggs);
        let colors = results.get("colors").unwrap();

        if let AggregationResult::Buckets { buckets } = colors {
            assert_eq!(buckets.len(), 3);
            assert_eq!(buckets[0].key, "red");
            assert_eq!(buckets[0].doc_count, 3);
            assert_eq!(buckets[1].key, "blue");
            assert_eq!(buckets[1].doc_count, 1);
            assert_eq!(buckets[2].key, "green");
            assert_eq!(buckets[2].doc_count, 1);
        } else {
            panic!("expected Buckets result");
        }
    }

    #[test]
    fn terms_aggregation_size_limit() {
        let docs = vec![
            scored("d1", 1.0, vec![("x", FieldValue::Text("a".into()))]),
            scored("d2", 1.0, vec![("x", FieldValue::Text("b".into()))]),
            scored("d3", 1.0, vec![("x", FieldValue::Text("c".into()))]),
        ];

        let mut aggs = HashMap::new();
        aggs.insert(
            "top".to_string(),
            Aggregation::Terms {
                field: "x".into(),
                size: 2,
            },
        );

        let results = compute_aggregations(&docs, &aggs);
        if let AggregationResult::Buckets { buckets } = &results["top"] {
            assert_eq!(buckets.len(), 2);
        } else {
            panic!("expected Buckets");
        }
    }

    // -- Range aggregation --

    #[test]
    fn range_aggregation_basic() {
        let docs = vec![
            scored("d1", 1.0, vec![("price", FieldValue::Number(5.0))]),
            scored("d2", 1.0, vec![("price", FieldValue::Number(15.0))]),
            scored("d3", 1.0, vec![("price", FieldValue::Number(25.0))]),
            scored("d4", 1.0, vec![("price", FieldValue::Number(50.0))]),
            scored("d5", 1.0, vec![("price", FieldValue::Number(150.0))]),
        ];

        let mut aggs = HashMap::new();
        aggs.insert(
            "price_ranges".to_string(),
            Aggregation::Range {
                field: "price".into(),
                ranges: vec![
                    RangeBucketDef {
                        key: Some("cheap".into()),
                        from: None,
                        to: Some(10.0),
                    },
                    RangeBucketDef {
                        key: Some("mid".into()),
                        from: Some(10.0),
                        to: Some(100.0),
                    },
                    RangeBucketDef {
                        key: Some("expensive".into()),
                        from: Some(100.0),
                        to: None,
                    },
                ],
            },
        );

        let results = compute_aggregations(&docs, &aggs);
        if let AggregationResult::Buckets { buckets } = &results["price_ranges"] {
            assert_eq!(buckets.len(), 3);
            assert_eq!(buckets[0].key, "cheap");
            assert_eq!(buckets[0].doc_count, 1); // 5
            assert_eq!(buckets[1].key, "mid");
            assert_eq!(buckets[1].doc_count, 3); // 15, 25, 50
            assert_eq!(buckets[2].key, "expensive");
            assert_eq!(buckets[2].doc_count, 1); // 150
        } else {
            panic!("expected Buckets");
        }
    }

    // -- Date histogram --

    #[test]
    fn date_histogram_aggregation() {
        // Timestamps: 0, 3600, 7200, 7201, 86400
        let docs = vec![
            scored("d1", 1.0, vec![("ts", FieldValue::Number(0.0))]),
            scored("d2", 1.0, vec![("ts", FieldValue::Number(3600.0))]),
            scored("d3", 1.0, vec![("ts", FieldValue::Number(7200.0))]),
            scored("d4", 1.0, vec![("ts", FieldValue::Number(7201.0))]),
            scored("d5", 1.0, vec![("ts", FieldValue::Number(86400.0))]),
        ];

        let mut aggs = HashMap::new();
        aggs.insert(
            "hourly".to_string(),
            Aggregation::DateHistogram {
                field: "ts".into(),
                interval: DateInterval::Hour,
            },
        );

        let results = compute_aggregations(&docs, &aggs);
        if let AggregationResult::Buckets { buckets } = &results["hourly"] {
            // 0, 3600, 7200, 86400
            assert_eq!(buckets.len(), 4);
            assert_eq!(buckets[0].key, "0");
            assert_eq!(buckets[0].doc_count, 1);
            assert_eq!(buckets[1].key, "3600");
            assert_eq!(buckets[1].doc_count, 1);
            assert_eq!(buckets[2].key, "7200");
            assert_eq!(buckets[2].doc_count, 2); // 7200 and 7201
            assert_eq!(buckets[3].key, "86400");
            assert_eq!(buckets[3].doc_count, 1);
        } else {
            panic!("expected Buckets");
        }
    }

    // -- Metric aggregations --

    #[test]
    fn avg_aggregation() {
        let docs = vec![
            scored("d1", 1.0, vec![("val", FieldValue::Number(10.0))]),
            scored("d2", 1.0, vec![("val", FieldValue::Number(20.0))]),
            scored("d3", 1.0, vec![("val", FieldValue::Number(30.0))]),
        ];

        let mut aggs = HashMap::new();
        aggs.insert(
            "avg_val".to_string(),
            Aggregation::Avg {
                field: "val".into(),
            },
        );

        let results = compute_aggregations(&docs, &aggs);
        if let AggregationResult::Metric { value: Some(v) } = &results["avg_val"] {
            assert!((v - 20.0).abs() < f64::EPSILON);
        } else {
            panic!("expected Metric with value");
        }
    }

    #[test]
    fn sum_aggregation() {
        let docs = vec![
            scored("d1", 1.0, vec![("val", FieldValue::Number(10.0))]),
            scored("d2", 1.0, vec![("val", FieldValue::Number(20.0))]),
        ];

        let mut aggs = HashMap::new();
        aggs.insert(
            "total".to_string(),
            Aggregation::Sum {
                field: "val".into(),
            },
        );

        let results = compute_aggregations(&docs, &aggs);
        if let AggregationResult::Metric { value: Some(v) } = &results["total"] {
            assert!((v - 30.0).abs() < f64::EPSILON);
        } else {
            panic!("expected Metric with value");
        }
    }

    #[test]
    fn min_max_aggregation() {
        let docs = vec![
            scored("d1", 1.0, vec![("val", FieldValue::Number(5.0))]),
            scored("d2", 1.0, vec![("val", FieldValue::Number(50.0))]),
            scored("d3", 1.0, vec![("val", FieldValue::Number(25.0))]),
        ];

        let mut aggs = HashMap::new();
        aggs.insert(
            "min_val".to_string(),
            Aggregation::Min {
                field: "val".into(),
            },
        );
        aggs.insert(
            "max_val".to_string(),
            Aggregation::Max {
                field: "val".into(),
            },
        );

        let results = compute_aggregations(&docs, &aggs);
        if let AggregationResult::Metric { value: Some(v) } = &results["min_val"] {
            assert!((v - 5.0).abs() < f64::EPSILON);
        } else {
            panic!("expected Metric with min value");
        }
        if let AggregationResult::Metric { value: Some(v) } = &results["max_val"] {
            assert!((v - 50.0).abs() < f64::EPSILON);
        } else {
            panic!("expected Metric with max value");
        }
    }

    #[test]
    fn cardinality_aggregation() {
        let docs = vec![
            scored("d1", 1.0, vec![("cat", FieldValue::Text("a".into()))]),
            scored("d2", 1.0, vec![("cat", FieldValue::Text("b".into()))]),
            scored("d3", 1.0, vec![("cat", FieldValue::Text("a".into()))]),
            scored("d4", 1.0, vec![("cat", FieldValue::Text("c".into()))]),
        ];

        let mut aggs = HashMap::new();
        aggs.insert(
            "unique_cats".to_string(),
            Aggregation::Cardinality {
                field: "cat".into(),
            },
        );

        let results = compute_aggregations(&docs, &aggs);
        if let AggregationResult::Metric { value: Some(v) } = &results["unique_cats"] {
            assert!((v - 3.0).abs() < f64::EPSILON);
        } else {
            panic!("expected Metric with cardinality value");
        }
    }

    #[test]
    fn metric_aggregation_empty_docs() {
        let docs: Vec<ScoredDocument> = vec![];
        let mut aggs = HashMap::new();
        aggs.insert(
            "avg_val".to_string(),
            Aggregation::Avg {
                field: "val".into(),
            },
        );

        let results = compute_aggregations(&docs, &aggs);
        if let AggregationResult::Metric { value } = &results["avg_val"] {
            assert!(value.is_none());
        } else {
            panic!("expected Metric with None value");
        }
    }

    // -- Sorting tests --

    #[test]
    fn sort_by_numeric_field_asc() {
        let mut docs = vec![
            scored("d1", 1.0, vec![("price", FieldValue::Number(30.0))]),
            scored("d2", 2.0, vec![("price", FieldValue::Number(10.0))]),
            scored("d3", 3.0, vec![("price", FieldValue::Number(20.0))]),
        ];

        apply_sort(
            &mut docs,
            &[SortClause {
                field: "price".into(),
                order: SortOrder::Asc,
            }],
        );

        assert_eq!(docs[0].document.id.as_str(), "d2"); // 10
        assert_eq!(docs[1].document.id.as_str(), "d3"); // 20
        assert_eq!(docs[2].document.id.as_str(), "d1"); // 30

        // Verify sort values are populated
        assert_eq!(docs[0].sort, vec![SortValue::Number(10.0)]);
    }

    #[test]
    fn sort_by_score_desc() {
        let mut docs = vec![
            scored("d1", 1.5, vec![]),
            scored("d2", 3.0, vec![]),
            scored("d3", 2.0, vec![]),
        ];

        apply_sort(
            &mut docs,
            &[SortClause {
                field: "_score".into(),
                order: SortOrder::Desc,
            }],
        );

        assert_eq!(docs[0].document.id.as_str(), "d2"); // 3.0
        assert_eq!(docs[1].document.id.as_str(), "d3"); // 2.0
        assert_eq!(docs[2].document.id.as_str(), "d1"); // 1.5
    }

    #[test]
    fn multi_field_sort() {
        let mut docs = vec![
            scored(
                "d1",
                1.0,
                vec![
                    ("category", FieldValue::Text("b".into())),
                    ("price", FieldValue::Number(20.0)),
                ],
            ),
            scored(
                "d2",
                1.0,
                vec![
                    ("category", FieldValue::Text("a".into())),
                    ("price", FieldValue::Number(30.0)),
                ],
            ),
            scored(
                "d3",
                1.0,
                vec![
                    ("category", FieldValue::Text("a".into())),
                    ("price", FieldValue::Number(10.0)),
                ],
            ),
        ];

        apply_sort(
            &mut docs,
            &[
                SortClause {
                    field: "category".into(),
                    order: SortOrder::Asc,
                },
                SortClause {
                    field: "price".into(),
                    order: SortOrder::Asc,
                },
            ],
        );

        // a/10, a/30, b/20
        assert_eq!(docs[0].document.id.as_str(), "d3");
        assert_eq!(docs[1].document.id.as_str(), "d2");
        assert_eq!(docs[2].document.id.as_str(), "d1");
    }

    #[test]
    fn default_sort_by_score() {
        let mut docs = vec![
            scored("d1", 1.0, vec![]),
            scored("d2", 3.0, vec![]),
            scored("d3", 2.0, vec![]),
        ];

        apply_sort(&mut docs, &[]);

        assert_eq!(docs[0].document.id.as_str(), "d2");
        assert_eq!(docs[1].document.id.as_str(), "d3");
        assert_eq!(docs[2].document.id.as_str(), "d1");
    }

    // -- Search-after tests --

    #[test]
    fn search_after_filters_correctly() {
        let mut docs = vec![
            scored("d1", 1.0, vec![("price", FieldValue::Number(10.0))]),
            scored("d2", 1.0, vec![("price", FieldValue::Number(20.0))]),
            scored("d3", 1.0, vec![("price", FieldValue::Number(30.0))]),
            scored("d4", 1.0, vec![("price", FieldValue::Number(40.0))]),
        ];

        let sort_clauses = vec![SortClause {
            field: "price".into(),
            order: SortOrder::Asc,
        }];

        // Cursor: after price=20 → should get 30, 40
        apply_search_after(&mut docs, &[SortValue::Number(20.0)], &sort_clauses);

        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].document.id.as_str(), "d3");
        assert_eq!(docs[1].document.id.as_str(), "d4");
    }

    #[test]
    fn search_after_desc_order() {
        let mut docs = vec![
            scored("d1", 1.0, vec![("price", FieldValue::Number(40.0))]),
            scored("d2", 1.0, vec![("price", FieldValue::Number(30.0))]),
            scored("d3", 1.0, vec![("price", FieldValue::Number(20.0))]),
            scored("d4", 1.0, vec![("price", FieldValue::Number(10.0))]),
        ];

        let sort_clauses = vec![SortClause {
            field: "price".into(),
            order: SortOrder::Desc,
        }];

        // Cursor: after price=30 (desc) → should keep 20, 10
        apply_search_after(&mut docs, &[SortValue::Number(30.0)], &sort_clauses);

        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].document.id.as_str(), "d3"); // price 20
        assert_eq!(docs[1].document.id.as_str(), "d4"); // price 10
    }

    // -- Collapse tests --

    #[test]
    fn collapse_deduplicates_by_field() {
        let mut docs = vec![
            scored(
                "d1",
                3.0,
                vec![("category", FieldValue::Text("electronics".into()))],
            ),
            scored(
                "d2",
                2.0,
                vec![("category", FieldValue::Text("electronics".into()))],
            ),
            scored(
                "d3",
                1.0,
                vec![("category", FieldValue::Text("books".into()))],
            ),
            scored(
                "d4",
                0.5,
                vec![("category", FieldValue::Text("books".into()))],
            ),
        ];

        apply_collapse(
            &mut docs,
            &CollapseOptions {
                field: "category".into(),
            },
        );

        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].document.id.as_str(), "d1"); // first electronics
        assert_eq!(docs[1].document.id.as_str(), "d3"); // first books
    }

    // -- SearchOptions serde --

    #[test]
    fn search_options_serde_roundtrip() {
        let opts = SearchOptions {
            size: 20,
            from: 0,
            sort: vec![SortClause {
                field: "price".into(),
                order: SortOrder::Asc,
            }],
            search_after: Some(vec![SortValue::Number(42.0)]),
            aggregations: {
                let mut m = HashMap::new();
                m.insert(
                    "avg_price".to_string(),
                    Aggregation::Avg {
                        field: "price".into(),
                    },
                );
                m
            },
            collapse: Some(CollapseOptions {
                field: "brand".into(),
            }),
        };

        let json = serde_json::to_string(&opts).unwrap();
        let back: SearchOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(opts, back);
    }

    // -- Combined: sort + search_after + aggregations --

    #[test]
    fn combined_sort_search_after_aggregations() {
        let mut docs = vec![
            scored(
                "d1",
                1.0,
                vec![
                    ("price", FieldValue::Number(10.0)),
                    ("cat", FieldValue::Text("A".into())),
                ],
            ),
            scored(
                "d2",
                1.0,
                vec![
                    ("price", FieldValue::Number(20.0)),
                    ("cat", FieldValue::Text("B".into())),
                ],
            ),
            scored(
                "d3",
                1.0,
                vec![
                    ("price", FieldValue::Number(30.0)),
                    ("cat", FieldValue::Text("A".into())),
                ],
            ),
            scored(
                "d4",
                1.0,
                vec![
                    ("price", FieldValue::Number(40.0)),
                    ("cat", FieldValue::Text("B".into())),
                ],
            ),
        ];

        let sort_clauses = vec![SortClause {
            field: "price".into(),
            order: SortOrder::Asc,
        }];

        // 1. Compute aggregations over ALL matched docs first.
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
                field: "cat".into(),
                size: 10,
            },
        );

        let agg_results = compute_aggregations(&docs, &aggs);

        // Verify avg is over all 4 docs.
        if let AggregationResult::Metric { value: Some(v) } = &agg_results["avg_price"] {
            assert!((v - 25.0).abs() < f64::EPSILON);
        } else {
            panic!("expected Metric");
        }

        // 2. Sort.
        apply_sort(&mut docs, &sort_clauses);

        // 3. Search-after: skip past price=20.
        apply_search_after(&mut docs, &[SortValue::Number(20.0)], &sort_clauses);

        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].document.id.as_str(), "d3"); // 30
        assert_eq!(docs[1].document.id.as_str(), "d4"); // 40
    }
}
