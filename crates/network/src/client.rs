//! gRPC client for MSearchDB inter-node communication.
//!
//! [`NodeClient`] wraps a tonic `Channel` and provides typed methods for every
//! RPC defined in `proto/msearchdb.proto`.  Connection establishment uses
//! exponential backoff (100 ms → 200 ms → 400 ms → 800 ms → 1600 ms,
//! max 5 retries).
//!
//! # Examples
//!
//! ```ignore
//! use msearchdb_network::client::NodeClient;
//! use msearchdb_core::cluster::NodeAddress;
//!
//! let client = NodeClient::connect(&NodeAddress::new("127.0.0.1", 9300)).await?;
//! let health = client.health_check().await?;
//! ```

use std::time::Duration;

use tonic::transport::Channel;

use msearchdb_consensus::types::{RaftCommand, RaftResponse};
use msearchdb_core::cluster::NodeAddress;
use msearchdb_core::document::{Document, DocumentId};
use msearchdb_core::error::{DbError, DbResult};
use msearchdb_core::query::{Query, SearchResult};

use crate::proto;

// ---------------------------------------------------------------------------
// HealthStatus
// ---------------------------------------------------------------------------

/// Summary of a remote node's health returned by [`NodeClient::health_check`].
#[derive(Clone, Debug, PartialEq)]
pub struct HealthStatus {
    /// The remote node's id.
    pub node_id: u64,
    /// Status string: "Leader", "Follower", "Candidate", "Offline".
    pub status: String,
    /// Node id of the current leader (0 if unknown).
    pub leader_id: u64,
    /// Current Raft term.
    pub current_term: u64,
    /// Last applied log index.
    pub last_applied: u64,
    /// Whether the node reports itself as healthy.
    pub is_healthy: bool,
}

// ---------------------------------------------------------------------------
// Backoff configuration
// ---------------------------------------------------------------------------

/// Default initial backoff delay for connection retries.
const INITIAL_BACKOFF_MS: u64 = 100;

/// Maximum number of connection retry attempts.
const MAX_RETRIES: u32 = 5;

/// Compute the backoff duration for a given attempt (0-indexed).
///
/// Uses exponential backoff: `INITIAL_BACKOFF_MS * 2^attempt`.
pub(crate) fn backoff_duration(attempt: u32) -> Duration {
    Duration::from_millis(INITIAL_BACKOFF_MS * 2u64.pow(attempt))
}

// ---------------------------------------------------------------------------
// NodeClient
// ---------------------------------------------------------------------------

/// A gRPC client for communicating with a remote MSearchDB node.
///
/// Wraps tonic-generated service clients and provides a clean, typed API
/// for all inter-node RPCs.
#[derive(Clone)]
pub struct NodeClient {
    /// The underlying tonic channel (multiplexed HTTP/2 connection).
    channel: Channel,
    /// Network address of the remote node.
    addr: String,
}

impl NodeClient {
    /// Connect to a remote node at the given [`NodeAddress`].
    ///
    /// Retries up to [`MAX_RETRIES`] times with exponential backoff starting
    /// at 100 ms.
    pub async fn connect(addr: &NodeAddress) -> DbResult<Self> {
        let endpoint = format!("http://{}:{}", addr.host, addr.port);
        Self::connect_to_endpoint(&endpoint).await
    }

    /// Connect using a raw endpoint string (e.g. `"http://127.0.0.1:9300"`).
    pub async fn connect_to_endpoint(endpoint: &str) -> DbResult<Self> {
        let mut last_err = None;

        for attempt in 0..MAX_RETRIES {
            match Channel::from_shared(endpoint.to_string())
                .map_err(|e| DbError::NetworkError(format!("invalid endpoint: {}", e)))?
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .connect()
                .await
            {
                Ok(channel) => {
                    tracing::info!(endpoint, "connected to remote node");
                    return Ok(Self {
                        channel,
                        addr: endpoint.to_string(),
                    });
                }
                Err(e) => {
                    let delay = backoff_duration(attempt);
                    tracing::warn!(
                        endpoint,
                        attempt = attempt + 1,
                        max_retries = MAX_RETRIES,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        "connection failed, retrying"
                    );
                    last_err = Some(e);
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Err(DbError::NetworkError(format!(
            "failed to connect to {} after {} retries: {}",
            endpoint,
            MAX_RETRIES,
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown".into())
        )))
    }

    /// Create a client from an already-established tonic [`Channel`].
    ///
    /// Useful for testing and connection pooling.
    pub fn from_channel(channel: Channel, addr: String) -> Self {
        Self { channel, addr }
    }

    /// Return the address this client is connected to.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    // -- Raft RPCs ---------------------------------------------------------

    /// Send an `AppendEntries` RPC to the remote node.
    pub async fn send_append_entries(&self, payload: Vec<u8>) -> DbResult<Vec<u8>> {
        let mut client = proto::node_service_client::NodeServiceClient::new(self.channel.clone());

        let resp = client
            .append_entries(proto::AppendEntriesRequest { payload })
            .await
            .map_err(|e| DbError::NetworkError(format!("append_entries RPC failed: {}", e)))?;

        Ok(resp.into_inner().payload)
    }

    /// Send a `RequestVote` RPC to the remote node.
    pub async fn send_vote(&self, payload: Vec<u8>) -> DbResult<Vec<u8>> {
        let mut client = proto::node_service_client::NodeServiceClient::new(self.channel.clone());

        let resp = client
            .request_vote(proto::VoteRequest { payload })
            .await
            .map_err(|e| DbError::NetworkError(format!("request_vote RPC failed: {}", e)))?;

        Ok(resp.into_inner().payload)
    }

    /// Forward a write command to the remote node (typically the leader).
    pub async fn forward_write(&self, cmd: &RaftCommand) -> DbResult<RaftResponse> {
        let command = serde_json::to_vec(cmd)
            .map_err(|e| DbError::SerializationError(format!("serialize RaftCommand: {}", e)))?;

        let mut client = proto::node_service_client::NodeServiceClient::new(self.channel.clone());

        let resp = client
            .forward_write(proto::WriteRequest { command })
            .await
            .map_err(|e| DbError::NetworkError(format!("forward_write RPC failed: {}", e)))?;

        let inner = resp.into_inner();

        if inner.success {
            let doc_id = if inner.document_id.is_empty() {
                None
            } else {
                Some(DocumentId::new(inner.document_id))
            };
            Ok(RaftResponse {
                success: true,
                document_id: doc_id,
            })
        } else {
            Err(DbError::NetworkError(format!(
                "forward_write rejected: {}",
                inner.error
            )))
        }
    }

    // -- Query RPCs --------------------------------------------------------

    /// Execute a search query on the remote node.
    pub async fn search(&self, query: &Query, limit: usize) -> DbResult<SearchResult> {
        let query_bytes = serde_json::to_vec(query)
            .map_err(|e| DbError::SerializationError(format!("serialize Query: {}", e)))?;

        let mut client = proto::query_service_client::QueryServiceClient::new(self.channel.clone());

        let resp = client
            .search(proto::SearchRequest {
                query: query_bytes,
                limit: limit as u32,
            })
            .await
            .map_err(|e| DbError::NetworkError(format!("search RPC failed: {}", e)))?;

        let inner = resp.into_inner();

        if !inner.error.is_empty() {
            return Err(DbError::NetworkError(format!(
                "remote search error: {}",
                inner.error
            )));
        }

        let result: SearchResult = serde_json::from_slice(&inner.result)
            .map_err(|e| DbError::SerializationError(format!("deserialize SearchResult: {}", e)))?;

        Ok(result)
    }

    /// Retrieve a document by id from the remote node.
    pub async fn get(&self, id: &DocumentId) -> DbResult<Option<Document>> {
        let mut client = proto::query_service_client::QueryServiceClient::new(self.channel.clone());

        let resp = client
            .get(proto::GetRequest {
                document_id: id.as_str().to_owned(),
            })
            .await
            .map_err(|e| DbError::NetworkError(format!("get RPC failed: {}", e)))?;

        let inner = resp.into_inner();

        if !inner.error.is_empty() {
            return Err(DbError::NetworkError(format!(
                "remote get error: {}",
                inner.error
            )));
        }

        if !inner.found {
            return Ok(None);
        }

        let doc: Document = serde_json::from_slice(&inner.document)
            .map_err(|e| DbError::SerializationError(format!("deserialize Document: {}", e)))?;

        Ok(Some(doc))
    }

    /// Perform a health check on the remote node.
    pub async fn health_check(&self) -> DbResult<HealthStatus> {
        let mut client = proto::node_service_client::NodeServiceClient::new(self.channel.clone());

        let resp = client
            .health_check(proto::HealthRequest {})
            .await
            .map_err(|e| DbError::NetworkError(format!("health_check RPC failed: {}", e)))?;

        let inner = resp.into_inner();

        Ok(HealthStatus {
            node_id: inner.node_id,
            status: inner.status,
            leader_id: inner.leader_id,
            current_term: inner.current_term,
            last_applied: inner.last_applied,
            is_healthy: inner.is_healthy,
        })
    }

    /// Request to join a remote cluster.
    pub async fn join_cluster(&self, node_id: u64, address: &str) -> DbResult<()> {
        let mut client = proto::node_service_client::NodeServiceClient::new(self.channel.clone());

        let resp = client
            .join_cluster(proto::JoinRequest {
                node_id,
                address: address.to_string(),
            })
            .await
            .map_err(|e| DbError::NetworkError(format!("join_cluster RPC failed: {}", e)))?;

        let inner = resp.into_inner();

        if inner.success {
            Ok(())
        } else {
            Err(DbError::NetworkError(format!(
                "join_cluster rejected: {}",
                inner.error
            )))
        }
    }
}

impl std::fmt::Debug for NodeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeClient")
            .field("addr", &self.addr)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_duration_exponential() {
        assert_eq!(backoff_duration(0), Duration::from_millis(100));
        assert_eq!(backoff_duration(1), Duration::from_millis(200));
        assert_eq!(backoff_duration(2), Duration::from_millis(400));
        assert_eq!(backoff_duration(3), Duration::from_millis(800));
        assert_eq!(backoff_duration(4), Duration::from_millis(1600));
    }

    #[test]
    fn health_status_construction() {
        let hs = HealthStatus {
            node_id: 1,
            status: "Leader".into(),
            leader_id: 1,
            current_term: 3,
            last_applied: 10,
            is_healthy: true,
        };
        assert_eq!(hs.node_id, 1);
        assert!(hs.is_healthy);
    }

    #[test]
    fn node_client_debug_format() {
        // We can't construct a real Channel without a connection, but
        // we can verify the Debug impl compiles.
        let _fmt_fn = |c: &NodeClient| format!("{:?}", c);
    }

    #[tokio::test]
    async fn connect_to_unreachable_returns_error() {
        let addr = NodeAddress::new("127.0.0.1", 1); // port 1 is unlikely to be open
        let result = NodeClient::connect(&addr).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            DbError::NetworkError(msg) => {
                assert!(msg.contains("failed to connect"));
            }
            other => panic!("expected NetworkError, got: {:?}", other),
        }
    }
}
