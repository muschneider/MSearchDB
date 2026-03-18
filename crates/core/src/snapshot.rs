//! Snapshot types for Raft snapshot and backup/restore operations.
//!
//! A [`SnapshotMeta`](SnapshotInfo) describes a point-in-time snapshot of the
//! database state (RocksDB checkpoint + Tantivy index), compressed with zstd
//! and stored under `data_dir/snapshots/`.
//!
//! # Lifecycle
//!
//! 1. **Create**: RocksDB `Checkpoint` + tar the Tantivy index directory,
//!    compress with zstd, write to `snapshots/<id>.snap`.
//! 2. **Rotate**: Keep the last `max_snapshots` (default 3), delete older ones.
//! 3. **Transfer**: Stream snapshot chunks via gRPC `InstallSnapshot`.
//! 4. **Restore**: Decompress, untar RocksDB checkpoint and Tantivy index,
//!    replace current data directories.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// SnapshotInfo
// ---------------------------------------------------------------------------

/// Metadata for a stored snapshot.
///
/// Serialized to JSON alongside the snapshot archive for identification
/// and ordering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    /// Unique snapshot identifier (e.g., `"1-42-1710000000"`).
    pub id: String,

    /// The Raft term at the time of the snapshot.
    pub term: u64,

    /// The Raft log index up to which this snapshot covers.
    pub last_log_index: u64,

    /// Unix timestamp (seconds) when the snapshot was created.
    pub created_at: u64,

    /// Size of the compressed snapshot file in bytes.
    pub size_bytes: u64,

    /// Number of documents captured in the snapshot.
    pub doc_count: u64,
}

impl SnapshotInfo {
    /// Create a new snapshot info with the given parameters.
    pub fn new(
        id: impl Into<String>,
        term: u64,
        last_log_index: u64,
        size_bytes: u64,
        doc_count: u64,
    ) -> Self {
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            id: id.into(),
            term,
            last_log_index,
            created_at,
            size_bytes,
            doc_count,
        }
    }
}

// ---------------------------------------------------------------------------
// SnapshotConfig
// ---------------------------------------------------------------------------

/// Configuration for snapshot behaviour.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotConfig {
    /// Maximum number of snapshots to retain on disk.
    pub max_snapshots: usize,

    /// Size of each chunk when streaming snapshots over gRPC (bytes).
    pub chunk_size: usize,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            max_snapshots: 3,
            chunk_size: 1024 * 1024, // 1 MB
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_info_new_sets_created_at() {
        let info = SnapshotInfo::new("snap-1", 1, 100, 1024, 50);
        assert_eq!(info.id, "snap-1");
        assert_eq!(info.term, 1);
        assert_eq!(info.last_log_index, 100);
        assert_eq!(info.size_bytes, 1024);
        assert_eq!(info.doc_count, 50);
        assert!(info.created_at > 0);
    }

    #[test]
    fn snapshot_info_serde_roundtrip() {
        let info = SnapshotInfo::new("snap-2", 3, 200, 2048, 100);
        let json = serde_json::to_string(&info).unwrap();
        let back: SnapshotInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn snapshot_config_defaults() {
        let cfg = SnapshotConfig::default();
        assert_eq!(cfg.max_snapshots, 3);
        assert_eq!(cfg.chunk_size, 1024 * 1024);
    }
}
