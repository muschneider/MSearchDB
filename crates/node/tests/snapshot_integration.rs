//! Integration tests for snapshot creation, rotation, and restore.
//!
//! Tests the full lifecycle: write documents → snapshot → wipe → restore → verify.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::snapshot::SnapshotConfig;
use msearchdb_core::traits::StorageBackend;
use msearchdb_node::snapshot_manager::SnapshotManager;
use msearchdb_storage::rocksdb_backend::{RocksDbStorage, StorageOptions};

/// Helper: create a RocksDB storage at the given path.
fn open_storage(path: &Path) -> Arc<RocksDbStorage> {
    fs::create_dir_all(path).unwrap();
    Arc::new(RocksDbStorage::new(path, StorageOptions::default()).unwrap())
}

/// Helper: insert `count` documents using the StorageBackend trait.
async fn insert_docs(storage: &Arc<RocksDbStorage>, count: usize) {
    let backend: &dyn StorageBackend = storage.as_ref();
    for i in 0..count {
        let doc = Document::new(DocumentId::new(format!("doc-{:06}", i)))
            .with_field("value", FieldValue::Number(i as f64))
            .with_field("title", FieldValue::Text(format!("Document number {}", i)));
        backend.put(doc).await.unwrap();
    }
}

/// Helper: verify all documents from 0..count exist with correct values.
async fn verify_docs(storage: &Arc<RocksDbStorage>, count: usize) {
    let backend: &dyn StorageBackend = storage.as_ref();
    for i in 0..count {
        let id = DocumentId::new(format!("doc-{:06}", i));
        let doc = backend.get(&id).await.unwrap();
        assert_eq!(doc.id, id);
        assert_eq!(
            doc.get_field("value"),
            Some(&FieldValue::Number(i as f64)),
            "document doc-{:06} has wrong value",
            i
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_create_and_list() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let storage = open_storage(&data_dir.join("storage"));

    insert_docs(&storage, 100).await;

    let manager = SnapshotManager::new(&data_dir, storage, SnapshotConfig::default());
    let info = manager.create_snapshot(1, 100, 100).unwrap();

    assert_eq!(info.term, 1);
    assert_eq!(info.last_log_index, 100);
    assert_eq!(info.doc_count, 100);
    assert!(info.size_bytes > 0);

    let list = manager.list_snapshots().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, info.id);
}

#[tokio::test]
async fn snapshot_get_by_id() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let storage = open_storage(&data_dir.join("storage"));

    let manager = SnapshotManager::new(&data_dir, storage, SnapshotConfig::default());
    let info = manager.create_snapshot(1, 50, 0).unwrap();

    let fetched = manager.get_snapshot(&info.id).unwrap();
    assert_eq!(fetched.id, info.id);
    assert_eq!(fetched.term, 1);

    // Non-existent snapshot
    let missing = manager.get_snapshot("nonexistent");
    assert!(missing.is_err());
}

#[tokio::test]
async fn snapshot_rotation_keeps_max() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let storage = open_storage(&data_dir.join("storage"));

    let config = SnapshotConfig {
        max_snapshots: 2,
        ..Default::default()
    };
    let manager = SnapshotManager::new(&data_dir, storage, config);

    // Create 5 snapshots with small delays to get unique timestamps
    for i in 0..5 {
        manager.create_snapshot(1, (i + 1) * 10, 0).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let list = manager.list_snapshots().unwrap();
    assert_eq!(list.len(), 2, "should keep only max_snapshots=2");

    // The kept snapshots should be the two most recent
    assert!(list[0].last_log_index >= list[1].last_log_index);
}

#[tokio::test]
async fn snapshot_restore_recovers_all_docs() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let storage_path = data_dir.join("storage");

    // Phase 1: Insert documents and create snapshot
    let doc_count = 1000;
    {
        let storage = open_storage(&storage_path);
        insert_docs(&storage, doc_count).await;

        let manager = SnapshotManager::new(&data_dir, storage.clone(), SnapshotConfig::default());
        let info = manager
            .create_snapshot(1, doc_count as u64, doc_count as u64)
            .unwrap();

        // Verify snapshot file exists
        let snap_path = manager.snapshot_path(&info.id);
        assert!(snap_path.exists());

        // Read snapshot bytes for later restore
        let bytes = manager.read_snapshot_bytes(&info.id).unwrap();
        assert!(!bytes.is_empty());

        // Drop storage to release RocksDB lock
        drop(storage);

        // Wipe storage directory
        fs::remove_dir_all(&storage_path).unwrap();
        assert!(!storage_path.exists());

        // Restore from bytes
        manager.restore_from_bytes(&bytes).unwrap();
    }

    // Phase 2: Re-open and verify
    {
        let storage = open_storage(&storage_path);
        verify_docs(&storage, doc_count).await;
    }
}

#[tokio::test]
async fn snapshot_100k_docs_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let storage_path = data_dir.join("storage");

    let doc_count = 100_000;

    // Phase 1: Bulk insert 100k docs
    let snapshot_id;
    {
        let storage = open_storage(&storage_path);
        let backend: &dyn StorageBackend = storage.as_ref();

        // Insert in batches for performance
        for batch_start in (0..doc_count).step_by(1000) {
            let batch_end = std::cmp::min(batch_start + 1000, doc_count);
            for i in batch_start..batch_end {
                let doc = Document::new(DocumentId::new(format!("doc-{:06}", i)))
                    .with_field("value", FieldValue::Number(i as f64))
                    .with_field("title", FieldValue::Text(format!("Document number {}", i)));
                backend.put(doc).await.unwrap();
            }
        }

        // Create snapshot
        let manager = SnapshotManager::new(&data_dir, storage.clone(), SnapshotConfig::default());
        let info = manager
            .create_snapshot(1, doc_count as u64, doc_count as u64)
            .unwrap();
        snapshot_id = info.id.clone();

        assert!(info.size_bytes > 0);
        assert_eq!(info.doc_count, doc_count as u64);

        // Drop storage (releases RocksDB lock)
        drop(storage);
    }

    // Phase 2: Wipe data directory and restore
    {
        fs::remove_dir_all(&storage_path).unwrap();

        let snap_path = data_dir
            .join("snapshots")
            .join(format!("{}.snap", snapshot_id));
        assert!(snap_path.exists(), "snapshot file should survive wipe");

        // Create a minimal storage for the manager constructor (not used for restore)
        let temp_storage_path = data_dir.join("temp_storage_for_restore");
        fs::create_dir_all(&temp_storage_path).unwrap();
        let temp_storage =
            Arc::new(RocksDbStorage::new(&temp_storage_path, StorageOptions::default()).unwrap());
        let manager = SnapshotManager::new(&data_dir, temp_storage, SnapshotConfig::default());

        manager.restore_from_path(&snap_path).unwrap();

        // Clean up temp storage
        drop(manager);
        let _ = fs::remove_dir_all(&temp_storage_path);
    }

    // Phase 3: Verify ALL 100k documents are present
    {
        let storage = open_storage(&storage_path);
        let backend: &dyn StorageBackend = storage.as_ref();

        // Spot check some documents across the range
        let check_indices = [0, 1, 100, 999, 10_000, 50_000, 99_999];
        for &i in &check_indices {
            let id = DocumentId::new(format!("doc-{:06}", i));
            let doc = backend.get(&id).await.unwrap();
            assert_eq!(doc.id, id);
            assert_eq!(
                doc.get_field("value"),
                Some(&FieldValue::Number(i as f64)),
                "document doc-{:06} has wrong value after restore",
                i
            );
        }

        // Full count verification: scan all documents
        let all_docs = backend
            .scan(
                DocumentId::new("\0")..=DocumentId::new("\u{10ffff}"),
                usize::MAX,
            )
            .await
            .unwrap();

        assert_eq!(
            all_docs.len(),
            doc_count,
            "expected {} documents after restore, got {}",
            doc_count,
            all_docs.len()
        );
    }
}

#[tokio::test]
async fn snapshot_read_bytes_and_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let storage_path = data_dir.join("storage");

    // Phase 1: Insert and snapshot
    let bytes;
    {
        let storage = open_storage(&storage_path);
        insert_docs(&storage, 500).await;
        let manager = SnapshotManager::new(&data_dir, storage.clone(), SnapshotConfig::default());
        let info = manager.create_snapshot(2, 500, 500).unwrap();

        // Read as bytes (simulates gRPC transfer)
        bytes = manager.read_snapshot_bytes(&info.id).unwrap();
        assert!(!bytes.is_empty());

        // Drop storage and manager to release RocksDB lock
        drop(storage);
    }

    // Phase 2: Wipe and restore
    fs::remove_dir_all(&storage_path).unwrap();

    // Create a temp storage for the manager constructor (needed but not used for restore)
    let temp_path = data_dir.join("temp_restore_storage");
    fs::create_dir_all(&temp_path).unwrap();
    let temp_storage = open_storage(&temp_path);
    let manager = SnapshotManager::new(&data_dir, temp_storage, SnapshotConfig::default());
    manager.restore_from_bytes(&bytes).unwrap();
    drop(manager);
    let _ = fs::remove_dir_all(&temp_path);

    // Phase 3: Verify
    let storage = open_storage(&storage_path);
    verify_docs(&storage, 500).await;
}

#[tokio::test]
async fn snapshot_nonexistent_restore_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let storage_path = data_dir.join("storage");
    let storage = open_storage(&storage_path);

    let manager = SnapshotManager::new(&data_dir, storage, SnapshotConfig::default());

    let result = manager.restore_from_path(Path::new("/nonexistent/path.snap"));
    assert!(result.is_err());
}
