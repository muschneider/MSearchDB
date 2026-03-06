//! Query DSL and search result types for MSearchDB.
//!
//! MSearchDB supports a composable query language including full-text search,
//! exact term matching, numeric range filters, and boolean combinations.
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

use crate::document::Document;

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
// Search results
// ---------------------------------------------------------------------------

/// A document paired with its relevance score.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoredDocument {
    /// The matched document.
    pub document: Document,
    /// TF-IDF or BM25 relevance score.
    pub score: f32,
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
}

impl SearchResult {
    /// Create an empty search result (useful for no-match cases).
    pub fn empty(took_ms: u64) -> Self {
        Self {
            documents: Vec::new(),
            total: 0,
            took_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{DocumentId, FieldValue};

    #[test]
    fn operator_default_is_or() {
        assert_eq!(Operator::default(), Operator::Or);
    }

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
    }

    #[test]
    fn scored_document_serde_roundtrip() {
        let doc = Document::new(DocumentId::new("scored-1"))
            .with_field("title", FieldValue::Text("test".into()));

        let scored = ScoredDocument {
            document: doc,
            score: 1.5,
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
            }],
            total: 1,
            took_ms: 12,
        };

        let json = serde_json::to_string(&result).unwrap();
        let back: SearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, back);
    }
}
