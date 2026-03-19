//! Application state shared across all HTTP handlers.
//!
//! [`AppState`] is the central container for backend services. It is wrapped
//! in [`Arc`] and passed to axum as router state via [`axum::Router::with_state`].
//!
//! # State vs Extension
//!
//! axum offers two ways to share data with handlers:
//!
//! - **[`State<T>`]**: compile-time typed, extracted from the router's shared
//!   state.  Preferred for data known at compile time.
//! - **[`Extension<T>`]**: runtime typed via `TypeMap`.  Used for per-request
//!   values injected by middleware (e.g., request id, auth context).
//!
//! MSearchDB uses `State<AppState>` for backend services and `Extension` for
//! per-request metadata like `RequestId`.
//!
//! [`State<T>`]: axum::extract::State
//! [`Extension<T>`]: axum::extract::Extension

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_core::cluster::NodeId;
use msearchdb_core::collection::{CollectionAlias, CollectionSettings};
use msearchdb_core::read_coordinator::ReadCoordinator;
use msearchdb_core::traits::{IndexBackend, StorageBackend};
use msearchdb_network::connection_pool::ConnectionPool;

use crate::cache::DocumentCache;
use crate::metrics::Metrics;
use crate::session::SessionManager;
use crate::snapshot_manager::SnapshotManager;
use crate::write_batcher::WriteBatcher;

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// Shared application state passed to every axum handler via [`axum::extract::State`].
///
/// All fields are behind `Arc` so that cloning the state is cheap (just
/// incrementing reference counts).
#[derive(Clone)]
pub struct AppState {
    /// The Raft consensus node for proposing write operations.
    pub raft_node: Arc<RaftNode>,

    /// The storage backend for direct reads.
    pub storage: Arc<dyn StorageBackend>,

    /// The search index backend for direct reads.
    pub index: Arc<dyn IndexBackend>,

    /// The gRPC connection pool for forwarding requests to peer nodes.
    pub connection_pool: Arc<ConnectionPool>,

    /// Map of collection names to their metadata.
    ///
    /// In a production system this would be persisted; here it is an in-memory
    /// registry protected by a read-write lock.
    pub collections: Arc<RwLock<HashMap<String, CollectionMeta>>>,

    /// Map of alias names to their definitions.
    ///
    /// Aliases map a single name to one or more backing collections.
    /// Queries against an alias fan out across all targets and merge results.
    pub aliases: Arc<RwLock<HashMap<String, CollectionAlias>>>,

    /// Optional API key for authentication.
    /// If `None`, authentication is disabled.
    pub api_key: Option<String>,

    /// Prometheus metrics registry.
    pub metrics: Arc<Metrics>,

    /// This node's unique identifier, used for vector clock increments.
    pub local_node_id: NodeId,

    /// Read coordinator for consistency-level-aware distributed reads.
    pub read_coordinator: Arc<ReadCoordinator>,

    /// Optional snapshot manager for backup/restore operations.
    pub snapshot_manager: Option<Arc<SnapshotManager>>,

    /// L1 per-node LRU document cache.
    ///
    /// Provides sub-microsecond reads for frequently accessed documents.
    /// Invalidated on every write and delete operation.
    pub document_cache: Arc<DocumentCache>,

    /// Session manager for read-your-writes consistency.
    ///
    /// Tracks the locally applied Raft log index so that clients with a
    /// session token can wait until their write is visible.
    pub session_manager: Arc<SessionManager>,

    /// Write batcher for amortised Raft proposal overhead.
    ///
    /// Buffers up to 100 documents or 10ms before flushing as a single
    /// Raft BatchInsert entry.
    pub write_batcher: Arc<WriteBatcher>,
}

/// Metadata for a single collection.
///
/// Includes the dynamic field mapping that tracks field names → types,
/// and the per-collection settings (shards, replication, analyzers).
/// This mapping is updated as new documents are indexed (schema evolution).
#[derive(Clone, Debug)]
pub struct CollectionMeta {
    /// Human-readable collection name.
    pub name: String,

    /// Number of documents indexed (approximate).
    pub doc_count: u64,

    /// Dynamic field mapping (field names → types).
    ///
    /// Updated each time a document is indexed with previously unseen fields.
    pub mapping: msearchdb_core::collection::FieldMapping,

    /// Per-collection settings (shards, replication factor, analyzers).
    ///
    /// Specified at creation time and immutable once set.
    pub settings: CollectionSettings,
}
