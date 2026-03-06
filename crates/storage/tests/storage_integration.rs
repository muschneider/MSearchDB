//! Integration tests for the MSearchDB storage engine.
//!
//! These tests exercise larger-scale scenarios: bulk writes, latency
//! assertions, and persistence across open/close cycles.

use std::time::Instant;

use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::traits::StorageBackend;
use msearchdb_storage::rocksdb_backend::{RocksDbStorage, StorageOptions};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn open_storage(dir: &TempDir) -> RocksDbStorage {
    RocksDbStorage::new(dir.path(), StorageOptions::default()).expect("open storage")
}

// ---------------------------------------------------------------------------
// Bulk write and read 10,000 documents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bulk_put_and_read_10k_documents() {
    let dir = TempDir::new().unwrap();
    let storage = open_storage(&dir);
    let total = 10_000;

    // Write 10,000 documents.
    for i in 0..total {
        let doc = Document::new(DocumentId::new(format!("bulk-{:06}", i)))
            .with_field("index", FieldValue::Number(i as f64))
            .with_field("title", FieldValue::Text(format!("Document #{}", i)));
        StorageBackend::put(&storage, doc).await.unwrap();
    }

    // Read all 10,000 documents back and verify.
    for i in 0..total {
        let id = DocumentId::new(format!("bulk-{:06}", i));
        let doc = StorageBackend::get(&storage, &id).await.unwrap();
        assert_eq!(doc.id, id);
        assert_eq!(doc.get_field("index"), Some(&FieldValue::Number(i as f64)));
    }
}

// ---------------------------------------------------------------------------
// p99 read latency < 10ms
// ---------------------------------------------------------------------------

#[tokio::test]
async fn p99_read_latency_under_10ms() {
    let dir = TempDir::new().unwrap();
    let storage = open_storage(&dir);
    let total = 1_000;

    // Populate.
    for i in 0..total {
        let doc = Document::new(DocumentId::new(format!("latency-{:04}", i)))
            .with_field("data", FieldValue::Text("payload".into()));
        StorageBackend::put(&storage, doc).await.unwrap();
    }

    // Measure individual read latencies.
    let mut latencies = Vec::with_capacity(total);
    for i in 0..total {
        let id = DocumentId::new(format!("latency-{:04}", i));
        let start = Instant::now();
        let _doc = StorageBackend::get(&storage, &id).await.unwrap();
        latencies.push(start.elapsed());
    }

    latencies.sort();

    let p99_index = (latencies.len() as f64 * 0.99) as usize;
    let p99 = latencies[p99_index.min(latencies.len() - 1)];

    eprintln!("p99 read latency: {:?}", p99);
    assert!(
        p99.as_millis() < 10,
        "p99 read latency was {:?}, expected < 10ms",
        p99
    );
}

// ---------------------------------------------------------------------------
// Persistence across reopen
// ---------------------------------------------------------------------------

#[tokio::test]
async fn survives_close_and_reopen() {
    let dir = TempDir::new().unwrap();

    // Phase 1: write data.
    {
        let storage = open_storage(&dir);
        for i in 0..100 {
            let doc = Document::new(DocumentId::new(format!("persist-{:03}", i)))
                .with_field("phase", FieldValue::Text("written".into()));
            StorageBackend::put(&storage, doc).await.unwrap();
        }
        // Storage is dropped here — RocksDB flushes and closes.
    }

    // Phase 2: reopen and verify data is still present.
    {
        let storage = open_storage(&dir);
        for i in 0..100 {
            let id = DocumentId::new(format!("persist-{:03}", i));
            let doc = StorageBackend::get(&storage, &id).await.unwrap();
            assert_eq!(doc.id, id);
            assert_eq!(
                doc.get_field("phase"),
                Some(&FieldValue::Text("written".into()))
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Scan across full range after bulk insert
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scan_full_range_after_bulk() {
    let dir = TempDir::new().unwrap();
    let storage = open_storage(&dir);

    for i in 0..500 {
        let doc = Document::new(DocumentId::new(format!("scan-{:04}", i)));
        StorageBackend::put(&storage, doc).await.unwrap();
    }

    let range = DocumentId::new("scan-0000")..=DocumentId::new("scan-9999");
    let all = StorageBackend::scan(&storage, range, 1000).await.unwrap();
    assert_eq!(all.len(), 500);
}
