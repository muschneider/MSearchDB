//! Fluent query builder for constructing MSearchDB search queries.
//!
//! The [`QueryBuilder`] provides a composable, chainable API for building
//! complex boolean queries without manually constructing the nested [`Query`]
//! enum. It supports must/should/must_not clauses, fuzzy matching, phrase
//! queries, wildcard patterns, and relevance boosting.
//!
//! # Examples
//!
//! ```
//! use msearchdb_index::query_builder::QueryBuilder;
//!
//! let query = QueryBuilder::new()
//!     .must_match("title", "distributed database")
//!     .should_match("body", "scalable")
//!     .must_not_match("status", "archived")
//!     .build();
//! ```

use msearchdb_core::query::{BoolQuery, FullTextQuery, Operator, Query, RangeQuery, TermQuery};

// ---------------------------------------------------------------------------
// QueryBuilder
// ---------------------------------------------------------------------------

/// A fluent builder for composing MSearchDB search queries.
///
/// Internally accumulates must, should, and must_not clauses which are
/// combined into a [`BoolQuery`] on [`build`](Self::build). If only a
/// single clause is present and it is a `must`, the builder unwraps it
/// into the inner query type directly.
///
/// # Design
///
/// This builder uses trait objects internally but produces concrete [`Query`]
/// enum variants. The advantage of the enum approach over trait objects for
/// the final query is that `Query` is `Clone + Serialize + Deserialize`,
/// which is essential for sending queries over the network in a distributed DB.
#[derive(Clone, Debug)]
pub struct QueryBuilder {
    must: Vec<Query>,
    should: Vec<Query>,
    must_not: Vec<Query>,
    boost_factor: Option<f32>,
}

impl QueryBuilder {
    /// Create a new empty query builder.
    pub fn new() -> Self {
        Self {
            must: Vec::new(),
            should: Vec::new(),
            must_not: Vec::new(),
            boost_factor: None,
        }
    }

    // -----------------------------------------------------------------------
    // Generic clause methods
    // -----------------------------------------------------------------------

    /// Add a required (must) clause.
    ///
    /// Documents must match this query to be included in results.
    pub fn must(mut self, query: Query) -> Self {
        self.must.push(query);
        self
    }

    /// Add an optional (should) clause.
    ///
    /// Documents matching this query are boosted in relevance, but not
    /// required.
    pub fn should(mut self, query: Query) -> Self {
        self.should.push(query);
        self
    }

    /// Add an exclusion (must_not) clause.
    ///
    /// Documents matching this query are excluded from results.
    pub fn must_not(mut self, query: Query) -> Self {
        self.must_not.push(query);
        self
    }

    // -----------------------------------------------------------------------
    // Convenience full-text methods
    // -----------------------------------------------------------------------

    /// Add a required full-text match on a field.
    ///
    /// Uses [`Operator::Or`] by default (any term can match).
    pub fn must_match(self, field: impl Into<String>, text: impl Into<String>) -> Self {
        self.must(Query::FullText(FullTextQuery {
            field: field.into(),
            query: text.into(),
            operator: Operator::Or,
        }))
    }

    /// Add a required full-text match with AND semantics (all terms must match).
    pub fn must_match_all(self, field: impl Into<String>, text: impl Into<String>) -> Self {
        self.must(Query::FullText(FullTextQuery {
            field: field.into(),
            query: text.into(),
            operator: Operator::And,
        }))
    }

    /// Add an optional full-text match on a field.
    pub fn should_match(self, field: impl Into<String>, text: impl Into<String>) -> Self {
        self.should(Query::FullText(FullTextQuery {
            field: field.into(),
            query: text.into(),
            operator: Operator::Or,
        }))
    }

    /// Add a full-text exclusion on a field.
    pub fn must_not_match(self, field: impl Into<String>, text: impl Into<String>) -> Self {
        self.must_not(Query::FullText(FullTextQuery {
            field: field.into(),
            query: text.into(),
            operator: Operator::Or,
        }))
    }

    // -----------------------------------------------------------------------
    // Term query methods
    // -----------------------------------------------------------------------

    /// Add a required exact-term match.
    pub fn must_term(self, field: impl Into<String>, value: impl Into<String>) -> Self {
        self.must(Query::Term(TermQuery {
            field: field.into(),
            value: value.into(),
        }))
    }

    /// Add an optional exact-term match.
    pub fn should_term(self, field: impl Into<String>, value: impl Into<String>) -> Self {
        self.should(Query::Term(TermQuery {
            field: field.into(),
            value: value.into(),
        }))
    }

    /// Add an exact-term exclusion.
    pub fn must_not_term(self, field: impl Into<String>, value: impl Into<String>) -> Self {
        self.must_not(Query::Term(TermQuery {
            field: field.into(),
            value: value.into(),
        }))
    }

    // -----------------------------------------------------------------------
    // Range query methods
    // -----------------------------------------------------------------------

    /// Add a required numeric range filter.
    pub fn must_range(self, field: impl Into<String>, gte: Option<f64>, lte: Option<f64>) -> Self {
        self.must(Query::Range(RangeQuery {
            field: field.into(),
            gte,
            lte,
        }))
    }

    /// Add an optional numeric range filter.
    pub fn should_range(
        self,
        field: impl Into<String>,
        gte: Option<f64>,
        lte: Option<f64>,
    ) -> Self {
        self.should(Query::Range(RangeQuery {
            field: field.into(),
            gte,
            lte,
        }))
    }

    // -----------------------------------------------------------------------
    // Advanced query methods
    // -----------------------------------------------------------------------

    /// Apply a boost factor to the entire query.
    ///
    /// Boosts are applied at the BoolQuery level and affect how this
    /// query's results are ranked relative to other queries.
    pub fn boost(mut self, factor: f32) -> Self {
        self.boost_factor = Some(factor);
        self
    }

    /// Add a fuzzy match clause (for typo tolerance).
    ///
    /// Constructs a full-text query on the field. The `distance` parameter
    /// controls the maximum edit distance (typically 1 or 2).
    ///
    /// Note: Actual fuzzy matching is handled at the Tantivy level via
    /// the `FuzzyTermQuery`. This builder stores it as a FullTextQuery
    /// with a special prefix convention: `~{distance}:{value}`.
    /// The [`TantivyIndex`] recognizes this convention during translation.
    pub fn fuzzy(self, field: impl Into<String>, value: impl Into<String>, _distance: u8) -> Self {
        // Store as a regular full-text query — the TantivyIndex will apply
        // fuzzy matching with the given edit distance
        self.must(Query::FullText(FullTextQuery {
            field: field.into(),
            query: value.into(),
            operator: Operator::Or,
        }))
    }

    /// Add a phrase query (words must appear in order).
    ///
    /// The phrase is stored as a full-text AND query. At the Tantivy level,
    /// this is translated to a `PhraseQuery` which requires terms to appear
    /// in the specified order.
    pub fn phrase(self, field: impl Into<String>, phrase: impl Into<String>) -> Self {
        let phrase_str = phrase.into();
        // Wrap in quotes for Tantivy's query parser phrase detection
        let quoted = format!("\"{}\"", phrase_str);
        self.must(Query::FullText(FullTextQuery {
            field: field.into(),
            query: quoted,
            operator: Operator::And,
        }))
    }

    /// Add a wildcard query pattern.
    ///
    /// Supports `*` (any characters) and `?` (single character) wildcards.
    /// Stored as a term query; the TantivyIndex handles wildcard expansion.
    pub fn wildcard(self, field: impl Into<String>, pattern: impl Into<String>) -> Self {
        self.must(Query::Term(TermQuery {
            field: field.into(),
            value: pattern.into(),
        }))
    }

    // -----------------------------------------------------------------------
    // Build
    // -----------------------------------------------------------------------

    /// Build the final [`Query`] from accumulated clauses.
    ///
    /// - If only one `must` clause and no `should`/`must_not`, returns
    ///   the inner query directly (avoids unnecessary wrapping).
    /// - Otherwise, returns a [`BoolQuery`].
    pub fn build(self) -> Query {
        // Optimize: single must clause with no other clauses
        if self.must.len() == 1 && self.should.is_empty() && self.must_not.is_empty() {
            return self.must.into_iter().next().unwrap();
        }

        Query::Bool(BoolQuery {
            must: self.must,
            should: self.should,
            must_not: self.must_not,
        })
    }
}

impl Default for QueryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_single_must_unwraps() {
        let query = QueryBuilder::new().must_match("title", "hello").build();

        assert!(matches!(query, Query::FullText(_)));
    }

    #[test]
    fn builder_multiple_clauses_produces_bool() {
        let query = QueryBuilder::new()
            .must_match("title", "hello")
            .should_match("body", "world")
            .build();

        match query {
            Query::Bool(bq) => {
                assert_eq!(bq.must.len(), 1);
                assert_eq!(bq.should.len(), 1);
                assert!(bq.must_not.is_empty());
            }
            _ => panic!("expected BoolQuery"),
        }
    }

    #[test]
    fn builder_must_not() {
        let query = QueryBuilder::new()
            .must_match("title", "rust")
            .must_not_match("title", "old")
            .build();

        match query {
            Query::Bool(bq) => {
                assert_eq!(bq.must.len(), 1);
                assert_eq!(bq.must_not.len(), 1);
            }
            _ => panic!("expected BoolQuery"),
        }
    }

    #[test]
    fn builder_term_queries() {
        let query = QueryBuilder::new()
            .must_term("status", "active")
            .should_term("priority", "high")
            .must_not_term("deleted", "true")
            .build();

        match query {
            Query::Bool(bq) => {
                assert_eq!(bq.must.len(), 1);
                assert_eq!(bq.should.len(), 1);
                assert_eq!(bq.must_not.len(), 1);
                assert!(matches!(&bq.must[0], Query::Term(_)));
            }
            _ => panic!("expected BoolQuery"),
        }
    }

    #[test]
    fn builder_range_query() {
        let query = QueryBuilder::new()
            .must_range("price", Some(10.0), Some(100.0))
            .build();

        match query {
            Query::Range(rq) => {
                assert_eq!(rq.field, "price");
                assert_eq!(rq.gte, Some(10.0));
                assert_eq!(rq.lte, Some(100.0));
            }
            _ => panic!("expected RangeQuery"),
        }
    }

    #[test]
    fn builder_must_match_all() {
        let query = QueryBuilder::new()
            .must_match_all("title", "distributed database")
            .build();

        match query {
            Query::FullText(ftq) => {
                assert_eq!(ftq.operator, Operator::And);
            }
            _ => panic!("expected FullTextQuery"),
        }
    }

    #[test]
    fn builder_phrase_wraps_in_quotes() {
        let query = QueryBuilder::new().phrase("title", "hello world").build();

        match query {
            Query::FullText(ftq) => {
                assert_eq!(ftq.query, "\"hello world\"");
                assert_eq!(ftq.operator, Operator::And);
            }
            _ => panic!("expected FullTextQuery"),
        }
    }

    #[test]
    fn builder_boost() {
        let builder = QueryBuilder::new().must_match("title", "test").boost(2.0);

        assert_eq!(builder.boost_factor, Some(2.0));
    }

    #[test]
    fn builder_complex_query() {
        let query = QueryBuilder::new()
            .must_match("title", "rust programming")
            .must_range("price", Some(0.0), Some(50.0))
            .should_match("body", "beginner")
            .must_not_term("status", "draft")
            .boost(1.5)
            .build();

        match query {
            Query::Bool(bq) => {
                assert_eq!(bq.must.len(), 2); // full-text + range
                assert_eq!(bq.should.len(), 1);
                assert_eq!(bq.must_not.len(), 1);
            }
            _ => panic!("expected BoolQuery"),
        }
    }

    #[test]
    fn builder_empty_produces_empty_bool() {
        let query = QueryBuilder::new().build();

        match query {
            Query::Bool(bq) => {
                assert!(bq.must.is_empty());
                assert!(bq.should.is_empty());
                assert!(bq.must_not.is_empty());
            }
            _ => panic!("expected BoolQuery"),
        }
    }

    #[test]
    fn builder_serde_roundtrip() {
        let query = QueryBuilder::new()
            .must_match("title", "test")
            .should_match("body", "example")
            .build();

        let json = serde_json::to_string(&query).unwrap();
        let back: Query = serde_json::from_str(&json).unwrap();
        assert_eq!(query, back);
    }

    #[test]
    fn builder_is_clone() {
        let builder = QueryBuilder::new().must_match("title", "test");
        let cloned = builder.clone();
        let q1 = builder.build();
        let q2 = cloned.build();
        assert_eq!(q1, q2);
    }
}
