//! Vector clock for tracking causal ordering across cluster nodes.
//!
//! A [`VectorClock`] is a map from [`NodeId`] to a logical counter. Each node
//! increments its own counter on every write, and incoming clocks are merged
//! component-wise using the maximum. This allows distributed nodes to detect
//! causal ordering (happened-before), concurrency, and conflicts without
//! relying on synchronized wall clocks.
//!
//! # Examples
//!
//! ```
//! use msearchdb_core::vector_clock::VectorClock;
//! use msearchdb_core::cluster::NodeId;
//!
//! let mut vc1 = VectorClock::new();
//! vc1.increment(NodeId::new(1));
//! vc1.increment(NodeId::new(1));
//!
//! let mut vc2 = VectorClock::new();
//! vc2.increment(NodeId::new(2));
//!
//! // Neither dominates the other — they are concurrent.
//! assert!(vc1.is_concurrent_with(&vc2));
//!
//! // Merge resolves to the component-wise max.
//! let merged = vc1.merge(&vc2);
//! assert_eq!(merged.get(NodeId::new(1)), 2);
//! assert_eq!(merged.get(NodeId::new(2)), 1);
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

use crate::cluster::NodeId;

// ---------------------------------------------------------------------------
// VectorClock
// ---------------------------------------------------------------------------

/// A vector clock that maps each [`NodeId`] to a monotonically increasing
/// logical counter.
///
/// Vector clocks enable detection of causal ordering without synchronized
/// physical clocks. Two events `a` and `b` satisfy:
///
/// - `a` **happened before** `b` if `a`'s clock is dominated by `b`'s.
/// - `a` and `b` are **concurrent** if neither dominates the other.
///
/// Uses [`BTreeMap`] for deterministic serialization order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorClock {
    /// Per-node logical counters.
    clocks: BTreeMap<u64, u64>,
}

impl VectorClock {
    /// Create a new empty vector clock.
    pub fn new() -> Self {
        Self {
            clocks: BTreeMap::new(),
        }
    }

    /// Increment the counter for the given node and return the new value.
    ///
    /// If the node has no entry yet, it starts at 1.
    pub fn increment(&mut self, node_id: NodeId) -> u64 {
        let counter = self.clocks.entry(node_id.as_u64()).or_insert(0);
        *counter += 1;
        *counter
    }

    /// Return the counter value for a given node, or 0 if absent.
    pub fn get(&self, node_id: NodeId) -> u64 {
        self.clocks.get(&node_id.as_u64()).copied().unwrap_or(0)
    }

    /// Merge two vector clocks by taking the component-wise maximum.
    ///
    /// The result captures all causal history from both clocks.
    pub fn merge(&self, other: &VectorClock) -> VectorClock {
        let mut merged = self.clocks.clone();
        for (&node, &count) in &other.clocks {
            let entry = merged.entry(node).or_insert(0);
            if count > *entry {
                *entry = count;
            }
        }
        VectorClock { clocks: merged }
    }

    /// Merge another clock into this clock in-place.
    pub fn merge_in_place(&mut self, other: &VectorClock) {
        for (&node, &count) in &other.clocks {
            let entry = self.clocks.entry(node).or_insert(0);
            if count > *entry {
                *entry = count;
            }
        }
    }

    /// Return `true` if this clock **dominates** `other`.
    ///
    /// Clock `A` dominates clock `B` if every component of `A` is ≥ the
    /// corresponding component of `B`, and at least one is strictly greater.
    /// This means `A` happened strictly after `B`.
    pub fn dominates(&self, other: &VectorClock) -> bool {
        let mut dominated = false;
        let mut at_least_as_large = true;

        // Check all entries in `other`.
        for (&node, &count) in &other.clocks {
            let self_count = self.clocks.get(&node).copied().unwrap_or(0);
            if self_count < count {
                at_least_as_large = false;
                break;
            }
            if self_count > count {
                dominated = true;
            }
        }

        if !at_least_as_large {
            return false;
        }

        // Check entries in `self` that are not in `other`.
        for (&node, &count) in &self.clocks {
            if count > 0 && !other.clocks.contains_key(&node) {
                dominated = true;
            }
        }

        dominated
    }

    /// Return `true` if the two clocks are **concurrent** (neither dominates).
    pub fn is_concurrent_with(&self, other: &VectorClock) -> bool {
        !self.dominates(other) && !other.dominates(self) && self != other
    }

    /// Return `true` if the clock has no entries (or all entries are 0).
    pub fn is_empty(&self) -> bool {
        self.clocks.values().all(|&v| v == 0)
    }

    /// Return the number of nodes tracked by this clock.
    pub fn len(&self) -> usize {
        self.clocks.len()
    }

    /// Return a sum of all counters — a rough proxy for "total version".
    ///
    /// Useful for quick comparisons when a single scalar is needed.
    pub fn total(&self) -> u64 {
        self.clocks.values().sum()
    }
}

impl Default for VectorClock {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for VectorClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VC{{")?;
        for (i, (&node, &count)) in self.clocks.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "node-{}:{}", node, count)?;
        }
        write!(f, "}}")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_vector_clock_is_empty() {
        let vc = VectorClock::new();
        assert!(vc.is_empty());
        assert_eq!(vc.len(), 0);
        assert_eq!(vc.total(), 0);
    }

    #[test]
    fn increment_creates_entry() {
        let mut vc = VectorClock::new();
        let val = vc.increment(NodeId::new(1));
        assert_eq!(val, 1);
        assert_eq!(vc.get(NodeId::new(1)), 1);
        assert!(!vc.is_empty());
    }

    #[test]
    fn increment_is_monotonic() {
        let mut vc = VectorClock::new();
        assert_eq!(vc.increment(NodeId::new(1)), 1);
        assert_eq!(vc.increment(NodeId::new(1)), 2);
        assert_eq!(vc.increment(NodeId::new(1)), 3);
        assert_eq!(vc.get(NodeId::new(1)), 3);
    }

    #[test]
    fn get_missing_node_returns_zero() {
        let vc = VectorClock::new();
        assert_eq!(vc.get(NodeId::new(42)), 0);
    }

    #[test]
    fn merge_takes_component_wise_max() {
        let mut vc1 = VectorClock::new();
        vc1.increment(NodeId::new(1)); // {1:1}
        vc1.increment(NodeId::new(1)); // {1:2}

        let mut vc2 = VectorClock::new();
        vc2.increment(NodeId::new(1)); // {1:1}
        vc2.increment(NodeId::new(2)); // {1:1, 2:1}
        vc2.increment(NodeId::new(2)); // {1:1, 2:2}

        let merged = vc1.merge(&vc2);
        assert_eq!(merged.get(NodeId::new(1)), 2); // max(2, 1)
        assert_eq!(merged.get(NodeId::new(2)), 2); // max(0, 2)
    }

    #[test]
    fn merge_in_place_works() {
        let mut vc1 = VectorClock::new();
        vc1.increment(NodeId::new(1));
        vc1.increment(NodeId::new(1));

        let mut vc2 = VectorClock::new();
        vc2.increment(NodeId::new(2));

        vc1.merge_in_place(&vc2);
        assert_eq!(vc1.get(NodeId::new(1)), 2);
        assert_eq!(vc1.get(NodeId::new(2)), 1);
    }

    #[test]
    fn dominates_after_increment() {
        let mut vc1 = VectorClock::new();
        vc1.increment(NodeId::new(1)); // {1:1}

        let mut vc2 = vc1.clone();
        vc2.increment(NodeId::new(1)); // {1:2}

        assert!(vc2.dominates(&vc1));
        assert!(!vc1.dominates(&vc2));
    }

    #[test]
    fn equal_clocks_do_not_dominate() {
        let mut vc1 = VectorClock::new();
        vc1.increment(NodeId::new(1));
        let vc2 = vc1.clone();

        assert!(!vc1.dominates(&vc2));
        assert!(!vc2.dominates(&vc1));
        assert!(!vc1.is_concurrent_with(&vc2)); // equal, not concurrent
    }

    #[test]
    fn concurrent_clocks_detected() {
        let mut vc1 = VectorClock::new();
        vc1.increment(NodeId::new(1)); // {1:1}

        let mut vc2 = VectorClock::new();
        vc2.increment(NodeId::new(2)); // {2:1}

        assert!(!vc1.dominates(&vc2));
        assert!(!vc2.dominates(&vc1));
        assert!(vc1.is_concurrent_with(&vc2));
    }

    #[test]
    fn total_sums_all_counters() {
        let mut vc = VectorClock::new();
        vc.increment(NodeId::new(1)); // 1
        vc.increment(NodeId::new(1)); // 2
        vc.increment(NodeId::new(2)); // 1
        assert_eq!(vc.total(), 3);
    }

    #[test]
    fn display_format() {
        let mut vc = VectorClock::new();
        vc.increment(NodeId::new(1));
        vc.increment(NodeId::new(2));
        let s = format!("{}", vc);
        assert!(s.contains("node-1:1"));
        assert!(s.contains("node-2:1"));
    }

    #[test]
    fn serde_roundtrip() {
        let mut vc = VectorClock::new();
        vc.increment(NodeId::new(1));
        vc.increment(NodeId::new(1));
        vc.increment(NodeId::new(3));

        let json = serde_json::to_string(&vc).unwrap();
        let back: VectorClock = serde_json::from_str(&json).unwrap();
        assert_eq!(vc, back);
    }

    #[test]
    fn dominates_with_superset_nodes() {
        let mut vc1 = VectorClock::new();
        vc1.increment(NodeId::new(1));
        vc1.increment(NodeId::new(2));

        let mut vc2 = VectorClock::new();
        vc2.increment(NodeId::new(1));
        // vc2 has no entry for node 2

        // vc1 dominates: vc1[1]=1 >= vc2[1]=1, vc1[2]=1 > vc2[2]=0
        assert!(vc1.dominates(&vc2));
        assert!(!vc2.dominates(&vc1));
    }

    #[test]
    fn default_is_empty() {
        let vc = VectorClock::default();
        assert!(vc.is_empty());
    }
}
