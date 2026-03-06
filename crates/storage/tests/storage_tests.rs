//! Unit-level integration tests for msearchdb-storage.
//!
//! These tests exercise the RocksDB backend, WAL, and memtable modules
//! end-to-end using temporary directories for hermetic isolation.

use std::sync::Arc;

use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::error::DbError;
use msearchdb_core::traits::StorageBackend;
use msearchdb_storage::memtable::MemTable;
use msearchdb_storage::rocksdb_backend::{
    RocksDbStorage, StorageOptions, CF_DOCUMENTS, CF_METADATA,
};
use msearchdb_storage::wal::{WalOperation, WalReader, WalWriter};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_storage() -> (TempDir, RocksDbStorage) {
    let dir = TempDir::new().expect("create temp dir");
    let storage = RocksDbStorage::new(dir.path(), StorageOptions::default()).expect("open storage");
    (dir, storage)
}

fn sample_document(id: &str) -> Document {
    Document::new(DocumentId::new(id))
        .with_field("title", FieldValue::Text(format!("Document {}", id)))
        .with_field("score", FieldValue::Number(42.5))
        .with_field("active", FieldValue::Boolean(true))
        .with_field(
            "tags",
            FieldValue::Array(vec![
                FieldValue::Text("rust".into()),
                FieldValue::Text("database".into()),
            ]),
        )
}

// ---------------------------------------------------------------------------
// RocksDB: put / get / delete roundtrip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_get_delete_roundtrip() {
    let (_dir, storage) = temp_storage();

    let doc = sample_document("roundtrip-1");

    // Put via the trait.
    StorageBackend::put(&storage, doc.clone()).await.unwrap();

    // Get via the trait.
    let loaded = StorageBackend::get(&storage, &DocumentId::new("roundtrip-1"))
        .await
        .unwrap();
    assert_eq!(loaded.id, doc.id);
    assert_eq!(loaded.get_field("title"), doc.get_field("title"));
    assert_eq!(loaded.get_field("score"), doc.get_field("score"));
    assert_eq!(loaded.get_field("active"), doc.get_field("active"));
    assert_eq!(loaded.get_field("tags"), doc.get_field("tags"));

    // Delete via the trait.
    StorageBackend::delete(&storage, &DocumentId::new("roundtrip-1"))
        .await
        .unwrap();

    let result = StorageBackend::get(&storage, &DocumentId::new("roundtrip-1")).await;
    assert!(matches!(result, Err(DbError::NotFound(_))));
}

// ---------------------------------------------------------------------------
// Document serialization with all FieldValue types
// ---------------------------------------------------------------------------

#[tokio::test]
async fn document_serialization_all_field_types() {
    let (_dir, storage) = temp_storage();

    let doc = Document::new(DocumentId::new("all-types"))
        .with_field("text", FieldValue::Text("hello world".into()))
        .with_field("number", FieldValue::Number(3.15))
        .with_field("integer", FieldValue::Number(42.0))
        .with_field("negative", FieldValue::Number(-99.9))
        .with_field("bool_true", FieldValue::Boolean(true))
        .with_field("bool_false", FieldValue::Boolean(false))
        .with_field(
            "array_mixed",
            FieldValue::Array(vec![
                FieldValue::Text("nested".into()),
                FieldValue::Number(1.0),
                FieldValue::Boolean(false),
                FieldValue::Array(vec![FieldValue::Text("deep".into())]),
            ]),
        )
        .with_field("empty_text", FieldValue::Text(String::new()))
        .with_field("empty_array", FieldValue::Array(Vec::new()));

    storage.put_document(&doc).await.unwrap();
    let loaded = storage
        .get_document(&DocumentId::new("all-types"))
        .await
        .unwrap()
        .expect("document should exist");

    // Verify every field matches after MessagePack roundtrip.
    assert_eq!(loaded.fields.len(), doc.fields.len());
    for (name, value) in &doc.fields {
        assert_eq!(
            loaded.get_field(name),
            Some(value),
            "field '{}' mismatch",
            name
        );
    }
}

// ---------------------------------------------------------------------------
// Scan with prefix matching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scan_with_prefix_matching() {
    let (_dir, storage) = temp_storage();

    // Insert documents with distinct prefixes.
    for i in 0..5 {
        let doc = Document::new(DocumentId::new(format!("alpha-{}", i)))
            .with_field("group", FieldValue::Text("alpha".into()));
        StorageBackend::put(&storage, doc).await.unwrap();
    }
    for i in 0..3 {
        let doc = Document::new(DocumentId::new(format!("beta-{}", i)))
            .with_field("group", FieldValue::Text("beta".into()));
        StorageBackend::put(&storage, doc).await.unwrap();
    }

    // Raw prefix scan in the documents CF.
    let alpha_results = storage.scan(CF_DOCUMENTS, b"alpha-", 100).await.unwrap();
    assert_eq!(alpha_results.len(), 5);

    let beta_results = storage.scan(CF_DOCUMENTS, b"beta-", 100).await.unwrap();
    assert_eq!(beta_results.len(), 3);
}

// ---------------------------------------------------------------------------
// Scan via StorageBackend trait (range-based)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trait_scan_with_range() {
    let (_dir, storage) = temp_storage();

    for i in 0..10 {
        let doc = Document::new(DocumentId::new(format!("doc-{:02}", i)));
        StorageBackend::put(&storage, doc).await.unwrap();
    }

    let range = DocumentId::new("doc-03")..=DocumentId::new("doc-07");
    let results = StorageBackend::scan(&storage, range, 100).await.unwrap();
    assert_eq!(results.len(), 5);
    assert_eq!(results[0].id.as_str(), "doc-03");
    assert_eq!(results[4].id.as_str(), "doc-07");
}

#[tokio::test]
async fn trait_scan_with_limit() {
    let (_dir, storage) = temp_storage();

    for i in 0..20 {
        let doc = Document::new(DocumentId::new(format!("item-{:02}", i)));
        StorageBackend::put(&storage, doc).await.unwrap();
    }

    let range = DocumentId::new("item-00")..=DocumentId::new("item-99");
    let results = StorageBackend::scan(&storage, range, 5).await.unwrap();
    assert_eq!(results.len(), 5);
}

// ---------------------------------------------------------------------------
// Concurrent reads with tokio::spawn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_reads_multiple_tasks() {
    let (_dir, storage) = temp_storage();
    let storage = Arc::new(storage);

    // Insert several documents.
    for i in 0..50 {
        let doc = sample_document(&format!("concurrent-{}", i));
        storage.put_document(&doc).await.unwrap();
    }

    // Spawn multiple concurrent reader tasks.
    let mut handles = Vec::new();
    for i in 0..50 {
        let storage = Arc::clone(&storage);
        handles.push(tokio::spawn(async move {
            let id = DocumentId::new(format!("concurrent-{}", i));
            let doc = storage.get_document(&id).await.unwrap();
            assert!(doc.is_some(), "missing document concurrent-{}", i);
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// WAL: CRC checksum validation
// ---------------------------------------------------------------------------

#[test]
fn wal_crc_checksum_validation() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.wal");

    {
        let mut writer = WalWriter::new(&path).unwrap();
        writer
            .append(WalOperation::Put {
                key: b"k1".to_vec(),
                value: b"v1".to_vec(),
            })
            .unwrap();
        writer.sync().unwrap();
    }

    // Valid read succeeds.
    let entries = WalReader::read_all(&path).unwrap();
    assert_eq!(entries.len(), 1);

    // Corrupt a byte in the payload.
    {
        let mut data = std::fs::read(&path).unwrap();
        // The payload starts at byte 8 (4 len + 4 crc).
        // Flip a bit in the payload.
        let last = data.len() - 1;
        data[last] ^= 0xFF;
        std::fs::write(&path, &data).unwrap();
    }

    let result = WalReader::read_all(&path);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("CRC mismatch"), "unexpected error: {}", err);
}

// ---------------------------------------------------------------------------
// WAL: recovery simulation
// ---------------------------------------------------------------------------

#[test]
fn wal_recovery_write_crash_replay() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("recovery.wal");

    // Write phase — simulate writes then crash (no checkpoint).
    {
        let mut writer = WalWriter::new(&path).unwrap();
        writer
            .append(WalOperation::Put {
                key: b"doc-a".to_vec(),
                value: b"value-a".to_vec(),
            })
            .unwrap();
        writer
            .append(WalOperation::Put {
                key: b"doc-b".to_vec(),
                value: b"value-b".to_vec(),
            })
            .unwrap();
        writer
            .append(WalOperation::Delete {
                key: b"doc-a".to_vec(),
            })
            .unwrap();
        writer.sync().unwrap();
        // Drop without checkpoint — simulates process crash.
    }

    // Recovery phase — read all entries (no checkpoint, so all are replayed).
    let (puts, deletes) = WalReader::replay(&path).unwrap();
    assert_eq!(puts.len(), 2);
    assert_eq!(deletes.len(), 1);
    assert_eq!(puts[0].0, b"doc-a");
    assert_eq!(puts[1].0, b"doc-b");
    assert_eq!(deletes[0], b"doc-a");
}

// ---------------------------------------------------------------------------
// Memtable: concurrent insert and read
// ---------------------------------------------------------------------------

#[test]
fn memtable_concurrent_insert_and_read() {
    use std::thread;

    let mt = Arc::new(MemTable::new(1024 * 1024));
    let num_threads = 8;
    let entries_per_thread = 200;

    let mut handles = Vec::new();
    for t in 0..num_threads {
        let mt = Arc::clone(&mt);
        handles.push(thread::spawn(move || {
            for i in 0..entries_per_thread {
                let key = format!("thread{}-key{:04}", t, i);
                let val = format!("value-{}", i);
                mt.put(key.as_bytes(), val.as_bytes());
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(mt.len(), num_threads * entries_per_thread);

    // Concurrent reads.
    let mut read_handles = Vec::new();
    for t in 0..num_threads {
        let mt = Arc::clone(&mt);
        read_handles.push(thread::spawn(move || {
            for i in 0..entries_per_thread {
                let key = format!("thread{}-key{:04}", t, i);
                let val = mt.get(key.as_bytes());
                assert!(val.is_some(), "missing: {}", key);
            }
        }));
    }

    for h in read_handles {
        h.join().unwrap();
    }
}

// ---------------------------------------------------------------------------
// Memtable: flush to RocksDB
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memtable_flush_to_rocksdb() {
    let (_dir, storage) = temp_storage();
    let mt = MemTable::new(1024 * 1024);

    // Put entries in memtable.
    for i in 0..10 {
        let key = format!("flush-key-{}", i);
        let val = format!("flush-val-{}", i);
        mt.put(key.as_bytes(), val.as_bytes());
    }
    assert_eq!(mt.len(), 10);

    // Flush to RocksDB.
    mt.flush(&storage, CF_METADATA).await.unwrap();
    assert_eq!(mt.len(), 0);

    // Verify data is in RocksDB.
    for i in 0..10 {
        let key = format!("flush-key-{}", i);
        let expected = format!("flush-val-{}", i);
        let val = storage.get(CF_METADATA, key.as_bytes()).await.unwrap();
        assert_eq!(val, Some(expected.into_bytes()));
    }
}

// ---------------------------------------------------------------------------
// List documents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_documents_with_pagination() {
    let (_dir, storage) = temp_storage();

    for i in 0..20 {
        let doc = Document::new(DocumentId::new(format!("list-{:02}", i)));
        storage.put_document(&doc).await.unwrap();
    }

    // First page.
    let page1 = storage.list_documents(0, 5).await.unwrap();
    assert_eq!(page1.len(), 5);

    // Second page.
    let page2 = storage.list_documents(5, 5).await.unwrap();
    assert_eq!(page2.len(), 5);

    // No overlap.
    assert_ne!(page1[0].id, page2[0].id);

    // Beyond end.
    let page_last = storage.list_documents(18, 10).await.unwrap();
    assert_eq!(page_last.len(), 2);
}

// ---------------------------------------------------------------------------
// Delete non-existent via trait returns NotFound
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_nonexistent_returns_not_found() {
    let (_dir, storage) = temp_storage();

    let result = StorageBackend::delete(&storage, &DocumentId::new("ghost")).await;
    assert!(matches!(result, Err(DbError::NotFound(_))));
}

// ---------------------------------------------------------------------------
// Get non-existent via trait returns NotFound
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_nonexistent_returns_not_found() {
    let (_dir, storage) = temp_storage();

    let result = StorageBackend::get(&storage, &DocumentId::new("nonexistent")).await;
    assert!(matches!(result, Err(DbError::NotFound(_))));
}

// ---------------------------------------------------------------------------
// Overwrite document
// ---------------------------------------------------------------------------

#[tokio::test]
async fn overwrite_existing_document() {
    let (_dir, storage) = temp_storage();

    let doc_v1 =
        Document::new(DocumentId::new("overwrite")).with_field("version", FieldValue::Number(1.0));
    StorageBackend::put(&storage, doc_v1).await.unwrap();

    let doc_v2 =
        Document::new(DocumentId::new("overwrite")).with_field("version", FieldValue::Number(2.0));
    StorageBackend::put(&storage, doc_v2).await.unwrap();

    let loaded = StorageBackend::get(&storage, &DocumentId::new("overwrite"))
        .await
        .unwrap();
    assert_eq!(loaded.get_field("version"), Some(&FieldValue::Number(2.0)));
}
