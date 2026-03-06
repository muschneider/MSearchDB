//! Cluster routing for document placement and query fan-out.
//!
//! The [`ClusterRouter`] sits on top of the consistent hash ring and makes
//! high-level decisions about where documents live and which nodes to query.
//! It handles:
//!
//! - **Document routing**: Mapping a [`DocumentId`] to its replica set.
//! - **Query routing**: Deciding between scatter-to-all (full-text search) and
//!   targeted reads (get-by-id).
//! - **Node membership**: Adding/removing nodes with rebalancing awareness.
//! - **Quorum checks**: Verifying the cluster can satisfy consistency guarantees.
//!
//! # Examples
//!
//! ```
//! use msearchdb_core::cluster::{NodeAddress, NodeId, NodeInfo, NodeStatus};
//! use msearchdb_core::cluster_router::ClusterRouter;
//! use msearchdb_core::document::DocumentId;
//!
//! let nodes = vec![
//!     NodeInfo { id: NodeId::new(1), address: NodeAddress::new("10.0.0.1", 9200), status: NodeStatus::Follower },
//!     NodeInfo { id: NodeId::new(2), address: NodeAddress::new("10.0.0.2", 9200), status: NodeStatus::Follower },
//!     NodeInfo { id: NodeId::new(3), address: NodeAddress::new("10.0.0.3", 9200), status: NodeStatus::Follower },
//! ];
//!
//! let router = ClusterRouter::new(nodes, 3);
//! let replicas = router.route_document(&DocumentId::new("doc-1"));
//! assert_eq!(replicas.len(), 3);
//! ```

use crate::cluster::{NodeId, NodeInfo, NodeStatus};
use crate::consistent_hash::ConsistentHashRing;
use crate::document::DocumentId;
use crate::query::Query;
use crate::rebalancer::RebalancePlan;

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// ClusterRouter
// ---------------------------------------------------------------------------

/// Routes documents and queries to appropriate cluster nodes.
///
/// Uses a [`ConsistentHashRing<NodeId>`] to map document identifiers to a
/// replica set of `replication_factor` distinct nodes. Query routing is
/// strategy-dependent: full-text searches scatter to all healthy nodes,
/// while get-by-id requests target only the replica set.
#[derive(Debug, Clone)]
pub struct ClusterRouter {
    /// The consistent hash ring mapping keys to [`NodeId`]s.
    ring: ConsistentHashRing<NodeId>,
    /// Metadata about each known node.
    nodes: HashMap<NodeId, NodeInfo>,
    /// Number of replicas for each document (default: 3).
    replication_factor: u8,
}

impl ClusterRouter {
    /// Create a new router from a list of nodes and a replication factor.
    ///
    /// All provided nodes are added to the hash ring. The replication factor
    /// determines how many distinct nodes are returned by [`route_document`].
    pub fn new(nodes: Vec<NodeInfo>, replication_factor: u8) -> Self {
        let mut ring = ConsistentHashRing::new();
        let mut node_map = HashMap::new();

        for node in nodes {
            let id_str = node.id.as_u64().to_string();
            ring.add_node(&id_str, node.id);
            node_map.insert(node.id, node);
        }

        Self {
            ring,
            nodes: node_map,
            replication_factor,
        }
    }

    /// Return the [`NodeId`]s that should hold replicas of the given document.
    ///
    /// Returns up to `replication_factor` distinct nodes. If the cluster has
    /// fewer nodes than the replication factor, all available nodes are returned.
    pub fn route_document(&self, doc_id: &DocumentId) -> Vec<NodeId> {
        self.ring
            .get_nodes(doc_id.as_str(), self.replication_factor as usize)
            .into_iter()
            .copied()
            .collect()
    }

    /// Determine which nodes should handle a query.
    ///
    /// - **Full-text / range / bool queries**: Scatter to ALL healthy nodes
    ///   because the search index is distributed; every shard must be queried.
    /// - **Term queries on `_id`**: Only the replica nodes for that specific id
    ///   need to be contacted (an optimisation for get-by-id).
    pub fn route_query(&self, query: &Query, _collection: &str) -> Vec<NodeId> {
        // For term queries targeting a specific document id, we can route
        // directly to the replica set.
        if let Query::Term(term) = query {
            if term.field == "_id" {
                return self
                    .ring
                    .get_nodes(&term.value, self.replication_factor as usize)
                    .into_iter()
                    .copied()
                    .collect();
            }
        }

        // For all other query types, scatter to every healthy node.
        self.healthy_nodes().iter().map(|n| n.id).collect()
    }

    /// Add a new node to the cluster.
    ///
    /// The node is inserted into the hash ring and the node metadata table.
    /// Use [`rebalance_plan`](Self::rebalance_plan) afterward to compute which
    /// documents need to migrate.
    pub fn add_node(&mut self, node: NodeInfo) {
        let id_str = node.id.as_u64().to_string();
        self.ring.add_node(&id_str, node.id);
        self.nodes.insert(node.id, node);
    }

    /// Remove a node from the cluster.
    ///
    /// The node is marked [`NodeStatus::Offline`] in the metadata table and
    /// removed from the hash ring so that new writes are not routed to it.
    /// Existing replicas on surviving nodes remain available.
    pub fn remove_node(&mut self, node_id: NodeId) {
        let id_str = node_id.as_u64().to_string();
        self.ring.remove_node(&id_str);
        if let Some(info) = self.nodes.get_mut(&node_id) {
            info.status = NodeStatus::Offline;
        }
    }

    /// Compute a rebalance plan for adding `new_node` to the cluster.
    ///
    /// Samples a set of representative keys and determines which documents
    /// should migrate from existing nodes to the new node. This is a
    /// best-effort plan — the actual document ids to move must be resolved
    /// against the storage engine.
    pub fn rebalance_plan(&self, new_node: &NodeInfo) -> RebalancePlan {
        // Build a hypothetical ring WITH the new node.
        let mut future_ring = self.ring.clone();
        let id_str = new_node.id.as_u64().to_string();
        future_ring.add_node(&id_str, new_node.id);

        let mut moves = Vec::new();

        // Sample keys and find those that would move to the new node.
        for i in 0..10_000 {
            let sample_key = format!("__rebalance_probe_{i}");
            let old_primary = self.ring.get_primary(&sample_key);
            let new_primary = future_ring.get_primary(&sample_key);

            if let (Some(&old_id), Some(&new_id)) = (old_primary, new_primary) {
                if old_id != new_id && new_id == new_node.id {
                    moves.push(crate::rebalancer::DataMove {
                        document_id: DocumentId::new(sample_key),
                        from_node: old_id,
                        to_node: new_id,
                    });
                }
            }
        }

        RebalancePlan { moves }
    }

    /// Return all nodes whose status is **not** [`NodeStatus::Offline`].
    pub fn healthy_nodes(&self) -> Vec<&NodeInfo> {
        self.nodes
            .values()
            .filter(|n| n.status != NodeStatus::Offline)
            .collect()
    }

    /// Check whether the cluster can satisfy a quorum (majority) of replicas.
    ///
    /// For a replication factor of 3, quorum requires at least 2 healthy nodes.
    /// In general: `healthy >= ceil(rf / 2)` which simplifies to
    /// `healthy >= (rf / 2) + 1`.
    pub fn can_satisfy_quorum(&self) -> bool {
        let healthy = self.healthy_nodes().len();
        let quorum = (self.replication_factor as usize / 2) + 1;
        healthy >= quorum
    }

    /// Return the current replication factor.
    pub fn replication_factor(&self) -> u8 {
        self.replication_factor
    }

    /// Return the total number of known nodes (including offline).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Look up metadata for a specific node.
    pub fn get_node(&self, id: NodeId) -> Option<&NodeInfo> {
        self.nodes.get(&id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::NodeAddress;

    /// Helper: create a 3-node cluster with the given status.
    fn make_nodes(statuses: &[NodeStatus]) -> Vec<NodeInfo> {
        statuses
            .iter()
            .enumerate()
            .map(|(i, &status)| NodeInfo {
                id: NodeId::new((i + 1) as u64),
                address: NodeAddress::new(format!("10.0.0.{}", i + 1), 9200),
                status,
            })
            .collect()
    }

    fn three_healthy_nodes() -> Vec<NodeInfo> {
        make_nodes(&[
            NodeStatus::Follower,
            NodeStatus::Follower,
            NodeStatus::Follower,
        ])
    }

    #[test]
    fn route_document_returns_replication_factor_nodes() {
        let router = ClusterRouter::new(three_healthy_nodes(), 3);
        let replicas = router.route_document(&DocumentId::new("doc-1"));
        assert_eq!(replicas.len(), 3);

        // All distinct.
        let mut unique = replicas.clone();
        unique.sort_by_key(|id| id.as_u64());
        unique.dedup();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn route_document_is_deterministic() {
        let router = ClusterRouter::new(three_healthy_nodes(), 3);
        let a = router.route_document(&DocumentId::new("stable-doc"));
        let b = router.route_document(&DocumentId::new("stable-doc"));
        assert_eq!(a, b);
    }

    #[test]
    fn route_query_full_text_scatters_to_all_healthy() {
        let router = ClusterRouter::new(three_healthy_nodes(), 3);
        let query = Query::FullText(crate::query::FullTextQuery {
            field: "body".into(),
            query: "search terms".into(),
            operator: crate::query::Operator::And,
        });

        let targets = router.route_query(&query, "products");
        // Should scatter to all 3 healthy nodes.
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn route_query_term_id_routes_to_replicas() {
        let router = ClusterRouter::new(three_healthy_nodes(), 3);
        let query = Query::Term(crate::query::TermQuery {
            field: "_id".into(),
            value: "doc-42".into(),
        });

        let targets = router.route_query(&query, "products");
        assert_eq!(targets.len(), 3);

        // Same as route_document for that id.
        let replicas = router.route_document(&DocumentId::new("doc-42"));
        assert_eq!(targets, replicas);
    }

    #[test]
    fn route_query_range_scatters() {
        let router = ClusterRouter::new(three_healthy_nodes(), 3);
        let query = Query::Range(crate::query::RangeQuery {
            field: "price".into(),
            gte: Some(10.0),
            lte: Some(100.0),
        });

        let targets = router.route_query(&query, "products");
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn add_node_increases_node_count() {
        let mut router = ClusterRouter::new(three_healthy_nodes(), 3);
        assert_eq!(router.node_count(), 3);

        router.add_node(NodeInfo {
            id: NodeId::new(4),
            address: NodeAddress::new("10.0.0.4", 9200),
            status: NodeStatus::Follower,
        });
        assert_eq!(router.node_count(), 4);
    }

    #[test]
    fn remove_node_marks_offline() {
        let mut router = ClusterRouter::new(three_healthy_nodes(), 3);
        router.remove_node(NodeId::new(2));

        let node2 = router.get_node(NodeId::new(2)).unwrap();
        assert_eq!(node2.status, NodeStatus::Offline);
        assert_eq!(router.healthy_nodes().len(), 2);
    }

    #[test]
    fn can_satisfy_quorum_with_three_healthy() {
        let router = ClusterRouter::new(three_healthy_nodes(), 3);
        assert!(router.can_satisfy_quorum());
    }

    #[test]
    fn can_satisfy_quorum_with_two_healthy() {
        let nodes = make_nodes(&[
            NodeStatus::Follower,
            NodeStatus::Follower,
            NodeStatus::Offline,
        ]);
        let router = ClusterRouter::new(nodes, 3);
        // Quorum for RF=3 is 2, and we have 2 healthy nodes.
        assert!(router.can_satisfy_quorum());
    }

    #[test]
    fn cannot_satisfy_quorum_with_one_healthy() {
        let nodes = make_nodes(&[
            NodeStatus::Follower,
            NodeStatus::Offline,
            NodeStatus::Offline,
        ]);
        let router = ClusterRouter::new(nodes, 3);
        assert!(!router.can_satisfy_quorum());
    }

    #[test]
    fn rebalance_plan_identifies_moves() {
        let router = ClusterRouter::new(three_healthy_nodes(), 3);
        let new_node = NodeInfo {
            id: NodeId::new(4),
            address: NodeAddress::new("10.0.0.4", 9200),
            status: NodeStatus::Follower,
        };

        let plan = router.rebalance_plan(&new_node);
        // Some keys should move to node-4.
        assert!(
            !plan.moves.is_empty(),
            "Rebalance plan should have moves when adding a node"
        );

        // All moves should target the new node.
        for m in &plan.moves {
            assert_eq!(m.to_node, NodeId::new(4));
            assert_ne!(m.from_node, NodeId::new(4));
        }
    }

    #[test]
    fn healthy_nodes_excludes_offline() {
        let nodes = make_nodes(&[
            NodeStatus::Leader,
            NodeStatus::Follower,
            NodeStatus::Offline,
        ]);
        let router = ClusterRouter::new(nodes, 3);
        let healthy = router.healthy_nodes();
        assert_eq!(healthy.len(), 2);
        for h in &healthy {
            assert_ne!(h.status, NodeStatus::Offline);
        }
    }
}
