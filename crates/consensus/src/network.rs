//! Raft network layer (stub) for MSearchDB.
//!
//! Implements [`openraft::RaftNetworkFactory`] and [`openraft::RaftNetwork`]
//! with placeholder stubs that return `Unreachable` errors.  The real gRPC
//! transport will be filled in **Prompt 6** (the `msearchdb-network` crate).
//!
//! For integration tests in this crate, we provide a separate **channel-based**
//! in-process network (see [`ChannelNetworkFactory`]) that routes messages
//! through `tokio::sync::mpsc` channels without any real I/O.

use std::collections::HashMap;
use std::sync::Arc;

use openraft::error::{InstallSnapshotError, RPCError, RaftError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::BasicNode;
use tokio::sync::RwLock;

use crate::types::TypeConfig;

// ---------------------------------------------------------------------------
// StubNetworkFactory — placeholder for gRPC transport
// ---------------------------------------------------------------------------

/// A placeholder network factory that always returns [`StubNetwork`] clients.
///
/// Every RPC call on [`StubNetwork`] returns `Err(Unreachable)`.
/// This is a deliberate stub — the real transport is implemented in
/// `msearchdb-network` and will be wired in at node startup.
pub struct StubNetworkFactory;

impl RaftNetworkFactory<TypeConfig> for StubNetworkFactory {
    type Network = StubNetwork;

    async fn new_client(&mut self, _target: u64, _node: &BasicNode) -> Self::Network {
        StubNetwork
    }
}

/// A stub network client where every RPC returns `Unreachable`.
pub struct StubNetwork;

impl RaftNetwork<TypeConfig> for StubNetwork {
    async fn append_entries(
        &mut self,
        _rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        Err(RPCError::Unreachable(Unreachable::new(
            &std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "stub network: gRPC not yet implemented",
            ),
        )))
    }

    async fn install_snapshot(
        &mut self,
        _rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        Err(RPCError::Unreachable(Unreachable::new(
            &std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "stub network: gRPC not yet implemented",
            ),
        )))
    }

    async fn vote(
        &mut self,
        _rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        Err(RPCError::Unreachable(Unreachable::new(
            &std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "stub network: gRPC not yet implemented",
            ),
        )))
    }
}

// ---------------------------------------------------------------------------
// ChannelNetworkFactory — in-process transport for integration tests
// ---------------------------------------------------------------------------

/// Type alias for a Raft instance in this crate.
pub type RaftHandle = openraft::Raft<TypeConfig>;

/// A network factory backed by in-process Raft handles.
///
/// Maps each target `NodeId` (u64) to the corresponding [`RaftHandle`].
/// When a client is requested, a [`ChannelNetwork`] wrapping the target's
/// handle is returned.  This allows a cluster of Raft nodes to communicate
/// entirely within a single process — ideal for integration tests.
#[derive(Clone)]
pub struct ChannelNetworkFactory {
    /// Map of node id → raft handle.
    pub routers: Arc<RwLock<HashMap<u64, RaftHandle>>>,
}

impl ChannelNetworkFactory {
    /// Create a new channel network factory.
    pub fn new(routers: Arc<RwLock<HashMap<u64, RaftHandle>>>) -> Self {
        Self { routers }
    }
}

impl RaftNetworkFactory<TypeConfig> for ChannelNetworkFactory {
    type Network = ChannelNetwork;

    async fn new_client(&mut self, target: u64, _node: &BasicNode) -> Self::Network {
        ChannelNetwork {
            target,
            routers: Arc::clone(&self.routers),
        }
    }
}

/// An in-process network client that dispatches RPCs to the target node's
/// [`RaftHandle`] directly.
pub struct ChannelNetwork {
    target: u64,
    routers: Arc<RwLock<HashMap<u64, RaftHandle>>>,
}

impl ChannelNetwork {
    /// Get a clone of the target Raft handle, or return `Unreachable`.
    async fn get_target(&self) -> Result<RaftHandle, RPCError<u64, BasicNode, RaftError<u64>>> {
        let routers = self.routers.read().await;
        routers.get(&self.target).cloned().ok_or_else(|| {
            RPCError::Unreachable(Unreachable::new(&std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                format!("node {} not found in router map", self.target),
            )))
        })
    }
}

impl RaftNetwork<TypeConfig> for ChannelNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let raft = self.get_target().await?;
        raft.append_entries(rpc)
            .await
            .map_err(|e| RPCError::Network(openraft::error::NetworkError::new(&e)))
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        let raft = self.get_target().await.map_err(|e| {
            RPCError::Network(openraft::error::NetworkError::new(&std::io::Error::other(
                e.to_string(),
            )))
        })?;
        raft.install_snapshot(rpc)
            .await
            .map_err(|e| RPCError::Network(openraft::error::NetworkError::new(&e)))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        let raft = self.get_target().await?;
        raft.vote(rpc)
            .await
            .map_err(|e| RPCError::Network(openraft::error::NetworkError::new(&e)))
    }
}
