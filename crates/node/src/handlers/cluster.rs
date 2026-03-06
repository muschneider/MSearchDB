//! Cluster management handlers.
//!
//! These endpoints expose the cluster topology, health status, and node
//! management operations.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use msearchdb_core::cluster::{ClusterState, NodeAddress, NodeId, NodeInfo, NodeStatus};

use crate::dto::{ClusterHealthResponse, ErrorResponse, JoinNodeRequest};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// GET /_cluster/health — cluster health
// ---------------------------------------------------------------------------

/// Return the cluster health status.
///
/// Health is determined by:
/// - **green**: leader exists, quorum met.
/// - **yellow**: leader exists, but degraded.
/// - **red**: no leader or node is unhealthy.
pub async fn cluster_health(State(state): State<AppState>) -> impl IntoResponse {
    let is_leader = state.raft_node.is_leader();
    let leader_id = state.raft_node.current_leader();

    let status = if leader_id.is_some() { "green" } else { "red" };

    let resp = ClusterHealthResponse {
        status: status.to_string(),
        number_of_nodes: 1, // single-node for now
        leader_id,
        is_leader,
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
