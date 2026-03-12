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
use msearchdb_core::read_coordinator::ReadCoordinator;
use msearchdb_core::traits::{IndexBackend, StorageBackend};
use msearchdb_network::connection_pool::ConnectionPool;

use crate::metrics::Metrics;

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

    /// Optional API key for authentication.
    /// If `None`, authentication is disabled.
    pub api_key: Option<String>,

    /// Prometheus metrics registry.
    pub metrics: Arc<Metrics>,

    /// This node's unique identifier, used for vector clock increments.
    pub local_node_id: NodeId,

    /// Read coordinator for consistency-level-aware distributed reads.
    pub read_coordinator: Arc<ReadCoordinator>,
}

/// Metadata for a single collection.
#[derive(Clone, Debug)]
pub struct CollectionMeta {
    /// Human-readable collection name.
    pub name: String,

    /// Number of documents indexed (approximate).
    pub doc_count: u64,
}
