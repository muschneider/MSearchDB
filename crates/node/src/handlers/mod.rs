//! HTTP request handlers for the MSearchDB REST API.
//!
//! Each sub-module corresponds to a logical group of endpoints:
//!
//! | Module | Endpoints |
//! |--------|-----------|
//! | [`collections`] | `PUT/DELETE/GET /collections/{name}` |
//! | [`documents`] | `POST/PUT/GET/DELETE /collections/{name}/docs/{id}` |
//! | [`search`] | `POST/GET /collections/{name}/_search` |
//! | [`bulk`] | `POST /collections/{name}/docs/_bulk` |
//! | [`aliases`] | `PUT/DELETE/GET /_aliases/{name}`, `GET /_aliases` |
//! | [`cluster`] | `GET /_cluster/health`, `GET /_cluster/state`, `GET /_nodes` |
//! | [`admin`] | `POST /_refresh`, `GET /_stats` |

pub mod admin;
pub mod aliases;
pub mod bulk;
pub mod cluster;
pub mod collections;
pub mod documents;
pub mod search;
pub mod snapshot;

use std::sync::Arc;

use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_consensus::types::{RaftCommand, RaftResponse};
use msearchdb_core::cluster::{NodeAddress, NodeId};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_network::connection_pool::ConnectionPool;

// ---------------------------------------------------------------------------
// Write-forwarding helper
// ---------------------------------------------------------------------------

/// Propose a [`RaftCommand`] to the local Raft leader, or forward it to the
/// leader if this node is a follower.
///
/// Returns `Err(DbError::NetworkError("no leader available"))` when no leader
/// is known (HTTP handlers should map this to 503 Service Unavailable).
pub async fn propose_or_forward(
    raft_node: &Arc<RaftNode>,
    connection_pool: &Arc<ConnectionPool>,
    cmd: RaftCommand,
) -> DbResult<RaftResponse> {
    match raft_node.propose(cmd.clone()).await {
        Ok(resp) => Ok(resp),
        Err(DbError::ConsensusError(msg))
            if msg.contains("not leader") || msg.contains("forward") =>
        {
            // Not the leader — forward to the current leader.
            let leader_id = raft_node.current_leader().ok_or_else(|| {
                DbError::NetworkError("no leader available".to_string())
            })?;

            // Look up the leader's gRPC address from Raft membership.
            let metrics = raft_node.metrics().await;
            let leader_addr = metrics
                .membership_config
                .membership()
                .nodes()
                .find(|(id, _)| **id == leader_id)
                .map(|(_, basic_node)| {
                    let parts: Vec<&str> = basic_node.addr.rsplitn(2, ':').collect();
                    if parts.len() == 2 {
                        let port = parts[0].parse::<u16>().unwrap_or(9300);
                        NodeAddress::new(parts[1], port)
                    } else {
                        NodeAddress::new(&basic_node.addr, 9300)
                    }
                })
                .ok_or_else(|| {
                    DbError::NetworkError(format!(
                        "leader node-{} not found in membership",
                        leader_id
                    ))
                })?;

            let node_id = NodeId::new(leader_id);
            let client = connection_pool.get(&node_id, &leader_addr).await?;

            client.forward_write(&cmd).await
        }
        Err(e) => Err(e),
    }
}
