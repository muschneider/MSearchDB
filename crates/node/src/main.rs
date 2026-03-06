//! # msearchdb-node
//!
//! MSearchDB database node binary.  This is the main entry point for running
//! a single database node in a cluster.
//!
//! The node starts:
//! 1. A Raft consensus node for write replication.
//! 2. An HTTP REST API server on the configured port.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_core::config::NodeConfig;
use msearchdb_core::traits::{IndexBackend, StorageBackend};
use msearchdb_index::schema_builder::SchemaConfig;
use msearchdb_index::tantivy_index::TantivyIndex;
use msearchdb_network::connection_pool::ConnectionPool;
use msearchdb_node::state::AppState;
use openraft::BasicNode;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("MSearchDB node starting...");

    let config = NodeConfig::default();
    tracing::info!(
        node_id = %config.node_id,
        http_port = config.http_port,
        grpc_port = config.grpc_port,
        "loaded configuration"
    );

    // Initialise storage backend (in-memory Tantivy index for now)
    let index: Arc<dyn IndexBackend> = Arc::new(
        TantivyIndex::new_in_ram(SchemaConfig::default()).expect("failed to create tantivy index"),
    );

    // Initialise a simple in-memory storage backend for the binary.
    // In production this would be RocksDbStorage.
    let storage: Arc<dyn StorageBackend> = Arc::new(InMemoryStorage::new());

    // Create the Raft node
    let raft_node = RaftNode::new(&config, storage.clone(), index.clone())
        .await
        .expect("failed to create raft node");
    let raft_node = Arc::new(raft_node);

    // Initialise as single-node cluster
    let mut members = BTreeMap::new();
    members.insert(config.node_id.as_u64(), BasicNode::default());
    if let Err(e) = raft_node.initialize(members).await {
        tracing::warn!("raft init (may already be initialised): {}", e);
    }

    // Build application state
    let state = AppState {
        raft_node,
        storage,
        index,
        connection_pool: Arc::new(ConnectionPool::new()),
        collections: Arc::new(RwLock::new(std::collections::HashMap::new())),
        api_key: std::env::var("MSEARCHDB_API_KEY").ok(),
    };

    // Build the router
    let app = msearchdb_node::build_router(state);

    // Start the HTTP server
    let addr = format!("0.0.0.0:{}", config.http_port);
    tracing::info!(addr = %addr, "HTTP server listening");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind TCP listener");

    axum::serve(listener, app).await.expect("HTTP server error");
}

// ---------------------------------------------------------------------------
// In-memory storage for the binary (mirrors the one in raft_node.rs)
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use msearchdb_core::document::{Document, DocumentId};
use msearchdb_core::error::{DbError, DbResult};
use std::collections::HashMap;
use std::ops::RangeInclusive;
use tokio::sync::Mutex;

struct InMemoryStorage {
    docs: Mutex<HashMap<String, Document>>,
}

impl InMemoryStorage {
    fn new() -> Self {
        Self {
            docs: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl StorageBackend for InMemoryStorage {
    async fn get(&self, id: &DocumentId) -> DbResult<Document> {
        let docs = self.docs.lock().await;
        docs.get(id.as_str())
            .cloned()
            .ok_or_else(|| DbError::NotFound(format!("document '{}' not found", id)))
    }

    async fn put(&self, document: Document) -> DbResult<()> {
        let mut docs = self.docs.lock().await;
        docs.insert(document.id.as_str().to_owned(), document);
        Ok(())
    }

    async fn delete(&self, id: &DocumentId) -> DbResult<()> {
        let mut docs = self.docs.lock().await;
        docs.remove(id.as_str())
            .map(|_| ())
            .ok_or_else(|| DbError::NotFound(format!("document '{}' not found", id)))
    }

    async fn scan(
        &self,
        _range: RangeInclusive<DocumentId>,
        _limit: usize,
    ) -> DbResult<Vec<Document>> {
        let docs = self.docs.lock().await;
        Ok(docs.values().cloned().collect())
    }
}
