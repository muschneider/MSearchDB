//! In-memory write buffer (memtable) backed by a lock-free skip list.
//!
//! The [`MemTable`] accumulates key-value writes in memory before they are
//! flushed to the persistent RocksDB backend. It uses
//! [`crossbeam_skiplist::SkipMap`] for concurrent, lock-free reads and writes
//! without requiring a `Mutex`.
//!
//! # Thread Safety
//!
//! [`MemTable`] is `Send + Sync` and can be shared across async tasks via
//! [`Arc`]. The underlying skip list handles its own synchronization.
//!
//! # Examples
//!
//! ```no_run
//! use msearchdb_storage::memtable::MemTable;
//! use msearchdb_storage::rocksdb_backend::{RocksDbStorage, StorageOptions};
//! use std::path::Path;
//! use std::sync::Arc;
//!
//! # async fn example() -> msearchdb_core::error::DbResult<()> {
//! let storage = Arc::new(
//!     RocksDbStorage::new(Path::new("/tmp/db"), StorageOptions::default())?
//! );
//! let memtable = MemTable::new(64 * 1024 * 1024); // 64 MB capacity
//!
//! memtable.put(b"key-1", b"value-1");
//! assert_eq!(memtable.get(b"key-1"), Some(b"value-1".to_vec()));
//!
//! memtable.flush(&storage, "documents").await?;
//! assert_eq!(memtable.len(), 0);
//! # Ok(())
//! # }
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_skiplist::SkipMap;

use crate::rocksdb_backend::RocksDbStorage;
use msearchdb_core::error::DbResult;

// ---------------------------------------------------------------------------
// MemTable
// ---------------------------------------------------------------------------

/// In-memory write buffer using a lock-free skip list.
///
/// Entries are ordered by key, enabling efficient range scans. The memtable
/// tracks its approximate size in bytes and can be flushed to a
/// [`RocksDbStorage`] backend when it reaches capacity.
pub struct MemTable {
    /// Lock-free ordered map of key -> value.
    map: SkipMap<Vec<u8>, Vec<u8>>,
    /// Approximate total size of all keys and values in bytes.
    size_bytes: AtomicUsize,
    /// Maximum capacity in bytes before a flush should be triggered.
    capacity: usize,
}

impl MemTable {
    /// Create a new empty memtable with the given capacity (in bytes).
    pub fn new(capacity: usize) -> Self {
        Self {
            map: SkipMap::new(),
            size_bytes: AtomicUsize::new(0),
            capacity,
        }
    }

    /// Insert or update a key-value pair.
    ///
    /// This is a lock-free operation. The approximate size counter is updated
    /// atomically (it may slightly over-count if the key already existed).
    pub fn put(&self, key: &[u8], value: &[u8]) {
        let entry_size = key.len() + value.len();
        self.map.insert(key.to_vec(), value.to_vec());
        self.size_bytes.fetch_add(entry_size, Ordering::Relaxed);
    }

    /// Look up a value by key. Returns `None` if the key is not present.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.get(key).map(|entry| entry.value().clone())
    }

    /// Delete a key from the memtable. Returns `true` if the key existed.
    pub fn delete(&self, key: &[u8]) -> bool {
        if let Some(entry) = self.map.remove(key) {
            let freed = entry.key().len() + entry.value().len();
            self.size_bytes.fetch_sub(freed, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Return the number of entries in the memtable.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Return `true` if the memtable is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Return the approximate size in bytes of all entries.
    pub fn size_bytes(&self) -> usize {
        self.size_bytes.load(Ordering::Relaxed)
    }

    /// Return the configured capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Return `true` if the memtable has reached or exceeded its capacity.
    pub fn should_flush(&self) -> bool {
        self.size_bytes.load(Ordering::Relaxed) >= self.capacity
    }

    /// Flush all entries to RocksDB via batch writes, then clear the memtable.
    ///
    /// Each key-value pair is written to the specified column family.
    pub async fn flush(&self, storage: &RocksDbStorage, cf_name: &str) -> DbResult<()> {
        // Collect all entries (the skip list is concurrent-safe to iterate).
        let entries: Vec<(Vec<u8>, Vec<u8>)> = self
            .map
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();

        tracing::info!(
            entries = entries.len(),
            cf = cf_name,
            "flushing memtable to RocksDB"
        );

        for (key, value) in &entries {
            storage.put(cf_name, key, value).await?;
        }

        // Clear the memtable after successful flush.
        self.clear();

        Ok(())
    }

    /// Remove all entries and reset the size counter.
    pub fn clear(&self) {
        // Remove entries one at a time — SkipMap doesn't have a bulk clear.
        while self.map.pop_front().is_some() {}
        self.size_bytes.store(0, Ordering::Relaxed);
    }

    /// Iterate over all entries in sorted key order.
    ///
    /// Returns a `Vec` of `(key, value)` pairs.
    pub fn iter(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.map
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }
}

// MemTable is Send + Sync because SkipMap<Vec<u8>, Vec<u8>> is Send + Sync
// and AtomicUsize is Send + Sync. This is automatically derived.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_put_get_delete() {
        let mt = MemTable::new(1024);

        mt.put(b"key1", b"val1");
        mt.put(b"key2", b"val2");

        assert_eq!(mt.get(b"key1"), Some(b"val1".to_vec()));
        assert_eq!(mt.get(b"key2"), Some(b"val2".to_vec()));
        assert_eq!(mt.get(b"key3"), None);
        assert_eq!(mt.len(), 2);

        assert!(mt.delete(b"key1"));
        assert!(!mt.delete(b"key1")); // already deleted
        assert_eq!(mt.get(b"key1"), None);
        assert_eq!(mt.len(), 1);
    }

    #[test]
    fn size_tracking() {
        let mt = MemTable::new(1024);
        mt.put(b"abc", b"12345"); // 3 + 5 = 8
        assert_eq!(mt.size_bytes(), 8);

        mt.put(b"def", b"67"); // 3 + 2 = 5
        assert_eq!(mt.size_bytes(), 13);
    }

    #[test]
    fn should_flush_at_capacity() {
        let mt = MemTable::new(10);
        mt.put(b"aaaa", b"bbbb"); // 4+4 = 8
        assert!(!mt.should_flush());

        mt.put(b"cc", b"dd"); // 2+2 = 4, total ~12
        assert!(mt.should_flush());
    }

    #[test]
    fn clear_resets_everything() {
        let mt = MemTable::new(1024);
        mt.put(b"k", b"v");
        assert!(!mt.is_empty());

        mt.clear();
        assert!(mt.is_empty());
        assert_eq!(mt.len(), 0);
        assert_eq!(mt.size_bytes(), 0);
    }

    #[test]
    fn sorted_iteration() {
        let mt = MemTable::new(4096);
        mt.put(b"charlie", b"3");
        mt.put(b"alpha", b"1");
        mt.put(b"bravo", b"2");

        let entries = mt.iter();
        let keys: Vec<Vec<u8>> = entries.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(
            keys,
            vec![b"alpha".to_vec(), b"bravo".to_vec(), b"charlie".to_vec()]
        );
    }

    #[test]
    fn concurrent_put_and_read() {
        use std::sync::Arc;
        use std::thread;

        let mt = Arc::new(MemTable::new(1024 * 1024));
        let num_threads = 8;
        let entries_per_thread = 100;

        let mut handles = Vec::new();

        // Spawn writer threads.
        for t in 0..num_threads {
            let mt = Arc::clone(&mt);
            handles.push(thread::spawn(move || {
                for i in 0..entries_per_thread {
                    let key = format!("t{}-k{}", t, i);
                    let val = format!("v{}", i);
                    mt.put(key.as_bytes(), val.as_bytes());
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(mt.len(), num_threads * entries_per_thread);

        // Verify all entries can be read.
        for t in 0..num_threads {
            for i in 0..entries_per_thread {
                let key = format!("t{}-k{}", t, i);
                assert!(mt.get(key.as_bytes()).is_some(), "missing key: {}", key);
            }
        }
    }
}
