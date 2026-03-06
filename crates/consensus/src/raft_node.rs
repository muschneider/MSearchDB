//! High-level Raft node wrapper for MSearchDB.
//!
//! [`RaftNode`] wraps an [`openraft::Raft`] instance and provides a clean,
//! application-oriented API for proposing commands, querying leadership, and
//! managing cluster membership.
//!
//! # Usage
//!
//! ```ignore
//! let node = RaftNode::new(config, storage, index, network_factory).await?;
//!
//! // Only the leader can propose writes.
//! let response = node.propose(RaftCommand::InsertDocument { document }).await?;
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use openraft::{BasicNode, ChangeMembers, Config, Raft, ServerState};

use msearchdb_core::cluster::NodeAddress;
use msearchdb_core::config::NodeConfig;
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::traits::{IndexBackend, StorageBackend};

use crate::log_store::MemLogStore;
use crate::network::{ChannelNetworkFactory, StubNetworkFactory};
use crate::state_machine::DbStateMachine;
use crate::types::{RaftCommand, RaftResponse, TypeConfig};

// ---------------------------------------------------------------------------
// RaftNode
// ---------------------------------------------------------------------------

/// A high-level wrapper around an openraft `Raft` instance.
///
/// Provides application-level methods like [`propose`](RaftNode::propose) and
/// [`is_leader`](RaftNode::is_leader), hiding the generic openraft machinery.
pub struct RaftNode {
    /// The underlying openraft handle.
    raft: Raft<TypeConfig>,

    /// The node id of this Raft instance.
    node_id: u64,
}

impl RaftNode {
    /// Create a new Raft node with the **stub** network factory.
    ///
    /// This is suitable for single-node operation or when the real gRPC
    /// transport has not yet been wired.
    pub async fn new(
        config: &NodeConfig,
        storage: Arc<dyn StorageBackend>,
        index: Arc<dyn IndexBackend>,
    ) -> DbResult<Self> {
        let raft_config = Config {
            heartbeat_interval: 500,
            election_timeout_min: 1500,
            election_timeout_max: 3000,
            ..Default::default()
        };

        let raft_config = Arc::new(
            raft_config
                .validate()
                .map_err(|e| DbError::ConsensusError(format!("invalid raft config: {}", e)))?,
        );

        let log_store = MemLogStore::new();
        let state_machine = DbStateMachine::new(storage, index);
        let network = StubNetworkFactory;

        let node_id = config.node_id.as_u64();

        let raft = Raft::new(node_id, raft_config, network, log_store, state_machine)
            .await
            .map_err(|e| DbError::ConsensusError(format!("failed to create raft node: {}", e)))?;

        Ok(Self { raft, node_id })
    }

    /// Create a new Raft node with the **channel-based** in-process network.
    ///
    /// Used by integration tests to form a cluster without real I/O.
    pub async fn new_with_channel_network(
        node_id: u64,
        network: ChannelNetworkFactory,
    ) -> DbResult<(Self, Raft<TypeConfig>)> {
        let raft_config = Config {
            heartbeat_interval: 200,
            election_timeout_min: 500,
            election_timeout_max: 1000,
            ..Default::default()
        };

        let raft_config = Arc::new(
            raft_config
                .validate()
                .map_err(|e| DbError::ConsensusError(format!("invalid raft config: {}", e)))?,
        );

        let log_store = MemLogStore::new();

        // Use trivial in-memory mocks for the state machine backends.
        let storage: Arc<dyn StorageBackend> = Arc::new(InMemoryStorage::new());
        let index: Arc<dyn IndexBackend> = Arc::new(NoopIndex);
        let state_machine = DbStateMachine::new(storage, index);

        let raft = Raft::new(node_id, raft_config, network, log_store, state_machine)
            .await
            .map_err(|e| DbError::ConsensusError(format!("failed to create raft node: {}", e)))?;

        let handle = raft.clone();
        Ok((Self { raft, node_id }, handle))
    }

    /// Propose a [`RaftCommand`] through Raft consensus.
    ///
    /// Only the **leader** can accept proposals.  If this node is a follower,
    /// an error is returned containing a hint about which node is the current
    /// leader.
    ///
    /// The call blocks until the command has been committed by a quorum
    /// (at least 2 out of 3 nodes with `replication_factor = 3`).
    pub async fn propose(&self, cmd: RaftCommand) -> DbResult<RaftResponse> {
        let result = self.raft.client_write(cmd).await;

        match result {
            Ok(resp) => Ok(resp.data),
            Err(e) => match e.into_api_error() {
                Some(api_err) => {
                    use openraft::error::ClientWriteError;
                    match api_err {
                        ClientWriteError::ForwardToLeader(fwd) => {
                            let leader_hint = fwd
                                .leader_id
                                .map(|id| format!("node-{}", id))
                                .unwrap_or_else(|| "unknown".to_string());
                            Err(DbError::ConsensusError(format!(
                                "not leader; forward to {}",
                                leader_hint
                            )))
                        }
                        ClientWriteError::ChangeMembershipError(e) => Err(DbError::ConsensusError(
                            format!("membership change error: {}", e),
                        )),
                    }
                }
                None => Err(DbError::ConsensusError("fatal raft error".to_string())),
            },
        }
    }

    /// Add a learner node to the cluster.
    ///
    /// A learner receives replicated logs but does not vote in elections.
    /// Call [`change_membership`](RaftNode::change_membership) to promote it
    /// to a full voter.
    pub async fn add_learner(&self, node_id: u64, addr: &NodeAddress) -> DbResult<()> {
        let node = BasicNode {
            addr: format!("{}:{}", addr.host, addr.port),
        };
        self.raft
            .add_learner(node_id, node, true)
            .await
            .map_err(|e| DbError::ConsensusError(format!("add_learner failed: {}", e)))?;
        Ok(())
    }

    /// Change the voting membership of the cluster.
    ///
    /// The given `members` become the new voter set; nodes not listed become
    /// non-voters (learners).
    pub async fn change_membership(&self, members: Vec<u64>) -> DbResult<()> {
        let member_set: BTreeMap<u64, BasicNode> = members
            .into_iter()
            .map(|id| (id, BasicNode::default()))
            .collect();
        self.raft
            .change_membership(ChangeMembers::SetNodes(member_set), true)
            .await
            .map_err(|e| DbError::ConsensusError(format!("change_membership failed: {}", e)))?;
        Ok(())
    }

    /// Return the node id of the current leader, if known.
    pub fn current_leader(&self) -> Option<u64> {
        let metrics = self.raft.metrics().borrow().clone();
        metrics.current_leader
    }

    /// Return `true` if this node is currently the Raft leader.
    pub fn is_leader(&self) -> bool {
        let metrics = self.raft.metrics().borrow().clone();
        metrics.state == ServerState::Leader
    }

    /// Return the current Raft metrics snapshot.
    pub async fn metrics(&self) -> openraft::RaftMetrics<u64, BasicNode> {
        self.raft.metrics().borrow().clone()
    }

    /// Return this node's id.
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Return a reference to the underlying openraft handle.
    pub fn raft_handle(&self) -> &Raft<TypeConfig> {
        &self.raft
    }

    /// Initialise the cluster with the given set of voter nodes.
    ///
    /// Must only be called once on a fresh cluster.
    pub async fn initialize(&self, members: BTreeMap<u64, BasicNode>) -> DbResult<()> {
        self.raft
            .initialize(members)
            .await
            .map_err(|e| DbError::ConsensusError(format!("initialize failed: {}", e)))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// In-memory mock backends for channel-network tests
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use msearchdb_core::document::{Document, DocumentId};
use msearchdb_core::error::DbResult as CoreResult;
use msearchdb_core::query::{Query, SearchResult};
use std::collections::HashMap;
use std::ops::RangeInclusive;
use tokio::sync::Mutex;

/// Minimal in-memory storage backend used by [`RaftNode::new_with_channel_network`].
pub(crate) struct InMemoryStorage {
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
    async fn get(&self, id: &DocumentId) -> CoreResult<Document> {
        let docs = self.docs.lock().await;
        docs.get(id.as_str())
            .cloned()
            .ok_or_else(|| DbError::NotFound(id.to_string()))
    }

    async fn put(&self, document: Document) -> CoreResult<()> {
        let mut docs = self.docs.lock().await;
        docs.insert(document.id.as_str().to_owned(), document);
        Ok(())
    }

    async fn delete(&self, id: &DocumentId) -> CoreResult<()> {
        let mut docs = self.docs.lock().await;
        docs.remove(id.as_str())
            .map(|_| ())
            .ok_or_else(|| DbError::NotFound(id.to_string()))
    }

    async fn scan(
        &self,
        _range: RangeInclusive<DocumentId>,
        _limit: usize,
    ) -> CoreResult<Vec<Document>> {
        let docs = self.docs.lock().await;
        Ok(docs.values().cloned().collect())
    }
}

/// No-op index backend used by [`RaftNode::new_with_channel_network`].
struct NoopIndex;

#[async_trait]
impl IndexBackend for NoopIndex {
    async fn index_document(&self, _document: &Document) -> CoreResult<()> {
        Ok(())
    }

    async fn search(&self, _query: &Query) -> CoreResult<SearchResult> {
        Ok(SearchResult::empty(0))
    }

    async fn delete_document(&self, _id: &DocumentId) -> CoreResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use msearchdb_core::config::NodeConfig;

    #[tokio::test]
    async fn create_single_node_raft() {
        let config = NodeConfig::default();
        let storage: Arc<dyn StorageBackend> = Arc::new(InMemoryStorage::new());
        let index: Arc<dyn IndexBackend> = Arc::new(NoopIndex);

        let node = RaftNode::new(&config, storage, index).await;
        assert!(node.is_ok());
    }

    #[tokio::test]
    async fn propose_on_non_leader_returns_error() {
        let config = NodeConfig::default();
        let storage: Arc<dyn StorageBackend> = Arc::new(InMemoryStorage::new());
        let index: Arc<dyn IndexBackend> = Arc::new(NoopIndex);

        let node = RaftNode::new(&config, storage, index).await.unwrap();

        // The node is not initialised, so it is not a leader.
        let result = node
            .propose(RaftCommand::DeleteCollection { name: "x".into() })
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            DbError::ConsensusError(msg) => {
                // Should contain "not leader" or "forward to" or similar.
                assert!(
                    msg.contains("not leader")
                        || msg.contains("forward")
                        || msg.contains("Forward")
                        || msg.contains("fatal"),
                    "unexpected error message: {}",
                    msg
                );
            }
            other => panic!("expected ConsensusError, got: {:?}", other),
        }
    }
}
