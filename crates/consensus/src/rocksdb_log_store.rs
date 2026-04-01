//! Durable RocksDB-backed Raft log store for MSearchDB.
//!
//! Implements [`openraft::RaftLogStorage`] and [`openraft::RaftLogReader`]
//! with all state persisted to a dedicated RocksDB instance at
//! `{data_dir}/raft-log/`.  This guarantees that Raft log entries, the
//! persisted vote, and the committed index survive process crashes.
//!
//! # Storage layout
//!
//! | Key pattern          | Value                          |
//! |----------------------|--------------------------------|
//! | `log:{index:020}`    | JSON-encoded `Entry<TypeConfig>`|
//! | `__vote__`           | JSON-encoded `Vote<u64>`       |
//! | `__committed__`      | JSON-encoded `LogId<u64>`      |
//! | `__last_purged__`    | JSON-encoded `LogId<u64>`      |

use std::fmt::Debug;
use std::io;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Arc;

use openraft::storage::{LogFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::{Entry, LogId, OptionalSend, StorageError, StorageIOError, Vote};
use tokio::sync::RwLock;

use crate::types::TypeConfig;

/// Create an [`io::Error`] from a message string for use as a
/// [`StorageIOError`] source.
fn io_err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

// ---------------------------------------------------------------------------
// Special keys
// ---------------------------------------------------------------------------

const VOTE_KEY: &[u8] = b"__vote__";
const COMMITTED_KEY: &[u8] = b"__committed__";
const LAST_PURGED_KEY: &[u8] = b"__last_purged__";

/// Format a log index into a fixed-width key for ordered iteration.
fn log_key(index: u64) -> Vec<u8> {
    format!("log:{:020}", index).into_bytes()
}

/// Parse a log key back to its index.
fn parse_log_key(key: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(key).ok()?;
    s.strip_prefix("log:")?.parse().ok()
}

// ---------------------------------------------------------------------------
// RocksDbLogStore
// ---------------------------------------------------------------------------

/// A durable Raft log store backed by a dedicated RocksDB instance.
///
/// All writes use RocksDB's WAL for crash safety.  The store is wrapped in
/// a `RwLock` to satisfy openraft's `&mut self` contract while allowing
/// the cloned `LogReader` to take read locks independently.
#[derive(Clone)]
pub struct RocksDbLogStore {
    db: Arc<RwLock<rocksdb::DB>>,
}

impl Debug for RocksDbLogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksDbLogStore").finish()
    }
}

#[allow(clippy::result_large_err)]
impl RocksDbLogStore {
    /// Open (or create) a RocksDB instance at the given path.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, StorageError<u64>> {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.set_write_buffer_size(4 * 1024 * 1024); // 4 MB
        opts.set_max_write_buffer_number(2);

        let db = rocksdb::DB::open(&opts, path.as_ref()).map_err(|e| {
            StorageIOError::write_logs(&io_err(format!(
                "failed to open raft-log RocksDB: {}",
                e
            )))
        })?;

        Ok(Self {
            db: Arc::new(RwLock::new(db)),
        })
    }
}

// ---------------------------------------------------------------------------
// RaftLogReader
// ---------------------------------------------------------------------------

#[allow(clippy::result_large_err)]
impl RaftLogReader<TypeConfig> for RocksDbLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>> {
        let db = self.db.read().await;

        let start = match range.start_bound() {
            std::ops::Bound::Included(&s) => s,
            std::ops::Bound::Excluded(&s) => s + 1,
            std::ops::Bound::Unbounded => 0,
        };

        let end = match range.end_bound() {
            std::ops::Bound::Included(&e) => Some(e + 1),
            std::ops::Bound::Excluded(&e) => Some(e),
            std::ops::Bound::Unbounded => None,
        };

        let start_key = log_key(start);
        let iter = db.iterator(rocksdb::IteratorMode::From(
            &start_key,
            rocksdb::Direction::Forward,
        ));

        let mut entries = Vec::new();
        for item in iter {
            let (key, value) = item.map_err(|e| {
                StorageIOError::read_logs(&io_err(format!("RocksDB iterator error: {}", e)))
            })?;

            if let Some(idx) = parse_log_key(&key) {
                if let Some(end_idx) = end {
                    if idx >= end_idx {
                        break;
                    }
                }
                if idx < start {
                    continue;
                }
                let entry: Entry<TypeConfig> =
                    serde_json::from_slice(&value).map_err(|e| {
                        StorageIOError::read_logs(&io_err(format!(
                            "failed to deserialize log entry {}: {}",
                            idx, e
                        )))
                    })?;
                entries.push(entry);
            } else {
                break;
            }
        }

        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// RaftLogStorage
// ---------------------------------------------------------------------------

#[allow(clippy::result_large_err)]
impl RaftLogStorage<TypeConfig> for RocksDbLogStore {
    type LogReader = RocksDbLogStore;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<u64>> {
        let db = self.db.read().await;

        let last_purged: Option<LogId<u64>> = db
            .get(LAST_PURGED_KEY)
            .map_err(|e| {
                StorageIOError::read_logs(&io_err(format!("failed to read last_purged: {}", e)))
            })?
            .map(|v| {
                serde_json::from_slice(&v).map_err(|e| {
                    StorageIOError::read_logs(&io_err(format!(
                        "failed to deserialize last_purged: {}",
                        e
                    )))
                })
            })
            .transpose()?;

        let last_log_id = {
            let mut iter = db.iterator(rocksdb::IteratorMode::End);
            let mut found = None;
            while let Some(Ok((key, value))) = iter.next() {
                if parse_log_key(&key).is_some() {
                    let entry: Entry<TypeConfig> =
                        serde_json::from_slice(&value).map_err(|e| {
                            StorageIOError::read_logs(&io_err(format!(
                                "failed to deserialize last log entry: {}",
                                e
                            )))
                        })?;
                    found = Some(entry.log_id);
                    break;
                }
            }
            found
        };

        let last = last_log_id.or(last_purged);

        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id: last,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let db = self.db.write().await;
        let value = serde_json::to_vec(vote).map_err(|e| {
            StorageIOError::write_vote(&io_err(format!("failed to serialize vote: {}", e)))
        })?;
        db.put(VOTE_KEY, value).map_err(|e| {
            StorageIOError::write_vote(&io_err(format!("failed to write vote: {}", e)))
        })?;
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        let db = self.db.read().await;
        match db.get(VOTE_KEY).map_err(|e| {
            StorageIOError::read_vote(&io_err(format!("failed to read vote: {}", e)))
        })? {
            Some(v) => {
                let vote: Vote<u64> = serde_json::from_slice(&v).map_err(|e| {
                    StorageIOError::read_vote(&io_err(format!(
                        "failed to deserialize vote: {}",
                        e
                    )))
                })?;
                Ok(Some(vote))
            }
            None => Ok(None),
        }
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let db = self.db.write().await;
        let mut batch = rocksdb::WriteBatch::default();

        for entry in entries {
            let key = log_key(entry.log_id.index);
            let value = serde_json::to_vec(&entry).map_err(|e| {
                StorageIOError::write_logs(&io_err(format!(
                    "failed to serialize log entry: {}",
                    e
                )))
            })?;
            batch.put(key, value);
        }

        db.write(batch).map_err(|e| {
            StorageIOError::write_logs(&io_err(format!("failed to write log batch: {}", e)))
        })?;

        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let db = self.db.write().await;

        let start_key = log_key(log_id.index);
        let end_key = log_key(u64::MAX);
        let mut batch = rocksdb::WriteBatch::default();

        let iter = db.iterator(rocksdb::IteratorMode::From(
            &start_key,
            rocksdb::Direction::Forward,
        ));

        for item in iter {
            let (key, _) = item.map_err(|e| {
                StorageIOError::write_logs(&io_err(format!(
                    "iterator error during truncate: {}",
                    e
                )))
            })?;
            if key.as_ref() > end_key.as_slice() {
                break;
            }
            if parse_log_key(&key).is_some() {
                batch.delete(&key);
            } else {
                break;
            }
        }

        db.write(batch).map_err(|e| {
            StorageIOError::write_logs(&io_err(format!("failed to truncate logs: {}", e)))
        })?;

        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let db = self.db.write().await;

        let start_key = log_key(0);
        let end_key = log_key(log_id.index);
        let mut batch = rocksdb::WriteBatch::default();

        let iter = db.iterator(rocksdb::IteratorMode::From(
            &start_key,
            rocksdb::Direction::Forward,
        ));

        for item in iter {
            let (key, _) = item.map_err(|e| {
                StorageIOError::write_logs(&io_err(format!(
                    "iterator error during purge: {}",
                    e
                )))
            })?;
            if key.as_ref() > end_key.as_slice() {
                break;
            }
            if parse_log_key(&key).is_some() {
                batch.delete(&key);
            } else {
                break;
            }
        }

        let purged_value = serde_json::to_vec(&log_id).map_err(|e| {
            StorageIOError::write_logs(&io_err(format!(
                "failed to serialize last_purged: {}",
                e
            )))
        })?;
        batch.put(LAST_PURGED_KEY, purged_value);

        db.write(batch).map_err(|e| {
            StorageIOError::write_logs(&io_err(format!("failed to purge logs: {}", e)))
        })?;

        Ok(())
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        let db = self.db.write().await;
        match committed {
            Some(log_id) => {
                let value = serde_json::to_vec(&log_id).map_err(|e| {
                    StorageIOError::write_logs(&io_err(format!(
                        "failed to serialize committed: {}",
                        e
                    )))
                })?;
                db.put(COMMITTED_KEY, value).map_err(|e| {
                    StorageIOError::write_logs(&io_err(format!(
                        "failed to write committed: {}",
                        e
                    )))
                })?;
            }
            None => {
                let _ = db.delete(COMMITTED_KEY);
            }
        }
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        let db = self.db.read().await;
        match db.get(COMMITTED_KEY).map_err(|e| {
            StorageIOError::read_logs(&io_err(format!("failed to read committed: {}", e)))
        })? {
            Some(v) => {
                let log_id: LogId<u64> = serde_json::from_slice(&v).map_err(|e| {
                    StorageIOError::read_logs(&io_err(format!(
                        "failed to deserialize committed: {}",
                        e
                    )))
                })?;
                Ok(Some(log_id))
            }
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::EntryPayload;
    use tempfile::TempDir;

    fn make_entry(index: u64, term: u64) -> Entry<TypeConfig> {
        Entry {
            log_id: LogId::new(openraft::CommittedLeaderId::new(term, 1), index),
            payload: EntryPayload::Blank,
        }
    }

    fn make_store() -> (RocksDbLogStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = RocksDbLogStore::new(dir.path()).unwrap();
        (store, dir)
    }

    /// Helper: directly insert entries into the RocksDB store.
    async fn insert_entries(store: &RocksDbLogStore, entries: Vec<Entry<TypeConfig>>) {
        let db = store.db.write().await;
        for entry in &entries {
            let key = log_key(entry.log_id.index);
            let value = serde_json::to_vec(entry).unwrap();
            db.put(key, value).unwrap();
        }
    }

    #[tokio::test]
    async fn log_state_empty() {
        let (mut store, _dir) = make_store();
        let state = store.get_log_state().await.unwrap();
        assert!(state.last_purged_log_id.is_none());
        assert!(state.last_log_id.is_none());
    }

    #[tokio::test]
    async fn save_and_read_vote() {
        let (mut store, _dir) = make_store();
        assert!(store.read_vote().await.unwrap().is_none());

        let vote = Vote::new(1, 42);
        store.save_vote(&vote).await.unwrap();

        let read_back = store.read_vote().await.unwrap().unwrap();
        assert_eq!(read_back, vote);
    }

    #[tokio::test]
    async fn save_and_read_committed() {
        let (mut store, _dir) = make_store();
        assert!(store.read_committed().await.unwrap().is_none());

        let log_id = LogId::new(openraft::CommittedLeaderId::new(1, 1), 5);
        store.save_committed(Some(log_id)).await.unwrap();

        let read_back = store.read_committed().await.unwrap().unwrap();
        assert_eq!(read_back, log_id);
    }

    #[tokio::test]
    async fn append_and_read_entries() {
        let (mut store, _dir) = make_store();
        let entries = vec![make_entry(1, 1), make_entry(2, 1), make_entry(3, 1)];
        insert_entries(&store, entries).await;

        let read = store.try_get_log_entries(1_u64..4_u64).await.unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(read[0].log_id.index, 1);
        assert_eq!(read[2].log_id.index, 3);
    }

    #[tokio::test]
    async fn log_state_after_insert() {
        let (mut store, _dir) = make_store();
        insert_entries(&store, vec![make_entry(1, 1), make_entry(2, 1)]).await;

        let state = store.get_log_state().await.unwrap();
        assert!(state.last_purged_log_id.is_none());
        let last = state.last_log_id.unwrap();
        assert_eq!(last.index, 2);
    }

    #[tokio::test]
    async fn purge_removes_up_to_index() {
        let (mut store, _dir) = make_store();
        insert_entries(
            &store,
            vec![make_entry(1, 1), make_entry(2, 1), make_entry(3, 1)],
        )
        .await;

        let purge_id = LogId::new(openraft::CommittedLeaderId::new(1, 1), 2);
        store.purge(purge_id).await.unwrap();

        let entries = store.try_get_log_entries(1_u64..10_u64).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].log_id.index, 3);

        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id, Some(purge_id));
    }

    #[tokio::test]
    async fn truncate_removes_from_index() {
        let (mut store, _dir) = make_store();
        insert_entries(
            &store,
            vec![make_entry(1, 1), make_entry(2, 1), make_entry(3, 1)],
        )
        .await;

        store
            .truncate(LogId::new(openraft::CommittedLeaderId::new(1, 1), 2))
            .await
            .unwrap();

        let entries = store.try_get_log_entries(1_u64..10_u64).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].log_id.index, 1);
    }
}
