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

    // Count active nodes from the cluster manager's health state.
    let health_snapshot = state.cluster_manager.health_snapshot();
    let number_of_nodes = health_snapshot.len().max(1) as u64;
    let active_nodes = health_snapshot
        .iter()
        .filter(|h| h.status == NodeStatus::Leader || h.status == NodeStatus::Follower)
        .count()
        .max(1) as u64;

    // Build replication lag map from metrics.
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
                        size_bytes: 0,
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
    // Read the actual cluster topology from the cluster router.
    let router = state.cluster_router.read().await;
    let nodes: Vec<NodeInfo> = router.healthy_nodes().into_iter().cloned().collect();
    drop(router);

    // If no nodes are tracked yet, fall back to the local node.
    let nodes = if nodes.is_empty() {
        let node_id = state.raft_node.node_id();
        let is_leader = state.raft_node.is_leader();
        let node_status = if is_leader {
            NodeStatus::Leader
        } else {
            NodeStatus::Follower
        };
        vec![NodeInfo {
            id: NodeId::new(node_id),
            address: NodeAddress::new("127.0.0.1", 9200),
            status: node_status,
        }]
    } else {
        nodes
    };

    let cluster = ClusterState {
        nodes,
        leader: state.raft_node.current_leader().map(NodeId::new),
    };

    Json(serde_json::to_value(cluster).unwrap())
}

// ---------------------------------------------------------------------------
// GET /_nodes — list nodes
// ---------------------------------------------------------------------------

/// List all known nodes in the cluster.
pub async fn list_nodes(State(state): State<AppState>) -> impl IntoResponse {
    // Build the node list from the cluster manager's health snapshot,
    // enriched with health status from the failure detector.
    let health_snapshot = state.cluster_manager.health_snapshot();

    let nodes: Vec<serde_json::Value> = if health_snapshot.is_empty() {
        // Fallback: report this node only.
        let node_id = state.raft_node.node_id();
        let is_leader = state.raft_node.is_leader();
        let status = if is_leader {
            NodeStatus::Leader
        } else {
            NodeStatus::Follower
        };
        vec![serde_json::to_value(NodeInfo {
            id: NodeId::new(node_id),
            address: NodeAddress::new("127.0.0.1", 9200),
            status,
        })
        .unwrap()]
    } else {
        health_snapshot
            .iter()
            .map(|h| {
                serde_json::json!({
                    "id": h.node_id.as_u64(),
                    "status": format!("{}", h.status),
                    "last_seen_ms_ago": h.last_seen.elapsed().as_millis() as u64,
                    "consecutive_failures": h.consecutive_failures,
                    "latency_ms": h.latency_ms,
                })
            })
            .collect()
    };

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
