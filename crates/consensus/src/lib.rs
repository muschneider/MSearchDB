//! # msearchdb-consensus
//!
//! Raft consensus protocol implementation for MSearchDB.
//!
//! This crate implements the Raft consensus protocol on top of
//! [`openraft`], providing leader election, log replication, and
//! state machine management for MSearchDB.
//!
//! ## Architecture
//!
//! ```text
//!  ┌────────────┐     propose()     ┌───────────────┐
//!  │  RaftNode   ├─────────────────►│  openraft::Raft │
//!  └──────┬─────┘                   └───────┬───────┘
//!         │                                 │
//!         │                     ┌───────────┴───────────┐
//!         │                     │                       │
//!         │              ┌──────▼──────┐        ┌───────▼──────┐
//!         │              │ MemLogStore  │        │DbStateMachine│
//!         │              └─────────────┘        └──────────────┘
//!         │
//!         │  network layer
//!         ▼
//!  ┌──────────────────┐
//!  │ NetworkFactory    │  (StubNetworkFactory or ChannelNetworkFactory)
//!  └──────────────────┘
//! ```
//!
//! ## Modules
//!
//! - [`types`]: Core type configuration (`TypeConfig`, `RaftCommand`, `RaftResponse`).
//! - [`state_machine`]: The Raft state machine applying commands to storage/index.
//! - [`log_store`]: In-memory Raft log storage.
//! - [`network`]: Network transport layer (stub + in-process channel).
//! - [`raft_node`]: High-level `RaftNode` wrapper.
//! - [`quorum`]: Quorum math and cluster health checking.

pub mod log_store;
pub mod network;
pub mod quorum;
pub mod raft_node;
pub mod rocksdb_log_store;
pub mod state_machine;
pub mod types;

pub use msearchdb_core;
