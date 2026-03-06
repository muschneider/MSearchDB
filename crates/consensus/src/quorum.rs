//! Quorum tracking and cluster health checking.
//!
//! This module provides two utilities:
//!
//! - [`QuorumTracker`]: determines whether a replication factor's quorum
//!   has been met.
//! - [`ClusterHealthChecker`]: tracks node liveness and reports whether
//!   the cluster can accept writes or serve reads.
//!
//! # Quorum Math
//!
//! For a cluster with **replication factor** (RF) = N, the quorum (majority)
//! is `(N / 2) + 1`.
//!
//! | RF | Quorum | Can tolerate failures |
//! |----|--------|----------------------|
//! | 1  |   1    | 0                    |
//! | 3  |   2    | 1                    |
//! | 5  |   3    | 2                    |
//! | 7  |   4    | 3                    |
//!
//! This is the standard Raft majority rule.  A write is committed once
//! **quorum** nodes have acknowledged it.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// QuorumTracker
// ---------------------------------------------------------------------------

/// Tracks acknowledgments for a log entry and determines quorum.
///
/// # Examples
///
/// ```
/// use msearchdb_consensus::quorum::QuorumTracker;
///
/// assert!(QuorumTracker::is_committed(3, 2));  // 2/3 = quorum
/// assert!(!QuorumTracker::is_committed(3, 1)); // 1/3 = no quorum
/// assert!(QuorumTracker::is_committed(5, 3));  // 3/5 = quorum
/// assert!(!QuorumTracker::is_committed(5, 2)); // 2/5 = no quorum
/// ```
pub struct QuorumTracker;

impl QuorumTracker {
    /// Return the quorum size for the given replication factor.
    ///
    /// Quorum = `(replication_factor / 2) + 1` (integer division).
    pub fn quorum_size(replication_factor: u8) -> u8 {
        (replication_factor / 2) + 1
    }

    /// Return `true` if the number of acknowledgments meets quorum.
    ///
    /// A replication factor of 0 is degenerate — returns `false`.
    pub fn is_committed(replication_factor: u8, acks: u8) -> bool {
        if replication_factor == 0 {
            return false;
        }
        acks >= Self::quorum_size(replication_factor)
    }
}

// ---------------------------------------------------------------------------
// ClusterHealthChecker
// ---------------------------------------------------------------------------

/// A lightweight checker that tracks which nodes in the cluster are online
/// and determines operational capability.
///
/// # Examples
///
/// ```
/// use msearchdb_consensus::quorum::ClusterHealthChecker;
///
/// let mut checker = ClusterHealthChecker::new(3);
/// checker.set_online(1, true);
/// checker.set_online(2, true);
/// checker.set_online(3, false);
///
/// assert!(checker.can_write());   // 2 online >= quorum of 2
/// assert!(checker.can_read());    // at least 1 online
/// assert_eq!(checker.count_online_nodes(), 2);
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterHealthChecker {
    /// The replication factor for this cluster.
    replication_factor: u8,

    /// Per-node online status, keyed by node id.
    nodes: Vec<NodeLiveness>,
}

/// Tracks whether a single node is online.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct NodeLiveness {
    node_id: u64,
    online: bool,
}

impl ClusterHealthChecker {
    /// Create a new health checker for a cluster with the given replication
    /// factor.  All nodes start as offline.
    pub fn new(replication_factor: u8) -> Self {
        Self {
            replication_factor,
            nodes: Vec::new(),
        }
    }

    /// Set the online/offline status of a node.
    ///
    /// If the node is not yet tracked, it is added.
    pub fn set_online(&mut self, node_id: u64, online: bool) {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.node_id == node_id) {
            n.online = online;
        } else {
            self.nodes.push(NodeLiveness { node_id, online });
        }
    }

    /// Return the number of currently online nodes.
    pub fn count_online_nodes(&self) -> u8 {
        self.nodes.iter().filter(|n| n.online).count() as u8
    }

    /// Return `true` if enough nodes are online to form a write quorum.
    ///
    /// Requires `count_online_nodes() >= quorum_size(replication_factor)`.
    pub fn can_write(&self) -> bool {
        QuorumTracker::is_committed(self.replication_factor, self.count_online_nodes())
    }

    /// Return `true` if at least one node is online (reads can be served).
    pub fn can_read(&self) -> bool {
        self.count_online_nodes() >= 1
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- QuorumTracker tests ------------------------------------------------

    #[test]
    fn quorum_rf_1_needs_1() {
        assert_eq!(QuorumTracker::quorum_size(1), 1);
        assert!(QuorumTracker::is_committed(1, 1));
        assert!(!QuorumTracker::is_committed(1, 0));
    }

    #[test]
    fn quorum_rf_3_needs_2() {
        assert_eq!(QuorumTracker::quorum_size(3), 2);
        assert!(QuorumTracker::is_committed(3, 2));
        assert!(QuorumTracker::is_committed(3, 3));
        assert!(!QuorumTracker::is_committed(3, 1));
        assert!(!QuorumTracker::is_committed(3, 0));
    }

    #[test]
    fn quorum_rf_5_needs_3() {
        assert_eq!(QuorumTracker::quorum_size(5), 3);
        assert!(QuorumTracker::is_committed(5, 3));
        assert!(QuorumTracker::is_committed(5, 4));
        assert!(QuorumTracker::is_committed(5, 5));
        assert!(!QuorumTracker::is_committed(5, 2));
    }

    #[test]
    fn quorum_rf_7_needs_4() {
        assert_eq!(QuorumTracker::quorum_size(7), 4);
        assert!(QuorumTracker::is_committed(7, 4));
        assert!(!QuorumTracker::is_committed(7, 3));
    }

    #[test]
    fn quorum_rf_0_is_degenerate() {
        assert!(!QuorumTracker::is_committed(0, 0));
        assert!(!QuorumTracker::is_committed(0, 1));
    }

    // -- ClusterHealthChecker tests -----------------------------------------

    #[test]
    fn cluster_3_nodes_all_up_can_write() {
        let mut checker = ClusterHealthChecker::new(3);
        checker.set_online(1, true);
        checker.set_online(2, true);
        checker.set_online(3, true);
        assert!(checker.can_write());
        assert!(checker.can_read());
        assert_eq!(checker.count_online_nodes(), 3);
    }

    #[test]
    fn cluster_3_nodes_2_up_can_write() {
        let mut checker = ClusterHealthChecker::new(3);
        checker.set_online(1, true);
        checker.set_online(2, true);
        checker.set_online(3, false);
        assert!(checker.can_write());
        assert!(checker.can_read());
        assert_eq!(checker.count_online_nodes(), 2);
    }

    #[test]
    fn cluster_3_nodes_1_up_cannot_write() {
        let mut checker = ClusterHealthChecker::new(3);
        checker.set_online(1, true);
        checker.set_online(2, false);
        checker.set_online(3, false);
        assert!(!checker.can_write());
        assert!(checker.can_read()); // can still read from the one online node
        assert_eq!(checker.count_online_nodes(), 1);
    }

    #[test]
    fn cluster_3_nodes_0_up_cannot_do_anything() {
        let mut checker = ClusterHealthChecker::new(3);
        checker.set_online(1, false);
        checker.set_online(2, false);
        checker.set_online(3, false);
        assert!(!checker.can_write());
        assert!(!checker.can_read());
        assert_eq!(checker.count_online_nodes(), 0);
    }

    #[test]
    fn cluster_health_node_status_toggle() {
        let mut checker = ClusterHealthChecker::new(3);
        checker.set_online(1, true);
        assert_eq!(checker.count_online_nodes(), 1);

        // Toggle offline
        checker.set_online(1, false);
        assert_eq!(checker.count_online_nodes(), 0);

        // Toggle back online
        checker.set_online(1, true);
        assert_eq!(checker.count_online_nodes(), 1);
    }

    #[test]
    fn cluster_health_empty_checker() {
        let checker = ClusterHealthChecker::new(3);
        assert!(!checker.can_write());
        assert!(!checker.can_read());
        assert_eq!(checker.count_online_nodes(), 0);
    }
}
