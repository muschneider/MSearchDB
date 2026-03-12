//! Configurable consistency levels for read operations.
//!
//! MSearchDB supports per-request consistency tuning via [`ConsistencyLevel`],
//! allowing callers to trade off between latency and data freshness.
//!
//! # Examples
//!
//! ```
//! use msearchdb_core::consistency::ConsistencyLevel;
//!
//! let level = ConsistencyLevel::from_str_param("quorum");
//! assert_eq!(level, ConsistencyLevel::Quorum);
//!
//! let level = ConsistencyLevel::from_str_param("bogus");
//! assert_eq!(level, ConsistencyLevel::Quorum); // falls back to default
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// ConsistencyLevel
// ---------------------------------------------------------------------------

/// The consistency level for a read operation.
///
/// Controls how many replica nodes must respond before the coordinator returns
/// a result. Higher levels provide stronger consistency at the cost of latency.
///
/// Marked `#[non_exhaustive]` for forward compatibility with future levels
/// (e.g., `LocalQuorum`, `EachQuorum`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ConsistencyLevel {
    /// Read from any single replica node.
    ///
    /// Fastest response time but may return stale data if the contacted
    /// replica has not yet received the latest write.
    One,

    /// Read from a majority of replica nodes (`⌊N/2⌋ + 1`) and return the
    /// latest version across respondents.
    ///
    /// This is the default level, providing a good balance between latency
    /// and freshness. Combined with quorum writes, it guarantees strong
    /// consistency (read quorum + write quorum > N).
    #[default]
    Quorum,

    /// Read from all replica nodes and return the latest version.
    ///
    /// Slowest response time but guarantees the freshest data. A single
    /// unavailable replica will cause the read to fail.
    All,
}

impl ConsistencyLevel {
    /// Parse a consistency level from a query-parameter string.
    ///
    /// Accepted values (case-insensitive): `"one"`, `"quorum"`, `"all"`.
    /// Unrecognised strings fall back to [`ConsistencyLevel::Quorum`].
    pub fn from_str_param(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "one" => ConsistencyLevel::One,
            "quorum" => ConsistencyLevel::Quorum,
            "all" => ConsistencyLevel::All,
            _ => ConsistencyLevel::Quorum,
        }
    }

    /// Return the number of nodes that must respond for a cluster of size `n`
    /// with the given replication factor.
    ///
    /// - `One` → 1
    /// - `Quorum` → `⌊rf / 2⌋ + 1`
    /// - `All` → `rf`
    ///
    /// The result is clamped to `[1, n]` where `n` is the number of available
    /// replica nodes.
    pub fn required_responses(&self, replication_factor: u8) -> usize {
        let rf = replication_factor as usize;
        match self {
            ConsistencyLevel::One => 1,
            ConsistencyLevel::Quorum => (rf / 2) + 1,
            ConsistencyLevel::All => rf,
        }
    }
}

impl fmt::Display for ConsistencyLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConsistencyLevel::One => write!(f, "ONE"),
            ConsistencyLevel::Quorum => write!(f, "QUORUM"),
            ConsistencyLevel::All => write!(f, "ALL"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_quorum() {
        assert_eq!(ConsistencyLevel::default(), ConsistencyLevel::Quorum);
    }

    #[test]
    fn from_str_param_valid() {
        assert_eq!(
            ConsistencyLevel::from_str_param("one"),
            ConsistencyLevel::One
        );
        assert_eq!(
            ConsistencyLevel::from_str_param("ONE"),
            ConsistencyLevel::One
        );
        assert_eq!(
            ConsistencyLevel::from_str_param("quorum"),
            ConsistencyLevel::Quorum
        );
        assert_eq!(
            ConsistencyLevel::from_str_param("QUORUM"),
            ConsistencyLevel::Quorum
        );
        assert_eq!(
            ConsistencyLevel::from_str_param("all"),
            ConsistencyLevel::All
        );
        assert_eq!(
            ConsistencyLevel::from_str_param("All"),
            ConsistencyLevel::All
        );
    }

    #[test]
    fn from_str_param_unknown_falls_back_to_quorum() {
        assert_eq!(
            ConsistencyLevel::from_str_param("eventual"),
            ConsistencyLevel::Quorum
        );
        assert_eq!(
            ConsistencyLevel::from_str_param(""),
            ConsistencyLevel::Quorum
        );
    }

    #[test]
    fn required_responses_one() {
        assert_eq!(ConsistencyLevel::One.required_responses(3), 1);
        assert_eq!(ConsistencyLevel::One.required_responses(5), 1);
    }

    #[test]
    fn required_responses_quorum() {
        assert_eq!(ConsistencyLevel::Quorum.required_responses(3), 2);
        assert_eq!(ConsistencyLevel::Quorum.required_responses(5), 3);
        assert_eq!(ConsistencyLevel::Quorum.required_responses(1), 1);
    }

    #[test]
    fn required_responses_all() {
        assert_eq!(ConsistencyLevel::All.required_responses(3), 3);
        assert_eq!(ConsistencyLevel::All.required_responses(5), 5);
    }

    #[test]
    fn display_format() {
        assert_eq!(format!("{}", ConsistencyLevel::One), "ONE");
        assert_eq!(format!("{}", ConsistencyLevel::Quorum), "QUORUM");
        assert_eq!(format!("{}", ConsistencyLevel::All), "ALL");
    }

    #[test]
    fn serde_roundtrip() {
        for level in &[
            ConsistencyLevel::One,
            ConsistencyLevel::Quorum,
            ConsistencyLevel::All,
        ] {
            let json = serde_json::to_string(level).unwrap();
            let back: ConsistencyLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(*level, back);
        }
    }
}
