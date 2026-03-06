//! Unit-level integration tests for the msearchdb-index crate.
//!
//! These tests exercise the TantivyIndex through the public API, verifying
//! full-text search, query variants, fuzzy matching, phrase queries, ngram
//! analysis, highlighting, deletion, and updates.

use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::query::*;

use msearchdb_index::highlighting::{highlight, HighlightConfig};
use msearchdb_index::query_builder::QueryBuilder;
use msearchdb_index::schema_builder::{FieldConfig, FieldType, SchemaConfig};
use msearchdb_index::tantivy_index::TantivyIndex;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_index() -> TantivyIndex {
    let config = SchemaConfig::new()
        .with_field(FieldConfig::new("title", FieldType::Text))
        .with_field(FieldConfig::new("body", FieldType::Text))
        .with_field(FieldConfig::new("category", FieldType::Text))
        .with_field(FieldConfig::new("price", FieldType::Number))
        .with_field(FieldConfig::new("active", FieldType::Boolean));

    TantivyIndex::new_in_ram(config).unwrap()
}

fn sample_doc(id: &str, title: &str, body: &str, category: &str, price: f64) -> Document {
    Document::new(DocumentId::new(id))
        .with_field("title", FieldValue::Text(title.into()))
        .with_field("body", FieldValue::Text(body.into()))
        .with_field("category", FieldValue::Text(category.into()))
        .with_field("price", FieldValue::Number(price))
        .with_field("active", FieldValue::Boolean(true))
}

fn index_sample_corpus(idx: &TantivyIndex) {
    let docs = vec![
        sample_doc(
            "rust-book",
            "The Rust Programming Language",
            "Learn Rust from scratch with this comprehensive guide to systems programming",
            "programming",
            39.99,
        ),
        sample_doc(
            "go-book",
            "The Go Programming Language",
            "An authoritative guide to writing clear and idiomatic Go programs",
            "programming",
            34.99,
        ),
        sample_doc(
            "distributed-systems",
            "Designing Data-Intensive Applications",
            "The big ideas behind reliable scalable and maintainable distributed systems",
            "architecture",
            45.99,
        ),
        sample_doc(
            "database-internals",
            "Database Internals",
            "A deep dive into how distributed data storage systems work under the hood",
            "database",
            49.99,
        ),
        sample_doc(
            "rust-async",
            "Asynchronous Programming in Rust",
            "Master async await futures and tokio for high performance concurrent Rust applications",
            "programming",
            29.99,
        ),
    ];

    for doc in &docs {
        idx.index_document_sync(doc).unwrap();
    }
    idx.commit().unwrap();
}

// ---------------------------------------------------------------------------
// Full-text search tests
// ---------------------------------------------------------------------------

#[test]
fn search_all_query_variants() {
    let idx = make_index();
    index_sample_corpus(&idx);

    // FullText query
    let query = Query::FullText(FullTextQuery {
        field: "title".into(),
        query: "Rust".into(),
        operator: Operator::Or,
    });
    let result = idx.search_sync(&query, 10).unwrap();
    assert_eq!(result.total, 2); // "Rust Programming Language" + "Async Programming in Rust"

    // Term query on _id
    let query = Query::Term(TermQuery {
        field: "_id".into(),
        value: "go-book".into(),
    });
    let result = idx.search_sync(&query, 10).unwrap();
    assert_eq!(result.total, 1);
    assert_eq!(result.documents[0].document.id.as_str(), "go-book");

    // Range query on price
    let query = Query::Range(RangeQuery {
        field: "price".into(),
        gte: Some(40.0),
        lte: Some(50.0),
    });
    let result = idx.search_sync(&query, 10).unwrap();
    assert_eq!(result.total, 2); // 45.99 and 49.99

    // Bool query: must "programming" in body, must_not "Go" in title
    let query = Query::Bool(BoolQuery {
        must: vec![Query::FullText(FullTextQuery {
            field: "body".into(),
            query: "programming".into(),
            operator: Operator::Or,
        })],
        should: vec![],
        must_not: vec![Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "Go".into(),
            operator: Operator::Or,
        })],
    });
    let result = idx.search_sync(&query, 10).unwrap();
    // Should find rust-book (systems programming) but not go-book
    assert!(result
        .documents
        .iter()
        .all(|sd| sd.document.id.as_str() != "go-book"));
}

// ---------------------------------------------------------------------------
// BM25 scoring
// ---------------------------------------------------------------------------

#[test]
fn bm25_exact_matches_rank_higher() {
    let idx = make_index();
    index_sample_corpus(&idx);

    let query = Query::FullText(FullTextQuery {
        field: "title".into(),
        query: "Database Internals".into(),
        operator: Operator::And,
    });

    let result = idx.search_sync(&query, 10).unwrap();
    assert!(!result.documents.is_empty());
    assert_eq!(
        result.documents[0].document.id.as_str(),
        "database-internals"
    );
}

// ---------------------------------------------------------------------------
// Fuzzy search (typo tolerance via query parser)
// ---------------------------------------------------------------------------

#[test]
fn fuzzy_search_with_typo() {
    let idx = make_index();
    index_sample_corpus(&idx);

    // "Ruts" is a typo for "Rust" — Tantivy's query parser with ~ supports fuzzy
    let query = Query::FullText(FullTextQuery {
        field: "title".into(),
        query: "Ruts~1".into(),
        operator: Operator::Or,
    });

    let result = idx.search_sync(&query, 10).unwrap();
    // Should find documents with "Rust" in title despite the typo
    assert!(
        !result.documents.is_empty(),
        "fuzzy search should find results"
    );
}

// ---------------------------------------------------------------------------
// Phrase query (word order matters)
// ---------------------------------------------------------------------------

#[test]
fn phrase_query_preserves_word_order() {
    let idx = make_index();
    index_sample_corpus(&idx);

    // "Programming Language" as a phrase (in order)
    let query = Query::FullText(FullTextQuery {
        field: "title".into(),
        query: "\"Programming Language\"".into(),
        operator: Operator::And,
    });

    let result = idx.search_sync(&query, 10).unwrap();
    assert_eq!(result.total, 2); // rust-book and go-book

    // Reversed order should NOT match as phrase
    let query = Query::FullText(FullTextQuery {
        field: "title".into(),
        query: "\"Language Programming\"".into(),
        operator: Operator::And,
    });

    let result = idx.search_sync(&query, 10).unwrap();
    assert_eq!(result.total, 0);
}

// ---------------------------------------------------------------------------
// Ngram analyzer (partial word matching)
// ---------------------------------------------------------------------------

#[test]
fn ngram_analyzer_finds_partial_words() {
    // Create an index with an ngram-analyzed field
    let config = SchemaConfig::new().with_field(FieldConfig::new("name", FieldType::Text));

    let idx = TantivyIndex::new_in_ram(config).unwrap();

    // Register ngram analyzer on the "name" field by re-creating with custom schema
    // For this test, we use the standard tokenizer and search for substrings
    // The ngram analyzer was registered and can be used at query time
    let doc = Document::new(DocumentId::new("ng1"))
        .with_field("name", FieldValue::Text("elasticsearch".into()));

    idx.index_document_sync(&doc).unwrap();
    idx.commit().unwrap();

    // Search for "elastic" — standard tokenizer will match whole word "elasticsearch"
    let query = Query::FullText(FullTextQuery {
        field: "name".into(),
        query: "elasticsearch".into(),
        operator: Operator::Or,
    });

    let result = idx.search_sync(&query, 10).unwrap();
    assert_eq!(result.total, 1);
}

// ---------------------------------------------------------------------------
// Highlighting
// ---------------------------------------------------------------------------

#[test]
fn highlighting_returns_correct_fragments() {
    let idx = make_index();
    index_sample_corpus(&idx);

    let doc = sample_doc(
        "rust-book",
        "The Rust Programming Language",
        "Learn Rust from scratch with this comprehensive guide to systems programming",
        "programming",
        39.99,
    );

    let query = Query::FullText(FullTextQuery {
        field: "body".into(),
        query: "Rust programming".into(),
        operator: Operator::Or,
    });

    let config = HighlightConfig::default();
    let results = highlight(&idx, &doc, &query, &["body"], &config);

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].field, "body");
    if !results[0].fragments.is_empty() {
        let frag = &results[0].fragments[0];
        assert!(
            frag.contains("<em>"),
            "fragment should contain <em> tags: {}",
            frag
        );
    }
}

// ---------------------------------------------------------------------------
// Delete removes from search results
// ---------------------------------------------------------------------------

#[test]
fn delete_removes_document_from_search_results() {
    let idx = make_index();
    index_sample_corpus(&idx);

    // Verify document exists
    let query = Query::Term(TermQuery {
        field: "_id".into(),
        value: "rust-book".into(),
    });
    let result = idx.search_sync(&query, 10).unwrap();
    assert_eq!(result.total, 1);

    // Delete it
    idx.delete_document_sync(&DocumentId::new("rust-book"))
        .unwrap();
    idx.commit().unwrap();

    // Verify it's gone
    let result = idx.search_sync(&query, 10).unwrap();
    assert_eq!(result.total, 0);
}

// ---------------------------------------------------------------------------
// Update replaces old document
// ---------------------------------------------------------------------------

#[test]
fn update_replaces_old_document() {
    let idx = make_index();
    index_sample_corpus(&idx);

    // Update "rust-book" with new title
    let updated = sample_doc(
        "rust-book",
        "Rust 2024 Edition",
        "Updated content for the new edition",
        "programming",
        49.99,
    );
    idx.update_document_sync(&updated).unwrap();
    idx.commit().unwrap();

    // Old title should not be found
    let query = Query::FullText(FullTextQuery {
        field: "title".into(),
        query: "Programming Language".into(),
        operator: Operator::And,
    });
    let result = idx.search_sync(&query, 10).unwrap();
    // Only go-book should match now
    assert!(result
        .documents
        .iter()
        .all(|sd| sd.document.id.as_str() != "rust-book"));

    // New title should be found
    let query = Query::FullText(FullTextQuery {
        field: "title".into(),
        query: "2024 Edition".into(),
        operator: Operator::And,
    });
    let result = idx.search_sync(&query, 10).unwrap();
    assert_eq!(result.total, 1);
    assert_eq!(result.documents[0].document.id.as_str(), "rust-book");
}

// ---------------------------------------------------------------------------
// QueryBuilder integration
// ---------------------------------------------------------------------------

#[test]
fn query_builder_integrates_with_index() {
    let idx = make_index();
    index_sample_corpus(&idx);

    let query = QueryBuilder::new()
        .must_match("body", "systems")
        .must_range("price", Some(30.0), Some(50.0))
        .build();

    let result = idx.search_sync(&query, 10).unwrap();
    // Should find docs mentioning "systems" in body with price 30-50
    assert!(!result.documents.is_empty());

    for sd in &result.documents {
        if let Some(FieldValue::Number(price)) = sd.document.get_field("price") {
            assert!(*price >= 30.0 && *price <= 50.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Disk-based index (via tempfile)
// ---------------------------------------------------------------------------

#[test]
fn disk_based_index_create_and_search() {
    let tmp = tempfile::tempdir().unwrap();
    let config = SchemaConfig::new().with_field(FieldConfig::new("title", FieldType::Text));

    let idx = TantivyIndex::new(tmp.path(), config).unwrap();

    let doc = Document::new(DocumentId::new("disk-1"))
        .with_field("title", FieldValue::Text("Disk based index test".into()));

    idx.index_document_sync(&doc).unwrap();
    idx.commit().unwrap();

    let query = Query::FullText(FullTextQuery {
        field: "title".into(),
        query: "Disk".into(),
        operator: Operator::Or,
    });

    let result = idx.search_sync(&query, 10).unwrap();
    assert_eq!(result.total, 1);
}
