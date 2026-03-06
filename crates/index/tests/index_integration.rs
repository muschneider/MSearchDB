//! Large-scale integration tests for the msearchdb-index crate.
//!
//! These tests verify performance, recall, and concurrency characteristics
//! of the Tantivy-backed search index under realistic workloads.

use std::sync::Arc;
use std::time::Instant;

use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::query::*;
use msearchdb_core::traits::IndexBackend;

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

/// Generate fake documents for bulk testing.
///
/// Each document has a unique ID and predictable content that can be
/// queried deterministically.
fn generate_docs(count: usize) -> Vec<Document> {
    let categories = [
        "programming",
        "database",
        "networking",
        "security",
        "devops",
    ];
    let adjectives = [
        "advanced",
        "practical",
        "modern",
        "essential",
        "comprehensive",
    ];
    let topics = [
        "distributed systems",
        "machine learning",
        "web development",
        "cloud architecture",
        "data engineering",
    ];

    (0..count)
        .map(|i| {
            let cat = categories[i % categories.len()];
            let adj = adjectives[i % adjectives.len()];
            let topic = topics[i % topics.len()];

            Document::new(DocumentId::new(format!("doc-{:06}", i)))
                .with_field(
                    "title",
                    FieldValue::Text(format!("{} guide to {} number {}", adj, topic, i)),
                )
                .with_field(
                    "body",
                    FieldValue::Text(format!(
                        "This is a {} resource about {}. \
                         It covers key concepts and best practices for \
                         building reliable and scalable solutions. \
                         Document identifier is {}.",
                        adj, topic, i
                    )),
                )
                .with_field("category", FieldValue::Text(cat.into()))
                .with_field("price", FieldValue::Number((i as f64) * 0.5 + 10.0))
                .with_field("active", FieldValue::Boolean(i % 3 != 0))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Bulk indexing + search latency
// ---------------------------------------------------------------------------

#[test]
fn bulk_index_100k_documents_and_search_latency() {
    let idx = make_index();
    let docs = generate_docs(100_000);

    // Index all documents in batches
    let index_start = Instant::now();
    for (i, doc) in docs.iter().enumerate() {
        idx.index_document_sync(doc).unwrap();
        // Commit every 10,000 documents for realistic batching
        if (i + 1) % 10_000 == 0 {
            idx.commit().unwrap();
        }
    }
    // Final commit
    idx.commit().unwrap();
    let index_time = index_start.elapsed();
    eprintln!(
        "Indexed 100,000 documents in {:.2}s",
        index_time.as_secs_f64()
    );

    // Run 100 searches and measure p99 latency
    let mut latencies: Vec<u128> = Vec::with_capacity(100);

    for i in 0..100 {
        let query_text = match i % 5 {
            0 => "distributed systems",
            1 => "machine learning",
            2 => "web development",
            3 => "cloud architecture",
            _ => "data engineering",
        };

        let query = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: query_text.into(),
            operator: Operator::Or,
        });

        let start = Instant::now();
        let result = idx.search_sync(&query, 10).unwrap();
        let latency = start.elapsed().as_millis();
        latencies.push(latency);

        assert!(
            !result.documents.is_empty(),
            "query '{}' should have results",
            query_text
        );
    }

    latencies.sort();
    let p99_idx = (latencies.len() as f64 * 0.99) as usize;
    let p99 = latencies[p99_idx.min(latencies.len() - 1)];
    eprintln!("Search p99 latency: {}ms", p99);

    // Assert p99 < 50ms
    assert!(
        p99 < 50,
        "p99 search latency was {}ms, expected < 50ms",
        p99
    );
}

// ---------------------------------------------------------------------------
// Recall: all indexed documents are findable
// ---------------------------------------------------------------------------

#[test]
fn recall_all_indexed_documents_findable_by_id() {
    let idx = make_index();
    let count = 1_000;
    let docs = generate_docs(count);

    for doc in &docs {
        idx.index_document_sync(doc).unwrap();
    }
    idx.commit().unwrap();

    // Verify every document is findable by its _id
    let mut found = 0;
    for doc in &docs {
        let query = Query::Term(TermQuery {
            field: "_id".into(),
            value: doc.id.as_str().to_owned(),
        });

        let result = idx.search_sync(&query, 1).unwrap();
        if result.total == 1 {
            found += 1;
        }
    }

    assert_eq!(
        found, count,
        "expected all {} documents to be findable, found {}",
        count, found
    );
}

// ---------------------------------------------------------------------------
// Concurrent indexing with 10 parallel tokio tasks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_indexing_10_tasks() {
    let idx = Arc::new(make_index());
    let docs_per_task = 1_000;
    let num_tasks = 10;

    let mut handles = Vec::new();

    for task_id in 0..num_tasks {
        let idx = Arc::clone(&idx);
        let handle = tokio::spawn(async move {
            for i in 0..docs_per_task {
                let doc_id = format!("task-{}-doc-{}", task_id, i);
                let doc = Document::new(DocumentId::new(&doc_id))
                    .with_field(
                        "title",
                        FieldValue::Text(format!(
                            "Concurrent document {} from task {}",
                            i, task_id
                        )),
                    )
                    .with_field("body", FieldValue::Text("concurrent test body".into()))
                    .with_field("category", FieldValue::Text("test".into()))
                    .with_field("price", FieldValue::Number(i as f64))
                    .with_field("active", FieldValue::Boolean(true));

                idx.index_document(&doc).await.unwrap();
            }
        });
        handles.push(handle);
    }

    // Wait for all tasks to complete
    for handle in handles {
        handle.await.unwrap();
    }

    // Commit all buffered writes
    idx.commit().unwrap();

    // Verify total document count
    let query = Query::FullText(FullTextQuery {
        field: "body".into(),
        query: "concurrent".into(),
        operator: Operator::Or,
    });

    let result = idx.search_sync(&query, 100_000).unwrap();
    let total = result.total as usize;

    // We should have all 10,000 documents
    assert_eq!(
        total,
        num_tasks * docs_per_task,
        "expected {} documents, found {}",
        num_tasks * docs_per_task,
        total
    );
}

// ---------------------------------------------------------------------------
// Concurrent search with 50 parallel queries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_search_50_queries() {
    let idx = Arc::new(make_index());

    // Index a corpus first
    let docs = generate_docs(10_000);
    for doc in &docs {
        idx.index_document_sync(doc).unwrap();
    }
    idx.commit().unwrap();

    let num_queries = 50;
    let mut handles = Vec::new();

    for i in 0..num_queries {
        let idx = Arc::clone(&idx);
        let query_text = match i % 5 {
            0 => "distributed systems",
            1 => "machine learning",
            2 => "web development",
            3 => "cloud architecture",
            _ => "data engineering",
        };
        let query_text = query_text.to_owned();

        let handle = tokio::spawn(async move {
            let query = Query::FullText(FullTextQuery {
                field: "title".into(),
                query: query_text.clone(),
                operator: Operator::Or,
            });

            let result = idx.search(&query).await.unwrap();
            assert!(
                !result.documents.is_empty(),
                "query '{}' should return results",
                query_text
            );
            result.total
        });
        handles.push(handle);
    }

    // Collect all results
    let mut total_hits = 0u64;
    for handle in handles {
        total_hits += handle.await.unwrap();
    }

    eprintln!("50 concurrent queries returned {} total hits", total_hits);
    assert!(total_hits > 0, "should have some hits across all queries");
}
