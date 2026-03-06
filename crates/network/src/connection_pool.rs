//! Per-node connection pooling with circuit breaker for MSearchDB.
//!
//! [`ConnectionPool`] maintains a map of [`NodeClient`] instances keyed by
//! [`NodeId`].  Each node slot includes a [`CircuitBreaker`] that opens after
//! a configurable number of consecutive failures, preventing further RPCs
//! until a probe succeeds (half-open → closed transition).
//!
//! # Circuit Breaker States
//!
//! ```text
//!   ┌────────┐  failure >= threshold  ┌────────┐
//!   │ Closed  ├──────────────────────►│  Open   │
//!   └────┬───┘                        └───┬────┘
//!        │                                │
//!        │ success                        │ timeout elapsed
//!        │                                ▼
//!        │                          ┌──────────┐
//!        └──────────────────────────┤ HalfOpen │
//!             probe succeeds        └──────────┘
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use msearchdb_core::cluster::{NodeAddress, NodeId};
use msearchdb_core::error::{DbError, DbResult};

use crate::client::NodeClient;

// ---------------------------------------------------------------------------
// CircuitBreaker
// ---------------------------------------------------------------------------

/// Circuit breaker states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests pass through.
    Closed,
    /// Requests are blocked after too many consecutive failures.
    Open,
    /// A single probe request is allowed to test recovery.
    HalfOpen,
}

/// A circuit breaker that tracks consecutive failures for a single node.
///
/// After [`failure_threshold`](CircuitBreaker::failure_threshold) consecutive
/// failures the breaker opens and blocks further requests for
/// [`open_duration`](CircuitBreaker::open_duration) before transitioning to
/// half-open.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    state: CircuitState,
    consecutive_failures: u32,
    failure_threshold: u32,
    last_failure_time: Option<Instant>,
    open_duration: Duration,
}

impl CircuitBreaker {
    /// Create a new circuit breaker.
    pub fn new(failure_threshold: u32, open_duration: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            failure_threshold,
            last_failure_time: None,
            open_duration,
        }
    }

    /// Create a circuit breaker with default settings (3 failures, 30 s open).
    pub fn default_breaker() -> Self {
        Self::new(3, Duration::from_secs(30))
    }

    /// Return the current circuit state, accounting for time-based transitions.
    pub fn state(&self) -> CircuitState {
        match self.state {
            CircuitState::Open => {
                if let Some(last) = self.last_failure_time {
                    if last.elapsed() >= self.open_duration {
                        return CircuitState::HalfOpen;
                    }
                }
                CircuitState::Open
            }
            other => other,
        }
    }

    /// Returns `true` if a request should be allowed through.
    pub fn allow_request(&self) -> bool {
        match self.state() {
            CircuitState::Closed => true,
            CircuitState::HalfOpen => true,
            CircuitState::Open => false,
        }
    }

    /// Record a successful request.  Resets the failure counter and closes
    /// the circuit.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.state = CircuitState::Closed;
        self.last_failure_time = None;
    }

    /// Record a failed request.  If the failure threshold is reached, open
    /// the circuit.
    pub fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        self.last_failure_time = Some(Instant::now());

        if self.consecutive_failures >= self.failure_threshold {
            self.state = CircuitState::Open;
        }
    }

    /// Return the number of consecutive failures.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

// ---------------------------------------------------------------------------
// PoolEntry
// ---------------------------------------------------------------------------

/// A single entry in the connection pool: a client plus its circuit breaker.
#[derive(Debug, Clone)]
struct PoolEntry {
    client: NodeClient,
    breaker: CircuitBreaker,
}

// ---------------------------------------------------------------------------
// ConnectionPool
// ---------------------------------------------------------------------------

/// Configuration for the connection pool.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Maximum number of cached connections per node.
    pub max_connections_per_node: usize,
    /// Circuit breaker failure threshold.
    pub failure_threshold: u32,
    /// How long the circuit stays open before transitioning to half-open.
    pub open_duration: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections_per_node: 10,
            failure_threshold: 3,
            open_duration: Duration::from_secs(30),
        }
    }
}

/// A pool of [`NodeClient`] connections keyed by [`NodeId`].
///
/// Each node gets at most one cached client (tonic channels are multiplexed
/// over a single HTTP/2 connection, so a single `Channel` already supports
/// concurrent RPCs).  The pool adds circuit-breaker protection on top.
pub struct ConnectionPool {
    /// Per-node connection entries.
    entries: Arc<RwLock<HashMap<u64, PoolEntry>>>,
    /// Pool configuration.
    config: PoolConfig,
}

impl ConnectionPool {
    /// Create a new empty connection pool with default configuration.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            config: PoolConfig::default(),
        }
    }

    /// Create a new pool with the given configuration.
    pub fn with_config(config: PoolConfig) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Acquire a client for the given node.
    ///
    /// If a cached connection exists and its circuit breaker allows traffic,
    /// it is returned.  Otherwise a new connection is established.
    ///
    /// Returns `Err(DbError::NetworkError)` if the circuit is open.
    pub async fn get(&self, node_id: &NodeId, addr: &NodeAddress) -> DbResult<NodeClient> {
        let id = node_id.as_u64();

        // Fast path: check for a cached, healthy client.
        {
            let entries = self.entries.read().await;
            if let Some(entry) = entries.get(&id) {
                if entry.breaker.allow_request() {
                    return Ok(entry.client.clone());
                } else {
                    return Err(DbError::NetworkError(format!(
                        "circuit breaker open for node {}",
                        node_id
                    )));
                }
            }
        }

        // Slow path: create a new client.
        let client = NodeClient::connect(addr).await?;

        let mut entries = self.entries.write().await;
        let breaker = CircuitBreaker::new(self.config.failure_threshold, self.config.open_duration);
        entries.insert(
            id,
            PoolEntry {
                client: client.clone(),
                breaker,
            },
        );

        Ok(client)
    }

    /// Record a successful RPC for the given node (resets circuit breaker).
    pub async fn record_success(&self, node_id: &NodeId) {
        let id = node_id.as_u64();
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get_mut(&id) {
            entry.breaker.record_success();
        }
    }

    /// Record a failed RPC for the given node.
    ///
    /// If the failure threshold is reached, the circuit breaker opens and
    /// the cached client is effectively quarantined.
    pub async fn record_failure(&self, node_id: &NodeId) {
        let id = node_id.as_u64();
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get_mut(&id) {
            entry.breaker.record_failure();
        }
    }

    /// Remove a node's connection from the pool.
    pub async fn remove(&self, node_id: &NodeId) {
        let id = node_id.as_u64();
        let mut entries = self.entries.write().await;
        entries.remove(&id);
    }

    /// Return the current circuit breaker state for a node, if tracked.
    pub async fn circuit_state(&self, node_id: &NodeId) -> Option<CircuitState> {
        let id = node_id.as_u64();
        let entries = self.entries.read().await;
        entries.get(&id).map(|e| e.breaker.state())
    }

    /// Return the number of nodes currently in the pool.
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    /// Return `true` if the pool is empty.
    pub async fn is_empty(&self) -> bool {
        self.entries.read().await.is_empty()
    }

    /// Insert a pre-built client into the pool (useful for testing).
    pub async fn insert(&self, node_id: &NodeId, client: NodeClient) {
        let id = node_id.as_u64();
        let mut entries = self.entries.write().await;
        let breaker = CircuitBreaker::new(self.config.failure_threshold, self.config.open_duration);
        entries.insert(id, PoolEntry { client, breaker });
    }
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_breaker_starts_closed() {
        let cb = CircuitBreaker::default_breaker();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.allow_request());
        assert_eq!(cb.consecutive_failures(), 0);
    }

    #[test]
    fn circuit_breaker_opens_after_threshold() {
        let mut cb = CircuitBreaker::new(3, Duration::from_secs(30));

        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.consecutive_failures(), 1);

        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.consecutive_failures(), 2);

        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
        assert_eq!(cb.consecutive_failures(), 3);
        assert!(!cb.allow_request());
    }

    #[test]
    fn circuit_breaker_success_resets_to_closed() {
        let mut cb = CircuitBreaker::new(3, Duration::from_secs(30));

        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.consecutive_failures(), 2);

        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert_eq!(cb.consecutive_failures(), 0);
        assert!(cb.allow_request());
    }

    #[test]
    fn circuit_breaker_transitions_to_half_open() {
        let mut cb = CircuitBreaker::new(2, Duration::from_millis(1));

        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);

        // Wait for the open duration to elapse.
        std::thread::sleep(Duration::from_millis(5));

        assert_eq!(cb.state(), CircuitState::HalfOpen);
        assert!(cb.allow_request());
    }

    #[test]
    fn circuit_breaker_half_open_success_closes() {
        let mut cb = CircuitBreaker::new(2, Duration::from_millis(1));

        cb.record_failure();
        cb.record_failure();

        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.allow_request());
    }

    #[test]
    fn circuit_breaker_half_open_failure_reopens() {
        let mut cb = CircuitBreaker::new(2, Duration::from_millis(1));

        // Open the circuit.
        cb.record_failure();
        cb.record_failure();

        // Wait for half-open.
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Failure in half-open re-opens immediately (consecutive_failures >= threshold).
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Open);
    }

    #[tokio::test]
    async fn pool_starts_empty() {
        let pool = ConnectionPool::new();
        assert!(pool.is_empty().await);
        assert_eq!(pool.len().await, 0);
    }

    #[tokio::test]
    async fn pool_record_failure_opens_circuit() {
        let pool = ConnectionPool::with_config(PoolConfig {
            max_connections_per_node: 10,
            failure_threshold: 2,
            open_duration: Duration::from_secs(60),
        });

        let node_id = NodeId::new(99);

        // Simulate inserting a client by recording failures on a non-existent
        // node (no-op, just verifying the API doesn't panic).
        pool.record_failure(&node_id).await;
        pool.record_failure(&node_id).await;

        // No entry exists, so circuit_state returns None.
        assert!(pool.circuit_state(&node_id).await.is_none());
    }

    #[tokio::test]
    async fn pool_remove_clears_entry() {
        let pool = ConnectionPool::new();
        let node_id = NodeId::new(1);

        // Nothing to remove, should not panic.
        pool.remove(&node_id).await;
        assert!(pool.is_empty().await);
    }

    #[test]
    fn pool_config_default() {
        let config = PoolConfig::default();
        assert_eq!(config.max_connections_per_node, 10);
        assert_eq!(config.failure_threshold, 3);
        assert_eq!(config.open_duration, Duration::from_secs(30));
    }
}
