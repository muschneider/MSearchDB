//! Snapshot manager for MSearchDB.
//!
//! Coordinates the creation, compression, rotation, and restoration of
//! database snapshots. A snapshot consists of:
//!
//! 1. A RocksDB **checkpoint** (hard-linked point-in-time copy).
//! 2. A **tar archive** of the Tantivy index directory.
//!
//! Both are bundled into a single tar archive and compressed with **zstd**.
//!
//! # Directory layout
//!
//! ```text
//! data_dir/
//!   snapshots/
//!     <snapshot-id>.snap       — compressed archive
//!     <snapshot-id>.meta.json  — SnapshotInfo metadata
//! ```
//!
//! # Rotation
//!
//! Only the last `max_snapshots` (default 3) snapshots are retained.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::snapshot::{SnapshotConfig, SnapshotInfo};
use msearchdb_storage::rocksdb_backend::RocksDbStorage;

// ---------------------------------------------------------------------------
// SnapshotManager
// ---------------------------------------------------------------------------

/// Manages lifecycle of database snapshots.
///
/// Holds references to the storage engine and paths needed to create
/// consistent snapshots of both RocksDB and Tantivy data.
pub struct SnapshotManager {
    /// Base data directory (contains `storage/`, `index/`, `snapshots/`).
    data_dir: PathBuf,

    /// Reference to the RocksDB storage for creating checkpoints.
    storage: Arc<RocksDbStorage>,

    /// Snapshot configuration (retention count, chunk size).
    config: SnapshotConfig,
}

impl SnapshotManager {
    /// Create a new snapshot manager.
    pub fn new(
        data_dir: impl Into<PathBuf>,
        storage: Arc<RocksDbStorage>,
        config: SnapshotConfig,
    ) -> Self {
        Self {
            data_dir: data_dir.into(),
            storage,
            config,
        }
    }

    /// Return the directory where snapshots are stored.
    pub fn snapshots_dir(&self) -> PathBuf {
        self.data_dir.join("snapshots")
    }

    /// Ensure the snapshots directory exists.
    fn ensure_snapshots_dir(&self) -> DbResult<()> {
        let dir = self.snapshots_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Create a new snapshot and return its metadata.
    ///
    /// Steps:
    /// 1. Create a temporary directory for staging.
    /// 2. Create a RocksDB checkpoint in `staging/rocksdb/`.
    /// 3. Copy the Tantivy index directory into `staging/index/`.
    /// 4. Tar the staging directory and compress with zstd.
    /// 5. Write the compressed archive to `snapshots/<id>.snap`.
    /// 6. Write metadata to `snapshots/<id>.meta.json`.
    /// 7. Rotate old snapshots.
    pub fn create_snapshot(
        &self,
        term: u64,
        last_log_index: u64,
        doc_count: u64,
    ) -> DbResult<SnapshotInfo> {
        self.ensure_snapshots_dir()?;

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let snapshot_id = format!("{}-{}-{}", term, last_log_index, timestamp);

        let snapshots_dir = self.snapshots_dir();
        let staging_dir = snapshots_dir.join(format!("staging-{}", snapshot_id));

        // Clean up any leftover staging dir
        if staging_dir.exists() {
            fs::remove_dir_all(&staging_dir)?;
        }
        fs::create_dir_all(&staging_dir)?;

        // Step 1: RocksDB checkpoint
        let rocksdb_staging = staging_dir.join("rocksdb");
        self.storage.create_checkpoint(&rocksdb_staging)?;
        tracing::info!(snapshot_id = %snapshot_id, "RocksDB checkpoint created");

        // Step 2: Copy Tantivy index
        let index_source = self.data_dir.join("index");
        let index_staging = staging_dir.join("index");
        if index_source.exists() {
            copy_dir_recursive(&index_source, &index_staging)?;
            tracing::info!(snapshot_id = %snapshot_id, "Tantivy index copied");
        }

        // Step 3: Create tar and compress with zstd
        let snap_path = snapshots_dir.join(format!("{}.snap", snapshot_id));
        let snap_file =
            fs::File::create(&snap_path).map_err(|e| DbError::SnapshotError(e.to_string()))?;
        let zstd_encoder = zstd::Encoder::new(snap_file, 3)
            .map_err(|e| DbError::SnapshotError(format!("zstd encoder init: {}", e)))?;
        let mut tar_builder = tar::Builder::new(zstd_encoder);
        tar_builder
            .append_dir_all(".", &staging_dir)
            .map_err(|e| DbError::SnapshotError(format!("tar append: {}", e)))?;
        let zstd_encoder = tar_builder
            .into_inner()
            .map_err(|e| DbError::SnapshotError(format!("tar finish: {}", e)))?;
        zstd_encoder
            .finish()
            .map_err(|e| DbError::SnapshotError(format!("zstd finish: {}", e)))?;

        // Step 4: Clean up staging
        let _ = fs::remove_dir_all(&staging_dir);

        // Step 5: Compute size and write metadata
        let snap_size = fs::metadata(&snap_path).map(|m| m.len()).unwrap_or(0);
        let info = SnapshotInfo::new(&snapshot_id, term, last_log_index, snap_size, doc_count);

        let meta_path = snapshots_dir.join(format!("{}.meta.json", snapshot_id));
        let meta_json = serde_json::to_string_pretty(&info)
            .map_err(|e| DbError::SerializationError(e.to_string()))?;
        fs::write(&meta_path, meta_json)?;

        tracing::info!(
            snapshot_id = %snapshot_id,
            size_bytes = snap_size,
            doc_count = doc_count,
            "snapshot created successfully"
        );

        // Step 6: Rotate old snapshots
        self.rotate_snapshots()?;

        Ok(info)
    }

    /// List all available snapshots, sorted by creation time (newest first).
    pub fn list_snapshots(&self) -> DbResult<Vec<SnapshotInfo>> {
        let dir = self.snapshots_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut snapshots = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with(".meta.json"))
                    .unwrap_or(false)
            {
                let content = fs::read_to_string(&path)?;
                if let Ok(info) = serde_json::from_str::<SnapshotInfo>(&content) {
                    snapshots.push(info);
                }
            }
        }

        snapshots.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(snapshots)
    }

    /// Get the path to a snapshot archive by id.
    pub fn snapshot_path(&self, snapshot_id: &str) -> PathBuf {
        self.snapshots_dir().join(format!("{}.snap", snapshot_id))
    }

    /// Get snapshot info by id.
    pub fn get_snapshot(&self, snapshot_id: &str) -> DbResult<SnapshotInfo> {
        let meta_path = self
            .snapshots_dir()
            .join(format!("{}.meta.json", snapshot_id));
        if !meta_path.exists() {
            return Err(DbError::NotFound(format!(
                "snapshot '{}' not found",
                snapshot_id
            )));
        }
        let content = fs::read_to_string(&meta_path)?;
        serde_json::from_str(&content).map_err(|e| DbError::SerializationError(e.to_string()))
    }

    /// Restore the database from a snapshot archive.
    ///
    /// Steps:
    /// 1. Decompress the zstd archive.
    /// 2. Extract the tar contents to a temporary directory.
    /// 3. Replace `data_dir/storage/` with extracted `rocksdb/`.
    /// 4. Replace `data_dir/index/` with extracted `index/`.
    pub fn restore_from_path(&self, snapshot_path: &Path) -> DbResult<()> {
        if !snapshot_path.exists() {
            return Err(DbError::SnapshotError(format!(
                "snapshot file not found: {}",
                snapshot_path.display()
            )));
        }

        let restore_dir = self.data_dir.join("restore_staging");
        if restore_dir.exists() {
            fs::remove_dir_all(&restore_dir)?;
        }
        fs::create_dir_all(&restore_dir)?;

        // Decompress and extract
        let snap_file =
            fs::File::open(snapshot_path).map_err(|e| DbError::SnapshotError(e.to_string()))?;
        let zstd_decoder =
            zstd::Decoder::new(snap_file).map_err(|e| DbError::SnapshotError(e.to_string()))?;
        let mut tar_archive = tar::Archive::new(zstd_decoder);
        tar_archive
            .unpack(&restore_dir)
            .map_err(|e| DbError::SnapshotError(format!("tar unpack: {}", e)))?;

        // Replace storage directory
        let restored_rocksdb = restore_dir.join("rocksdb");
        let storage_dir = self.data_dir.join("storage");
        if restored_rocksdb.exists() {
            if storage_dir.exists() {
                fs::remove_dir_all(&storage_dir)?;
            }
            fs::rename(&restored_rocksdb, &storage_dir)?;
            tracing::info!("restored RocksDB storage from snapshot");
        }

        // Replace index directory
        let restored_index = restore_dir.join("index");
        let index_dir = self.data_dir.join("index");
        if restored_index.exists() {
            if index_dir.exists() {
                fs::remove_dir_all(&index_dir)?;
            }
            fs::rename(&restored_index, &index_dir)?;
            tracing::info!("restored Tantivy index from snapshot");
        }

        // Clean up
        let _ = fs::remove_dir_all(&restore_dir);

        tracing::info!(
            snapshot = %snapshot_path.display(),
            "database restored from snapshot"
        );

        Ok(())
    }

    /// Restore from an in-memory snapshot archive (bytes).
    ///
    /// Writes the bytes to a temporary file and delegates to
    /// [`restore_from_path`](Self::restore_from_path).
    pub fn restore_from_bytes(&self, data: &[u8]) -> DbResult<()> {
        self.ensure_snapshots_dir()?;
        let temp_path = self.snapshots_dir().join("_restore_temp.snap");
        fs::write(&temp_path, data)?;
        let result = self.restore_from_path(&temp_path);
        let _ = fs::remove_file(&temp_path);
        result
    }

    /// Read a snapshot file as bytes (for streaming via gRPC).
    pub fn read_snapshot_bytes(&self, snapshot_id: &str) -> DbResult<Vec<u8>> {
        let path = self.snapshot_path(snapshot_id);
        if !path.exists() {
            return Err(DbError::NotFound(format!(
                "snapshot file '{}' not found",
                snapshot_id
            )));
        }
        let mut file = fs::File::open(&path).map_err(|e| DbError::SnapshotError(e.to_string()))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| DbError::SnapshotError(e.to_string()))?;
        Ok(buf)
    }

    /// Delete old snapshots, keeping only the most recent `max_snapshots`.
    fn rotate_snapshots(&self) -> DbResult<()> {
        let snapshots = self.list_snapshots()?;
        if snapshots.len() <= self.config.max_snapshots {
            return Ok(());
        }

        // snapshots is sorted newest-first; remove the tail
        for old_snap in &snapshots[self.config.max_snapshots..] {
            let snap_path = self.snapshot_path(&old_snap.id);
            let meta_path = self
                .snapshots_dir()
                .join(format!("{}.meta.json", old_snap.id));
            if snap_path.exists() {
                let _ = fs::remove_file(&snap_path);
            }
            if meta_path.exists() {
                let _ = fs::remove_file(&meta_path);
            }
            tracing::info!(snapshot_id = %old_snap.id, "rotated old snapshot");
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &Path, dst: &Path) -> DbResult<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            // Skip the "collections" sub-indexes lock files and temp dirs
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use msearchdb_core::document::{Document, DocumentId, FieldValue};
    use msearchdb_core::traits::StorageBackend;
    use msearchdb_storage::rocksdb_backend::StorageOptions;

    fn setup_test_env(dir: &Path) -> (Arc<RocksDbStorage>, SnapshotManager) {
        let storage_dir = dir.join("storage");
        fs::create_dir_all(&storage_dir).unwrap();

        let storage =
            Arc::new(RocksDbStorage::new(&storage_dir, StorageOptions::default()).unwrap());

        let manager = SnapshotManager::new(dir, storage.clone(), SnapshotConfig::default());
        (storage, manager)
    }

    #[tokio::test]
    async fn create_and_list_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let (storage, manager) = setup_test_env(tmp.path());

        // Insert some documents via the StorageBackend trait
        let backend: &dyn StorageBackend = storage.as_ref();
        for i in 0..10 {
            let doc = Document::new(DocumentId::new(format!("doc-{}", i)))
                .with_field("title", FieldValue::Text(format!("Test {}", i)));
            backend.put(doc).await.unwrap();
        }

        // Create snapshot
        let info = manager.create_snapshot(1, 100, 10).unwrap();
        assert_eq!(info.term, 1);
        assert_eq!(info.last_log_index, 100);
        assert_eq!(info.doc_count, 10);
        assert!(info.size_bytes > 0);

        // List snapshots
        let snapshots = manager.list_snapshots().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].id, info.id);

        // Snapshot file exists
        let snap_path = manager.snapshot_path(&info.id);
        assert!(snap_path.exists());
    }

    #[tokio::test]
    async fn snapshot_rotation_keeps_max() {
        let tmp = tempfile::tempdir().unwrap();
        let config = SnapshotConfig {
            max_snapshots: 2,
            ..Default::default()
        };
        let storage_dir = tmp.path().join("storage");
        fs::create_dir_all(&storage_dir).unwrap();
        let storage =
            Arc::new(RocksDbStorage::new(&storage_dir, StorageOptions::default()).unwrap());
        let manager = SnapshotManager::new(tmp.path(), storage, config);

        // Create 4 snapshots
        for i in 0..4 {
            // Small delay so timestamps differ
            manager.create_snapshot(1, i * 10, 0).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let snapshots = manager.list_snapshots().unwrap();
        assert_eq!(snapshots.len(), 2);
    }

    #[tokio::test]
    async fn create_snapshot_and_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let storage_dir = data_dir.join("storage");

        // Phase 1: Create storage, insert docs, snapshot, read bytes
        let snap_bytes;
        {
            fs::create_dir_all(&storage_dir).unwrap();
            let storage =
                Arc::new(RocksDbStorage::new(&storage_dir, StorageOptions::default()).unwrap());

            let backend: &dyn StorageBackend = storage.as_ref();
            for i in 0..50 {
                let doc = Document::new(DocumentId::new(format!("doc-{:04}", i)))
                    .with_field("value", FieldValue::Number(i as f64));
                backend.put(doc).await.unwrap();
            }

            let manager =
                SnapshotManager::new(&data_dir, storage.clone(), SnapshotConfig::default());
            let info = manager.create_snapshot(1, 50, 50).unwrap();

            snap_bytes = manager.read_snapshot_bytes(&info.id).unwrap();
            assert!(!snap_bytes.is_empty());

            // Drop everything to release RocksDB lock
        }

        // Phase 2: Wipe and restore
        let _ = fs::remove_dir_all(&storage_dir);
        let _ = fs::remove_dir_all(data_dir.join("index"));

        // Create a temp storage for the manager constructor
        let temp_path = data_dir.join("temp_restore");
        fs::create_dir_all(&temp_path).unwrap();
        let temp_storage =
            Arc::new(RocksDbStorage::new(&temp_path, StorageOptions::default()).unwrap());
        let manager = SnapshotManager::new(&data_dir, temp_storage, SnapshotConfig::default());
        manager.restore_from_bytes(&snap_bytes).unwrap();
        drop(manager);
        let _ = fs::remove_dir_all(&temp_path);

        // Phase 3: Verify restored data
        let restored_storage =
            Arc::new(RocksDbStorage::new(&storage_dir, StorageOptions::default()).unwrap());
        let restored_backend: &dyn StorageBackend = restored_storage.as_ref();

        for i in 0..50 {
            let doc = restored_backend
                .get(&DocumentId::new(format!("doc-{:04}", i)))
                .await
                .unwrap();
            assert_eq!(doc.id, DocumentId::new(format!("doc-{:04}", i)));
            assert_eq!(doc.get_field("value"), Some(&FieldValue::Number(i as f64)));
        }
    }
}
