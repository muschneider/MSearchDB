//! Read coordination with configurable consistency and vector-clock conflict resolution.
//!
//! The [`ReadCoordinator`] fans out read requests to replica nodes according to
//! the requested [`ConsistencyLevel`], collects responses, and resolves
//! conflicts using [`VectorClock`] comparison.
//!
//! # Architecture
//!
//! ```text
//!  Client read request (?consistency=quorum)
//!       │
//!       ▼
//!  ┌────────────────────────────────────┐
//!  │         ReadCoordinator            │
//!  │                                    │
//!  │  1. Determine replica set          │
//!  │  2. Fan out reads to N replicas    │
//!  │  3. Wait for required_responses    │
//!  │  4. Compare vector clocks          │
//!  │  5. Return latest document         │
//!  │  6. (async) Repair stale replicas  │
//!  └────────────────────────────────────┘
//! ```

use crate::cluster::NodeId;
use crate::consistency::ConsistencyLevel;
use crate::document::Document;
use crate::error::{DbError, DbResult};
use crate::vector_clock::VectorClock;

// ---------------------------------------------------------------------------
// ReplicaResponse
// ---------------------------------------------------------------------------

/// A response from a single replica node for a read operation.
#[derive(Clone, Debug)]
pub struct ReplicaResponse {
    /// The node that provided this response.
    pub node_id: NodeId,
    /// The document data (if found).
    pub document: Document,
    /// The vector clock of the document on this replica.
    pub version: VectorClock,
}

// ---------------------------------------------------------------------------
// ReadResolution
// ---------------------------------------------------------------------------

/// The result of resolving multiple replica responses.
#[derive(Clone, Debug)]
pub struct ReadResolution {
    /// The document with the latest (dominant) vector clock.
    pub document: Document,
    /// The resolved vector clock (merge of all respondent clocks).
    pub version: VectorClock,
    /// Nodes that had a stale version and should be repaired.
    pub stale_nodes: Vec<NodeId>,
    /// Total number of replicas that responded.
    pub response_count: usize,
}

// ---------------------------------------------------------------------------
// ReadCoordinator
// ---------------------------------------------------------------------------

/// Coordinates distributed reads across replica nodes with configurable
/// consistency levels and vector-clock-based conflict resolution.
///
/// The coordinator does not perform I/O itself — it receives pre-fetched
/// [`ReplicaResponse`]s and resolves them. The caller is responsible for
/// fanning out reads to nodes and collecting responses.
#[derive(Clone, Debug)]
pub struct ReadCoordinator {
    /// Number of replicas per document.
    replication_factor: u8,
}

impl ReadCoordinator {
    /// Create a new coordinator with the given replication factor.
    pub fn new(replication_factor: u8) -> Self {
        Self { replication_factor }
    }

    /// Return the configured replication factor.
    pub fn replication_factor(&self) -> u8 {
        self.replication_factor
    }

    /// Validate that enough responses have been collected for the requested
    /// consistency level.
    ///
    /// Returns `Ok(())` if `response_count >= required`, or
    /// `Err(DbError::ConsistencyError)` otherwise.
    pub fn validate_response_count(
        &self,
        level: ConsistencyLevel,
        response_count: usize,
    ) -> DbResult<()> {
        let required = level.required_responses(self.replication_factor);
        if response_count >= required {
            Ok(())
        } else {
            Err(DbError::ConsistencyError(format!(
                "consistency level {} requires {} responses but only {} received \
                 (replication_factor={})",
                level, required, response_count, self.replication_factor
            )))
        }
    }

    /// Resolve a set of replica responses into a single authoritative document.
    ///
    /// # Algorithm
    ///
    /// 1. Validate that enough responses arrived for the consistency level.
    /// 2. Find the response(s) with the dominant vector clock.
    /// 3. If multiple responses are concurrent (no single dominant), merge
    ///    their vector clocks and use the one with the highest `total()` as
    ///    a tiebreaker (last-writer-wins heuristic on total counter).
    /// 4. Identify stale nodes (those whose clock is dominated by the winner).
    /// 5. Return the latest document and list of stale nodes for read repair.
    ///
    /// # Errors
    ///
    /// - [`DbError::ConsistencyError`] if not enough responses for the level.
    /// - [`DbError::NotFound`] if responses is empty.
    pub fn resolve(
        &self,
        responses: &[ReplicaResponse],
        level: ConsistencyLevel,
    ) -> DbResult<ReadResolution> {
        if responses.is_empty() {
            return Err(DbError::NotFound(
                "no replica responses received".to_string(),
            ));
        }

        self.validate_response_count(level, responses.len())?;

        // Find the "winner" — the response with the most advanced clock.
        let winner_idx = self.pick_winner(responses);
        let winner = &responses[winner_idx];

        // Merge all clocks into a single resolved clock.
        let mut merged_clock = winner.version.clone();
        for resp in responses {
            merged_clock.merge_in_place(&resp.version);
        }

        // Identify stale replicas (winner dominates them).
        let stale_nodes: Vec<NodeId> = responses
            .iter()
            .filter(|r| r.node_id != winner.node_id && winner.version.dominates(&r.version))
            .map(|r| r.node_id)
            .collect();

        Ok(ReadResolution {
            document: winner.document.clone(),
            version: merged_clock,
            stale_nodes,
            response_count: responses.len(),
        })
    }

    /// Pick the index of the "winning" response.
    ///
    /// Strategy:
    /// 1. If one response's clock dominates all others, it wins.
    /// 2. Otherwise (concurrent clocks), pick the one with the highest `total()`
    ///    as a deterministic tiebreaker.
    fn pick_winner(&self, responses: &[ReplicaResponse]) -> usize {
        let mut best_idx = 0;

        for i in 1..responses.len() {
            let current_best = &responses[best_idx].version;
            let candidate = &responses[i].version;

            if candidate.dominates(current_best) {
                // Candidate is strictly newer.
                best_idx = i;
            } else if current_best.dominates(candidate) {
                // Current best is still newer — keep it.
            } else {
                // Concurrent: use total() as tiebreaker (higher total wins).
                if candidate.total() > current_best.total() {
                    best_idx = i;
                }
                // On tie, keep the earlier index for determinism.
            }
        }

        best_idx
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{DocumentId, FieldValue};

    /// Helper: build a document with a single field.
    fn make_doc(id: &str, value: &str) -> Document {
        Document::new(DocumentId::new(id)).with_field("data", FieldValue::Text(value.into()))
    }

    /// Helper: build a vector clock with specified node->count pairs.
    fn make_clock(entries: &[(u64, u64)]) -> VectorClock {
        let mut vc = VectorClock::new();
        for &(node, count) in entries {
            for _ in 0..count {
                vc.increment(NodeId::new(node));
            }
        }
        vc
    }

    #[test]
    fn resolve_single_response_at_one() {
        let coord = ReadCoordinator::new(3);
        let responses = vec![ReplicaResponse {
            node_id: NodeId::new(1),
            document: make_doc("d1", "latest"),
            version: make_clock(&[(1, 3)]),
        }];

        let result = coord.resolve(&responses, ConsistencyLevel::One).unwrap();
        assert_eq!(result.document.id.as_str(), "d1");
        assert_eq!(result.response_count, 1);
        assert!(result.stale_nodes.is_empty());
    }

    #[test]
    fn resolve_fails_quorum_with_one_response() {
        let coord = ReadCoordinator::new(3);
        let responses = vec![ReplicaResponse {
            node_id: NodeId::new(1),
            document: make_doc("d1", "v1"),
            version: make_clock(&[(1, 1)]),
        }];

        let result = coord.resolve(&responses, ConsistencyLevel::Quorum);
        assert!(result.is_err());
        assert!(matches!(result, Err(DbError::ConsistencyError(_))));
    }

    #[test]
    fn resolve_picks_dominant_clock() {
        let coord = ReadCoordinator::new(3);

        // Node 1 has an older version.
        let old_clock = make_clock(&[(1, 1)]);
        // Node 2 has a newer version (superset of node 1's clock).
        let new_clock = make_clock(&[(1, 1), (2, 2)]);

        let responses = vec![
            ReplicaResponse {
                node_id: NodeId::new(1),
                document: make_doc("d1", "old"),
                version: old_clock,
            },
            ReplicaResponse {
                node_id: NodeId::new(2),
                document: make_doc("d1", "new"),
                version: new_clock,
            },
        ];

        let result = coord.resolve(&responses, ConsistencyLevel::Quorum).unwrap();
        assert_eq!(
            result.document.get_field("data"),
            Some(&FieldValue::Text("new".into()))
        );
        assert_eq!(result.stale_nodes, vec![NodeId::new(1)]);
    }

    #[test]
    fn resolve_concurrent_uses_total_tiebreaker() {
        let coord = ReadCoordinator::new(3);

        // Concurrent clocks: node 1 wrote on node 1, node 2 wrote on node 2.
        let clock1 = make_clock(&[(1, 3)]); // total = 3
        let clock2 = make_clock(&[(2, 5)]); // total = 5

        let responses = vec![
            ReplicaResponse {
                node_id: NodeId::new(1),
                document: make_doc("d1", "from-1"),
                version: clock1,
            },
            ReplicaResponse {
                node_id: NodeId::new(2),
                document: make_doc("d1", "from-2"),
                version: clock2,
            },
        ];

        let result = coord.resolve(&responses, ConsistencyLevel::Quorum).unwrap();
        // Node 2 has higher total, so it wins.
        assert_eq!(
            result.document.get_field("data"),
            Some(&FieldValue::Text("from-2".into()))
        );
    }

    #[test]
    fn resolve_all_agree_no_stale_nodes() {
        let coord = ReadCoordinator::new(3);
        let clock = make_clock(&[(1, 5), (2, 3)]);

        let responses = vec![
            ReplicaResponse {
                node_id: NodeId::new(1),
                document: make_doc("d1", "same"),
                version: clock.clone(),
            },
            ReplicaResponse {
                node_id: NodeId::new(2),
                document: make_doc("d1", "same"),
                version: clock.clone(),
            },
            ReplicaResponse {
                node_id: NodeId::new(3),
                document: make_doc("d1", "same"),
                version: clock,
            },
        ];

        let result = coord.resolve(&responses, ConsistencyLevel::All).unwrap();
        assert!(result.stale_nodes.is_empty());
        assert_eq!(result.response_count, 3);
    }

    #[test]
    fn resolve_all_fails_with_two_responses() {
        let coord = ReadCoordinator::new(3);
        let responses = vec![
            ReplicaResponse {
                node_id: NodeId::new(1),
                document: make_doc("d1", "v1"),
                version: make_clock(&[(1, 1)]),
            },
            ReplicaResponse {
                node_id: NodeId::new(2),
                document: make_doc("d1", "v1"),
                version: make_clock(&[(1, 1)]),
            },
        ];

        let result = coord.resolve(&responses, ConsistencyLevel::All);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_empty_responses_returns_not_found() {
        let coord = ReadCoordinator::new(3);
        let result = coord.resolve(&[], ConsistencyLevel::One);
        assert!(matches!(result, Err(DbError::NotFound(_))));
    }

    #[test]
    fn resolve_three_way_split_picks_highest_total() {
        let coord = ReadCoordinator::new(3);

        // All three clocks are concurrent with each other.
        let responses = vec![
            ReplicaResponse {
                node_id: NodeId::new(1),
                document: make_doc("d1", "v1"),
                version: make_clock(&[(1, 2)]), // total=2
            },
            ReplicaResponse {
                node_id: NodeId::new(2),
                document: make_doc("d1", "v2"),
                version: make_clock(&[(2, 4)]), // total=4, winner
            },
            ReplicaResponse {
                node_id: NodeId::new(3),
                document: make_doc("d1", "v3"),
                version: make_clock(&[(3, 3)]), // total=3
            },
        ];

        let result = coord.resolve(&responses, ConsistencyLevel::All).unwrap();
        assert_eq!(
            result.document.get_field("data"),
            Some(&FieldValue::Text("v2".into()))
        );
    }

    #[test]
    fn resolve_identifies_multiple_stale_nodes() {
        let coord = ReadCoordinator::new(3);

        // Node 1 has the latest; nodes 2 and 3 are stale.
        let latest_clock = make_clock(&[(1, 5), (2, 3)]);
        let stale_clock_a = make_clock(&[(1, 3), (2, 2)]);
        let stale_clock_b = make_clock(&[(1, 2)]);

        let responses = vec![
            ReplicaResponse {
                node_id: NodeId::new(1),
                document: make_doc("d1", "latest"),
                version: latest_clock,
            },
            ReplicaResponse {
                node_id: NodeId::new(2),
                document: make_doc("d1", "stale-a"),
                version: stale_clock_a,
            },
            ReplicaResponse {
                node_id: NodeId::new(3),
                document: make_doc("d1", "stale-b"),
                version: stale_clock_b,
            },
        ];

        let result = coord.resolve(&responses, ConsistencyLevel::All).unwrap();
        assert_eq!(
            result.document.get_field("data"),
            Some(&FieldValue::Text("latest".into()))
        );
        assert_eq!(result.stale_nodes.len(), 2);
        assert!(result.stale_nodes.contains(&NodeId::new(2)));
        assert!(result.stale_nodes.contains(&NodeId::new(3)));
    }

    #[test]
    fn validate_response_count_ok() {
        let coord = ReadCoordinator::new(3);
        assert!(coord
            .validate_response_count(ConsistencyLevel::One, 1)
            .is_ok());
        assert!(coord
            .validate_response_count(ConsistencyLevel::Quorum, 2)
            .is_ok());
        assert!(coord
            .validate_response_count(ConsistencyLevel::All, 3)
            .is_ok());
    }

    #[test]
    fn validate_response_count_insufficient() {
        let coord = ReadCoordinator::new(3);
        assert!(coord
            .validate_response_count(ConsistencyLevel::Quorum, 1)
            .is_err());
        assert!(coord
            .validate_response_count(ConsistencyLevel::All, 2)
            .is_err());
    }

    #[test]
    fn replication_factor_accessor() {
        let coord = ReadCoordinator::new(5);
        assert_eq!(coord.replication_factor(), 5);
    }
}
