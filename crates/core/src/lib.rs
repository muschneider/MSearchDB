//! # msearchdb-core
//!
//! Core domain types and trait abstractions for MSearchDB, a distributed
//! NoSQL database with full-text search capabilities.
//!
//! This crate is the foundation of the MSearchDB workspace. It defines:
//!
//! - **Document model** ([`document`]): The primary data unit with typed field values.
//! - **Error types** ([`error`]): Ergonomic error handling via `thiserror`.
//! - **Query DSL** ([`query`]): Full-text, term, range, and boolean queries.
//! - **Cluster types** ([`cluster`]): Node identity, addressing, and cluster state.
//! - **Configuration** ([`config`]): Node configuration with TOML loading support.
//! - **Trait abstractions** ([`traits`]): Async interfaces for storage, indexing,
//!   replication, and health checking.
//! - **Consistent hashing** ([`consistent_hash`]): SHA-256 ring with virtual nodes.
//! - **Cluster routing** ([`cluster_router`]): Document placement and query fan-out.
//! - **Rebalancing** ([`rebalancer`]): Data migration plans for topology changes.
//!
//! # Design Principles
//!
//! - **Newtype pattern** for type safety (`DocumentId`, `NodeId`)
//! - **`#[non_exhaustive]`** on public enums for forward compatibility
//! - **Trait-based abstraction** over concrete implementations
//! - **Zero-cost serde** via derive macros

pub mod cluster;
pub mod cluster_router;
pub mod collection;
pub mod config;
pub mod consistency;
pub mod consistent_hash;
pub mod document;
pub mod error;
pub mod query;
pub mod read_coordinator;
pub mod rebalancer;
pub mod security;
pub mod snapshot;
pub mod traits;
pub mod vector_clock;

// Re-export the most commonly used types at crate root for ergonomics.
pub use cluster::{ClusterState, NodeId, NodeInfo, NodeStatus};
pub use cluster_router::ClusterRouter;
pub use collection::{
    Collection, CollectionAlias, CollectionSettings, FieldMapping, MappedFieldType,
};
pub use config::NodeConfig;
pub use consistency::ConsistencyLevel;
pub use consistent_hash::ConsistentHashRing;
pub use document::{Document, DocumentId, FieldValue};
pub use error::{DbError, DbResult};
pub use query::{
    Aggregation, AggregationBucket, AggregationResult, CollapseOptions, Query, SearchOptions,
    SearchResult, SortClause, SortOrder, SortValue,
};
pub use read_coordinator::{ReadCoordinator, ReadResolution, ReplicaResponse};
pub use rebalancer::{DataMove, RebalancePlan, RebalanceStatus};
pub use security::{
    ApiKeyEntry, ApiKeyRegistry, AuditEntry, AuditLogger, ConnectionCounter, JwtClaims, JwtManager,
    RateLimiter, Role, SecurityConfig, TokenBucket, ValidationConfig,
};
pub use snapshot::{SnapshotConfig, SnapshotInfo};
pub use vector_clock::VectorClock;
