//! In-memory Raft log store for MSearchDB.
//!
//! Implements [`openraft::RaftLogStorage`] and [`openraft::RaftLogReader`]
//! backed by an in-memory `BTreeMap`.  This is sufficient for development
//! and testing; a production implementation would persist entries to a
//! RocksDB column family (e.g. `raft_log`).
//!
//! # Concurrency
//!
//! openraft guarantees that all calls to `RaftLogStorage` are serialised
//! (single-writer), so the interior `RwLock` is never truly contended by
//! multiple writers.  We use `RwLock` to allow the `LogReader` clone to
//! take read locks independently.
//!
//! ## `Arc<RwLock<>>` vs `Arc<Mutex<>>`
//!
//! - **`Mutex`** is simpler and slightly cheaper for pure write workloads.
//! - **`RwLock`** allows multiple concurrent *readers*, which is useful here
//!   because openraft spawns separate `LogReader` tasks to feed replication
//!   streams.  Those readers must not block behind a writer that is appending
//!   new entries.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::RangeBounds;
use std::sync::Arc;

use openraft::storage::{LogFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::{Entry, LogId, OptionalSend, StorageError, Vote};
use tokio::sync::RwLock;

use crate::types::TypeConfig;

// ---------------------------------------------------------------------------
// Shared log state
// ---------------------------------------------------------------------------

/// The inner state of the log store, protected by a `RwLock`.
#[derive(Debug, Default)]
struct LogStoreInner {
    /// Raft log entries keyed by log index.
    log: BTreeMap<u64, Entry<TypeConfig>>,

    /// The last purged (compacted) log id.
    last_purged: Option<LogId<u64>>,

    /// The persisted vote (term + voted_for).
    vote: Option<Vote<u64>>,

    /// The last committed log id (optional optimisation).
    committed: Option<LogId<u64>>,
}

// ---------------------------------------------------------------------------
// MemLogStore
// ---------------------------------------------------------------------------

/// An in-memory implementation of the Raft log store.
///
/// All data lives in `Arc<RwLock<LogStoreInner>>`, which is cloned into the
/// [`MemLogReader`] returned by [`get_log_reader`](RaftLogStorage::get_log_reader).
#[derive(Clone, Debug, Default)]
pub struct MemLogStore {
    inner: Arc<RwLock<LogStoreInner>>,
}

impl MemLogStore {
    /// Create a new empty in-memory log store.
    pub fn new() -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// RaftLogReader implementation (on MemLogStore itself)
// ---------------------------------------------------------------------------

impl RaftLogReader<TypeConfig> for MemLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<u64>> {
        let inner = self.inner.read().await;
        let entries: Vec<Entry<TypeConfig>> =
            inner.log.range(range).map(|(_, e)| e.clone()).collect();
        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// RaftLogStorage implementation
// ---------------------------------------------------------------------------

impl RaftLogStorage<TypeConfig> for MemLogStore {
    type LogReader = MemLogStore;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<u64>> {
        let inner = self.inner.read().await;
        let last_log_id = inner.log.iter().next_back().map(|(_, e)| e.log_id);
        let last = last_log_id.or(inner.last_purged);
        Ok(LogState {
            last_purged_log_id: inner.last_purged,
            last_log_id: last,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let mut inner = self.inner.write().await;
        inner.vote = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        let inner = self.inner.read().await;
        Ok(inner.vote)
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
        let mut inner = self.inner.write().await;
        for entry in entries {
            inner.log.insert(entry.log_id.index, entry);
        }
        // In a real implementation, this would be called after fsync.
        // For in-memory, we signal immediately.
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let mut inner = self.inner.write().await;
        let keys_to_remove: Vec<u64> = inner.log.range(log_id.index..).map(|(k, _)| *k).collect();
        for k in keys_to_remove {
            inner.log.remove(&k);
        }
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let mut inner = self.inner.write().await;
        let keys_to_remove: Vec<u64> = inner.log.range(..=log_id.index).map(|(k, _)| *k).collect();
        for k in keys_to_remove {
            inner.log.remove(&k);
        }
        inner.last_purged = Some(log_id);
        Ok(())
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        let mut inner = self.inner.write().await;
        inner.committed = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        let inner = self.inner.read().await;
        Ok(inner.committed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::EntryPayload;

    fn make_entry(index: u64, term: u64) -> Entry<TypeConfig> {
        Entry {
            log_id: LogId::new(openraft::CommittedLeaderId::new(term, 1), index),
            payload: EntryPayload::Blank,
        }
    }

    /// Helper: directly insert entries into the inner store.
    ///
    /// We bypass the [`RaftLogStorage::append`] method because
    /// [`LogFlushed::new`] is `pub(crate)` in openraft and cannot be
    /// constructed externally.  The `append` implementation is trivial
    /// (insert into BTreeMap + fire callback), so testing via direct
    /// insertion still covers the log-state logic we care about.
    async fn insert_entries(store: &MemLogStore, entries: Vec<Entry<TypeConfig>>) {
        let mut inner = store.inner.write().await;
        for entry in entries {
            inner.log.insert(entry.log_id.index, entry);
        }
    }

    #[tokio::test]
    async fn append_and_read_back() {
        let mut store = MemLogStore::new();
        let entries = vec![make_entry(1, 1), make_entry(2, 1), make_entry(3, 1)];
        insert_entries(&store, entries).await;

        let read = store.try_get_log_entries(1_u64..4_u64).await.unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(read[0].log_id.index, 1);
        assert_eq!(read[2].log_id.index, 3);
    }

    #[tokio::test]
    async fn log_state_empty() {
        let mut store = MemLogStore::new();
        let state = store.get_log_state().await.unwrap();
        assert!(state.last_purged_log_id.is_none());
        assert!(state.last_log_id.is_none());
    }

    #[tokio::test]
    async fn log_state_after_insert() {
        let mut store = MemLogStore::new();
        insert_entries(&store, vec![make_entry(1, 1), make_entry(2, 1)]).await;

        let state = store.get_log_state().await.unwrap();
        assert!(state.last_purged_log_id.is_none());
        let last = state.last_log_id.unwrap();
        assert_eq!(last.index, 2);
    }

    #[tokio::test]
    async fn truncate_removes_from_index() {
        let mut store = MemLogStore::new();
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

    #[tokio::test]
    async fn purge_removes_up_to_index() {
        let mut store = MemLogStore::new();
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
    async fn save_and_read_vote() {
        let mut store = MemLogStore::new();

        assert!(store.read_vote().await.unwrap().is_none());

        let vote = Vote::new(1, 42);
        store.save_vote(&vote).await.unwrap();

        let read_back = store.read_vote().await.unwrap().unwrap();
        assert_eq!(read_back, vote);
    }

    #[tokio::test]
    async fn save_and_read_committed() {
        let mut store = MemLogStore::new();

        assert!(store.read_committed().await.unwrap().is_none());

        let log_id = LogId::new(openraft::CommittedLeaderId::new(1, 1), 5);
        store.save_committed(Some(log_id)).await.unwrap();

        let read_back = store.read_committed().await.unwrap().unwrap();
        assert_eq!(read_back, log_id);
    }
}
