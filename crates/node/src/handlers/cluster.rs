//! Cluster management handlers.
//!
//! These endpoints expose the cluster topology, health status, and node
//! management operations.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use msearchdb_core::cluster::{ClusterState, NodeAddress, NodeId, NodeInfo, NodeStatus};

use crate::dto::{ClusterHealthResponse, CollectionHealthInfo, ErrorResponse, JoinNodeRequest};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// GET /_cluster/health — cluster health
// ---------------------------------------------------------------------------

/// Return the cluster health status.
///
/// Health is determined by:
/// - **green**: all expected nodes are active (`number_of_nodes == active_nodes`).
/// - **yellow**: quorum is met (`active_nodes > number_of_nodes / 2`), but not
///   all nodes are reachable.
/// - **red**: no quorum (`active_nodes <= number_of_nodes / 2`) or no leader.
pub async fn cluster_health(State(state): State<AppState>) -> impl IntoResponse {
    let leader_id = state.raft_node.current_leader();
    let commit_index = state.metrics.raft_commit_index.get();

    // Count active nodes from node health metrics.
    // For now, single-node mode is always 1 active out of 1 total.
    let number_of_nodes: u64 = 1;
    let active_nodes: u64 = 1;

    // Build replication lag map from metrics.
    // In single-node mode there is no lag to report.
    let replication_lag = HashMap::new();

    // Build per-collection health information from the in-memory registry.
    let collections = {
        let coll = state.collections.read().await;
        coll.iter()
            .map(|(name, meta)| {
                (
                    name.clone(),
                    CollectionHealthInfo {
                        docs: meta.doc_count,
                        size_bytes: 0, // TODO: estimate from storage
                    },
                )
            })
            .collect::<HashMap<_, _>>()
    };

    // Determine overall cluster status.
    let status = if leader_id.is_none() {
        "red"
    } else if active_nodes == number_of_nodes {
        "green"
    } else if active_nodes > number_of_nodes / 2 {
        "yellow"
    } else {
        "red"
    };

    let resp = ClusterHealthResponse {
        status: status.to_string(),
        cluster_name: "msearchdb-cluster".to_string(),
        number_of_nodes,
        active_nodes,
        leader_node: leader_id,
        raft_commit_index: commit_index,
        replication_lag,
        collections,
    };

    Json(serde_json::to_value(resp).unwrap())
}

// ---------------------------------------------------------------------------
// GET /_cluster/state — cluster state
// ---------------------------------------------------------------------------

/// Return the full cluster state including all nodes and their statuses.
pub async fn cluster_state(State(state): State<AppState>) -> impl IntoResponse {
    let node_id = state.raft_node.node_id();
    let is_leader = state.raft_node.is_leader();

    let node_status = if is_leader {
        NodeStatus::Leader
    } else {
        NodeStatus::Follower
    };

    let cluster = ClusterState {
        nodes: vec![NodeInfo {
            id: NodeId::new(node_id),
            address: NodeAddress::new("127.0.0.1", 9200),
            status: node_status,
        }],
        leader: state.raft_node.current_leader().map(NodeId::new),
    };

    Json(serde_json::to_value(cluster).unwrap())
}

// ---------------------------------------------------------------------------
// GET /_nodes — list nodes
// ---------------------------------------------------------------------------

/// List all known nodes in the cluster.
pub async fn list_nodes(State(state): State<AppState>) -> impl IntoResponse {
    let node_id = state.raft_node.node_id();
    let is_leader = state.raft_node.is_leader();

    let status = if is_leader {
        NodeStatus::Leader
    } else {
        NodeStatus::Follower
    };

    let nodes = vec![NodeInfo {
        id: NodeId::new(node_id),
        address: NodeAddress::new("127.0.0.1", 9200),
        status,
    }];

    Json(serde_json::to_value(nodes).unwrap())
}

// ---------------------------------------------------------------------------
// POST /_nodes/{id}/_join — add node to cluster
// ---------------------------------------------------------------------------

/// Add a new node to the cluster as a learner.
pub async fn join_node(
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Json(body): Json<JoinNodeRequest>,
) -> impl IntoResponse {
    // Parse address
    let parts: Vec<&str> = body.address.split(':').collect();
    let (host, port) = if parts.len() == 2 {
        let port = parts[1].parse::<u16>().unwrap_or(9300);
        (parts[0].to_string(), port)
    } else {
        (body.address.clone(), 9300)
    };

    let addr = NodeAddress::new(host, port);

    match state.raft_node.add_learner(id, &addr).await {
        Ok(()) => {
            let body = serde_json::json!({
                "acknowledged": true,
                "node_id": id,
            });
            (StatusCode::OK, Json(body))
        }
        Err(e) => {
            let resp = ErrorResponse::internal(e.to_string());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::to_value(resp).unwrap()),
            )
        }
    }
}
