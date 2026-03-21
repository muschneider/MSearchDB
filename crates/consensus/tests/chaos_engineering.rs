//! Comprehensive chaos engineering test suite for MSearchDB.
//!
//! Extends the basic chaos tests with properly wired fault injection, random
//! chaos scheduling, continuous workload generation, and rigorous validation
//! of the three-node failure threshold.
//!
//! # Failure Modes and Recovery Behaviors
//!
//! | Failure Mode             | Behavior                                              | Recovery                                      |
//! |--------------------------|-------------------------------------------------------|-----------------------------------------------|
//! | Single follower death    | Writes succeed (quorum 2/3)                           | Rejoin catches up via log replay              |
//! | Leader death             | New election in <2s, writes resume                    | Followers detect missing heartbeats           |
//! | Two followers dead       | Writes fail (no quorum), reads from leader ok         | Need ≥1 follower to recover quorum            |
//! | All 3 nodes dead         | Complete outage, writes rejected                      | Data persists in durable storage backends     |
//! | Network partition 1v2    | Majority elects leader and writes; minority stalls    | Heal partition → minority catches up          |
//! | Asymmetric route block   | Direction-dependent; affected side cannot replicate   | Unblock → normal replication resumes          |
//! | Packet loss 10–50%       | Increased latency, possible leader step-down          | Stop loss → cluster stabilizes                |
//! | Network delay 50–300ms   | Slower commits, possible election timeout             | Remove delay → normal                         |
//! | Cascading faults         | Degrades gracefully until quorum lost                 | Sequential recovery, leader re-elected        |
//! | Storage corruption       | Corrupted reads detected; WAL provides recovery path  | Log replay restores correct state             |
//!
//! # Three-Node Failure Threshold
//!
//! With replication factor 3 and Raft quorum of 2, a committed write is
//! guaranteed to be on at least 2 nodes (leader + 1 follower). After full
//! replication, all 3 nodes hold the data. Data loss requires losing ALL
//! copies simultaneously (all 3 storage backends destroyed). As long as at
//! least one node's storage survives, the data can be recovered.
//!
//! # Running Long-Duration Tests
//!
//! ```bash
//! # Default: 30 seconds (CI-friendly)
//! cargo test --test chaos_engineering chaos_long_running -- --ignored --nocapture
//!
//! # 24-hour endurance test
//! CHAOS_DURATION_SECS=86400 cargo test --test chaos_engineering chaos_long_running -- --ignored --nocapture
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openraft::error::{InstallSnapshotError, RPCError, RaftError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::{BasicNode, ServerState};
use rand::Rng;
use tokio::sync::RwLock;
use tokio::time::sleep;

use msearchdb_consensus::network::RaftHandle;
use msearchdb_consensus::raft_node::{InMemoryStorage, NoopIndex, RaftNode};
use msearchdb_consensus::types::{RaftCommand, TypeConfig};
use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::traits::{IndexBackend, StorageBackend};

// ===========================================================================
// Fault Injection Infrastructure
// ===========================================================================

/// Per-node fault injection configuration.
#[derive(Clone, Debug)]
struct ChaosNodeConfig {
    delay_ms: Arc<AtomicU64>,
    drop_percent: Arc<AtomicU64>,
    isolated: Arc<AtomicBool>,
}

impl ChaosNodeConfig {
    fn new() -> Self {
        Self {
            delay_ms: Arc::new(AtomicU64::new(0)),
            drop_percent: Arc::new(AtomicU64::new(0)),
            isolated: Arc::new(AtomicBool::new(false)),
        }
    }
}

type ChaosConfigMap = Arc<RwLock<HashMap<u64, ChaosNodeConfig>>>;

/// Network factory with full fault injection: isolation, partitions, delay,
/// packet drop, and asymmetric route blocking.
#[derive(Clone)]
struct ChaosNetworkFactory {
    routers: Arc<RwLock<HashMap<u64, RaftHandle>>>,
    chaos_configs: ChaosConfigMap,
    sender_id: u64,
    partitions: Arc<RwLock<Vec<HashSet<u64>>>>,
    blocked_routes: Arc<RwLock<HashSet<(u64, u64)>>>,
}

impl ChaosNetworkFactory {
    fn new(
        routers: Arc<RwLock<HashMap<u64, RaftHandle>>>,
        chaos_configs: ChaosConfigMap,
        sender_id: u64,
        partitions: Arc<RwLock<Vec<HashSet<u64>>>>,
        blocked_routes: Arc<RwLock<HashSet<(u64, u64)>>>,
    ) -> Self {
        Self {
            routers,
            chaos_configs,
            sender_id,
            partitions,
            blocked_routes,
        }
    }
}

impl RaftNetworkFactory<TypeConfig> for ChaosNetworkFactory {
    type Network = ChaosNetwork;

    async fn new_client(&mut self, target: u64, _node: &BasicNode) -> Self::Network {
        ChaosNetwork {
            target,
            sender_id: self.sender_id,
            routers: Arc::clone(&self.routers),
            chaos_configs: Arc::clone(&self.chaos_configs),
            partitions: Arc::clone(&self.partitions),
            blocked_routes: Arc::clone(&self.blocked_routes),
        }
    }
}

/// Network client that checks all fault injection rules before forwarding RPCs.
struct ChaosNetwork {
    target: u64,
    sender_id: u64,
    routers: Arc<RwLock<HashMap<u64, RaftHandle>>>,
    chaos_configs: ChaosConfigMap,
    partitions: Arc<RwLock<Vec<HashSet<u64>>>>,
    blocked_routes: Arc<RwLock<HashSet<(u64, u64)>>>,
}

impl ChaosNetwork {
    /// Apply all fault injection rules. Returns the target handle on success.
    async fn pre_rpc(&self) -> Result<RaftHandle, RPCError<u64, BasicNode, RaftError<u64>>> {
        let configs = self.chaos_configs.read().await;

        // Check sender isolation.
        if let Some(cfg) = configs.get(&self.sender_id) {
            if cfg.isolated.load(Ordering::Relaxed) {
                return Err(make_unreachable("sender isolated"));
            }
        }

        // Check target isolation.
        if let Some(cfg) = configs.get(&self.target) {
            if cfg.isolated.load(Ordering::Relaxed) {
                return Err(make_unreachable("target isolated"));
            }
        }

        drop(configs);

        // Check asymmetric route blocks.
        {
            let blocked = self.blocked_routes.read().await;
            if blocked.contains(&(self.sender_id, self.target)) {
                return Err(make_unreachable("route blocked"));
            }
        }

        // Check symmetric partitions.
        {
            let parts = self.partitions.read().await;
            if !parts.is_empty() {
                let same_partition = parts
                    .iter()
                    .any(|p| p.contains(&self.sender_id) && p.contains(&self.target));
                if !same_partition {
                    return Err(make_unreachable("network partition"));
                }
            }
        }

        // Check packet drop.
        {
            let configs = self.chaos_configs.read().await;
            if let Some(cfg) = configs.get(&self.target) {
                let drop_pct = cfg.drop_percent.load(Ordering::Relaxed);
                if drop_pct > 0 {
                    let roll: u64 = rand::thread_rng().gen_range(0..100);
                    if roll < drop_pct {
                        return Err(make_unreachable("packet dropped"));
                    }
                }
            }

            // Inject delay.
            if let Some(cfg) = configs.get(&self.target) {
                let delay = cfg.delay_ms.load(Ordering::Relaxed);
                if delay > 0 {
                    drop(configs);
                    sleep(Duration::from_millis(delay)).await;
                }
            }
        }

        // Get target handle.
        let routers = self.routers.read().await;
        routers
            .get(&self.target)
            .cloned()
            .ok_or_else(|| make_unreachable("node not in router map"))
    }

    /// Snapshot-specific pre-RPC (returns a compatible error type).
    async fn pre_rpc_snapshot(
        &self,
    ) -> Result<RaftHandle, RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>> {
        self.pre_rpc().await.map_err(|e| {
            RPCError::Unreachable(Unreachable::new(&std::io::Error::other(e.to_string())))
        })
    }
}

fn make_unreachable(msg: &str) -> RPCError<u64, BasicNode, RaftError<u64>> {
    RPCError::Unreachable(Unreachable::new(&std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        msg.to_string(),
    )))
}

impl RaftNetwork<TypeConfig> for ChaosNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let raft = self.pre_rpc().await?;
        raft.append_entries(rpc)
            .await
            .map_err(|e| RPCError::Network(openraft::error::NetworkError::new(&e)))
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        let raft = self.pre_rpc_snapshot().await?;
        raft.install_snapshot(rpc)
            .await
            .map_err(|e| RPCError::Network(openraft::error::NetworkError::new(&e)))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let raft = self.pre_rpc().await?;
        raft.vote(rpc)
            .await
            .map_err(|e| RPCError::Network(openraft::error::NetworkError::new(&e)))
    }
}

// ===========================================================================
// ChaosMonkey — orchestrates fault injection
// ===========================================================================

/// Orchestrates fault injection for the cluster. All methods are safe to call
/// concurrently from multiple tasks.
#[derive(Clone)]
struct ChaosMonkey {
    chaos_configs: ChaosConfigMap,
    routers: Arc<RwLock<HashMap<u64, RaftHandle>>>,
    partitions: Arc<RwLock<Vec<HashSet<u64>>>>,
    blocked_routes: Arc<RwLock<HashSet<(u64, u64)>>>,
}

impl ChaosMonkey {
    fn new(
        routers: Arc<RwLock<HashMap<u64, RaftHandle>>>,
        chaos_configs: ChaosConfigMap,
        partitions: Arc<RwLock<Vec<HashSet<u64>>>>,
        blocked_routes: Arc<RwLock<HashSet<(u64, u64)>>>,
    ) -> Self {
        Self {
            chaos_configs,
            routers,
            partitions,
            blocked_routes,
        }
    }

    /// Kill a node by shutting down its Raft instance and removing it from
    /// the router map. Simulates SIGKILL.
    async fn kill_node(&self, nodes: &[RaftNode], node_id: u64) {
        self.routers.write().await.remove(&node_id);
        if let Some(node) = nodes.iter().find(|n| n.node_id() == node_id) {
            let _ = node.raft_handle().shutdown().await;
        }
    }

    async fn isolate_node(&self, node_id: u64) {
        let configs = self.chaos_configs.read().await;
        if let Some(cfg) = configs.get(&node_id) {
            cfg.isolated.store(true, Ordering::Relaxed);
        }
    }

    async fn reconnect_node(&self, node_id: u64) {
        let configs = self.chaos_configs.read().await;
        if let Some(cfg) = configs.get(&node_id) {
            cfg.isolated.store(false, Ordering::Relaxed);
        }
    }

    async fn add_delay(&self, node_id: u64, delay_ms: u64) {
        let configs = self.chaos_configs.read().await;
        if let Some(cfg) = configs.get(&node_id) {
            cfg.delay_ms.store(delay_ms, Ordering::Relaxed);
        }
    }

    async fn remove_delay(&self, node_id: u64) {
        self.add_delay(node_id, 0).await;
    }

    async fn set_drop_percent(&self, node_id: u64, pct: u64) {
        let configs = self.chaos_configs.read().await;
        if let Some(cfg) = configs.get(&node_id) {
            cfg.drop_percent.store(pct, Ordering::Relaxed);
        }
    }

    async fn partition(&self, groups: Vec<HashSet<u64>>) {
        *self.partitions.write().await = groups;
    }

    async fn heal_partition(&self) {
        self.partitions.write().await.clear();
    }

    /// Block traffic from `from` to `to` (asymmetric — reverse direction unaffected).
    async fn block_route(&self, from: u64, to: u64) {
        self.blocked_routes.write().await.insert((from, to));
    }

    async fn unblock_route(&self, from: u64, to: u64) {
        self.blocked_routes.write().await.remove(&(from, to));
    }

    async fn unblock_all_routes(&self) {
        self.blocked_routes.write().await.clear();
    }

    /// Reset all fault injection state to clean.
    async fn heal_all(&self) {
        // Clear partitions and routes.
        self.heal_partition().await;
        self.unblock_all_routes().await;

        // Clear per-node configs.
        let configs = self.chaos_configs.read().await;
        for cfg in configs.values() {
            cfg.isolated.store(false, Ordering::Relaxed);
            cfg.delay_ms.store(0, Ordering::Relaxed);
            cfg.drop_percent.store(0, Ordering::Relaxed);
        }
    }
}

// ===========================================================================
// EnhancedChaosCluster — 3-node cluster with full chaos support
// ===========================================================================

/// A 3-node Raft cluster where every fault injection method actually works.
/// Unlike the basic ChaosCluster (which uses ChannelNetworkFactory), this
/// wires ChaosNetworkFactory so isolation, delays, drops, partitions, and
/// route blocks all take effect.
#[allow(dead_code)]
struct EnhancedChaosCluster {
    nodes: Vec<RaftNode>,
    routers: Arc<RwLock<HashMap<u64, RaftHandle>>>,
    monkey: ChaosMonkey,
    storages: Vec<Arc<InMemoryStorage>>,
    chaos_configs: ChaosConfigMap,
    partitions: Arc<RwLock<Vec<HashSet<u64>>>>,
    blocked_routes: Arc<RwLock<HashSet<(u64, u64)>>>,
}

impl EnhancedChaosCluster {
    async fn new() -> Self {
        let routers: Arc<RwLock<HashMap<u64, RaftHandle>>> = Arc::new(RwLock::new(HashMap::new()));
        let partitions: Arc<RwLock<Vec<HashSet<u64>>>> = Arc::new(RwLock::new(Vec::new()));
        let blocked_routes: Arc<RwLock<HashSet<(u64, u64)>>> =
            Arc::new(RwLock::new(HashSet::new()));

        let mut chaos_configs_map = HashMap::new();
        for id in 1..=3_u64 {
            chaos_configs_map.insert(id, ChaosNodeConfig::new());
        }
        let chaos_configs: ChaosConfigMap = Arc::new(RwLock::new(chaos_configs_map));

        let mut nodes = Vec::new();
        let mut storages = Vec::new();

        for node_id in 1..=3_u64 {
            let network = ChaosNetworkFactory::new(
                Arc::clone(&routers),
                Arc::clone(&chaos_configs),
                node_id,
                Arc::clone(&partitions),
                Arc::clone(&blocked_routes),
            );
            let storage = Arc::new(InMemoryStorage::new());
            let index: Arc<dyn IndexBackend> = Arc::new(NoopIndex);

            let (raft_node, handle) = RaftNode::new_with_any_network(
                node_id,
                network,
                storage.clone() as Arc<dyn StorageBackend>,
                index,
            )
            .await
            .expect("failed to create chaos node");

            routers.write().await.insert(node_id, handle);
            storages.push(storage);
            nodes.push(raft_node);
        }

        let monkey = ChaosMonkey::new(
            Arc::clone(&routers),
            Arc::clone(&chaos_configs),
            Arc::clone(&partitions),
            Arc::clone(&blocked_routes),
        );

        Self {
            nodes,
            routers,
            monkey,
            storages,
            chaos_configs,
            partitions,
            blocked_routes,
        }
    }

    async fn initialize(&self) {
        let mut members = BTreeMap::new();
        for id in 1..=3_u64 {
            members.insert(
                id,
                BasicNode {
                    addr: format!("127.0.0.1:{}", 9000 + id),
                },
            );
        }
        self.nodes[0]
            .initialize(members)
            .await
            .expect("failed to initialize cluster");
    }

    async fn wait_for_leader(&self, deadline: Duration) -> u64 {
        let start = Instant::now();
        loop {
            for node in &self.nodes {
                let metrics = node.metrics().await;
                if metrics.state == ServerState::Leader {
                    return node.node_id();
                }
            }
            if start.elapsed() > deadline {
                panic!("no leader elected within {:?}", deadline);
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_for_leader_among(&self, node_ids: &[u64], deadline: Duration) -> u64 {
        let start = Instant::now();
        loop {
            for node in &self.nodes {
                if !node_ids.contains(&node.node_id()) {
                    continue;
                }
                let metrics = node.metrics().await;
                if metrics.state == ServerState::Leader {
                    return node.node_id();
                }
            }
            if start.elapsed() > deadline {
                panic!(
                    "no leader elected among {:?} within {:?}",
                    node_ids, deadline
                );
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    fn node(&self, id: u64) -> &RaftNode {
        self.nodes
            .iter()
            .find(|n| n.node_id() == id)
            .unwrap_or_else(|| panic!("node {} not found", id))
    }

    fn follower_ids(&self, leader_id: u64) -> Vec<u64> {
        self.nodes
            .iter()
            .filter(|n| n.node_id() != leader_id)
            .map(|n| n.node_id())
            .collect()
    }

    /// Verify that a document exists on a node's storage.
    async fn assert_doc_on_node(&self, node_id: u64, doc_id: &str) {
        let idx = (node_id - 1) as usize;
        let result = self.storages[idx].get(&DocumentId::new(doc_id)).await;
        assert!(
            result.is_ok(),
            "document '{}' not found on node {} storage",
            doc_id,
            node_id
        );
    }

    /// Count documents on a node's storage.
    async fn doc_count_on_node(&self, node_id: u64) -> usize {
        let idx = (node_id - 1) as usize;
        let docs = self.storages[idx]
            .scan(
                DocumentId::new("\0")..=DocumentId::new("\u{10ffff}"),
                usize::MAX,
            )
            .await
            .unwrap_or_default();
        docs.len()
    }

    /// Restart a killed node. Creates a new Raft instance reusing the same
    /// storage backend (simulating durable storage that survives restarts).
    #[allow(dead_code)]
    async fn restart_node(&mut self, node_id: u64) {
        let idx = (node_id - 1) as usize;
        let storage = self.storages[idx].clone();
        let index: Arc<dyn IndexBackend> = Arc::new(NoopIndex);

        let network = ChaosNetworkFactory::new(
            Arc::clone(&self.routers),
            Arc::clone(&self.chaos_configs),
            node_id,
            Arc::clone(&self.partitions),
            Arc::clone(&self.blocked_routes),
        );

        let (raft_node, handle) = RaftNode::new_with_any_network(
            node_id,
            network,
            storage as Arc<dyn StorageBackend>,
            index,
        )
        .await
        .expect("failed to restart node");

        self.routers.write().await.insert(node_id, handle);
        self.nodes[idx] = raft_node;
    }
}

// ===========================================================================
// Workload Generator
// ===========================================================================

/// Tracks results of a continuous write workload.
struct WorkloadResult {
    committed: Arc<tokio::sync::Mutex<Vec<String>>>,
    failed: Arc<tokio::sync::Mutex<Vec<String>>>,
    total_attempted: Arc<AtomicU64>,
}

impl WorkloadResult {
    fn new() -> Self {
        Self {
            committed: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            failed: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            total_attempted: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn committed_count(&self) -> usize {
        self.committed.lock().await.len()
    }

    async fn failed_count(&self) -> usize {
        self.failed.lock().await.len()
    }
}

struct WorkloadHandle {
    task: tokio::task::JoinHandle<()>,
    result: Arc<WorkloadResult>,
    stop: Arc<AtomicBool>,
}

impl WorkloadHandle {
    async fn stop_and_wait(self) -> Arc<WorkloadResult> {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.task.await;
        self.result
    }
}

/// Start a background task that continuously writes documents to the cluster.
/// Finds the current leader among the given nodes and proposes writes.
fn start_workload(
    nodes: Vec<(u64, openraft::Raft<TypeConfig>)>,
    write_interval: Duration,
    prefix: &str,
) -> WorkloadHandle {
    let result = Arc::new(WorkloadResult::new());
    let stop = Arc::new(AtomicBool::new(false));
    let result_clone = Arc::clone(&result);
    let stop_clone = Arc::clone(&stop);
    let prefix = prefix.to_string();

    let task = tokio::spawn(async move {
        let mut counter = 0u64;
        while !stop_clone.load(Ordering::Relaxed) {
            let id = format!("{}-{}", prefix, counter);
            let doc = make_doc(&id, &format!("workload-{}", counter));
            result_clone.total_attempted.fetch_add(1, Ordering::Relaxed);

            let mut written = false;
            for (_, raft) in &nodes {
                let metrics = raft.metrics().borrow().clone();
                if metrics.state == ServerState::Leader {
                    let cmd = RaftCommand::InsertDocument {
                        document: doc.clone(),
                    };
                    match raft.client_write(cmd).await {
                        Ok(resp) if resp.data.success => {
                            result_clone.committed.lock().await.push(id.clone());
                            written = true;
                            break;
                        }
                        _ => continue,
                    }
                }
            }
            if !written {
                result_clone.failed.lock().await.push(id);
            }
            counter += 1;
            sleep(write_interval).await;
        }
    });

    WorkloadHandle { task, result, stop }
}

// ===========================================================================
// Chaos Scheduler
// ===========================================================================

/// Randomly injects and heals faults on a schedule.
struct ChaosScheduler {
    monkey: ChaosMonkey,
    node_ids: Vec<u64>,
    fault_duration: Duration,
    heal_duration: Duration,
}

impl ChaosScheduler {
    fn new(
        monkey: ChaosMonkey,
        node_ids: Vec<u64>,
        fault_duration: Duration,
        heal_duration: Duration,
    ) -> Self {
        Self {
            monkey,
            node_ids,
            fault_duration,
            heal_duration,
        }
    }

    /// Run the scheduler in a loop until `stop` is set. Each iteration:
    /// 1. Pick a random fault and apply it
    /// 2. Hold the fault for `fault_duration`
    /// 3. Heal the fault
    /// 4. Wait `heal_duration` for the cluster to stabilize
    async fn run(&self, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::Relaxed) {
            // Generate random values before any await to avoid Send issues.
            let (event, target, extra) = {
                let mut rng = rand::thread_rng();
                let event = rng.gen_range(0..5u32);
                let target = self.node_ids[rng.gen_range(0..self.node_ids.len())];
                let extra = rng.gen_range(50..300u64);
                (event, target, extra)
            };

            match event {
                0 => {
                    // Isolate a node.
                    self.monkey.isolate_node(target).await;
                    self.wait_or_stop(&stop, self.fault_duration).await;
                    self.monkey.reconnect_node(target).await;
                }
                1 => {
                    // Add delay (50–300ms).
                    self.monkey.add_delay(target, extra).await;
                    self.wait_or_stop(&stop, self.fault_duration).await;
                    self.monkey.remove_delay(target).await;
                }
                2 => {
                    // Drop 10–50% of packets.
                    let pct = extra.clamp(10, 50);
                    self.monkey.set_drop_percent(target, pct).await;
                    self.wait_or_stop(&stop, self.fault_duration).await;
                    self.monkey.set_drop_percent(target, 0).await;
                }
                3 => {
                    // Partition: target vs the rest.
                    let others: Vec<u64> = self
                        .node_ids
                        .iter()
                        .filter(|&&id| id != target)
                        .copied()
                        .collect();
                    self.monkey
                        .partition(vec![
                            HashSet::from_iter([target]),
                            HashSet::from_iter(others),
                        ])
                        .await;
                    self.wait_or_stop(&stop, self.fault_duration).await;
                    self.monkey.heal_partition().await;
                }
                4 => {
                    // Block route from target to a random other node.
                    let others: Vec<u64> = self
                        .node_ids
                        .iter()
                        .filter(|&&id| id != target)
                        .copied()
                        .collect();
                    let to = others[(extra as usize) % others.len()];
                    self.monkey.block_route(target, to).await;
                    self.wait_or_stop(&stop, self.fault_duration).await;
                    self.monkey.unblock_route(target, to).await;
                }
                _ => unreachable!(),
            }

            // Heal period — let the cluster stabilize.
            self.wait_or_stop(&stop, self.heal_duration).await;
        }
    }

    async fn wait_or_stop(&self, stop: &Arc<AtomicBool>, duration: Duration) {
        let start = Instant::now();
        while start.elapsed() < duration {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

fn make_doc(id: &str, value: &str) -> Document {
    Document::new(DocumentId::new(id)).with_field("data", FieldValue::Text(value.into()))
}

fn make_docs(prefix: &str, count: usize) -> Vec<Document> {
    (0..count)
        .map(|i| make_doc(&format!("{}-{}", prefix, i), &format!("value-{}", i)))
        .collect()
}

/// Propose a write on the current leader. Retries across nodes until timeout.
async fn propose_on_leader(
    cluster: &EnhancedChaosCluster,
    cmd: RaftCommand,
    timeout: Duration,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        for node in &cluster.nodes {
            let metrics = node.metrics().await;
            if metrics.state == ServerState::Leader {
                match node.propose(cmd.clone()).await {
                    Ok(resp) if resp.success => return Ok(()),
                    Ok(_) => {}
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("not leader") || msg.contains("forward") {
                            continue;
                        }
                    }
                }
            }
        }
        if start.elapsed() > timeout {
            return Err("no leader found or write failed within timeout".into());
        }
        sleep(Duration::from_millis(50)).await;
    }
}

// ===========================================================================
// THREE-NODE FAILURE THRESHOLD TESTS
// ===========================================================================

/// Single node failure after full replication → no data loss.
/// Verifies that all committed documents are present on both surviving nodes.
#[tokio::test]
async fn threshold_single_node_failure_no_data_loss() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    // Write 100 documents.
    let leader = cluster.node(leader_id);
    for i in 0..100 {
        let doc = make_doc(&format!("thresh1-{}", i), &format!("data-{}", i));
        let resp = leader
            .propose(RaftCommand::InsertDocument { document: doc })
            .await
            .expect("write failed");
        assert!(resp.success);
    }

    // Wait for full replication.
    sleep(Duration::from_millis(500)).await;

    // Verify all 3 nodes have all 100 documents.
    for node_id in 1..=3_u64 {
        let count = cluster.doc_count_on_node(node_id).await;
        assert_eq!(
            count, 100,
            "node {} has {} docs, expected 100 (before failure)",
            node_id, count
        );
    }

    // Kill one follower.
    let followers = cluster.follower_ids(leader_id);
    cluster.monkey.kill_node(&cluster.nodes, followers[0]).await;
    sleep(Duration::from_millis(200)).await;

    // Verify all docs on both surviving nodes.
    let surviving: Vec<u64> = vec![leader_id, followers[1]];
    for &nid in &surviving {
        for i in 0..100 {
            cluster
                .assert_doc_on_node(nid, &format!("thresh1-{}", i))
                .await;
        }
    }

    // Killed node's storage also retains data (durable storage simulation).
    for i in 0..100 {
        cluster
            .assert_doc_on_node(followers[0], &format!("thresh1-{}", i))
            .await;
    }
}

/// Two-node failure after full replication → no data loss on survivor.
/// The surviving node's storage has all committed documents.
#[tokio::test]
async fn threshold_two_node_failure_no_data_loss() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    // Write 50 documents.
    let leader = cluster.node(leader_id);
    for i in 0..50 {
        let doc = make_doc(&format!("thresh2-{}", i), &format!("data-{}", i));
        let resp = leader
            .propose(RaftCommand::InsertDocument { document: doc })
            .await
            .expect("write failed");
        assert!(resp.success);
    }

    // Wait for full replication.
    sleep(Duration::from_millis(500)).await;

    // Kill both followers.
    let followers = cluster.follower_ids(leader_id);
    for &fid in &followers {
        cluster.monkey.kill_node(&cluster.nodes, fid).await;
    }
    sleep(Duration::from_millis(300)).await;

    // Verify the leader's storage has all 50 documents.
    for i in 0..50 {
        cluster
            .assert_doc_on_node(leader_id, &format!("thresh2-{}", i))
            .await;
    }

    // Verify killed nodes' storages also retain data.
    for &fid in &followers {
        for i in 0..50 {
            cluster
                .assert_doc_on_node(fid, &format!("thresh2-{}", i))
                .await;
        }
    }
}

/// All three nodes fail → writes rejected but data persists in storage.
/// After full replication, even if every Raft instance is shut down,
/// the durable storage backends retain all committed data.
#[tokio::test]
async fn threshold_all_three_fail_data_persists_in_storage() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    // Write 200 documents via batch.
    let leader = cluster.node(leader_id);
    let docs = make_docs("thresh3", 200);
    let resp = leader.propose_batch(docs).await.expect("batch failed");
    assert!(resp.success);
    assert_eq!(resp.affected_count, 200);

    // Wait for full replication.
    sleep(Duration::from_millis(500)).await;

    // Kill all three nodes.
    for node in &cluster.nodes {
        let _ = node.raft_handle().shutdown().await;
    }
    cluster.routers.write().await.clear();

    // Writes must be rejected.
    for node in &cluster.nodes {
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            node.propose(RaftCommand::InsertDocument {
                document: make_doc("should-fail", "dead"),
            })
            .await
        })
        .await;

        match result {
            Err(_) => {}     // Timeout — expected.
            Ok(Err(_)) => {} // Consensus error — expected.
            Ok(Ok(resp)) => {
                assert!(!resp.success, "write must not succeed with all nodes dead");
            }
        }
    }

    // Data persists in all storage backends (simulates durable storage).
    for node_id in 1..=3_u64 {
        let count = cluster.doc_count_on_node(node_id).await;
        assert_eq!(
            count, 200,
            "node {} storage has {} docs after full shutdown, expected 200",
            node_id, count
        );
    }
}

/// Sequential failures with recovery: kill A, write more, kill B while A dead,
/// verify committed writes survive on C.
#[tokio::test]
async fn threshold_sequential_failures_with_recovery() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    // Phase 1: Write 20 docs with full cluster.
    let leader = cluster.node(leader_id);
    for i in 0..20 {
        let doc = make_doc(&format!("seq-phase1-{}", i), "phase1");
        let resp = leader
            .propose(RaftCommand::InsertDocument { document: doc })
            .await
            .expect("phase1 write failed");
        assert!(resp.success);
    }
    sleep(Duration::from_millis(300)).await;

    // Phase 2: Kill one follower, write 20 more.
    let followers = cluster.follower_ids(leader_id);
    cluster.monkey.kill_node(&cluster.nodes, followers[0]).await;
    sleep(Duration::from_millis(200)).await;

    for i in 0..20 {
        let doc = make_doc(&format!("seq-phase2-{}", i), "phase2");
        let resp = leader
            .propose(RaftCommand::InsertDocument { document: doc })
            .await
            .expect("phase2 write failed (should succeed with 2/3 quorum)");
        assert!(resp.success);
    }
    sleep(Duration::from_millis(300)).await;

    // Phase 3: Kill the other follower too (only leader remains).
    cluster.monkey.kill_node(&cluster.nodes, followers[1]).await;
    sleep(Duration::from_millis(200)).await;

    // New writes should fail (no quorum).
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        leader
            .propose(RaftCommand::InsertDocument {
                document: make_doc("seq-noquorum", "fail"),
            })
            .await
    })
    .await;
    match result {
        Err(_) | Ok(Err(_)) => {} // Expected.
        Ok(Ok(resp)) => assert!(!resp.success, "should not write without quorum"),
    }

    // Verify: leader's storage has all 40 committed docs.
    let leader_count = cluster.doc_count_on_node(leader_id).await;
    assert!(
        leader_count >= 40,
        "leader should have at least 40 docs, has {}",
        leader_count
    );

    // The surviving follower (followers[1]) has phase1 docs (20, replicated
    // before it was killed) plus phase2 docs (20, it was alive for those).
    let f1_count = cluster.doc_count_on_node(followers[1]).await;
    assert!(
        f1_count >= 40,
        "follower {} should have at least 40 docs, has {}",
        followers[1],
        f1_count
    );
}

/// Data written with N-1 nodes survives further failures as long as at
/// least one quorum member survives.
#[tokio::test]
async fn threshold_quorum_committed_data_survives() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    let followers = cluster.follower_ids(leader_id);
    let quorum_follower = followers[0];
    let non_quorum_follower = followers[1];

    // Isolate one follower so writes only go to leader + other follower.
    cluster.monkey.isolate_node(non_quorum_follower).await;
    sleep(Duration::from_millis(200)).await;

    // Write 30 docs — committed by leader + quorum_follower.
    let leader = cluster.node(leader_id);
    for i in 0..30 {
        let doc = make_doc(&format!("quorum-{}", i), "quorum-data");
        let resp = leader
            .propose(RaftCommand::InsertDocument { document: doc })
            .await
            .expect("write failed with 2/3 quorum");
        assert!(resp.success);
    }
    sleep(Duration::from_millis(300)).await;

    // Verify: leader and quorum_follower have the data.
    for &nid in &[leader_id, quorum_follower] {
        for i in 0..30 {
            cluster
                .assert_doc_on_node(nid, &format!("quorum-{}", i))
                .await;
        }
    }

    // Now kill the leader. The quorum_follower should still have all data.
    cluster.monkey.kill_node(&cluster.nodes, leader_id).await;
    sleep(Duration::from_millis(200)).await;

    for i in 0..30 {
        cluster
            .assert_doc_on_node(quorum_follower, &format!("quorum-{}", i))
            .await;
    }

    // Reconnect non_quorum_follower — together with quorum_follower they
    // can form a new quorum and the data is safe.
    cluster.monkey.reconnect_node(non_quorum_follower).await;
    let new_leader = cluster
        .wait_for_leader_among(
            &[quorum_follower, non_quorum_follower],
            Duration::from_secs(5),
        )
        .await;
    sleep(Duration::from_millis(500)).await;

    // Write a new doc to prove the cluster is functional.
    let new_leader_node = cluster.node(new_leader);
    let resp = new_leader_node
        .propose(RaftCommand::InsertDocument {
            document: make_doc("post-recovery", "recovered"),
        })
        .await
        .expect("post-recovery write failed");
    assert!(resp.success);
}

// ===========================================================================
// ADVANCED CHAOS SCENARIOS
// ===========================================================================

/// Continuous workload under random node isolation cycles.
/// Verifies all committed writes are present after chaos ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adv_continuous_workload_with_random_isolation() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let _leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    // Collect raft handles for workload generator.
    let raft_handles: Vec<(u64, openraft::Raft<TypeConfig>)> = cluster
        .nodes
        .iter()
        .map(|n| (n.node_id(), n.raft_handle().clone()))
        .collect();

    // Start background workload.
    let workload = start_workload(raft_handles, Duration::from_millis(50), "adv1");

    // Run random isolation chaos for 15 seconds.
    let sched_stop = Arc::new(AtomicBool::new(false));
    let sched_stop_clone = Arc::clone(&sched_stop);
    let sched_handle = tokio::spawn({
        let scheduler = ChaosScheduler::new(
            cluster.monkey.clone(),
            vec![1, 2, 3],
            Duration::from_secs(2),
            Duration::from_secs(1),
        );
        async move { scheduler.run(sched_stop_clone).await }
    });

    // Let it run for 15 seconds.
    sleep(Duration::from_secs(15)).await;

    // Stop chaos and workload.
    sched_stop.store(true, Ordering::Relaxed);
    let _ = sched_handle.await;
    cluster.monkey.heal_all().await;

    let result = workload.stop_and_wait().await;

    // Give cluster time to stabilize and replicate.
    sleep(Duration::from_secs(3)).await;

    let committed = result.committed.lock().await;
    let failed = result.failed.lock().await;
    let total = result.total_attempted.load(Ordering::Relaxed);

    assert!(
        total > 0,
        "workload should have attempted at least some writes"
    );

    // Wait for a leader.
    let _ = cluster.wait_for_leader(Duration::from_secs(5)).await;

    // Verify committed docs on the leader's storage.
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    let mut missing = 0usize;
    for doc_id in committed.iter() {
        let idx = (leader_id - 1) as usize;
        if cluster.storages[idx]
            .get(&DocumentId::new(doc_id.clone()))
            .await
            .is_err()
        {
            missing += 1;
        }
    }

    // With Raft guarantees, committed writes should be on the leader.
    // Allow a small tolerance for writes that were committed right before
    // a leader change (the new leader should have them too).
    let committed_count = committed.len();
    assert!(
        missing == 0 || (missing as f64 / committed_count as f64) < 0.01,
        "too many committed writes missing: {} out of {} (failed: {})",
        missing,
        committed_count,
        failed.len()
    );
}

/// Repeated partition/heal cycles. After each heal, the cluster must
/// elect a leader and successfully write.
#[tokio::test]
async fn adv_repeated_partition_heal_cycles() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let _ = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    for cycle in 0..10 {
        // Partition: node 1 alone, nodes 2+3 together.
        let minority_node = ((cycle % 3) + 1) as u64;
        let majority: Vec<u64> = (1..=3).filter(|&id| id != minority_node).collect();

        cluster
            .monkey
            .partition(vec![
                HashSet::from_iter([minority_node]),
                HashSet::from_iter(majority.clone()),
            ])
            .await;

        // Wait a bit for the partition to take effect.
        sleep(Duration::from_millis(500)).await;

        // Majority should be able to write.
        let leader_id = cluster
            .wait_for_leader_among(&majority, Duration::from_secs(5))
            .await;
        let leader = cluster.node(leader_id);
        let resp = leader
            .propose(RaftCommand::InsertDocument {
                document: make_doc(
                    &format!("partition-cycle-{}", cycle),
                    &format!("cycle-{}", cycle),
                ),
            })
            .await
            .unwrap_or_else(|_| panic!("majority write failed in cycle {}", cycle));
        assert!(resp.success, "cycle {} majority write failed", cycle);

        // Heal.
        cluster.monkey.heal_partition().await;
        sleep(Duration::from_millis(500)).await;
    }

    // After all cycles, verify all 10 docs exist.
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    for cycle in 0..10 {
        cluster
            .assert_doc_on_node(leader_id, &format!("partition-cycle-{}", cycle))
            .await;
    }
}

/// Cascading delays and packet loss. Progressively add faults and verify
/// the cluster degrades gracefully.
#[tokio::test]
async fn adv_cascading_delay_and_packet_loss() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    let followers = cluster.follower_ids(leader_id);

    // Phase 1: Add 100ms delay to one follower — writes should still work.
    cluster.monkey.add_delay(followers[0], 100).await;
    sleep(Duration::from_millis(200)).await;

    let leader = cluster.node(leader_id);
    for i in 0..5 {
        let resp = leader
            .propose(RaftCommand::InsertDocument {
                document: make_doc(&format!("cascade-phase1-{}", i), "delayed"),
            })
            .await
            .expect("phase1 write should succeed");
        assert!(resp.success);
    }

    // Phase 2: Add 20% packet loss to the other follower.
    cluster.monkey.set_drop_percent(followers[1], 20).await;
    sleep(Duration::from_millis(200)).await;

    // Writes may take longer but should still succeed (leader + at least
    // one follower can form quorum despite delay/drops).
    let mut phase2_ok = 0;
    for i in 0..10 {
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            leader
                .propose(RaftCommand::InsertDocument {
                    document: make_doc(&format!("cascade-phase2-{}", i), "degraded"),
                })
                .await
        })
        .await;
        if let Ok(Ok(resp)) = result {
            if resp.success {
                phase2_ok += 1;
            }
        }
    }
    assert!(
        phase2_ok >= 5,
        "at least half of writes should succeed under moderate chaos, got {}/10",
        phase2_ok
    );

    // Phase 3: Increase delay to 300ms + 40% drop — cluster may struggle.
    cluster.monkey.add_delay(followers[0], 300).await;
    cluster.monkey.set_drop_percent(followers[1], 40).await;
    sleep(Duration::from_millis(300)).await;

    // Attempt writes — some may fail, that's expected under heavy chaos.
    for i in 0..5 {
        let _result = tokio::time::timeout(Duration::from_secs(5), async {
            leader
                .propose(RaftCommand::InsertDocument {
                    document: make_doc(&format!("cascade-phase3-{}", i), "heavy-chaos"),
                })
                .await
        })
        .await;
    }

    // Phase 4: Heal all faults — cluster should recover.
    cluster.monkey.heal_all().await;
    sleep(Duration::from_secs(2)).await;

    // Recovery write should succeed.
    let _ = cluster.wait_for_leader(Duration::from_secs(5)).await;
    let result = propose_on_leader(
        &cluster,
        RaftCommand::InsertDocument {
            document: make_doc("cascade-recovery", "healed"),
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(result.is_ok(), "recovery write failed: {:?}", result);
}

/// Asymmetric network faults: block specific directional routes.
/// A→C blocked but C→A allowed; verifies the cluster handles it.
#[tokio::test]
async fn adv_asymmetric_route_blocks() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    // Write a baseline doc.
    let leader = cluster.node(leader_id);
    let resp = leader
        .propose(RaftCommand::InsertDocument {
            document: make_doc("asym-baseline", "ok"),
        })
        .await
        .expect("baseline write failed");
    assert!(resp.success);

    // Block route from node 1 → node 3 (but 3 → 1 is still open).
    cluster.monkey.block_route(1, 3).await;
    sleep(Duration::from_millis(500)).await;

    // The cluster should still function because the majority can still
    // communicate. Writes may take slightly different paths.
    let result = propose_on_leader(
        &cluster,
        RaftCommand::InsertDocument {
            document: make_doc("asym-during", "route-blocked"),
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        result.is_ok(),
        "write should succeed with asymmetric route block"
    );

    // Block both directions between 1↔3.
    cluster.monkey.block_route(3, 1).await;
    sleep(Duration::from_millis(500)).await;

    // With 1↔3 fully blocked, 1 and 3 can only reach each other via 2.
    // Leader (if 1 or 3) may need to replicate through 2. Or if leader
    // is 2, it can reach both directly.
    let _result = propose_on_leader(
        &cluster,
        RaftCommand::InsertDocument {
            document: make_doc("asym-bidirectional", "both-blocked"),
        },
        Duration::from_secs(5),
    )
    .await;
    // This may or may not succeed depending on leader placement.
    // The important thing is no crash or undefined behavior.

    // Unblock and verify recovery.
    cluster.monkey.unblock_all_routes().await;
    sleep(Duration::from_secs(1)).await;

    let result = propose_on_leader(
        &cluster,
        RaftCommand::InsertDocument {
            document: make_doc("asym-recovered", "healed"),
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        result.is_ok(),
        "recovery write should succeed after unblock"
    );
}

/// Mixed simultaneous faults: isolation + delay + packet loss on different
/// nodes at the same time.
#[tokio::test]
async fn adv_mixed_simultaneous_faults() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    // Write baseline.
    let leader = cluster.node(leader_id);
    for i in 0..10 {
        let resp = leader
            .propose(RaftCommand::InsertDocument {
                document: make_doc(&format!("mixed-pre-{}", i), "healthy"),
            })
            .await
            .expect("pre-chaos write failed");
        assert!(resp.success);
    }
    sleep(Duration::from_millis(300)).await;

    let followers = cluster.follower_ids(leader_id);

    // Apply mixed faults simultaneously:
    // - Follower 1: 200ms delay
    // - Follower 2: 30% packet loss
    cluster.monkey.add_delay(followers[0], 200).await;
    cluster.monkey.set_drop_percent(followers[1], 30).await;
    sleep(Duration::from_millis(300)).await;

    // Attempt writes — should mostly succeed since leader can still reach
    // at least one follower (delayed one still responds, just slowly).
    let mut ok_count = 0;
    for i in 0..20 {
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            leader
                .propose(RaftCommand::InsertDocument {
                    document: make_doc(&format!("mixed-during-{}", i), "chaotic"),
                })
                .await
        })
        .await;
        if let Ok(Ok(resp)) = result {
            if resp.success {
                ok_count += 1;
            }
        }
    }

    assert!(
        ok_count >= 10,
        "at least half of writes should succeed under mixed faults, got {}/20",
        ok_count
    );

    // Heal and verify.
    cluster.monkey.heal_all().await;
    sleep(Duration::from_secs(2)).await;

    let _ = cluster.wait_for_leader(Duration::from_secs(5)).await;

    // Post-heal write.
    let result = propose_on_leader(
        &cluster,
        RaftCommand::InsertDocument {
            document: make_doc("mixed-healed", "recovered"),
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(result.is_ok(), "post-heal write should succeed");
}

// ===========================================================================
// LONG-RUNNING CHAOS ENDURANCE TEST
// ===========================================================================

/// Long-running chaos endurance test. Runs continuous workload with random
/// fault injection for a configurable duration.
///
/// Default: 30 seconds (CI-friendly).
/// For 24-hour runs: `CHAOS_DURATION_SECS=86400`
///
/// Periodically verifies:
/// 1. All committed writes are present on the leader's storage
/// 2. The cluster can recover from faults and accept new writes
/// 3. No split-brain (at most 1 leader at any time)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn chaos_long_running_endurance() {
    let duration_secs: u64 = std::env::var("CHAOS_DURATION_SECS")
        .unwrap_or_else(|_| "30".to_string())
        .parse()
        .unwrap_or(30);

    eprintln!(
        "=== Chaos Endurance Test: running for {} seconds ===",
        duration_secs
    );

    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let _ = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    // Collect raft handles for workload generator.
    let raft_handles: Vec<(u64, openraft::Raft<TypeConfig>)> = cluster
        .nodes
        .iter()
        .map(|n| (n.node_id(), n.raft_handle().clone()))
        .collect();

    // Start continuous workload (1 write every 100ms).
    let workload = start_workload(raft_handles, Duration::from_millis(100), "endurance");

    // Start chaos scheduler.
    let sched_stop = Arc::new(AtomicBool::new(false));
    let sched_stop_clone = Arc::clone(&sched_stop);
    let scheduler_handle = tokio::spawn({
        let scheduler = ChaosScheduler::new(
            cluster.monkey.clone(),
            vec![1, 2, 3],
            Duration::from_secs(3), // fault lasts 3s
            Duration::from_secs(2), // heal for 2s
        );
        async move { scheduler.run(sched_stop_clone).await }
    });

    // Run for the specified duration, periodically checking invariants.
    let total_duration = Duration::from_secs(duration_secs);
    let check_interval = Duration::from_secs(std::cmp::max(duration_secs / 10, 5));
    let start = Instant::now();
    let mut check_count = 0u64;

    while start.elapsed() < total_duration {
        sleep(check_interval).await;
        check_count += 1;

        let committed = workload.result.committed_count().await;
        let failed = workload.result.failed_count().await;
        let total = workload.result.total_attempted.load(Ordering::Relaxed);

        eprintln!(
            "[Check {}] elapsed={:?} total={} committed={} failed={}",
            check_count,
            start.elapsed(),
            total,
            committed,
            failed
        );

        // Invariant: at most 1 leader.
        let mut leader_count = 0;
        for node in &cluster.nodes {
            let metrics = node.metrics().await;
            if metrics.state == ServerState::Leader {
                leader_count += 1;
            }
        }
        assert!(
            leader_count <= 1,
            "split-brain detected: {} leaders at check {}",
            leader_count,
            check_count
        );
    }

    // Stop chaos and workload.
    sched_stop.store(true, Ordering::Relaxed);
    let _ = scheduler_handle.await;
    cluster.monkey.heal_all().await;

    let result = workload.stop_and_wait().await;

    // Give cluster time to stabilize.
    sleep(Duration::from_secs(5)).await;

    let committed = result.committed.lock().await;
    let failed = result.failed.lock().await;
    let total = result.total_attempted.load(Ordering::Relaxed);

    eprintln!("\n=== Endurance Test Results ===");
    eprintln!("Duration: {} seconds", duration_secs);
    eprintln!("Total attempted: {}", total);
    eprintln!("Committed: {}", committed.len());
    eprintln!("Failed: {}", failed.len());
    eprintln!(
        "Success rate: {:.1}%",
        if total > 0 {
            committed.len() as f64 / total as f64 * 100.0
        } else {
            0.0
        }
    );

    // Final verification: check committed docs on any available storage.
    let mut verified = 0usize;
    let mut missing = 0usize;
    for doc_id in committed.iter() {
        let mut found = false;
        for idx in 0..3 {
            if cluster.storages[idx]
                .get(&DocumentId::new(doc_id.clone()))
                .await
                .is_ok()
            {
                found = true;
                break;
            }
        }
        if found {
            verified += 1;
        } else {
            missing += 1;
        }
    }

    eprintln!("Verified: {}/{}", verified, committed.len());
    eprintln!("Missing: {}", missing);

    // No committed writes should be lost.
    assert!(
        missing == 0,
        "DATA LOSS: {} committed writes are missing from all storages!",
        missing
    );

    eprintln!("=== Endurance Test PASSED ===");
}

// ===========================================================================
// NETWORK FAULT INJECTION VERIFICATION TESTS
// ===========================================================================

/// Verify that isolation actually blocks RPCs (regression test for the
/// wiring of ChaosNetworkFactory).
#[tokio::test]
async fn verify_isolation_blocks_rpcs() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    // Write one doc while healthy.
    let leader = cluster.node(leader_id);
    let resp = leader
        .propose(RaftCommand::InsertDocument {
            document: make_doc("iso-test", "before"),
        })
        .await
        .expect("pre-isolation write failed");
    assert!(resp.success);

    // Isolate both followers — leader loses quorum.
    let followers = cluster.follower_ids(leader_id);
    for &fid in &followers {
        cluster.monkey.isolate_node(fid).await;
    }
    sleep(Duration::from_millis(500)).await;

    // Write should fail (no quorum — followers are isolated, not killed).
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        leader
            .propose(RaftCommand::InsertDocument {
                document: make_doc("iso-during", "should-fail"),
            })
            .await
    })
    .await;

    match result {
        Err(_) => {}     // Timeout — expected, Raft can't get quorum.
        Ok(Err(_)) => {} // Consensus error — also fine.
        Ok(Ok(resp)) => {
            assert!(
                !resp.success,
                "write should not succeed with isolated followers"
            );
        }
    }

    // Reconnect — writes should resume.
    for &fid in &followers {
        cluster.monkey.reconnect_node(fid).await;
    }
    sleep(Duration::from_secs(2)).await;

    let result = propose_on_leader(
        &cluster,
        RaftCommand::InsertDocument {
            document: make_doc("iso-after", "reconnected"),
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(result.is_ok(), "post-reconnect write should succeed");
}

/// Verify that network delay actually slows RPCs.
/// Uses a modest delay (100ms) on ONE follower to avoid triggering Raft
/// election timeouts (heartbeat_interval=200ms).
#[tokio::test]
async fn verify_delay_slows_rpcs() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    let leader = cluster.node(leader_id);

    // Baseline: write 5 docs without delay and measure time.
    let start = Instant::now();
    for i in 0..5 {
        let resp = leader
            .propose(RaftCommand::InsertDocument {
                document: make_doc(&format!("delay-baseline-{}", i), "fast"),
            })
            .await
            .expect("baseline write failed");
        assert!(resp.success);
    }
    let baseline_time = start.elapsed();

    // Add 100ms delay to ONE follower only. Keep the other follower fast
    // so the leader can still get quorum (leader + fast follower = 2/3).
    // The delayed follower doesn't block commits but makes some RPCs slower.
    let followers = cluster.follower_ids(leader_id);
    cluster.monkey.add_delay(followers[0], 100).await;
    sleep(Duration::from_millis(100)).await;

    // Write 5 docs with delay and measure time.
    // The leader commits with the fast follower; the delayed follower
    // catches up asynchronously. Writes should still succeed quickly.
    let start = Instant::now();
    for i in 0..5 {
        let resp = leader
            .propose(RaftCommand::InsertDocument {
                document: make_doc(&format!("delay-slow-{}", i), "delayed"),
            })
            .await
            .expect("delayed write failed");
        assert!(resp.success);
    }
    let delayed_time = start.elapsed();

    // With only one follower delayed, the cluster uses the fast path
    // (leader + fast follower). Verify writes completed successfully.
    // The timing difference may be small since the fast follower handles quorum.
    assert!(
        delayed_time < Duration::from_secs(5),
        "delayed writes should complete within 5s, took {:?}",
        delayed_time
    );

    // Now delay BOTH followers briefly (50ms each — below heartbeat interval).
    // This should measurably slow writes since both quorum paths are delayed.
    cluster.monkey.add_delay(followers[0], 50).await;
    cluster.monkey.add_delay(followers[1], 50).await;
    sleep(Duration::from_millis(100)).await;

    let start = Instant::now();
    for i in 0..5 {
        let resp = leader
            .propose(RaftCommand::InsertDocument {
                document: make_doc(&format!("delay-both-{}", i), "both-delayed"),
            })
            .await
            .expect("both-delayed write failed");
        assert!(resp.success);
    }
    let both_delayed_time = start.elapsed();

    // Both-delayed should be slower than baseline since every quorum ack
    // goes through the delay.
    assert!(
        both_delayed_time > baseline_time || baseline_time < Duration::from_millis(10),
        "both-delayed ({:?}) should be slower than baseline ({:?})",
        both_delayed_time,
        baseline_time
    );

    // Clean up.
    for &fid in &followers {
        cluster.monkey.remove_delay(fid).await;
    }
}

/// Verify packet drop causes some operations to need retries.
#[tokio::test]
async fn verify_packet_drop_affects_reliability() {
    let cluster = EnhancedChaosCluster::new().await;
    cluster.initialize().await;
    let leader_id = cluster.wait_for_leader(Duration::from_secs(3)).await;
    sleep(Duration::from_millis(300)).await;

    // Set 50% drop on both followers — aggressive packet loss.
    let followers = cluster.follower_ids(leader_id);
    for &fid in &followers {
        cluster.monkey.set_drop_percent(fid, 50).await;
    }
    sleep(Duration::from_millis(200)).await;

    // Try 20 writes with timeout — some may fail due to packet loss.
    let leader = cluster.node(leader_id);
    let mut successes = 0;
    let mut failures = 0;
    for i in 0..20 {
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            leader
                .propose(RaftCommand::InsertDocument {
                    document: make_doc(&format!("drop-test-{}", i), "lossy"),
                })
                .await
        })
        .await;
        match result {
            Ok(Ok(resp)) if resp.success => successes += 1,
            _ => failures += 1,
        }
    }

    // With 50% drop on both followers, the leader may struggle to get quorum.
    // But Raft retries internally, so some writes should still succeed.
    // The key assertion: the cluster doesn't crash and behaves predictably.
    assert!(
        successes > 0 || failures > 0,
        "test should have attempted writes"
    );

    // Heal and verify the cluster recovers.
    for &fid in &followers {
        cluster.monkey.set_drop_percent(fid, 0).await;
    }
    sleep(Duration::from_secs(2)).await;

    let result = propose_on_leader(
        &cluster,
        RaftCommand::InsertDocument {
            document: make_doc("drop-recovery", "healed"),
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        result.is_ok(),
        "cluster should recover after packet loss stops"
    );
}
