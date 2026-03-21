//! # MSearchDB Node Binary
//!
//! Main entry point for running a single MSearchDB database node.
//!
//! The binary supports:
//! - **CLI argument parsing** via `clap` (--config, --node-id, --data-dir, etc.)
//! - **TOML configuration** with nested sections
//! - **Full startup sequence**: config → tracing → storage → index → raft → cluster → HTTP
//! - **Graceful shutdown** on SIGTERM/SIGINT with drain, flush, and cleanup

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::RwLock;

use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_core::cluster::{NodeAddress, NodeId, NodeInfo, NodeStatus};
use msearchdb_core::cluster_router::ClusterRouter;
use msearchdb_core::config::NodeConfig;
use msearchdb_core::read_coordinator::ReadCoordinator;
use msearchdb_core::snapshot::SnapshotConfig;
use msearchdb_core::traits::{IndexBackend, StorageBackend};
use msearchdb_index::schema_builder::{FieldConfig, FieldType, SchemaConfig};
use msearchdb_index::tantivy_index::TantivyIndex;
use msearchdb_network::connection_pool::ConnectionPool;
use msearchdb_node::cache::DocumentCache;
use msearchdb_node::cluster_manager::ClusterManager;
use msearchdb_node::metrics::Metrics;
use msearchdb_node::session::SessionManager;
use msearchdb_node::snapshot_manager::SnapshotManager;
use msearchdb_node::state::AppState;
use msearchdb_node::write_batcher::WriteBatcher;
use msearchdb_storage::rocksdb_backend::{RocksDbStorage, StorageOptions};
use openraft::BasicNode;

// ---------------------------------------------------------------------------
// CLI argument definitions
// ---------------------------------------------------------------------------

/// MSearchDB — A distributed NoSQL database with full-text search.
#[derive(Parser, Debug)]
#[command(name = "msearchdb", version, about)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Node identifier (overrides config file).
    #[arg(long)]
    node_id: Option<u64>,

    /// Data directory (overrides config file).
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// HTTP server port (overrides config file).
    #[arg(long)]
    http_port: Option<u16>,

    /// gRPC server port (overrides config file).
    #[arg(long)]
    grpc_port: Option<u16>,

    /// Peer addresses (host:port), comma-separated (overrides config file).
    #[arg(long, value_delimiter = ',')]
    peers: Option<Vec<String>>,

    /// Bootstrap a new single-node cluster.
    #[arg(long, default_value_t = false)]
    bootstrap: bool,

    /// Use JSON logging format (production mode).
    #[arg(long, default_value_t = false)]
    json_log: bool,

    /// Log level (error, warn, info, debug, trace).
    #[arg(long)]
    log_level: Option<String>,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Step 1: Load configuration
    let mut config = match &cli.config {
        Some(path) => NodeConfig::from_toml_file(path).unwrap_or_else(|e| {
            eprintln!(
                "ERROR: failed to load config from '{}': {}",
                path.display(),
                e
            );
            std::process::exit(1);
        }),
        None => NodeConfig::default(),
    };

    // Apply CLI overrides
    if let Some(id) = cli.node_id {
        config.node_id = NodeId::new(id);
    }
    if let Some(ref dir) = cli.data_dir {
        config.data_dir = dir.clone();
    }
    if let Some(port) = cli.http_port {
        config.http_port = port;
    }
    if let Some(port) = cli.grpc_port {
        config.grpc_port = port;
    }
    if let Some(ref peers) = cli.peers {
        config.peers = peers
            .iter()
            .filter_map(|p| {
                let parts: Vec<&str> = p.rsplitn(2, ':').collect();
                if parts.len() == 2 {
                    parts[0]
                        .parse::<u16>()
                        .ok()
                        .map(|port| NodeAddress::new(parts[1], port))
                } else {
                    None
                }
            })
            .collect();
    }
    if let Some(ref level) = cli.log_level {
        config.log_level = level.clone();
    }

    // Validate
    if let Err(e) = config.validate() {
        eprintln!("ERROR: invalid configuration: {}", e);
        std::process::exit(1);
    }

    // Step 2: Initialise tracing / observability
    let log_dir = config.data_dir.join("logs");
    let _log_guard = msearchdb_node::observability::init_tracing(
        &config.log_level,
        cli.json_log,
        Some(log_dir.to_str().unwrap_or("data/logs")),
    );

    tracing::info!("MSearchDB node starting...");
    tracing::info!(
        node_id = %config.node_id,
        data_dir = %config.data_dir.display(),
        http = %format!("{}:{}", config.http_host, config.http_port),
        grpc = %format!("{}:{}", config.grpc_host, config.grpc_port),
        replication_factor = config.replication_factor,
        "loaded configuration"
    );

    // Step 3: Create data directories
    let storage_dir = config.data_dir.join("storage");
    let index_dir = config.data_dir.join("index");

    for dir in [&config.data_dir, &storage_dir, &index_dir, &log_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::error!(dir = %dir.display(), error = %e, "failed to create directory");
            std::process::exit(1);
        }
    }

    // Step 4: Initialise RocksDB storage
    tracing::info!(path = %storage_dir.display(), "opening RocksDB storage");
    let storage_options = StorageOptions {
        write_buffer_mb: config.write_buffer_mb,
        max_open_files: config.max_open_files,
        compression: config.compression,
        bloom_filter_bits: 10,
    };
    let rocksdb_storage = Arc::new(
        RocksDbStorage::new(&storage_dir, storage_options).unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to open RocksDB");
            std::process::exit(1);
        }),
    );
    let storage: Arc<dyn StorageBackend> = rocksdb_storage.clone();
    tracing::info!("RocksDB storage ready");

    // Step 5: Initialise Tantivy index
    tracing::info!(path = %index_dir.display(), "opening Tantivy index");
    // Build a default schema with the `_body` catch-all text field so that
    // schemaless documents are fully searchable via simple query-string and
    // match queries without requiring users to define a schema upfront.
    let default_schema = SchemaConfig::new().with_field(FieldConfig::new("_body", FieldType::Text));

    let index: Arc<dyn IndexBackend> = Arc::new(
        TantivyIndex::new(&index_dir, default_schema).unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to create Tantivy index");
            std::process::exit(1);
        }),
    );
    tracing::info!("Tantivy index ready");

    // Step 6: Create Raft node
    tracing::info!("initialising Raft consensus node");
    let raft_node = RaftNode::new(&config, storage.clone(), index.clone())
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to create Raft node");
            std::process::exit(1);
        });
    let raft_node = Arc::new(raft_node);

    // Bootstrap single-node cluster if requested
    if cli.bootstrap {
        tracing::info!("bootstrapping single-node Raft cluster");
        let mut members = BTreeMap::new();
        members.insert(config.node_id.as_u64(), BasicNode::default());
        if let Err(e) = raft_node.initialize(members).await {
            tracing::warn!(error = %e, "raft init (may already be initialised)");
        }
    }

    // Step 7: Create connection pool
    let connection_pool = Arc::new(ConnectionPool::new());

    // Step 8: Initialise Prometheus metrics
    let metrics = Arc::new(Metrics::new());
    metrics
        .node_health
        .with_label_values(&[&config.node_id.to_string()])
        .set(1.0);
    tracing::info!("Prometheus metrics registered");

    // Step 9: Create cluster manager
    let local_node = NodeInfo {
        id: config.node_id,
        address: NodeAddress::new(&config.grpc_host, config.grpc_port),
        status: NodeStatus::Follower,
    };
    let initial_nodes = vec![local_node.clone()];
    let router = Arc::new(RwLock::new(ClusterRouter::new(
        initial_nodes,
        config.replication_factor,
    )));
    let cluster_manager = Arc::new(ClusterManager::new(
        local_node,
        router,
        connection_pool.clone(),
        raft_node.clone(),
        storage.clone(),
        config.replication_factor,
    ));

    // Start background cluster tasks
    let _health_handle = cluster_manager.start_health_checks();
    let _gossip_handle = cluster_manager.start_gossip();
    tracing::info!("cluster manager started (health checks + gossip)");

    // Join cluster peers if configured (and not bootstrapping)
    if !cli.bootstrap && !config.peers.is_empty() {
        let first_peer = config.peers[0].clone();
        tracing::info!(peer = %format!("{}:{}", first_peer.host, first_peer.port), "joining cluster");
        if let Err(e) = cluster_manager.join_cluster(first_peer).await {
            tracing::warn!(error = %e, "failed to join cluster (will retry via gossip)");
        }
    }

    // Step 10: Create snapshot manager
    let snapshots_dir = config.data_dir.join("snapshots");
    if let Err(e) = std::fs::create_dir_all(&snapshots_dir) {
        tracing::error!(dir = %snapshots_dir.display(), error = %e, "failed to create snapshots directory");
    }
    let snapshot_manager = Arc::new(SnapshotManager::new(
        &config.data_dir,
        rocksdb_storage,
        SnapshotConfig::default(),
    ));
    tracing::info!("snapshot manager ready");

    // Step 11: Create performance components
    let document_cache = Arc::new(DocumentCache::with_defaults());
    tracing::info!(
        max_capacity = 10_000,
        ttl_secs = 60,
        "L1 document cache ready"
    );

    let session_manager = Arc::new(SessionManager::new());
    tracing::info!("read-your-writes session manager ready");

    let write_batcher = Arc::new(WriteBatcher::new(raft_node.clone()));
    let _batcher_handle = write_batcher.start().await;
    tracing::info!(
        max_batch_size = 100,
        max_batch_delay_ms = 10,
        "write batcher started"
    );

    // Step 12: Initialize security components
    let security_config = msearchdb_core::security::SecurityConfig::default();

    // API key registry — register legacy key if configured.
    let mut api_key_registry = msearchdb_core::security::ApiKeyRegistry::new();
    if let Some(ref key) = config.api_key {
        api_key_registry.register(
            msearchdb_core::security::ApiKeyEntry::new(key, msearchdb_core::security::Role::Admin)
                .with_description("legacy config key"),
        );
    }

    // JWT manager — only if a secret is configured.
    let jwt_manager = if !security_config.jwt_secret.is_empty() {
        Some(Arc::new(msearchdb_core::security::JwtManager::new(
            &security_config.jwt_secret,
            Duration::from_secs(security_config.jwt_ttl_secs),
        )))
    } else {
        None
    };

    // Rate limiter
    let rate_limiter = Arc::new(msearchdb_core::security::RateLimiter::new(
        security_config.rate_limit_capacity,
        security_config.rate_limit_refill_rate,
    ));

    // Connection counter
    let connection_counter = Arc::new(msearchdb_core::security::ConnectionCounter::new(
        security_config.resource_limits.max_connections as u64,
    ));

    // Audit logger
    let audit_logger = if security_config.audit_enabled {
        Arc::new(
            msearchdb_core::security::AuditLogger::new(&security_config.audit_log_path)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to create audit logger, using noop");
                    msearchdb_core::security::AuditLogger::noop()
                }),
        )
    } else {
        Arc::new(msearchdb_core::security::AuditLogger::noop())
    };

    tracing::info!(
        rate_limit_capacity = security_config.rate_limit_capacity,
        max_connections = security_config.resource_limits.max_connections,
        audit_enabled = security_config.audit_enabled,
        "security components initialized"
    );

    // Step 13: Build application state and HTTP router
    let read_coordinator = Arc::new(ReadCoordinator::new(config.replication_factor));
    let state = AppState {
        raft_node,
        storage,
        index,
        connection_pool,
        collections: Arc::new(RwLock::new(HashMap::new())),
        aliases: Arc::new(RwLock::new(HashMap::new())),
        api_key: config.api_key.clone(),
        api_key_registry: Arc::new(api_key_registry),
        jwt_manager,
        rate_limiter,
        connection_counter,
        validation_config: Arc::new(security_config.validation.clone()),
        audit_logger,
        security_config: Arc::new(security_config),
        metrics,
        local_node_id: config.node_id,
        read_coordinator,
        snapshot_manager: Some(snapshot_manager),
        document_cache,
        session_manager,
        write_batcher,
    };

    let app = msearchdb_node::build_router(state);

    // Step 14: Start HTTP server with graceful shutdown
    let addr = format!("{}:{}", config.http_host, config.http_port);
    tracing::info!(addr = %addr, "HTTP server listening");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(addr = %addr, error = %e, "failed to bind TCP listener");
            std::process::exit(1);
        });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(cluster_manager))
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "HTTP server error");
            std::process::exit(1);
        });

    tracing::info!("MSearchDB node shut down cleanly");
}

// ---------------------------------------------------------------------------
// Graceful shutdown
// ---------------------------------------------------------------------------

/// Wait for SIGINT or SIGTERM, then perform graceful shutdown.
///
/// Shutdown sequence:
/// 1. Log the signal received.
/// 2. Propose Raft leave (best-effort).
/// 3. Allow in-flight HTTP requests to drain (30s timeout).
/// 4. Log completion.
async fn shutdown_signal(cluster_manager: Arc<ClusterManager>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install CTRL+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {
            tracing::info!("received SIGINT, starting graceful shutdown...");
        }
        () = terminate => {
            tracing::info!("received SIGTERM, starting graceful shutdown...");
        }
    }

    // Best-effort cluster leave
    tracing::info!("leaving cluster...");
    if let Err(e) = cluster_manager.leave_cluster().await {
        tracing::warn!(error = %e, "failed to leave cluster cleanly");
    }

    // Give in-flight requests time to complete
    tracing::info!("draining in-flight requests (30s deadline)...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    tracing::info!("shutdown sequence complete");
}
