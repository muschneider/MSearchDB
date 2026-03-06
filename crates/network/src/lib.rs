//! # msearchdb-network
//!
//! gRPC networking layer for MSearchDB inter-node communication.
//!
//! This crate provides the transport between cluster nodes for Raft consensus
//! messages, data replication, health checks, and query forwarding.
//!
//! ## Architecture
//!
//! ```text
//!  ┌────────────────┐          gRPC           ┌────────────────┐
//!  │   NodeClient    ├───────────────────────►│ NodeServiceImpl │
//!  │   (client.rs)   │                        │  (server.rs)    │
//!  └───────┬────────┘                         └────────────────┘
//!          │
//!  ┌───────▼──────────┐
//!  │  ConnectionPool   │   per-node pooled channels
//!  │  (conn_pool.rs)   │   with circuit breaker
//!  └───────┬──────────┘
//!          │
//!  ┌───────▼──────────┐
//!  │  ScatterGather    │   fan-out search to N nodes
//!  │  (scatter.rs)     │   merge + dedup + re-rank
//!  └──────────────────┘
//! ```
//!
//! ## Modules
//!
//! - [`proto`]: Generated protobuf/gRPC stubs from `proto/msearchdb.proto`.
//! - [`server`]: tonic service implementations ([`NodeServiceImpl`], [`QueryServiceImpl`]).
//! - [`client`]: gRPC client with exponential-backoff retry ([`NodeClient`]).
//! - [`connection_pool`]: Per-node connection pooling with circuit breaker.
//! - [`scatter_gather`]: Distributed scatter-gather search coordinator.

pub mod client;
pub mod connection_pool;
pub mod scatter_gather;
pub mod server;

/// Generated protobuf and gRPC stubs for MSearchDB inter-node communication.
pub mod proto {
    tonic::include_proto!("msearchdb");
}

pub use msearchdb_core;
