//! Performance benchmark suite for MSearchDB.
//!
//! Measures:
//! - Single write latency (target: < 5ms p99)
//! - Batch write throughput (target: 50k docs/sec)
//! - Search latency p50/p99/p999 (target: < 50ms p99)
//! - L1 cache hit/miss performance
//!
//! Run with:
//! ```bash
//! cargo bench -p msearchdb-node
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};

use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::query::{FullTextQuery, Operator, Query};
use msearchdb_core::traits::IndexBackend;
use msearchdb_index::schema_builder::{FieldConfig, FieldType, SchemaConfig};
use msearchdb_index::tantivy_index::TantivyIndex;
use msearchdb_node::cache::DocumentCache;
use msearchdb_node::session::SessionManager;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_document(i: usize) -> Document {
    Document::new(DocumentId::new(format!("doc-{:06}", i)))
        .with_field("title", FieldValue::Text(format!("Product {}", i)))
        .with_field(
            "description",
            FieldValue::Text(format!(
                "This is a detailed description for product number {}. \
                 It contains multiple words for full-text search testing. \
                 Keywords: electronics gadget widget device tool",
                i
            )),
        )
        .with_field("price", FieldValue::Number(10.0 + (i as f64) * 0.5))
        .with_field("category", FieldValue::Text("electronics".into()))
        .with_field("in_stock", FieldValue::Boolean(i % 3 != 0))
}

fn create_index() -> TantivyIndex {
    let schema = SchemaConfig::new()
        .with_field(FieldConfig::new("_body", FieldType::Text))
        .with_field(FieldConfig::new("title", FieldType::Text))
        .with_field(FieldConfig::new("description", FieldType::Text))
        .with_field(FieldConfig::new("category", FieldType::Text))
        .with_field(FieldConfig::new("price", FieldType::Number));

    TantivyIndex::new_in_ram(schema).unwrap()
}

fn create_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Benchmarks: L1 Cache
// ---------------------------------------------------------------------------

fn bench_cache_hit(c: &mut Criterion) {
    let rt = create_runtime();
    let cache = DocumentCache::with_defaults();
    let doc = make_document(0);

    rt.block_on(async { cache.put("products", &doc).await });

    c.bench_function("cache/l1_hit", |b| {
        b.iter(|| {
            rt.block_on(async {
                cache.get("products", &DocumentId::new("doc-000000")).await
            })
        });
    });
}

fn bench_cache_miss(c: &mut Criterion) {
    let rt = create_runtime();
    let cache = DocumentCache::with_defaults();

    c.bench_function("cache/l1_miss", |b| {
        b.iter(|| {
            rt.block_on(async {
                cache.get("products", &DocumentId::new("missing")).await
            })
        });
    });
}

fn bench_cache_put(c: &mut Criterion) {
    let rt = create_runtime();
    let cache = DocumentCache::with_defaults();
    let counter = AtomicUsize::new(0);

    c.bench_function("cache/l1_put", |b| {
        b.iter(|| {
            let n = counter.fetch_add(1, Ordering::Relaxed);
            let doc = make_document(n);
            rt.block_on(async { cache.put("products", &doc).await })
        });
    });
}

fn bench_cache_invalidate(c: &mut Criterion) {
    let rt = create_runtime();
    let cache = DocumentCache::with_defaults();

    rt.block_on(async {
        for i in 0..1000 {
            cache.put("products", &make_document(i)).await;
        }
    });

    let counter = AtomicUsize::new(0);
    c.bench_function("cache/l1_invalidate", |b| {
        b.iter(|| {
            let n = counter.fetch_add(1, Ordering::Relaxed) % 1000;
            let id = DocumentId::new(format!("doc-{:06}", n));
            rt.block_on(async { cache.invalidate("products", &id).await })
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmarks: Index (Tantivy)
// ---------------------------------------------------------------------------

fn bench_index_single_write(c: &mut Criterion) {
    let rt = create_runtime();
    let index = create_index();
    let counter = AtomicUsize::new(0);

    c.bench_function("index/single_write", |b| {
        b.iter(|| {
            let n = counter.fetch_add(1, Ordering::Relaxed);
            let doc = make_document(n);
            rt.block_on(async {
                index.index_document(&doc).await.unwrap();
                index.commit_index().await.unwrap();
            })
        });
    });
}

fn bench_index_batch_write(c: &mut Criterion) {
    let rt = create_runtime();

    let mut group = c.benchmark_group("index/batch_write");
    group.throughput(Throughput::Elements(100));
    group.sample_size(20);

    let index = create_index();
    let batch_num = AtomicUsize::new(0);

    group.bench_function("100_docs", |b| {
        b.iter(|| {
            let bn = batch_num.fetch_add(1, Ordering::Relaxed);
            let base = bn * 100;
            let docs: Vec<Document> = (0..100).map(|i| make_document(base + i)).collect();
            rt.block_on(async {
                for doc in &docs {
                    index.index_document(doc).await.unwrap();
                }
                index.commit_index().await.unwrap();
            })
        });
    });

    group.finish();
}

fn bench_search_after_ingest(c: &mut Criterion) {
    let rt = create_runtime();
    let index = create_index();

    // Pre-populate index with documents.
    rt.block_on(async {
        for i in 0..5000 {
            index.index_document(&make_document(i)).await.unwrap();
        }
        index.commit_index().await.unwrap();
    });

    let mut group = c.benchmark_group("search");

    // Full-text search
    group.bench_function("fulltext_match", |b| {
        b.iter(|| {
            rt.block_on(async {
                let query = Query::FullText(FullTextQuery {
                    field: "description".to_string(),
                    query: "electronics gadget".to_string(),
                    operator: Operator::Or,
                });
                index.search(&query).await.unwrap()
            })
        });
    });

    // Term search
    group.bench_function("term_exact", |b| {
        b.iter(|| {
            rt.block_on(async {
                let query = Query::Term(msearchdb_core::query::TermQuery {
                    field: "category".to_string(),
                    value: "electronics".to_string(),
                });
                index.search(&query).await.unwrap()
            })
        });
    });

    // Range search
    group.bench_function("range_numeric", |b| {
        b.iter(|| {
            rt.block_on(async {
                let query = Query::Range(msearchdb_core::query::RangeQuery {
                    field: "price".to_string(),
                    gte: Some(100.0),
                    lte: Some(500.0),
                });
                index.search(&query).await.unwrap()
            })
        });
    });

    // Bool combined query
    group.bench_function("bool_combined", |b| {
        b.iter(|| {
            rt.block_on(async {
                let query = Query::Bool(msearchdb_core::query::BoolQuery {
                    must: vec![Query::FullText(FullTextQuery {
                        field: "description".to_string(),
                        query: "product".to_string(),
                        operator: Operator::Or,
                    })],
                    should: vec![Query::Term(msearchdb_core::query::TermQuery {
                        field: "category".to_string(),
                        value: "electronics".to_string(),
                    })],
                    must_not: vec![],
                });
                index.search(&query).await.unwrap()
            })
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: Session Manager
// ---------------------------------------------------------------------------

fn bench_session_token(c: &mut Criterion) {
    let mgr = SessionManager::new();

    c.bench_function("session/advance_index", |b| {
        let counter = AtomicUsize::new(0);
        b.iter(|| {
            let n = counter.fetch_add(1, Ordering::Relaxed);
            mgr.advance_applied_index(n as u64);
        });
    });

    c.bench_function("session/token_encode", |b| {
        let token = msearchdb_node::session::SessionToken::new(12345);
        b.iter(|| token.encode());
    });

    c.bench_function("session/token_decode", |b| {
        let encoded = "msearchdb:12345";
        b.iter(|| msearchdb_node::session::SessionToken::decode(encoded));
    });
}

fn bench_session_wait_already_applied(c: &mut Criterion) {
    let rt = create_runtime();
    let mgr = SessionManager::new();
    mgr.advance_applied_index(1000);

    c.bench_function("session/wait_already_applied", |b| {
        b.iter(|| {
            rt.block_on(async { mgr.wait_for_index(500).await.unwrap() })
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark groups
// ---------------------------------------------------------------------------

criterion_group!(
    name = cache_benches;
    config = Criterion::default().measurement_time(Duration::from_secs(5));
    targets = bench_cache_hit, bench_cache_miss, bench_cache_put, bench_cache_invalidate
);

criterion_group!(
    name = index_benches;
    config = Criterion::default()
        .measurement_time(Duration::from_secs(10))
        .sample_size(50);
    targets = bench_index_single_write, bench_index_batch_write, bench_search_after_ingest
);

criterion_group!(
    name = session_benches;
    config = Criterion::default().measurement_time(Duration::from_secs(3));
    targets = bench_session_token, bench_session_wait_already_applied
);

criterion_main!(cache_benches, index_benches, session_benches);
