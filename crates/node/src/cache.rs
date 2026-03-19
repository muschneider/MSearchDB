//! Multi-level document cache for MSearchDB.
//!
//! # Architecture
//!
//! ```text
//! Read path:  L1 (moka LRU) → L2 (Tantivy segment cache) → Storage (RocksDB)
//! Write path: Invalidate L1 → Write storage → Index
//! ```
//!
//! ## L1: Per-node In-Memory LRU Cache
//!
//! An in-memory LRU cache backed by [`moka`] for hot-path document reads.
//! Configurable max capacity (default 10,000 entries) and TTL (default 60
//! seconds).  Entries are automatically evicted when either limit is reached.
//!
//! Cache invalidation occurs on every write (insert, update, delete) via
//! [`DocumentCache::invalidate`].  In a multi-node cluster, invalidation
//! messages should be propagated through the gossip protocol.
//!
//! ## L2: Tantivy Segment Cache
//!
//! Tantivy's own segment-level caching is configured at index open time
//! via [`TantivyIndex`] reader settings.  See the `index` crate for details.
//!
//! [`TantivyIndex`]: msearchdb_index::tantivy_index::TantivyIndex

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moka::future::Cache;

use msearchdb_core::document::{Document, DocumentId};

// ---------------------------------------------------------------------------
// CacheConfig
// ---------------------------------------------------------------------------

/// Configuration for the L1 document cache.
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Maximum number of entries in the cache.
    pub max_capacity: u64,

    /// Time-to-live for each cache entry.
    pub ttl: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_capacity: 10_000,
            ttl: Duration::from_secs(60),
        }
    }
}

// ---------------------------------------------------------------------------
// CacheKey
// ---------------------------------------------------------------------------

/// Composite cache key: `(collection, document_id)`.
///
/// We store the key as a single owned `String` of the form
/// `"<collection>\0<doc_id>"` to avoid extra allocations from tuples.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct CacheKey(String);

impl CacheKey {
    fn new(collection: &str, doc_id: &str) -> Self {
        let mut key = String::with_capacity(collection.len() + 1 + doc_id.len());
        key.push_str(collection);
        key.push('\0');
        key.push_str(doc_id);
        Self(key)
    }
}

// ---------------------------------------------------------------------------
// DocumentCache
// ---------------------------------------------------------------------------

/// L1 per-node LRU document cache backed by [`moka`].
///
/// Thread-safe and fully async.  Intended to be shared across all request
/// handlers via [`AppState`](crate::state::AppState).
///
/// # Invalidation
///
/// Every write or delete must call [`invalidate`](DocumentCache::invalidate)
/// to ensure subsequent reads see the latest version.  For multi-node
/// clusters, a cache invalidation message should be broadcast so that
/// other nodes evict their stale copies.
pub struct DocumentCache {
    inner: Cache<CacheKey, Document>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl DocumentCache {
    /// Create a new document cache with the given configuration.
    pub fn new(config: CacheConfig) -> Self {
        let inner = Cache::builder()
            .max_capacity(config.max_capacity)
            .time_to_live(config.ttl)
            .support_invalidation_closures()
            .build();
        Self {
            inner,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Create a cache with default settings (10,000 entries, 60s TTL).
    pub fn with_defaults() -> Self {
        Self::new(CacheConfig::default())
    }

    /// Attempt to read a document from the cache.
    ///
    /// Returns `Some(document)` on hit, `None` on miss.
    pub async fn get(&self, collection: &str, id: &DocumentId) -> Option<Document> {
        let key = CacheKey::new(collection, id.as_str());
        match self.inner.get(&key).await {
            Some(doc) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(doc)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Insert or update a document in the cache.
    pub async fn put(&self, collection: &str, doc: &Document) {
        let key = CacheKey::new(collection, doc.id.as_str());
        self.inner.insert(key, doc.clone()).await;
    }

    /// Invalidate (evict) a single document from the cache.
    ///
    /// Must be called on every write and delete operation.
    pub async fn invalidate(&self, collection: &str, id: &DocumentId) {
        let key = CacheKey::new(collection, id.as_str());
        self.inner.invalidate(&key).await;
    }

    /// Invalidate all entries for a given collection.
    ///
    /// Used when an entire collection is dropped.
    pub async fn invalidate_collection(&self, collection: &str) {
        // moka does not support prefix-based invalidation, so we use
        // `invalidate_entries_if` with a predicate.
        let prefix = {
            let mut s = String::with_capacity(collection.len() + 1);
            s.push_str(collection);
            s.push('\0');
            s
        };
        self.inner
            .invalidate_entries_if(move |k, _v| k.0.starts_with(&prefix))
            .expect("invalidate_entries_if should not fail");
    }

    /// Return the number of cache hits since startup.
    pub fn hit_count(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Return the number of cache misses since startup.
    pub fn miss_count(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Return the current number of entries in the cache.
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }

    /// Return the hit rate as a fraction in `[0.0, 1.0]`.
    pub fn hit_rate(&self) -> f64 {
        let hits = self.hits.load(Ordering::Relaxed) as f64;
        let misses = self.misses.load(Ordering::Relaxed) as f64;
        let total = hits + misses;
        if total == 0.0 {
            0.0
        } else {
            hits / total
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use msearchdb_core::document::FieldValue;

    fn make_doc(id: &str) -> Document {
        Document::new(DocumentId::new(id)).with_field("title", FieldValue::Text("test".into()))
    }

    #[tokio::test]
    async fn cache_put_and_get() {
        let cache = DocumentCache::with_defaults();
        let doc = make_doc("doc-1");

        cache.put("products", &doc).await;
        let cached = cache.get("products", &DocumentId::new("doc-1")).await;
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().id.as_str(), "doc-1");
    }

    #[tokio::test]
    async fn cache_miss_returns_none() {
        let cache = DocumentCache::with_defaults();
        let result = cache.get("products", &DocumentId::new("missing")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn cache_invalidate_evicts_entry() {
        let cache = DocumentCache::with_defaults();
        let doc = make_doc("doc-1");

        cache.put("products", &doc).await;
        cache
            .invalidate("products", &DocumentId::new("doc-1"))
            .await;
        let result = cache.get("products", &DocumentId::new("doc-1")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn cache_hit_miss_counters() {
        let cache = DocumentCache::with_defaults();
        let doc = make_doc("doc-1");

        cache.put("products", &doc).await;

        // 1 hit
        cache.get("products", &DocumentId::new("doc-1")).await;
        // 1 miss
        cache.get("products", &DocumentId::new("missing")).await;

        assert_eq!(cache.hit_count(), 1);
        assert_eq!(cache.miss_count(), 1);
        assert!((cache.hit_rate() - 0.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn cache_collection_isolation() {
        let cache = DocumentCache::with_defaults();
        let doc = make_doc("doc-1");

        cache.put("products", &doc).await;
        let result = cache.get("orders", &DocumentId::new("doc-1")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn cache_invalidate_collection() {
        let cache = DocumentCache::with_defaults();

        for i in 0..5 {
            let doc = make_doc(&format!("doc-{}", i));
            cache.put("products", &doc).await;
        }
        let doc_other = make_doc("other-1");
        cache.put("orders", &doc_other).await;

        cache.invalidate_collection("products").await;

        // All products entries should be gone.
        for i in 0..5 {
            let result = cache
                .get("products", &DocumentId::new(&format!("doc-{}", i)))
                .await;
            assert!(result.is_none());
        }

        // orders entry should still be present.
        let result = cache.get("orders", &DocumentId::new("other-1")).await;
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn cache_ttl_eviction() {
        let config = CacheConfig {
            max_capacity: 100,
            ttl: Duration::from_millis(50),
        };
        let cache = DocumentCache::new(config);
        let doc = make_doc("doc-1");

        cache.put("products", &doc).await;

        // Should be present immediately.
        assert!(cache
            .get("products", &DocumentId::new("doc-1"))
            .await
            .is_some());

        // Wait for TTL expiry.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Should be evicted.
        assert!(cache
            .get("products", &DocumentId::new("doc-1"))
            .await
            .is_none());
    }

    #[tokio::test]
    async fn cache_capacity_limit() {
        let config = CacheConfig {
            max_capacity: 3,
            ttl: Duration::from_secs(60),
        };
        let cache = DocumentCache::new(config);

        for i in 0..10 {
            let doc = make_doc(&format!("doc-{}", i));
            cache.put("c", &doc).await;
        }

        // moka may not immediately evict, but entry_count should be bounded.
        // Allow some slack for async eviction.
        tokio::time::sleep(Duration::from_millis(100)).await;
        cache.inner.run_pending_tasks().await;
        assert!(cache.entry_count() <= 5); // allow some slack
    }
}
