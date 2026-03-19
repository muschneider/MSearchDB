//! Read-your-writes consistency via sticky session tokens.
//!
//! After a successful write, the server returns an `X-Session-Token` header
//! encoding the committed Raft log index.  Subsequent reads that include
//! this token will block until the local node has applied entries up to
//! that index, guaranteeing the client sees its own writes.
//!
//! # Token Format
//!
//! The token is a simple `"msearchdb:<log_index>"` string (e.g.,
//! `"msearchdb:42"`).  It is opaque to clients and cheap to parse.
//!
//! # Usage
//!
//! ```text
//! // After write:
//! response.header("X-Session-Token", SessionToken::new(42).encode());
//!
//! // Before read:
//! if let Some(token) = request.header("X-Session-Token") {
//!     let st = SessionToken::decode(&token)?;
//!     session_manager.wait_for_index(st.committed_index).await?;
//! }
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use msearchdb_core::error::{DbError, DbResult};

/// Header name for the session consistency token.
pub const SESSION_TOKEN_HEADER: &str = "x-session-token";

/// Maximum time to wait for the local node to catch up to the requested index.
const MAX_WAIT: Duration = Duration::from_secs(5);

/// Poll interval when waiting for the local applied index to advance.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

// ---------------------------------------------------------------------------
// SessionToken
// ---------------------------------------------------------------------------

/// A lightweight token that encodes the committed Raft log index.
///
/// Returned to the client after a successful write so that subsequent
/// reads can request read-your-writes consistency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionToken {
    /// The Raft log index that was committed for the client's write.
    pub committed_index: u64,
}

impl SessionToken {
    /// Create a new session token with the given committed index.
    pub fn new(committed_index: u64) -> Self {
        Self { committed_index }
    }

    /// Encode the token as a string suitable for an HTTP header value.
    pub fn encode(&self) -> String {
        format!("msearchdb:{}", self.committed_index)
    }

    /// Decode a token from a header value string.
    ///
    /// Returns `None` if the string is not a valid session token.
    pub fn decode(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("msearchdb:")?;
        let index = rest.parse::<u64>().ok()?;
        Some(Self {
            committed_index: index,
        })
    }
}

// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------

/// Manages read-your-writes consistency by tracking the locally applied
/// Raft log index and letting readers wait until a target index is reached.
pub struct SessionManager {
    /// The highest Raft log index that has been applied locally.
    applied_index: AtomicU64,
}

impl SessionManager {
    /// Create a new session manager starting at log index 0.
    pub fn new() -> Self {
        Self {
            applied_index: AtomicU64::new(0),
        }
    }

    /// Update the locally applied index after a Raft entry is committed.
    ///
    /// Called by the state machine or write handler after a successful
    /// Raft proposal.
    pub fn advance_applied_index(&self, index: u64) {
        self.applied_index.fetch_max(index, Ordering::Release);
    }

    /// Return the current locally applied index.
    pub fn current_applied_index(&self) -> u64 {
        self.applied_index.load(Ordering::Acquire)
    }

    /// Wait until the local applied index reaches at least `target_index`.
    ///
    /// Returns `Ok(())` once the target is met, or `Err(DbError::ConsistencyError)`
    /// if the wait times out.
    pub async fn wait_for_index(&self, target_index: u64) -> DbResult<()> {
        if self.applied_index.load(Ordering::Acquire) >= target_index {
            return Ok(());
        }

        let deadline = tokio::time::Instant::now() + MAX_WAIT;
        loop {
            if self.applied_index.load(Ordering::Acquire) >= target_index {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(DbError::ConsistencyError(format!(
                    "timed out waiting for applied index {} (current: {})",
                    target_index,
                    self.applied_index.load(Ordering::Acquire)
                )));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Generate a session token for a write that committed at the given index.
    pub fn token_for_write(&self, committed_index: u64) -> SessionToken {
        self.advance_applied_index(committed_index);
        SessionToken::new(committed_index)
    }
}

impl Default for SessionManager {
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
    fn session_token_encode_decode_roundtrip() {
        let token = SessionToken::new(42);
        let encoded = token.encode();
        assert_eq!(encoded, "msearchdb:42");

        let decoded = SessionToken::decode(&encoded);
        assert_eq!(decoded, Some(token));
    }

    #[test]
    fn session_token_decode_invalid() {
        assert!(SessionToken::decode("").is_none());
        assert!(SessionToken::decode("invalid").is_none());
        assert!(SessionToken::decode("msearchdb:").is_none());
        assert!(SessionToken::decode("msearchdb:abc").is_none());
        assert!(SessionToken::decode("other:42").is_none());
    }

    #[test]
    fn session_manager_advance_index() {
        let mgr = SessionManager::new();
        assert_eq!(mgr.current_applied_index(), 0);

        mgr.advance_applied_index(5);
        assert_eq!(mgr.current_applied_index(), 5);

        // Monotonic: lower values are ignored.
        mgr.advance_applied_index(3);
        assert_eq!(mgr.current_applied_index(), 5);

        mgr.advance_applied_index(10);
        assert_eq!(mgr.current_applied_index(), 10);
    }

    #[tokio::test]
    async fn session_manager_wait_already_applied() {
        let mgr = SessionManager::new();
        mgr.advance_applied_index(10);

        // Should return immediately.
        let result = mgr.wait_for_index(5).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn session_manager_wait_concurrent_advance() {
        let mgr = std::sync::Arc::new(SessionManager::new());
        let mgr2 = mgr.clone();

        // Advance in a separate task after a short delay.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            mgr2.advance_applied_index(10);
        });

        let result = mgr.wait_for_index(10).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn session_manager_wait_timeout() {
        let mgr = SessionManager::new();
        // Index 999 will never be reached — should time out.
        // Use a shorter timeout for the test (we can't easily override
        // MAX_WAIT, but 5s is acceptable for CI).  We verify the error
        // variant instead of actually waiting 5s by advancing partially.
        mgr.advance_applied_index(1);

        // This would normally wait 5s. We'll spawn a task that won't advance
        // far enough, and use tokio::time::pause to speed things up.
        tokio::time::pause();
        let result = mgr.wait_for_index(999).await;
        assert!(matches!(result, Err(DbError::ConsistencyError(_))));
    }

    #[test]
    fn session_manager_token_for_write() {
        let mgr = SessionManager::new();
        let token = mgr.token_for_write(42);
        assert_eq!(token.committed_index, 42);
        assert_eq!(mgr.current_applied_index(), 42);
    }
}
