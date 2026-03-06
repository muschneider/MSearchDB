//! gRPC server implementations for MSearchDB inter-node communication.
//!
//! This module provides [`NodeServiceImpl`] and [`QueryServiceImpl`], which
//! implement the tonic-generated service traits from `proto/msearchdb.proto`.
//!
//! [`NodeServiceImpl`] handles Raft consensus RPCs (AppendEntries, RequestVote,
//! InstallSnapshot), write forwarding, health checks, and cluster join requests.
//!
//! [`QueryServiceImpl`] handles search, document retrieval, and scatter-gather
//! streaming responses.

use std::pin::Pin;
use std::sync::Arc;

use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

use msearchdb_consensus::raft_node::RaftNode;
use msearchdb_consensus::types::{RaftCommand, TypeConfig};
use msearchdb_core::document::DocumentId;
use msearchdb_core::error::DbError;
use msearchdb_core::query::Query;
use msearchdb_core::traits::{IndexBackend, StorageBackend};
use openraft::raft::{
    AppendEntriesRequest as RaftAppendReq, InstallSnapshotRequest as RaftSnapshotReq,
    VoteRequest as RaftVoteReq,
};

use crate::proto;

// ---------------------------------------------------------------------------
// NodeServiceImpl
// ---------------------------------------------------------------------------

/// Implements the `NodeService` gRPC service for inter-node Raft communication.
///
/// Wraps a [`RaftNode`] and delegates consensus RPCs to the underlying openraft
/// handle.  Write forwarding deserialises the [`RaftCommand`] and proposes it
/// through local Raft consensus.
pub struct NodeServiceImpl {
    /// The local Raft node handle.
    raft: Arc<RaftNode>,
    /// Node id of this server.
    node_id: u64,
}

impl NodeServiceImpl {
    /// Create a new `NodeServiceImpl` wrapping the given Raft node.
    pub fn new(raft: Arc<RaftNode>, node_id: u64) -> Self {
        Self { raft, node_id }
    }
}

#[tonic::async_trait]
impl proto::node_service_server::NodeService for NodeServiceImpl {
    async fn append_entries(
        &self,
        request: Request<proto::AppendEntriesRequest>,
    ) -> Result<Response<proto::AppendEntriesResponse>, Status> {
        let inner = request.into_inner();

        let raft_req: RaftAppendReq<TypeConfig> =
            serde_json::from_slice(&inner.payload).map_err(|e| {
                Status::invalid_argument(format!("failed to deserialize AppendEntries: {}", e))
            })?;

        let result = self.raft.raft_handle().append_entries(raft_req).await;

        match result {
            Ok(resp) => {
                let payload = serde_json::to_vec(&resp).map_err(|e| {
                    Status::internal(format!("failed to serialize AppendEntries response: {}", e))
                })?;
                Ok(Response::new(proto::AppendEntriesResponse { payload }))
            }
            Err(e) => Err(Status::internal(format!("append_entries failed: {}", e))),
        }
    }

    async fn request_vote(
        &self,
        request: Request<proto::VoteRequest>,
    ) -> Result<Response<proto::VoteResponse>, Status> {
        let inner = request.into_inner();

        let raft_req: RaftVoteReq<u64> = serde_json::from_slice(&inner.payload).map_err(|e| {
            Status::invalid_argument(format!("failed to deserialize VoteRequest: {}", e))
        })?;

        let result = self.raft.raft_handle().vote(raft_req).await;

        match result {
            Ok(resp) => {
                let payload = serde_json::to_vec(&resp).map_err(|e| {
                    Status::internal(format!("failed to serialize VoteResponse: {}", e))
                })?;
                Ok(Response::new(proto::VoteResponse { payload }))
            }
            Err(e) => Err(Status::internal(format!("request_vote failed: {}", e))),
        }
    }

    async fn install_snapshot(
        &self,
        request: Request<Streaming<proto::SnapshotChunk>>,
    ) -> Result<Response<proto::InstallSnapshotResponse>, Status> {
        let mut stream = request.into_inner();

        let mut full_data = Vec::new();
        let mut metadata_bytes: Option<Vec<u8>> = None;

        // Receive all chunks from the streaming RPC.
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;

            // Capture metadata from the first chunk.
            if !chunk.metadata.is_empty() && metadata_bytes.is_none() {
                metadata_bytes = Some(chunk.metadata);
            }

            full_data.extend_from_slice(&chunk.data);

            if chunk.done {
                break;
            }
        }

        let meta = metadata_bytes
            .ok_or_else(|| Status::invalid_argument("no snapshot metadata received"))?;

        let raft_req: RaftSnapshotReq<TypeConfig> = serde_json::from_slice(&meta).map_err(|e| {
            Status::invalid_argument(format!("failed to deserialize snapshot metadata: {}", e))
        })?;

        let result = self.raft.raft_handle().install_snapshot(raft_req).await;

        match result {
            Ok(resp) => {
                let payload = serde_json::to_vec(&resp).map_err(|e| {
                    Status::internal(format!(
                        "failed to serialize InstallSnapshot response: {}",
                        e
                    ))
                })?;
                Ok(Response::new(proto::InstallSnapshotResponse { payload }))
            }
            Err(e) => Err(Status::internal(format!("install_snapshot failed: {}", e))),
        }
    }

    async fn forward_write(
        &self,
        request: Request<proto::WriteRequest>,
    ) -> Result<Response<proto::WriteResponse>, Status> {
        let inner = request.into_inner();

        let cmd: RaftCommand = serde_json::from_slice(&inner.command).map_err(|e| {
            Status::invalid_argument(format!("failed to deserialize RaftCommand: {}", e))
        })?;

        match self.raft.propose(cmd).await {
            Ok(resp) => Ok(Response::new(proto::WriteResponse {
                success: resp.success,
                document_id: resp
                    .document_id
                    .map(|id| id.as_str().to_owned())
                    .unwrap_or_default(),
                error: String::new(),
            })),
            Err(e) => Ok(Response::new(proto::WriteResponse {
                success: false,
                document_id: String::new(),
                error: e.to_string(),
            })),
        }
    }

    async fn health_check(
        &self,
        _request: Request<proto::HealthRequest>,
    ) -> Result<Response<proto::HealthResponse>, Status> {
        let metrics = self.raft.metrics().await;

        let status_str = if self.raft.is_leader() {
            "Leader"
        } else {
            "Follower"
        };

        let leader_id = metrics.current_leader.unwrap_or(0);
        let last_applied = metrics.last_applied.map(|la| la.index).unwrap_or(0);

        Ok(Response::new(proto::HealthResponse {
            node_id: self.node_id,
            status: status_str.to_string(),
            leader_id,
            current_term: metrics.current_term,
            last_applied,
            is_healthy: true,
        }))
    }

    async fn join_cluster(
        &self,
        request: Request<proto::JoinRequest>,
    ) -> Result<Response<proto::JoinResponse>, Status> {
        let inner = request.into_inner();

        let addr = msearchdb_core::cluster::NodeAddress::new(
            inner.address.clone(),
            0, // Port is embedded in the address string.
        );

        match self.raft.add_learner(inner.node_id, &addr).await {
            Ok(()) => {
                let leader_id = self.raft.current_leader().unwrap_or(0);
                Ok(Response::new(proto::JoinResponse {
                    success: true,
                    error: String::new(),
                    leader_id,
                }))
            }
            Err(e) => Ok(Response::new(proto::JoinResponse {
                success: false,
                error: e.to_string(),
                leader_id: self.raft.current_leader().unwrap_or(0),
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// QueryServiceImpl
// ---------------------------------------------------------------------------

/// Implements the `QueryService` gRPC service for search and document retrieval.
///
/// Wraps local [`StorageBackend`] and [`IndexBackend`] instances to serve
/// queries directly from the local node's data.
pub struct QueryServiceImpl {
    /// Local storage engine for document retrieval.
    storage: Arc<dyn StorageBackend>,
    /// Local index engine for search queries.
    index: Arc<dyn IndexBackend>,
    /// Node id of this server (included in scatter results).
    node_id: u64,
}

impl QueryServiceImpl {
    /// Create a new `QueryServiceImpl`.
    pub fn new(
        storage: Arc<dyn StorageBackend>,
        index: Arc<dyn IndexBackend>,
        node_id: u64,
    ) -> Self {
        Self {
            storage,
            index,
            node_id,
        }
    }
}

/// Type alias for the streaming response from the `Scatter` RPC.
type ScatterStream = Pin<Box<dyn Stream<Item = Result<proto::ScatterResult, Status>> + Send>>;

#[tonic::async_trait]
impl proto::query_service_server::QueryService for QueryServiceImpl {
    async fn search(
        &self,
        request: Request<proto::SearchRequest>,
    ) -> Result<Response<proto::SearchResponse>, Status> {
        let inner = request.into_inner();

        let query: Query = serde_json::from_slice(&inner.query)
            .map_err(|e| Status::invalid_argument(format!("failed to deserialize Query: {}", e)))?;

        match self.index.search(&query).await {
            Ok(result) => {
                let payload = serde_json::to_vec(&result).map_err(|e| {
                    Status::internal(format!("failed to serialize SearchResult: {}", e))
                })?;
                Ok(Response::new(proto::SearchResponse {
                    result: payload,
                    error: String::new(),
                }))
            }
            Err(e) => Ok(Response::new(proto::SearchResponse {
                result: Vec::new(),
                error: e.to_string(),
            })),
        }
    }

    async fn get(
        &self,
        request: Request<proto::GetRequest>,
    ) -> Result<Response<proto::GetResponse>, Status> {
        let inner = request.into_inner();
        let doc_id = DocumentId::new(inner.document_id);

        match self.storage.get(&doc_id).await {
            Ok(doc) => {
                let payload = serde_json::to_vec(&doc).map_err(|e| {
                    Status::internal(format!("failed to serialize Document: {}", e))
                })?;
                Ok(Response::new(proto::GetResponse {
                    document: payload,
                    found: true,
                    error: String::new(),
                }))
            }
            Err(DbError::NotFound(_)) => Ok(Response::new(proto::GetResponse {
                document: Vec::new(),
                found: false,
                error: String::new(),
            })),
            Err(e) => Ok(Response::new(proto::GetResponse {
                document: Vec::new(),
                found: false,
                error: e.to_string(),
            })),
        }
    }

    type ScatterStream = ScatterStream;

    async fn scatter(
        &self,
        request: Request<proto::ScatterRequest>,
    ) -> Result<Response<Self::ScatterStream>, Status> {
        let inner = request.into_inner();

        let query: Query = serde_json::from_slice(&inner.query)
            .map_err(|e| Status::invalid_argument(format!("failed to deserialize Query: {}", e)))?;

        let node_id = self.node_id;

        // Execute search locally and stream results one at a time.
        let result = self
            .index
            .search(&query)
            .await
            .map_err(|e| Status::internal(format!("local search failed: {}", e)))?;

        let docs = result.documents;

        let stream = tokio_stream::iter(docs.into_iter().map(move |scored| {
            let payload = serde_json::to_vec(&scored).map_err(|e| {
                Status::internal(format!("failed to serialize ScoredDocument: {}", e))
            })?;
            Ok(proto::ScatterResult {
                scored_document: payload,
                node_id,
            })
        }));

        Ok(Response::new(Box::pin(stream)))
    }
}

// ---------------------------------------------------------------------------
// Server startup helper
// ---------------------------------------------------------------------------

/// Start the gRPC server on the given address.
///
/// Binds both [`NodeServiceImpl`] and [`QueryServiceImpl`] to the same
/// tonic transport.
pub async fn start_grpc_server(
    addr: std::net::SocketAddr,
    node_service: NodeServiceImpl,
    query_service: QueryServiceImpl,
) -> Result<(), tonic::transport::Error> {
    tracing::info!(%addr, "starting gRPC server");

    tonic::transport::Server::builder()
        .add_service(proto::node_service_server::NodeServiceServer::new(
            node_service,
        ))
        .add_service(proto::query_service_server::QueryServiceServer::new(
            query_service,
        ))
        .serve(addr)
        .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proto_write_response_default_values() {
        let resp = proto::WriteResponse {
            success: true,
            document_id: "doc-1".into(),
            error: String::new(),
        };
        assert!(resp.success);
        assert_eq!(resp.document_id, "doc-1");
    }

    #[test]
    fn proto_health_response_construction() {
        let resp = proto::HealthResponse {
            node_id: 1,
            status: "Leader".into(),
            leader_id: 1,
            current_term: 5,
            last_applied: 42,
            is_healthy: true,
        };
        assert_eq!(resp.node_id, 1);
        assert_eq!(resp.status, "Leader");
        assert!(resp.is_healthy);
    }

    #[test]
    fn proto_join_response_construction() {
        let resp = proto::JoinResponse {
            success: true,
            error: String::new(),
            leader_id: 1,
        };
        assert!(resp.success);
    }

    #[test]
    fn proto_search_request_roundtrip() {
        use msearchdb_core::query::{FullTextQuery, Operator};
        let query = Query::FullText(FullTextQuery {
            field: "title".into(),
            query: "rust".into(),
            operator: Operator::Or,
        });
        let bytes = serde_json::to_vec(&query).unwrap();
        let req = proto::SearchRequest {
            query: bytes.clone(),
            limit: 10,
        };
        let back: Query = serde_json::from_slice(&req.query).unwrap();
        assert_eq!(query, back);
    }

    #[test]
    fn proto_get_request_construction() {
        let req = proto::GetRequest {
            document_id: "abc-123".into(),
        };
        assert_eq!(req.document_id, "abc-123");
    }

    #[test]
    fn proto_scatter_result_construction() {
        use msearchdb_core::document::{Document, DocumentId, FieldValue};
        use msearchdb_core::query::ScoredDocument;
        let scored = ScoredDocument {
            document: Document::new(DocumentId::new("s1"))
                .with_field("title", FieldValue::Text("test".into())),
            score: 1.5,
        };
        let bytes = serde_json::to_vec(&scored).unwrap();
        let result = proto::ScatterResult {
            scored_document: bytes.clone(),
            node_id: 2,
        };
        let back: ScoredDocument = serde_json::from_slice(&result.scored_document).unwrap();
        assert_eq!(scored, back);
        assert_eq!(result.node_id, 2);
    }
}
