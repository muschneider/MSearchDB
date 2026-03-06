//! Cluster topology types for MSearchDB.
//!
//! These types model the identity of nodes within a MSearchDB cluster, their
//! network addresses, operational status, and the overall cluster state.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// NodeId — newtype over u64
// ---------------------------------------------------------------------------

/// A unique numeric identifier for a node in the cluster.
///
/// Uses the *newtype pattern* so that a `u64` port number or index cannot be
/// accidentally passed where a node id is expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u64);

impl NodeId {
    /// Create a new `NodeId`.
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    /// Return the inner `u64` value.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node-{}", self.0)
    }
}

impl From<u64> for NodeId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

// ---------------------------------------------------------------------------
// NodeAddress
// ---------------------------------------------------------------------------

/// A resolvable network address for a cluster node.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeAddress {
    /// Hostname or IP address.
    pub host: String,
    /// TCP port number.
    pub port: u16,
}

impl NodeAddress {
    /// Create a new node address.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

impl fmt::Display for NodeAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

// ---------------------------------------------------------------------------
// NodeStatus
// ---------------------------------------------------------------------------

/// The operational status of a node in the Raft-based cluster.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum NodeStatus {
    /// This node is the current Raft leader.
    Leader,
    /// This node is a follower replicating the leader's log.
    #[default]
    Follower,
    /// This node is running a leader election.
    Candidate,
    /// This node is unreachable or shut down.
    Offline,
}

impl fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeStatus::Leader => write!(f, "Leader"),
            NodeStatus::Follower => write!(f, "Follower"),
            NodeStatus::Candidate => write!(f, "Candidate"),
            NodeStatus::Offline => write!(f, "Offline"),
        }
    }
}

// ---------------------------------------------------------------------------
// NodeInfo
// ---------------------------------------------------------------------------

/// Complete information about a single cluster node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// The node's unique identifier.
    pub id: NodeId,
    /// The node's network address.
    pub address: NodeAddress,
    /// The node's current operational status.
    pub status: NodeStatus,
}

// ---------------------------------------------------------------------------
// ClusterState
// ---------------------------------------------------------------------------

/// A snapshot of the entire cluster topology.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterState {
    /// All known nodes in the cluster.
    pub nodes: Vec<NodeInfo>,
    /// The current leader, if one has been elected.
    pub leader: Option<NodeId>,
}

impl ClusterState {
    /// Create an empty cluster state.
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            leader: None,
        }
    }

    /// Return the number of nodes in the cluster.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Look up a node by its id.
    pub fn get_node(&self, id: NodeId) -> Option<&NodeInfo> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_newtype() {
        let a = NodeId::new(1);
        let b: NodeId = 1_u64.into();
        assert_eq!(a, b);
        assert_eq!(a.as_u64(), 1);
    }

    #[test]
    fn node_id_display() {
        assert_eq!(format!("{}", NodeId::new(42)), "node-42");
    }

    #[test]
    fn node_address_display() {
        let addr = NodeAddress::new("127.0.0.1", 9200);
        assert_eq!(format!("{}", addr), "127.0.0.1:9200");
    }

    #[test]
    fn node_status_default_is_follower() {
        assert_eq!(NodeStatus::default(), NodeStatus::Follower);
    }

    #[test]
    fn node_status_display() {
        assert_eq!(format!("{}", NodeStatus::Leader), "Leader");
        assert_eq!(format!("{}", NodeStatus::Follower), "Follower");
        assert_eq!(format!("{}", NodeStatus::Candidate), "Candidate");
        assert_eq!(format!("{}", NodeStatus::Offline), "Offline");
    }

    #[test]
    fn cluster_state_empty() {
        let state = ClusterState::empty();
        assert_eq!(state.node_count(), 0);
        assert!(state.leader.is_none());
    }

    #[test]
    fn cluster_state_get_node() {
        let state = ClusterState {
            nodes: vec![
                NodeInfo {
                    id: NodeId::new(1),
                    address: NodeAddress::new("host1", 9200),
                    status: NodeStatus::Leader,
                },
                NodeInfo {
                    id: NodeId::new(2),
                    address: NodeAddress::new("host2", 9200),
                    status: NodeStatus::Follower,
                },
            ],
            leader: Some(NodeId::new(1)),
        };

        assert_eq!(state.node_count(), 2);
        let node = state.get_node(NodeId::new(1)).unwrap();
        assert_eq!(node.status, NodeStatus::Leader);
        assert!(state.get_node(NodeId::new(99)).is_none());
    }

    #[test]
    fn cluster_state_serde_roundtrip() {
        let state = ClusterState {
            nodes: vec![NodeInfo {
                id: NodeId::new(1),
                address: NodeAddress::new("localhost", 9200),
                status: NodeStatus::Follower,
            }],
            leader: None,
        };

        let json = serde_json::to_string(&state).unwrap();
        let back: ClusterState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }
}
