//! gRPC-backed Raft network transport for MSearchDB.
//!
//! Implements [`openraft::RaftNetworkFactory`] and [`openraft::RaftNetwork`]
//! using the gRPC client ([`NodeClient`]) as the underlying transport.
//!
//! This module bridges openraft's trait-based network abstraction to the
//! concrete gRPC RPCs defined in `proto/msearchdb.proto`, enabling
//! multi-node Raft consensus over the network.

use std::io;

use openraft::error::{InstallSnapshotError, RPCError, RaftError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::BasicNode;

use msearchdb_consensus::types::TypeConfig;

use crate::client::NodeClient;
use crate::proto;

// ---------------------------------------------------------------------------
// GrpcNetworkFactory
// ---------------------------------------------------------------------------

/// A [`RaftNetworkFactory`] that creates [`GrpcNetwork`] clients for each
/// target node.
///
/// Each call to [`new_client`](RaftNetworkFactory::new_client) establishes a
/// gRPC connection (or reuses an existing channel) to the target node's
/// address stored in [`BasicNode::addr`].
pub struct GrpcNetworkFactory;

impl RaftNetworkFactory<TypeConfig> for GrpcNetworkFactory {
    type Network = GrpcNetwork;

    async fn new_client(&mut self, _target: u64, node: &BasicNode) -> Self::Network {
        GrpcNetwork {
            addr: node.addr.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// GrpcNetwork
// ---------------------------------------------------------------------------

/// A Raft network client that sends RPCs over gRPC to a single remote node.
///
/// Payloads are serialized with `serde_json` and wrapped in the protobuf
/// envelope defined in `msearchdb.proto`.
pub struct GrpcNetwork {
    /// The remote endpoint address, e.g. `"192.168.1.10:9300"`.
    addr: String,
}

impl GrpcNetwork {
    /// Connect (or reconnect) to the remote node.
    async fn connect(&self) -> Result<NodeClient, RPCError<u64, BasicNode, RaftError<u64>>> {
        let endpoint = if self.addr.starts_with("http://") || self.addr.starts_with("https://") {
            self.addr.clone()
        } else {
            format!("http://{}", self.addr)
        };

        NodeClient::connect_to_endpoint(&endpoint).await.map_err(|e| {
            RPCError::Unreachable(Unreachable::new(&io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("failed to connect to {}: {}", self.addr, e),
            )))
        })
    }
}

impl RaftNetwork<TypeConfig> for GrpcNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let client = self.connect().await?;

        let payload = serde_json::to_vec(&rpc).map_err(|e| {
            RPCError::Unreachable(Unreachable::new(&io::Error::new(
                io::ErrorKind::InvalidData,
                format!("serialize AppendEntriesRequest: {}", e),
            )))
        })?;

        let resp_bytes = client.send_append_entries(payload).await.map_err(|e| {
            RPCError::Unreachable(Unreachable::new(&io::Error::new(
                io::ErrorKind::ConnectionReset,
                format!("append_entries RPC: {}", e),
            )))
        })?;

        let resp: AppendEntriesResponse<u64> =
            serde_json::from_slice(&resp_bytes).map_err(|e| {
                RPCError::Unreachable(Unreachable::new(&io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("deserialize AppendEntriesResponse: {}", e),
                )))
            })?;

        Ok(resp)
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        let client = self.connect().await.map_err(|e| {
            RPCError::Unreachable(Unreachable::new(&io::Error::new(
                io::ErrorKind::ConnectionRefused,
                e.to_string(),
            )))
        })?;

        // Serialize the full openraft InstallSnapshotRequest (including
        // embedded data) into the metadata field of the first chunk.
        // The server-side handler deserializes the entire request from
        // the metadata bytes.
        let metadata = serde_json::to_vec(&rpc).map_err(|e| {
            RPCError::Unreachable(Unreachable::new(&io::Error::new(
                io::ErrorKind::InvalidData,
                format!("serialize InstallSnapshotRequest: {}", e),
            )))
        })?;

        // Send as a single chunk with the full metadata.
        let chunk = proto::SnapshotChunk {
            offset: 0,
            data: Vec::new(),
            done: true,
            metadata,
        };

        let stream = tokio_stream::iter(vec![chunk]);
        let mut grpc_client =
            proto::node_service_client::NodeServiceClient::new(client.channel().clone());

        let resp = grpc_client.install_snapshot(stream).await.map_err(|e| {
            RPCError::Unreachable(Unreachable::new(&io::Error::new(
                io::ErrorKind::ConnectionReset,
                format!("install_snapshot RPC: {}", e),
            )))
        })?;

        let resp_bytes = resp.into_inner().payload;
        let resp: InstallSnapshotResponse<u64> =
            serde_json::from_slice(&resp_bytes).map_err(|e| {
                RPCError::Unreachable(Unreachable::new(&io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("deserialize InstallSnapshotResponse: {}", e),
                )))
            })?;

        Ok(resp)
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let client = self.connect().await?;

        let payload = serde_json::to_vec(&rpc).map_err(|e| {
            RPCError::Unreachable(Unreachable::new(&io::Error::new(
                io::ErrorKind::InvalidData,
                format!("serialize VoteRequest: {}", e),
            )))
        })?;

        let resp_bytes = client.send_vote(payload).await.map_err(|e| {
            RPCError::Unreachable(Unreachable::new(&io::Error::new(
                io::ErrorKind::ConnectionReset,
                format!("vote RPC: {}", e),
            )))
        })?;

        let resp: VoteResponse<u64> = serde_json::from_slice(&resp_bytes).map_err(|e| {
            RPCError::Unreachable(Unreachable::new(&io::Error::new(
                io::ErrorKind::InvalidData,
                format!("deserialize VoteResponse: {}", e),
            )))
        })?;

        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_network_factory_creates_client() {
        // Verify the factory produces a GrpcNetwork with the correct address.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut factory = GrpcNetworkFactory;
            let node = BasicNode {
                addr: "127.0.0.1:9300".to_string(),
            };
            let network = factory.new_client(1, &node).await;
            assert_eq!(network.addr, "127.0.0.1:9300");
        });
    }
}
