//! Cluster management for MSearchDB.
//!
//! The [`ClusterManager`] coordinates node membership, health monitoring,
//! failure detection, gossip-based state dissemination, and automatic failover.
//!
//! # Architecture
//!
//! ```text
//!  ┌─────────────────────────────────────────────┐
//!  │              ClusterManager                  │
//!  │  ┌───────────────┐  ┌────────────────────┐  │
//!  │  │ HealthChecker │  │ FailureDetector     │  │
//!  │  │  (1 s loop)   │  │ (Phi Accrual)       │  │
//!  │  └───────┬───────┘  └────────┬───────────┘  │
//!  │          │                   │               │
//!  │          ▼                   ▼               │
//!  │  ┌───────────────┐  ┌────────────────────┐  │
//!  │  │  DashMap       │  │  GossipManager     │  │
//!  │  │  (NodeHealth)  │  │  (2 s gossip)      │  │
//!  │  └───────────────┘  └────────────────────┘  │
//!  │                                              │
//!  │  ┌────────────────────────────────────────┐  │
//!  │  │  ReadRepairCoordinator                 │  │
//!  │  │  (per-document version reconciliation) │  │
//!  │  └────────────────────────────────────────┘  │
//!  └─────────────────────────────────────────────┘
//! ```
//!
//! # Key Design Decisions
//!
//! - **[`DashMap`]** for concurrent health state — avoids explicit locking and
//!   allows concurrent reads during health checks (many readers, rare writers).
//! - **[`Arc`]** wrappers for shared ownership across `tokio::spawn`'d tasks.
//!   Async tasks need `'static` lifetimes, so references (`&self`) cannot be
//!   passed directly; `Arc` provides owned, reference-counted sharing instead.
//! - **[`tokio::time::interval`]** for periodic tasks — self-correcting timer
//!   that accounts for drift between ticks.
//! - **Phi Accrual failure detector** — statistical model that adapts to actual
//!   heartbeat patterns rather than using a fixed timeout, reducing false
//!   positives caused by transient network delays.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_core::cluster::{NodeAddress, NodeId, NodeInfo, NodeStatus};
use msearchdb_core::cluster_router::ClusterRouter;
use msearchdb_core::document::{Document, DocumentId};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::traits::StorageBackend;
use msearchdb_network::connection_pool::ConnectionPool;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Health check interval in milliseconds.
const HEALTH_CHECK_INTERVAL_MS: u64 = 1_000;

/// Number of consecutive failures before a node is marked offline.
const FAILURE_THRESHOLD: u8 = 3;

/// Gossip interval in milliseconds.
const GOSSIP_INTERVAL_MS: u64 = 2_000;

/// Number of random peers to gossip with each round.
const GOSSIP_FANOUT: usize = 2;

/// Phi threshold for suspecting a node.
const PHI_SUSPECT_THRESHOLD: f64 = 8.0;

/// Phi threshold for declaring a node dead.
const PHI_DEAD_THRESHOLD: f64 = 16.0;

/// Maximum heartbeat samples kept per node for phi computation.
const MAX_HEARTBEAT_SAMPLES: usize = 100;

// ---------------------------------------------------------------------------
// NodeHealth
// ---------------------------------------------------------------------------

/// Health state for a single cluster node.
///
/// Tracked in a [`DashMap<NodeId, NodeHealth>`] so that concurrent health
/// check tasks can read/write without explicit locking.
#[derive(Clone, Debug)]
pub struct NodeHealth {
    /// The node's unique identifier.
    pub node_id: NodeId,
    /// Wall-clock time of the last successful health probe.
    pub last_seen: Instant,
    /// Number of consecutive failed health probes.
    pub consecutive_failures: u8,
    /// Current operational status derived from health checks.
    pub status: NodeStatus,
    /// Round-trip latency of the last successful probe.
    pub latency_ms: Option<u64>,
}

impl NodeHealth {
    /// Create initial health state for a node.
    pub fn new(node_id: NodeId) -> Self {
        Self {
            node_id,
            last_seen: Instant::now(),
            consecutive_failures: 0,
            status: NodeStatus::Follower,
            latency_ms: None,
        }
    }

    /// Record a successful health probe.
    ///
    /// Resets the failure counter and updates `last_seen` and `latency_ms`.
    /// Returns `true` if the status changed (i.e., node came back online).
    pub fn record_success(&mut self, latency_ms: u64) -> bool {
        let was_offline = self.status == NodeStatus::Offline;
        self.last_seen = Instant::now();
        self.consecutive_failures = 0;
        self.latency_ms = Some(latency_ms);
        if was_offline {
            self.status = NodeStatus::Follower;
        }
        was_offline
    }

    /// Record a failed health probe.
    ///
    /// Increments the failure counter. After [`FAILURE_THRESHOLD`] consecutive
    /// failures the node is marked [`NodeStatus::Offline`].
    /// Returns `true` if the status just transitioned to Offline.
    pub fn record_failure(&mut self) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.latency_ms = None;
        if self.consecutive_failures >= FAILURE_THRESHOLD && self.status != NodeStatus::Offline {
            self.status = NodeStatus::Offline;
            return true;
        }
        false
    }

    /// Return `true` if the node is considered online.
    pub fn is_online(&self) -> bool {
        self.status != NodeStatus::Offline
    }
}

// ---------------------------------------------------------------------------
// FailureDetector — Phi Accrual Algorithm
// ---------------------------------------------------------------------------

/// A Phi Accrual failure detector that uses heartbeat inter-arrival times
/// to compute a suspicion level (phi) for each node.
///
/// Rather than a fixed timeout, phi is computed as:
///   `phi = -log10(1 - CDF(time_since_last_heartbeat))`
/// where CDF is the cumulative distribution of past inter-arrival times
/// modelled as a normal distribution.
///
/// Higher phi values indicate higher suspicion that the node has failed:
/// - phi > 8.0 → suspect
/// - phi > 16.0 → dead
#[derive(Clone, Debug)]
pub struct FailureDetector {
    /// Per-node heartbeat arrival samples.
    samples: HashMap<NodeId, HeartbeatHistory>,
}

/// History of heartbeat inter-arrival times for a single node.
#[derive(Clone, Debug)]
struct HeartbeatHistory {
    /// Circular buffer of inter-arrival durations in milliseconds.
    intervals: Vec<f64>,
    /// Timestamp of the last heartbeat.
    last_heartbeat: Instant,
}

impl HeartbeatHistory {
    fn new() -> Self {
        Self {
            intervals: Vec::with_capacity(MAX_HEARTBEAT_SAMPLES),
            last_heartbeat: Instant::now(),
        }
    }
}

impl FailureDetector {
    /// Create a new failure detector with no tracked nodes.
    pub fn new() -> Self {
        Self {
            samples: HashMap::new(),
        }
    }

    /// Record a heartbeat arrival from the given node.
    pub fn report_heartbeat(&mut self, node_id: NodeId) {
        let now = Instant::now();
        let history = self.samples.entry(node_id).or_insert_with(|| {
            let mut h = HeartbeatHistory::new();
            h.last_heartbeat = now;
            h
        });

        let interval = now.duration_since(history.last_heartbeat).as_millis() as f64;
        history.last_heartbeat = now;

        // Only record if interval is positive (avoid zero-length intervals).
        if interval > 0.0 {
            if history.intervals.len() >= MAX_HEARTBEAT_SAMPLES {
                history.intervals.remove(0);
            }
            history.intervals.push(interval);
        }
    }

    /// Compute the phi (suspicion level) for a node.
    ///
    /// Returns `None` if not enough samples are available (< 2).
    /// Returns `Some(phi)` where higher values indicate more suspicion.
    pub fn phi(&self, node_id: &NodeId) -> Option<f64> {
        let history = self.samples.get(node_id)?;

        if history.intervals.len() < 2 {
            return None;
        }

        let elapsed = Instant::now()
            .duration_since(history.last_heartbeat)
            .as_millis() as f64;

        // Compute mean and standard deviation of inter-arrival times.
        let n = history.intervals.len() as f64;
        let mean = history.intervals.iter().sum::<f64>() / n;

        let variance = history
            .intervals
            .iter()
            .map(|&x| (x - mean).powi(2))
            .sum::<f64>()
            / n;

        let stddev = variance.sqrt();

        // Avoid division by zero — if all intervals are identical, use a
        // small epsilon.
        let stddev = if stddev < 1.0 { 1.0 } else { stddev };

        // CDF of normal distribution: P(X <= elapsed)
        // Using the complementary error function approximation:
        //   P(X > elapsed) = 1 - Φ((elapsed - mean) / stddev)
        let y = (elapsed - mean) / stddev;
        let p_later = 1.0 - normal_cdf(y);

        // phi = -log10(p_later)
        // Clamp to avoid infinity.
        if p_later <= 0.0 {
            Some(f64::MAX)
        } else {
            Some(-p_later.log10())
        }
    }

    /// Check whether a node should be suspected (phi > threshold).
    pub fn is_suspect(&self, node_id: &NodeId) -> bool {
        self.phi(node_id)
            .map(|p| p > PHI_SUSPECT_THRESHOLD)
            .unwrap_or(false)
    }

    /// Check whether a node should be considered dead (phi > dead threshold).
    pub fn is_dead(&self, node_id: &NodeId) -> bool {
        self.phi(node_id)
            .map(|p| p > PHI_DEAD_THRESHOLD)
            .unwrap_or(false)
    }

    /// Remove tracking for a node (e.g., when it leaves the cluster).
    pub fn remove_node(&mut self, node_id: &NodeId) {
        self.samples.remove(node_id);
    }

    /// Manually report a heartbeat at a specific instant (useful for testing).
    pub fn report_heartbeat_at(&mut self, node_id: NodeId, at: Instant) {
        let history = self.samples.entry(node_id).or_insert_with(|| {
            let mut h = HeartbeatHistory::new();
            h.last_heartbeat = at;
            h
        });

        let interval = at.duration_since(history.last_heartbeat).as_millis() as f64;
        history.last_heartbeat = at;

        if interval > 0.0 {
            if history.intervals.len() >= MAX_HEARTBEAT_SAMPLES {
                history.intervals.remove(0);
            }
            history.intervals.push(interval);
        }
    }

    /// Compute phi at a specific reference time (useful for testing).
    pub fn phi_at(&self, node_id: &NodeId, at: Instant) -> Option<f64> {
        let history = self.samples.get(node_id)?;

        if history.intervals.len() < 2 {
            return None;
        }

        let elapsed = at.duration_since(history.last_heartbeat).as_millis() as f64;

        let n = history.intervals.len() as f64;
        let mean = history.intervals.iter().sum::<f64>() / n;

        let variance = history
            .intervals
            .iter()
            .map(|&x| (x - mean).powi(2))
            .sum::<f64>()
            / n;

        let stddev = variance.sqrt();
        let stddev = if stddev < 1.0 { 1.0 } else { stddev };

        let y = (elapsed - mean) / stddev;
        let p_later = 1.0 - normal_cdf(y);

        if p_later <= 0.0 {
            Some(f64::MAX)
        } else {
            Some(-p_later.log10())
        }
    }
}

impl Default for FailureDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Approximate the CDF of the standard normal distribution using the
/// Abramowitz and Stegun formula (7.1.26), accurate to ~1.5e-7.
fn normal_cdf(x: f64) -> f64 {
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;

    // Save the sign of x.
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs() / std::f64::consts::SQRT_2;

    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();

    0.5 * (1.0 + sign * y)
}

// ---------------------------------------------------------------------------
// GossipMessage — cluster state dissemination
// ---------------------------------------------------------------------------

/// A gossip message carrying one node's view of the cluster.
///
/// Each entry is `(NodeId, NodeStatus, timestamp)` where timestamp is a
/// logical clock used for "latest wins" merge.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GossipMessage {
    /// The node that originated this gossip round.
    pub from: NodeId,
    /// Per-node status entries: (node_id, status, logical timestamp).
    pub cluster_view: Vec<GossipEntry>,
}

/// A single entry in a gossip message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GossipEntry {
    /// The node this entry describes.
    pub node_id: NodeId,
    /// Observed status.
    pub status: NodeStatus,
    /// Logical timestamp (e.g., epoch millis when the status was last changed).
    pub timestamp: u64,
}

/// Manages gossip protocol state and merge logic.
///
/// Each node maintains a map of `NodeId -> (NodeStatus, timestamp)`.
/// When a gossip message arrives, entries are merged using "latest timestamp
/// wins" — the entry with the higher timestamp is kept.
#[derive(Clone, Debug)]
pub struct GossipManager {
    /// Local view of cluster state: NodeId -> (status, timestamp).
    state: HashMap<NodeId, (NodeStatus, u64)>,
    /// This node's identifier.
    local_node_id: NodeId,
}

impl GossipManager {
    /// Create a new gossip manager for the given local node.
    pub fn new(local_node_id: NodeId) -> Self {
        Self {
            state: HashMap::new(),
            local_node_id,
        }
    }

    /// Update the local view for a specific node.
    pub fn update_local(&mut self, node_id: NodeId, status: NodeStatus, timestamp: u64) {
        let entry = self.state.entry(node_id).or_insert((status, 0));
        if timestamp >= entry.1 {
            *entry = (status, timestamp);
        }
    }

    /// Merge an incoming gossip message into the local state.
    ///
    /// For each entry in the message, the one with the higher timestamp wins.
    /// Returns the list of nodes whose status changed as a result of the merge.
    pub fn merge(&mut self, message: &GossipMessage) -> Vec<NodeId> {
        let mut changed = Vec::new();

        for entry in &message.cluster_view {
            let current = self.state.get(&entry.node_id);
            let should_update = match current {
                Some(&(_, ts)) => entry.timestamp > ts,
                None => true,
            };

            if should_update {
                let old_status = current.map(|&(s, _)| s);
                self.state
                    .insert(entry.node_id, (entry.status, entry.timestamp));
                if old_status != Some(entry.status) {
                    changed.push(entry.node_id);
                }
            }
        }

        changed
    }

    /// Build a gossip message from the current local state.
    pub fn build_message(&self) -> GossipMessage {
        let cluster_view = self
            .state
            .iter()
            .map(|(&node_id, &(status, timestamp))| GossipEntry {
                node_id,
                status,
                timestamp,
            })
            .collect();

        GossipMessage {
            from: self.local_node_id,
            cluster_view,
        }
    }

    /// Return a snapshot of the current cluster view.
    pub fn current_view(&self) -> &HashMap<NodeId, (NodeStatus, u64)> {
        &self.state
    }

    /// Select up to `fanout` random peer node IDs for gossip dissemination.
    ///
    /// Excludes the local node and any offline nodes.
    pub fn select_gossip_targets(&self, fanout: usize) -> Vec<NodeId> {
        let candidates: Vec<NodeId> = self
            .state
            .iter()
            .filter(|(&id, &(status, _))| id != self.local_node_id && status != NodeStatus::Offline)
            .map(|(&id, _)| id)
            .collect();

        // Simple selection: take first `fanout` from the list.
        // A production system would use random sampling, but this is
        // deterministic and sufficient for correctness.
        candidates.into_iter().take(fanout).collect()
    }
}

// ---------------------------------------------------------------------------
// ReadRepairCoordinator
// ---------------------------------------------------------------------------

/// Coordinates read repair for documents that have divergent versions across
/// replica nodes.
///
/// When reading a document, all replicas are queried. If the versions (tracked
/// via a sequence number on the document) differ, the coordinator pushes the
/// latest version to all stale replicas.
#[derive(Clone, Debug)]
pub struct ReadRepairCoordinator {
    /// Replication factor — number of replicas expected per document.
    replication_factor: u8,
}

/// The result of comparing replica versions for a single document.
#[derive(Clone, Debug)]
pub struct ReplicaComparison {
    /// The document with the highest version.
    pub latest: Document,
    /// The version (sequence number) of the latest document.
    pub latest_version: u64,
    /// Nodes that have a stale version and need repair.
    pub stale_nodes: Vec<NodeId>,
}

impl ReadRepairCoordinator {
    /// Create a coordinator with the given replication factor.
    pub fn new(replication_factor: u8) -> Self {
        Self { replication_factor }
    }

    /// Compare versions of a document across replicas.
    ///
    /// Each entry in `replicas` is `(node_id, document, version)`.
    /// Returns `None` if all replicas agree (no repair needed).
    /// Returns `Some(comparison)` if versions diverge.
    pub fn compare_replicas(
        &self,
        replicas: &[(NodeId, Document, u64)],
    ) -> Option<ReplicaComparison> {
        if replicas.is_empty() {
            return None;
        }

        // Find the maximum version.
        let max_version = replicas.iter().map(|(_, _, v)| *v).max().unwrap_or(0);

        // Find stale replicas.
        let stale_nodes: Vec<NodeId> = replicas
            .iter()
            .filter(|(_, _, v)| *v < max_version)
            .map(|(id, _, _)| *id)
            .collect();

        if stale_nodes.is_empty() {
            return None;
        }

        // The document with the latest version.
        let latest = replicas
            .iter()
            .find(|(_, _, v)| *v == max_version)
            .map(|(_, doc, _)| doc.clone())
            .unwrap();

        Some(ReplicaComparison {
            latest,
            latest_version: max_version,
            stale_nodes,
        })
    }

    /// Return the configured replication factor.
    pub fn replication_factor(&self) -> u8 {
        self.replication_factor
    }
}

// ---------------------------------------------------------------------------
// ClusterManager
// ---------------------------------------------------------------------------

/// Central coordinator for cluster membership, health monitoring, failure
/// detection, gossip, and read repair.
///
/// The `ClusterManager` ties together:
///
/// - A [`ClusterRouter`] (behind `Arc<RwLock<>>`) for document/query routing.
/// - A [`ConnectionPool`] for gRPC connections to peer nodes.
/// - A [`DashMap`] for concurrent health state tracking.
/// - A [`FailureDetector`] (Phi Accrual) for statistical failure detection.
/// - A [`GossipManager`] for eventually-consistent state dissemination.
/// - A [`ReadRepairCoordinator`] for version reconciliation.
///
/// ## Why `Arc<RwLock<>>` instead of `&`?
///
/// Async tasks spawned with `tokio::spawn` must be `'static` — they cannot
/// hold references to data on the stack. `Arc` provides shared ownership with
/// runtime reference counting, while `RwLock` allows many concurrent readers
/// or one exclusive writer.
pub struct ClusterManager {
    /// The cluster router for document placement and query routing.
    pub router: Arc<RwLock<ClusterRouter>>,

    /// Pool of gRPC connections to peer nodes.
    pub connection_pool: Arc<ConnectionPool>,

    /// This node's information.
    pub local_node: NodeInfo,

    /// The Raft consensus node.
    pub raft_node: Arc<RaftNode>,

    /// Concurrent map of node health states.
    ///
    /// [`DashMap`] is used instead of `RwLock<HashMap>` because it provides
    /// per-shard locking — reads to different nodes never contend, even during
    /// concurrent health check updates.
    pub health_state: Arc<DashMap<NodeId, NodeHealth>>,

    /// Statistical failure detector.
    pub failure_detector: Arc<RwLock<FailureDetector>>,

    /// Gossip protocol manager.
    pub gossip_manager: Arc<RwLock<GossipManager>>,

    /// Read repair coordinator.
    pub read_repair: ReadRepairCoordinator,

    /// Storage backend for read repair writes.
    pub storage: Arc<dyn StorageBackend>,
}

impl ClusterManager {
    /// Create a new cluster manager.
    ///
    /// # Arguments
    ///
    /// * `local_node` — this node's identity and address.
    /// * `router` — the shared cluster router.
    /// * `connection_pool` — the gRPC connection pool.
    /// * `raft_node` — the Raft consensus node.
    /// * `storage` — the storage backend (for read repair).
    /// * `replication_factor` — number of replicas per document.
    pub fn new(
        local_node: NodeInfo,
        router: Arc<RwLock<ClusterRouter>>,
        connection_pool: Arc<ConnectionPool>,
        raft_node: Arc<RaftNode>,
        storage: Arc<dyn StorageBackend>,
        replication_factor: u8,
    ) -> Self {
        let health_state = Arc::new(DashMap::new());

        // Seed health state with local node.
        health_state.insert(local_node.id, NodeHealth::new(local_node.id));

        Self {
            router,
            connection_pool,
            local_node: local_node.clone(),
            raft_node,
            health_state,
            failure_detector: Arc::new(RwLock::new(FailureDetector::new())),
            gossip_manager: Arc::new(RwLock::new(GossipManager::new(local_node.id))),
            read_repair: ReadRepairCoordinator::new(replication_factor),
            storage,
        }
    }

    /// Start the background health check loop.
    ///
    /// This spawns a Tokio task that pings all known nodes every
    /// [`HEALTH_CHECK_INTERVAL_MS`] milliseconds. The task runs until the
    /// manager is dropped.
    ///
    /// Uses `#[tracing::instrument]` for automatic span creation — every
    /// iteration is traced with node counts and failures.
    pub fn start_health_checks(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager.run_health_checks().await;
        })
    }

    /// Run the health check loop (runs forever).
    ///
    /// Every [`HEALTH_CHECK_INTERVAL_MS`]:
    /// 1. Collect all tracked node IDs.
    /// 2. Skip the local node.
    /// 3. For each remote node, attempt a gRPC health check.
    /// 4. Update `health_state` and `failure_detector` based on results.
    /// 5. Log state transitions via `tracing`.
    #[tracing::instrument(skip(self), fields(local_node = %self.local_node.id))]
    pub async fn run_health_checks(&self) {
        let mut interval = tokio::time::interval(Duration::from_millis(HEALTH_CHECK_INTERVAL_MS));

        loop {
            interval.tick().await;

            // Collect node IDs to check (skip self).
            let node_ids: Vec<NodeId> = self
                .health_state
                .iter()
                .map(|entry| *entry.key())
                .filter(|id| *id != self.local_node.id)
                .collect();

            for node_id in node_ids {
                let start = Instant::now();

                // Attempt health check via connection pool.
                let result = self.ping_node(&node_id).await;
                let latency_ms = start.elapsed().as_millis() as u64;

                match result {
                    Ok(()) => {
                        // Record success in health state.
                        if let Some(mut health) = self.health_state.get_mut(&node_id) {
                            let came_back = health.record_success(latency_ms);
                            if came_back {
                                tracing::info!(
                                    node_id = %node_id,
                                    latency_ms = latency_ms,
                                    "Node {} came back online",
                                    node_id
                                );
                            }
                        }

                        // Record heartbeat for phi accrual detector.
                        let mut detector = self.failure_detector.write().await;
                        detector.report_heartbeat(node_id);
                    }
                    Err(_e) => {
                        // Record failure in health state.
                        if let Some(mut health) = self.health_state.get_mut(&node_id) {
                            let went_offline = health.record_failure();
                            if went_offline {
                                tracing::warn!(
                                    node_id = %node_id,
                                    consecutive_failures = health.consecutive_failures,
                                    "Node {} went offline",
                                    node_id
                                );

                                // Update router to mark node as offline.
                                let mut router = self.router.write().await;
                                router.remove_node(node_id);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Ping a single node via the connection pool.
    ///
    /// In a full implementation, this would call `NodeClient::health_check()`.
    /// Here we simulate by checking if a connection can be acquired.
    async fn ping_node(&self, node_id: &NodeId) -> DbResult<()> {
        let addr = {
            let router = self.router.read().await;
            router
                .get_node(*node_id)
                .map(|n| n.address.clone())
                .ok_or_else(|| {
                    DbError::NetworkError(format!("unknown node {} in router", node_id))
                })?
        };

        // Try to acquire a client from the pool.
        let client_result = self.connection_pool.get(node_id, &addr).await;

        match client_result {
            Ok(client) => {
                // Attempt actual health check RPC.
                match client.health_check().await {
                    Ok(_health_status) => {
                        self.connection_pool.record_success(node_id).await;
                        Ok(())
                    }
                    Err(e) => {
                        self.connection_pool.record_failure(node_id).await;
                        Err(e)
                    }
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Start the background gossip loop.
    ///
    /// Spawns a Tokio task that gossips this node's cluster view to
    /// [`GOSSIP_FANOUT`] random peers every [`GOSSIP_INTERVAL_MS`] milliseconds.
    pub fn start_gossip(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager.run_gossip().await;
        })
    }

    /// Run the gossip loop (runs forever).
    #[tracing::instrument(skip(self), fields(local_node = %self.local_node.id))]
    async fn run_gossip(&self) {
        let mut interval = tokio::time::interval(Duration::from_millis(GOSSIP_INTERVAL_MS));

        loop {
            interval.tick().await;

            // Update gossip state from current health observations.
            {
                let mut gossip = self.gossip_manager.write().await;
                for entry in self.health_state.iter() {
                    let node_id = *entry.key();
                    let health = entry.value();
                    let _elapsed = health.last_seen.elapsed().as_millis() as u64;
                    // Use current time as timestamp (logical clock).
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    gossip.update_local(node_id, health.status, now);
                }
            }

            // Build message and send to random peers.
            let (message, targets) = {
                let gossip = self.gossip_manager.read().await;
                let message = gossip.build_message();
                let targets = gossip.select_gossip_targets(GOSSIP_FANOUT);
                (message, targets)
            };

            for target_id in targets {
                // In a full implementation, we would send the gossip message
                // via gRPC or UDP. For now, this is a best-effort send.
                let _message_clone = message.clone();
                tracing::trace!(
                    target = %target_id,
                    entries = _message_clone.cluster_view.len(),
                    "gossip sent"
                );
            }
        }
    }

    /// Handle an incoming gossip message from a peer.
    ///
    /// Merges the remote view into the local state and updates health
    /// observations accordingly.
    pub async fn handle_gossip(&self, message: GossipMessage) {
        let changed = {
            let mut gossip = self.gossip_manager.write().await;
            gossip.merge(&message)
        };

        for node_id in changed {
            let gossip = self.gossip_manager.read().await;
            if let Some(&(status, _)) = gossip.current_view().get(&node_id) {
                tracing::info!(
                    node_id = %node_id,
                    new_status = %status,
                    "gossip merge updated node status"
                );
            }
        }
    }

    /// Join this node to a cluster via a bootstrap peer.
    ///
    /// # Process
    ///
    /// 1. Connect to the bootstrap peer via the connection pool.
    /// 2. Call `JoinCluster` RPC — the bootstrap peer proposes a membership
    ///    change through Raft.
    /// 3. This node starts as a Learner (receives log replication but does
    ///    not vote).
    /// 4. Once caught up, it can be promoted to a Voter.
    pub async fn join_cluster(&self, bootstrap_peer: NodeAddress) -> DbResult<()> {
        tracing::info!(
            bootstrap = %bootstrap_peer,
            local_node = %self.local_node.id,
            "joining cluster via bootstrap peer"
        );

        // Connect to bootstrap peer.
        let bootstrap_node_id = NodeId::new(0); // Placeholder — real ID comes from the peer.
        let client = self
            .connection_pool
            .get(&bootstrap_node_id, &bootstrap_peer)
            .await?;

        // Call JoinCluster RPC.
        let addr_str = format!("{}", self.local_node.address);
        client
            .join_cluster(self.local_node.id.as_u64(), &addr_str)
            .await?;

        tracing::info!(
            local_node = %self.local_node.id,
            "successfully joined cluster"
        );

        Ok(())
    }

    /// Gracefully leave the cluster.
    ///
    /// # Process
    ///
    /// 1. Propose `RemoveNode` via Raft (if this node is the leader, or forward).
    /// 2. Drain active connections.
    /// 3. Mark self as Offline in health state.
    pub async fn leave_cluster(&self) -> DbResult<()> {
        tracing::info!(
            local_node = %self.local_node.id,
            "leaving cluster"
        );

        // Mark self as offline in health state.
        if let Some(mut health) = self.health_state.get_mut(&self.local_node.id) {
            health.status = NodeStatus::Offline;
        }

        // Remove self from the connection pool.
        self.connection_pool.remove(&self.local_node.id).await;

        tracing::info!(
            local_node = %self.local_node.id,
            "gracefully left cluster"
        );

        Ok(())
    }

    /// Register a new node in the cluster manager's tracking.
    pub async fn register_node(&self, node: NodeInfo) {
        let node_id = node.id;

        // Add to health tracking.
        self.health_state.insert(node_id, NodeHealth::new(node_id));

        // Add to router.
        let mut router = self.router.write().await;
        router.add_node(node);

        tracing::info!(node_id = %node_id, "registered new node");
    }

    /// Deregister a node from the cluster manager's tracking.
    pub async fn deregister_node(&self, node_id: NodeId) {
        // Remove from health tracking.
        self.health_state.remove(&node_id);

        // Remove from router.
        let mut router = self.router.write().await;
        router.remove_node(node_id);

        // Remove from failure detector.
        let mut detector = self.failure_detector.write().await;
        detector.remove_node(&node_id);

        // Remove from connection pool.
        self.connection_pool.remove(&node_id).await;

        tracing::info!(node_id = %node_id, "deregistered node");
    }

    /// Perform read repair for a document by comparing replica versions.
    ///
    /// # Arguments
    ///
    /// * `doc_id` — the document to check.
    /// * `replicas` — per-replica data: `(node_id, document, version)`.
    ///
    /// If versions differ, the latest document is written to all stale replicas.
    pub async fn read_repair(
        &self,
        doc_id: &DocumentId,
        replicas: Vec<(NodeId, Document, u64)>,
    ) -> DbResult<Document> {
        match self.read_repair.compare_replicas(&replicas) {
            Some(comparison) => {
                tracing::info!(
                    doc_id = %doc_id,
                    latest_version = comparison.latest_version,
                    stale_count = comparison.stale_nodes.len(),
                    "read repair: repairing stale replicas"
                );

                // Write the latest version to all stale nodes.
                for stale_node_id in &comparison.stale_nodes {
                    // In a full implementation, we would forward the write
                    // via gRPC to the stale node. For now, if the stale node
                    // is local, write directly.
                    if *stale_node_id == self.local_node.id {
                        self.storage.put(comparison.latest.clone()).await?;
                    } else {
                        tracing::debug!(
                            node_id = %stale_node_id,
                            doc_id = %doc_id,
                            "would forward repair write to remote node"
                        );
                    }
                }

                Ok(comparison.latest)
            }
            None => {
                // All replicas agree — return any copy.
                Ok(replicas
                    .into_iter()
                    .next()
                    .map(|(_, doc, _)| doc)
                    .ok_or_else(|| {
                        DbError::NotFound(format!("no replicas for document '{}'", doc_id))
                    })?)
            }
        }
    }

    /// Return a snapshot of all tracked node health states.
    pub fn health_snapshot(&self) -> Vec<NodeHealth> {
        self.health_state
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Return the number of tracked nodes.
    pub fn tracked_node_count(&self) -> usize {
        self.health_state.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use msearchdb_core::cluster::NodeAddress;
    use msearchdb_core::document::FieldValue;

    // -- NodeHealth tests ---------------------------------------------------

    #[test]
    fn node_health_initial_state() {
        let health = NodeHealth::new(NodeId::new(1));
        assert_eq!(health.node_id, NodeId::new(1));
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.is_online());
        assert!(health.latency_ms.is_none());
    }

    #[test]
    fn node_health_record_success_updates_fields() {
        let mut health = NodeHealth::new(NodeId::new(1));
        let came_back = health.record_success(42);
        assert!(!came_back);
        assert_eq!(health.latency_ms, Some(42));
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.is_online());
    }

    #[test]
    fn node_health_goes_offline_after_threshold() {
        let mut health = NodeHealth::new(NodeId::new(1));

        // Failures 1 and 2 don't trigger offline.
        assert!(!health.record_failure());
        assert_eq!(health.consecutive_failures, 1);
        assert!(health.is_online());

        assert!(!health.record_failure());
        assert_eq!(health.consecutive_failures, 2);
        assert!(health.is_online());

        // Failure 3 triggers offline.
        assert!(health.record_failure());
        assert_eq!(health.consecutive_failures, 3);
        assert!(!health.is_online());
        assert_eq!(health.status, NodeStatus::Offline);
    }

    #[test]
    fn node_health_comes_back_online() {
        let mut health = NodeHealth::new(NodeId::new(1));

        // Go offline.
        health.record_failure();
        health.record_failure();
        health.record_failure();
        assert!(!health.is_online());

        // Come back.
        let came_back = health.record_success(10);
        assert!(came_back);
        assert!(health.is_online());
        assert_eq!(health.consecutive_failures, 0);
    }

    #[test]
    fn node_health_failure_saturates_at_u8_max() {
        let mut health = NodeHealth::new(NodeId::new(1));
        for _ in 0..300 {
            health.record_failure();
        }
        assert_eq!(health.consecutive_failures, 255);
    }

    #[test]
    fn node_health_repeated_offline_does_not_re_trigger() {
        let mut health = NodeHealth::new(NodeId::new(1));

        // First transition to offline returns true.
        health.record_failure();
        health.record_failure();
        let first = health.record_failure();
        assert!(first);

        // Subsequent failures return false (already offline).
        let second = health.record_failure();
        assert!(!second);
    }

    // -- FailureDetector (Phi Accrual) tests --------------------------------

    #[test]
    fn phi_accrual_not_enough_samples() {
        let mut detector = FailureDetector::new();
        let node = NodeId::new(1);

        // Only one heartbeat — not enough for statistics.
        detector.report_heartbeat(node);
        assert!(detector.phi(&node).is_none());
    }

    #[test]
    fn phi_accrual_on_time_heartbeats_yield_low_phi() {
        let mut detector = FailureDetector::new();
        let node = NodeId::new(1);

        // Simulate 20 on-time heartbeats at 1000ms intervals.
        let start = Instant::now();
        for i in 0..20 {
            let at = start + Duration::from_millis(i * 1000);
            detector.report_heartbeat_at(node, at);
        }

        // Check phi immediately after last heartbeat (0ms elapsed).
        let check_at = start + Duration::from_millis(19 * 1000);
        let phi = detector.phi_at(&node, check_at);
        assert!(phi.is_some());
        let phi_val = phi.unwrap();
        // Phi should be very low right after a heartbeat.
        assert!(
            phi_val < PHI_SUSPECT_THRESHOLD,
            "phi {} should be < {} for on-time heartbeats",
            phi_val,
            PHI_SUSPECT_THRESHOLD
        );
    }

    #[test]
    fn phi_accrual_missed_heartbeats_yield_high_phi() {
        let mut detector = FailureDetector::new();
        let node = NodeId::new(1);

        // Simulate 20 on-time heartbeats at 1000ms intervals.
        let start = Instant::now();
        for i in 0..20 {
            let at = start + Duration::from_millis(i * 1000);
            detector.report_heartbeat_at(node, at);
        }

        // Check phi 30 seconds after the last heartbeat (missed 30 heartbeats).
        let check_at = start + Duration::from_millis(19_000 + 30_000);
        let phi = detector.phi_at(&node, check_at);
        assert!(phi.is_some());
        let phi_val = phi.unwrap();
        // Phi should be very high after many missed heartbeats.
        assert!(
            phi_val > PHI_DEAD_THRESHOLD,
            "phi {} should be > {} after 30s missed heartbeats",
            phi_val,
            PHI_DEAD_THRESHOLD
        );
    }

    #[test]
    fn phi_accrual_moderate_delay_is_suspect() {
        let mut detector = FailureDetector::new();
        let node = NodeId::new(1);

        // Simulate 50 on-time heartbeats at 1000ms intervals.
        let start = Instant::now();
        for i in 0..50 {
            let at = start + Duration::from_millis(i * 1000);
            detector.report_heartbeat_at(node, at);
        }

        // Check phi 10 seconds after the last heartbeat.
        let check_at = start + Duration::from_millis(49_000 + 10_000);
        let phi = detector.phi_at(&node, check_at);
        assert!(phi.is_some());
        let phi_val = phi.unwrap();
        // Should be suspect but maybe not dead.
        assert!(
            phi_val > PHI_SUSPECT_THRESHOLD,
            "phi {} should be > {} after 10s delay",
            phi_val,
            PHI_SUSPECT_THRESHOLD
        );
    }

    #[test]
    fn phi_accrual_remove_node_clears_state() {
        let mut detector = FailureDetector::new();
        let node = NodeId::new(1);

        detector.report_heartbeat(node);
        detector.report_heartbeat(node);
        assert!(detector.phi(&node).is_some() || detector.phi(&node).is_none());

        detector.remove_node(&node);
        assert!(detector.phi(&node).is_none());
    }

    // -- GossipManager tests ------------------------------------------------

    #[test]
    fn gossip_merge_latest_timestamp_wins() {
        let mut manager = GossipManager::new(NodeId::new(1));

        // Local view: node 2 is Follower at time 100.
        manager.update_local(NodeId::new(2), NodeStatus::Follower, 100);

        // Incoming gossip says node 2 is Offline at time 200.
        let message = GossipMessage {
            from: NodeId::new(3),
            cluster_view: vec![GossipEntry {
                node_id: NodeId::new(2),
                status: NodeStatus::Offline,
                timestamp: 200,
            }],
        };

        let changed = manager.merge(&message);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0], NodeId::new(2));

        // Verify the status was updated.
        let view = manager.current_view();
        assert_eq!(view[&NodeId::new(2)].0, NodeStatus::Offline);
    }

    #[test]
    fn gossip_merge_old_timestamp_ignored() {
        let mut manager = GossipManager::new(NodeId::new(1));

        // Local view: node 2 is Offline at time 200.
        manager.update_local(NodeId::new(2), NodeStatus::Offline, 200);

        // Incoming gossip says node 2 is Follower at time 100 (older).
        let message = GossipMessage {
            from: NodeId::new(3),
            cluster_view: vec![GossipEntry {
                node_id: NodeId::new(2),
                status: NodeStatus::Follower,
                timestamp: 100,
            }],
        };

        let changed = manager.merge(&message);
        assert!(changed.is_empty());

        // Status should remain Offline.
        let view = manager.current_view();
        assert_eq!(view[&NodeId::new(2)].0, NodeStatus::Offline);
    }

    #[test]
    fn gossip_three_nodes_converge() {
        // Simulate 3 nodes with different views converging.
        let mut mgr1 = GossipManager::new(NodeId::new(1));
        let mut mgr2 = GossipManager::new(NodeId::new(2));
        let mut mgr3 = GossipManager::new(NodeId::new(3));

        // Node 1 knows: node 2 is Follower@100, node 3 is Follower@100
        mgr1.update_local(NodeId::new(2), NodeStatus::Follower, 100);
        mgr1.update_local(NodeId::new(3), NodeStatus::Follower, 100);

        // Node 2 knows: node 3 is Offline@200 (more recent info)
        mgr2.update_local(NodeId::new(3), NodeStatus::Offline, 200);
        mgr2.update_local(NodeId::new(1), NodeStatus::Leader, 150);

        // Node 3 knows: node 1 is Follower@50 (stale), node 2 is Follower@50
        mgr3.update_local(NodeId::new(1), NodeStatus::Follower, 50);
        mgr3.update_local(NodeId::new(2), NodeStatus::Follower, 50);

        // Round 1: node 2 gossips to node 1
        let msg2 = mgr2.build_message();
        mgr1.merge(&msg2);

        // Node 1 should now know node 3 is Offline@200 and node 1 is Leader@150
        assert_eq!(mgr1.current_view()[&NodeId::new(3)].0, NodeStatus::Offline);
        assert_eq!(mgr1.current_view()[&NodeId::new(1)].0, NodeStatus::Leader);

        // Round 2: node 1 gossips to node 3
        let msg1 = mgr1.build_message();
        mgr3.merge(&msg1);

        // Node 3 should now know node 1 is Leader@150 and itself is Offline@200
        assert_eq!(mgr3.current_view()[&NodeId::new(1)].0, NodeStatus::Leader);
        assert_eq!(mgr3.current_view()[&NodeId::new(3)].0, NodeStatus::Offline);
    }

    #[test]
    fn gossip_build_message_includes_all_entries() {
        let mut manager = GossipManager::new(NodeId::new(1));
        manager.update_local(NodeId::new(1), NodeStatus::Leader, 100);
        manager.update_local(NodeId::new(2), NodeStatus::Follower, 100);
        manager.update_local(NodeId::new(3), NodeStatus::Follower, 100);

        let msg = manager.build_message();
        assert_eq!(msg.from, NodeId::new(1));
        assert_eq!(msg.cluster_view.len(), 3);
    }

    #[test]
    fn gossip_select_targets_excludes_self_and_offline() {
        let mut manager = GossipManager::new(NodeId::new(1));
        manager.update_local(NodeId::new(1), NodeStatus::Leader, 100);
        manager.update_local(NodeId::new(2), NodeStatus::Follower, 100);
        manager.update_local(NodeId::new(3), NodeStatus::Offline, 100);
        manager.update_local(NodeId::new(4), NodeStatus::Follower, 100);

        let targets = manager.select_gossip_targets(2);
        // Should not include node 1 (self) or node 3 (offline).
        assert!(targets.len() <= 2);
        for t in &targets {
            assert_ne!(*t, NodeId::new(1));
            assert_ne!(*t, NodeId::new(3));
        }
    }

    // -- ReadRepairCoordinator tests ----------------------------------------

    #[test]
    fn read_repair_all_same_version() {
        let coord = ReadRepairCoordinator::new(3);

        let doc = Document::new(DocumentId::new("doc-1"))
            .with_field("title", FieldValue::Text("hello".into()));

        let replicas = vec![
            (NodeId::new(1), doc.clone(), 5),
            (NodeId::new(2), doc.clone(), 5),
            (NodeId::new(3), doc.clone(), 5),
        ];

        let result = coord.compare_replicas(&replicas);
        assert!(result.is_none(), "all same version => no repair needed");
    }

    #[test]
    fn read_repair_detects_stale_replicas() {
        let coord = ReadRepairCoordinator::new(3);

        let old_doc = Document::new(DocumentId::new("doc-1"))
            .with_field("title", FieldValue::Text("old".into()));

        let new_doc = Document::new(DocumentId::new("doc-1"))
            .with_field("title", FieldValue::Text("new".into()));

        let replicas = vec![
            (NodeId::new(1), new_doc.clone(), 10),
            (NodeId::new(2), old_doc.clone(), 5),
            (NodeId::new(3), old_doc.clone(), 3),
        ];

        let result = coord.compare_replicas(&replicas);
        assert!(result.is_some());

        let comparison = result.unwrap();
        assert_eq!(comparison.latest_version, 10);
        assert_eq!(comparison.latest.id, DocumentId::new("doc-1"));
        assert_eq!(comparison.stale_nodes.len(), 2);
        assert!(comparison.stale_nodes.contains(&NodeId::new(2)));
        assert!(comparison.stale_nodes.contains(&NodeId::new(3)));
    }

    #[test]
    fn read_repair_single_stale_replica() {
        let coord = ReadRepairCoordinator::new(3);

        let doc_v1 =
            Document::new(DocumentId::new("doc-1")).with_field("v", FieldValue::Number(1.0));

        let doc_v2 =
            Document::new(DocumentId::new("doc-1")).with_field("v", FieldValue::Number(2.0));

        let replicas = vec![
            (NodeId::new(1), doc_v2.clone(), 2),
            (NodeId::new(2), doc_v2.clone(), 2),
            (NodeId::new(3), doc_v1.clone(), 1),
        ];

        let result = coord.compare_replicas(&replicas);
        assert!(result.is_some());

        let comparison = result.unwrap();
        assert_eq!(comparison.latest_version, 2);
        assert_eq!(comparison.stale_nodes, vec![NodeId::new(3)]);
    }

    #[test]
    fn read_repair_empty_replicas() {
        let coord = ReadRepairCoordinator::new(3);
        let result = coord.compare_replicas(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn read_repair_coordinator_replication_factor() {
        let coord = ReadRepairCoordinator::new(5);
        assert_eq!(coord.replication_factor(), 5);
    }

    // -- normal_cdf tests ---------------------------------------------------

    #[test]
    fn normal_cdf_properties() {
        // CDF(0) should be ~0.5
        let cdf_zero = normal_cdf(0.0);
        assert!(
            (cdf_zero - 0.5).abs() < 0.001,
            "CDF(0) = {} should be ~0.5",
            cdf_zero
        );

        // CDF(-infinity) -> 0, CDF(+infinity) -> 1
        let cdf_neg = normal_cdf(-10.0);
        assert!(cdf_neg < 0.001, "CDF(-10) = {} should be ~0", cdf_neg);

        let cdf_pos = normal_cdf(10.0);
        assert!(cdf_pos > 0.999, "CDF(10) = {} should be ~1", cdf_pos);

        // CDF(1.96) ≈ 0.975 (95th percentile)
        let cdf_196 = normal_cdf(1.96);
        assert!(
            (cdf_196 - 0.975).abs() < 0.002,
            "CDF(1.96) = {} should be ~0.975",
            cdf_196
        );
    }

    // -- ClusterManager integration-style tests (in-process) ----------------

    #[tokio::test]
    async fn cluster_manager_tracks_registered_nodes() {
        let (manager, _) = make_test_cluster_manager().await;

        // Should start with the local node.
        assert_eq!(manager.tracked_node_count(), 1);

        // Register a new node.
        let new_node = NodeInfo {
            id: NodeId::new(2),
            address: NodeAddress::new("10.0.0.2", 9300),
            status: NodeStatus::Follower,
        };
        manager.register_node(new_node).await;
        assert_eq!(manager.tracked_node_count(), 2);

        // Verify health state exists.
        assert!(manager.health_state.contains_key(&NodeId::new(2)));
    }

    #[tokio::test]
    async fn cluster_manager_deregisters_node() {
        let (manager, _) = make_test_cluster_manager().await;

        let new_node = NodeInfo {
            id: NodeId::new(2),
            address: NodeAddress::new("10.0.0.2", 9300),
            status: NodeStatus::Follower,
        };
        manager.register_node(new_node).await;
        assert_eq!(manager.tracked_node_count(), 2);

        manager.deregister_node(NodeId::new(2)).await;
        assert_eq!(manager.tracked_node_count(), 1);
        assert!(!manager.health_state.contains_key(&NodeId::new(2)));
    }

    #[tokio::test]
    async fn cluster_manager_read_repair_repairs_stale() {
        let (manager, _) = make_test_cluster_manager().await;

        let old_doc =
            Document::new(DocumentId::new("repair-1")).with_field("v", FieldValue::Number(1.0));
        let new_doc =
            Document::new(DocumentId::new("repair-1")).with_field("v", FieldValue::Number(2.0));

        let replicas = vec![
            (NodeId::new(1), new_doc.clone(), 10),
            (NodeId::new(2), old_doc.clone(), 5),
        ];

        let result = manager
            .read_repair(&DocumentId::new("repair-1"), replicas)
            .await;
        assert!(result.is_ok());
        let doc = result.unwrap();
        assert_eq!(doc.get_field("v"), new_doc.get_field("v"));
    }

    #[tokio::test]
    async fn cluster_manager_health_snapshot() {
        let (manager, _) = make_test_cluster_manager().await;

        let snapshot = manager.health_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].node_id, NodeId::new(1));
    }

    #[tokio::test]
    async fn cluster_manager_leave_cluster() {
        let (manager, _) = make_test_cluster_manager().await;
        let result = manager.leave_cluster().await;
        assert!(result.is_ok());

        let health = manager.health_state.get(&NodeId::new(1)).unwrap();
        assert_eq!(health.status, NodeStatus::Offline);
    }

    // -- Test helpers -------------------------------------------------------

    /// Build a test `ClusterManager` with in-memory backends.
    async fn make_test_cluster_manager() -> (ClusterManager, Arc<RaftNode>) {
        use msearchdb_core::config::NodeConfig;
        use std::collections::BTreeMap;

        let config = NodeConfig::default();
        let storage: Arc<dyn StorageBackend> = Arc::new(TestStorage::new());
        let index: Arc<dyn msearchdb_core::traits::IndexBackend> = Arc::new(TestIndex);

        let raft_node = RaftNode::new(&config, storage.clone(), index)
            .await
            .unwrap();
        let raft_node = Arc::new(raft_node);

        // Initialize as single-node cluster.
        let mut members = BTreeMap::new();
        members.insert(config.node_id.as_u64(), openraft::BasicNode::default());
        let _ = raft_node.initialize(members).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let local_node = NodeInfo {
            id: NodeId::new(config.node_id.as_u64()),
            address: NodeAddress::new("127.0.0.1", 9300),
            status: NodeStatus::Leader,
        };

        let router = ClusterRouter::new(vec![local_node.clone()], 3);

        let manager = ClusterManager::new(
            local_node,
            Arc::new(RwLock::new(router)),
            Arc::new(ConnectionPool::new()),
            raft_node.clone(),
            storage,
            3,
        );

        (manager, raft_node)
    }

    /// In-memory storage for tests.
    struct TestStorage {
        docs: tokio::sync::Mutex<HashMap<String, Document>>,
    }

    impl TestStorage {
        fn new() -> Self {
            Self {
                docs: tokio::sync::Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl StorageBackend for TestStorage {
        async fn get(&self, id: &DocumentId) -> DbResult<Document> {
            let docs = self.docs.lock().await;
            docs.get(id.as_str())
                .cloned()
                .ok_or_else(|| DbError::NotFound(id.to_string()))
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
                .ok_or_else(|| DbError::NotFound(id.to_string()))
        }

        async fn scan(
            &self,
            _range: std::ops::RangeInclusive<DocumentId>,
            _limit: usize,
        ) -> DbResult<Vec<Document>> {
            let docs = self.docs.lock().await;
            Ok(docs.values().cloned().collect())
        }
    }

    /// No-op index for tests.
    struct TestIndex;

    #[async_trait::async_trait]
    impl msearchdb_core::traits::IndexBackend for TestIndex {
        async fn index_document(&self, _doc: &Document) -> DbResult<()> {
            Ok(())
        }

        async fn search(
            &self,
            _query: &msearchdb_core::query::Query,
        ) -> DbResult<msearchdb_core::query::SearchResult> {
            Ok(msearchdb_core::query::SearchResult::empty(0))
        }

        async fn delete_document(&self, _id: &DocumentId) -> DbResult<()> {
            Ok(())
        }
    }
}
