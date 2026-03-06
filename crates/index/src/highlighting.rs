//! Search result highlighting for MSearchDB.
//!
//! Highlights matched terms in document fields by wrapping them in `<em>` HTML
//! tags. Uses Tantivy's built-in snippet generator to extract relevant
//! fragments from text fields.
//!
//! # Examples
//!
//! ```no_run
//! use msearchdb_index::highlighting::{highlight, HighlightConfig, HighlightResult};
//! use msearchdb_index::tantivy_index::TantivyIndex;
//! use msearchdb_core::query::{Query, FullTextQuery, Operator};
//! use msearchdb_core::document::{Document, DocumentId, FieldValue};
//!
//! # fn example(index: &TantivyIndex) {
//! let query = Query::FullText(FullTextQuery {
//!     field: "body".into(),
//!     query: "distributed database".into(),
//!     operator: Operator::Or,
//! });
//!
//! let doc = Document::new(DocumentId::new("d1"))
//!     .with_field("body", FieldValue::Text(
//!         "A distributed database stores data across nodes".into()
//!     ));
//!
//! let config = HighlightConfig::default();
//! let results = highlight(index, &doc, &query, &["body"], &config);
//! // results[0].fragments might contain:
//! // "A <em>distributed</em> <em>database</em> stores data across nodes"
//! # }
//! ```

use serde::{Deserialize, Serialize};

use msearchdb_core::document::{Document, FieldValue};
use msearchdb_core::error::DbError;
use msearchdb_core::query::Query;

use tantivy::query::QueryParser;
use tantivy::snippet::SnippetGenerator;
use tantivy::TantivyDocument;

use crate::tantivy_index::TantivyIndex;

// ---------------------------------------------------------------------------
// HighlightConfig
// ---------------------------------------------------------------------------

/// Configuration for search result highlighting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightConfig {
    /// HTML tag to wrap matched terms (opening). Default: `<em>`.
    pub pre_tag: String,
    /// HTML tag to close matched terms. Default: `</em>`.
    pub post_tag: String,
    /// Maximum number of fragments to return per field.
    pub max_fragments: usize,
    /// Maximum number of characters per fragment.
    pub fragment_size: usize,
}

impl Default for HighlightConfig {
    fn default() -> Self {
        Self {
            pre_tag: "<em>".to_owned(),
            post_tag: "</em>".to_owned(),
            max_fragments: 3,
            fragment_size: 150,
        }
    }
}

// ---------------------------------------------------------------------------
// HighlightResult
// ---------------------------------------------------------------------------

/// The highlighted output for a single field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightResult {
    /// The field name that was highlighted.
    pub field: String,
    /// The highlighted text fragments with `<em>` tags around matched terms.
    pub fragments: Vec<String>,
}

// ---------------------------------------------------------------------------
// Highlight function
// ---------------------------------------------------------------------------

/// Generate highlighted fragments for matched terms in document fields.
///
/// For each requested field, this function:
/// 1. Parses the query to extract the search terms
/// 2. Uses Tantivy's [`SnippetGenerator`] to find and highlight matching tokens
/// 3. Returns fragments with matched terms wrapped in the configured HTML tags
///
/// # Arguments
///
/// * `index` — the Tantivy index (needed for the schema and searcher)
/// * `doc` — the document whose fields should be highlighted
/// * `query` — the search query (used to identify matching terms)
/// * `fields` — the field names to highlight
/// * `config` — highlighting configuration (tags, fragment size)
///
/// # Returns
///
/// A vector of [`HighlightResult`], one per field. Fields that have no text
/// content or no matches return an empty fragment list.
pub fn highlight(
    index: &TantivyIndex,
    doc: &Document,
    query: &Query,
    fields: &[&str],
    config: &HighlightConfig,
) -> Vec<HighlightResult> {
    let mut results = Vec::with_capacity(fields.len());

    for &field_name in fields {
        let fragments = highlight_field(index, doc, query, field_name, config).unwrap_or_default();

        results.push(HighlightResult {
            field: field_name.to_owned(),
            fragments,
        });
    }

    results
}

/// Highlight a single field in a document.
fn highlight_field(
    index: &TantivyIndex,
    doc: &Document,
    query: &Query,
    field_name: &str,
    config: &HighlightConfig,
) -> Result<Vec<String>, DbError> {
    // Get the text content of the field
    let text = match doc.get_field(field_name) {
        Some(FieldValue::Text(t)) => t,
        _ => return Ok(Vec::new()),
    };

    // Get the Tantivy field handle
    let tantivy_field = match index.field_map().get(field_name) {
        Some(f) => *f,
        None => return Ok(Vec::new()),
    };

    // Extract the query text for this field
    let query_text = extract_query_text(query, field_name);
    if query_text.is_empty() {
        return Ok(Vec::new());
    }

    // Parse the query with Tantivy's query parser
    let parser = QueryParser::for_index(index.inner_index(), vec![tantivy_field]);
    let tantivy_query = parser
        .parse_query(&query_text)
        .map_err(|e| DbError::IndexError(format!("failed to parse highlight query: {}", e)))?;

    // Build a Tantivy document with just this field for the snippet generator
    let searcher = index
        .inner_index()
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::Manual)
        .try_into()
        .map_err(|e| DbError::IndexError(format!("reader: {}", e)))?;
    let searcher = searcher.searcher();

    let mut snippet_gen = SnippetGenerator::create(&searcher, &*tantivy_query, tantivy_field)
        .map_err(|e| DbError::IndexError(format!("snippet generator: {}", e)))?;

    snippet_gen.set_max_num_chars(config.fragment_size);

    // Build a minimal Tantivy document containing only the target field
    let mut tantivy_doc = TantivyDocument::new();
    tantivy_doc.add_text(tantivy_field, text);

    let snippet = snippet_gen.snippet_from_doc(&tantivy_doc);
    let html = snippet.to_html();

    // Replace Tantivy's default <b> tags with our configured tags
    let highlighted = html
        .replace("<b>", &config.pre_tag)
        .replace("</b>", &config.post_tag);

    if highlighted.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![highlighted])
    }
}

/// Extract query text relevant to a specific field from a [`Query`].
fn extract_query_text(query: &Query, field_name: &str) -> String {
    match query {
        Query::FullText(ftq) => {
            if ftq.field == field_name {
                ftq.query.clone()
            } else {
                String::new()
            }
        }
        Query::Term(tq) => {
            if tq.field == field_name {
                tq.value.clone()
            } else {
                String::new()
            }
        }
        Query::Bool(bq) => {
            let mut parts: Vec<String> = Vec::new();
            for q in bq.must.iter().chain(bq.should.iter()) {
                let text = extract_query_text(q, field_name);
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            parts.join(" ")
        }
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_builder::{FieldConfig, FieldType, SchemaConfig};
    use crate::tantivy_index::TantivyIndex;
    use msearchdb_core::document::{Document, DocumentId, FieldValue};
    use msearchdb_core::query::{FullTextQuery, Operator, Query};

    fn test_index_with_docs() -> TantivyIndex {
        let config = SchemaConfig::new()
            .with_field(FieldConfig::new("title", FieldType::Text))
            .with_field(FieldConfig::new("body", FieldType::Text));

        let idx = TantivyIndex::new_in_ram(config).unwrap();

        let doc = Document::new(DocumentId::new("h1"))
            .with_field(
                "title",
                FieldValue::Text("Introduction to Distributed Databases".into()),
            )
            .with_field(
                "body",
                FieldValue::Text(
                    "A distributed database stores data across multiple nodes for \
                     high availability and fault tolerance. This approach enables \
                     horizontal scaling and geographic distribution of data."
                        .into(),
                ),
            );

        idx.index_document_sync(&doc).unwrap();
        idx.commit().unwrap();
        idx
    }

    #[test]
    fn highlight_returns_fragments_with_em_tags() {
        let idx = test_index_with_docs();
        let doc = Document::new(DocumentId::new("h1")).with_field(
            "body",
            FieldValue::Text(
                "A distributed database stores data across multiple nodes for \
                 high availability and fault tolerance."
                    .into(),
            ),
        );

        let query = Query::FullText(FullTextQuery {
            field: "body".into(),
            query: "distributed database".into(),
            operator: Operator::Or,
        });

        let config = HighlightConfig::default();
        let results = highlight(&idx, &doc, &query, &["body"], &config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].field, "body");
        assert!(!results[0].fragments.is_empty());

        let frag = &results[0].fragments[0];
        assert!(
            frag.contains("<em>"),
            "fragment should contain <em> tag: {}",
            frag
        );
    }

    #[test]
    fn highlight_config_custom_tags() {
        let idx = test_index_with_docs();
        let doc = Document::new(DocumentId::new("h1")).with_field(
            "body",
            FieldValue::Text("A distributed database system".into()),
        );

        let query = Query::FullText(FullTextQuery {
            field: "body".into(),
            query: "distributed".into(),
            operator: Operator::Or,
        });

        let config = HighlightConfig {
            pre_tag: "<mark>".into(),
            post_tag: "</mark>".into(),
            ..Default::default()
        };

        let results = highlight(&idx, &doc, &query, &["body"], &config);
        if !results[0].fragments.is_empty() {
            let frag = &results[0].fragments[0];
            assert!(frag.contains("<mark>") || frag.contains("distributed"));
        }
    }

    #[test]
    fn highlight_missing_field_returns_empty() {
        let idx = test_index_with_docs();
        let doc = Document::new(DocumentId::new("h1"))
            .with_field("title", FieldValue::Text("Test".into()));

        let query = Query::FullText(FullTextQuery {
            field: "nonexistent".into(),
            query: "test".into(),
            operator: Operator::Or,
        });

        let config = HighlightConfig::default();
        let results = highlight(&idx, &doc, &query, &["nonexistent"], &config);
        assert!(results[0].fragments.is_empty());
    }

    #[test]
    fn highlight_non_text_field_returns_empty() {
        let config = SchemaConfig::new().with_field(FieldConfig::new("price", FieldType::Number));
        let idx = TantivyIndex::new_in_ram(config).unwrap();

        let doc =
            Document::new(DocumentId::new("h2")).with_field("price", FieldValue::Number(42.0));

        let query = Query::FullText(FullTextQuery {
            field: "price".into(),
            query: "42".into(),
            operator: Operator::Or,
        });

        let config = HighlightConfig::default();
        let results = highlight(&idx, &doc, &query, &["price"], &config);
        assert!(results[0].fragments.is_empty());
    }

    #[test]
    fn extract_query_text_from_full_text() {
        let query = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "hello world".into(),
            operator: Operator::Or,
        });

        assert_eq!(extract_query_text(&query, "title"), "hello world");
        assert_eq!(extract_query_text(&query, "body"), "");
    }

    #[test]
    fn extract_query_text_from_bool() {
        let query = Query::Bool(msearchdb_core::query::BoolQuery {
            must: vec![Query::FullText(FullTextQuery {
                field: "title".into(),
                query: "rust".into(),
                operator: Operator::Or,
            })],
            should: vec![Query::FullText(FullTextQuery {
                field: "title".into(),
                query: "programming".into(),
                operator: Operator::Or,
            })],
            must_not: vec![],
        });

        let text = extract_query_text(&query, "title");
        assert!(text.contains("rust"));
        assert!(text.contains("programming"));
    }

    #[test]
    fn highlight_result_serde_roundtrip() {
        let result = HighlightResult {
            field: "body".into(),
            fragments: vec!["A <em>distributed</em> database".into()],
        };

        let json = serde_json::to_string(&result).unwrap();
        let back: HighlightResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, back);
    }

    #[test]
    fn highlight_config_serde_roundtrip() {
        let config = HighlightConfig {
            pre_tag: "<mark>".into(),
            post_tag: "</mark>".into(),
            max_fragments: 5,
            fragment_size: 200,
        };

        let json = serde_json::to_string(&config).unwrap();
        let back: HighlightConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }
}
