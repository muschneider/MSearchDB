//! Node configuration for MSearchDB.
//!
//! Provides [`NodeConfig`] which can be constructed programmatically, loaded
//! from a TOML file, or fall back to sensible defaults.
//!
//! # Examples
//!
//! ```
//! use msearchdb_core::config::NodeConfig;
//!
//! let config = NodeConfig::default();
//! assert_eq!(config.http_port, 9200);
//! assert_eq!(config.grpc_port, 9300);
//! ```

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::cluster::{NodeAddress, NodeId};
use crate::error::{DbError, DbResult};

/// Configuration for a single MSearchDB node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Unique identifier for this node in the cluster.
    pub node_id: NodeId,

    /// Directory on disk for data files, WAL segments, and index shards.
    pub data_dir: PathBuf,

    /// Port for the HTTP REST API.
    pub http_port: u16,

    /// Port for gRPC inter-node communication.
    pub grpc_port: u16,

    /// Addresses of peer nodes in the cluster.
    pub peers: Vec<NodeAddress>,

    /// Number of replicas for each shard (including the primary).
    pub replication_factor: u8,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            node_id: NodeId::new(1),
            data_dir: PathBuf::from("data"),
            http_port: 9200,
            grpc_port: 9300,
            peers: Vec::new(),
            replication_factor: 1,
        }
    }
}

impl NodeConfig {
    /// Load configuration from a TOML file.
    ///
    /// Returns [`DbError::StorageError`] if the file cannot be read, or
    /// [`DbError::SerializationError`] if the TOML is malformed.
    pub fn from_toml_file(path: impl AsRef<std::path::Path>) -> DbResult<Self> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            DbError::StorageError(format!(
                "failed to read config file '{}': {}",
                path.as_ref().display(),
                e
            ))
        })?;
        Self::from_toml_str(&content)
    }

    /// Parse configuration from a TOML string.
    pub fn from_toml_str(s: &str) -> DbResult<Self> {
        toml::from_str(s)
            .map_err(|e| DbError::SerializationError(format!("failed to parse TOML config: {}", e)))
    }

    /// Serialize this configuration to a TOML string.
    pub fn to_toml_string(&self) -> DbResult<String> {
        toml::to_string_pretty(self).map_err(|e| {
            DbError::SerializationError(format!("failed to serialize config to TOML: {}", e))
        })
    }

    /// Validate that the configuration is internally consistent.
    pub fn validate(&self) -> DbResult<()> {
        if self.replication_factor == 0 {
            return Err(DbError::InvalidInput(
                "replication_factor must be at least 1".into(),
            ));
        }
        if self.http_port == self.grpc_port {
            return Err(DbError::InvalidInput(
                "http_port and grpc_port must be different".into(),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let cfg = NodeConfig::default();
        assert_eq!(cfg.node_id, NodeId::new(1));
        assert_eq!(cfg.data_dir, PathBuf::from("data"));
        assert_eq!(cfg.http_port, 9200);
        assert_eq!(cfg.grpc_port, 9300);
        assert!(cfg.peers.is_empty());
        assert_eq!(cfg.replication_factor, 1);
    }

    #[test]
    fn default_config_is_valid() {
        let cfg = NodeConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_replication() {
        let cfg = NodeConfig {
            replication_factor: 0,
            ..NodeConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, DbError::InvalidInput(_)));
        assert!(err.to_string().contains("replication_factor"));
    }

    #[test]
    fn validate_rejects_same_ports() {
        let cfg = NodeConfig {
            http_port: 9200,
            grpc_port: 9200,
            ..NodeConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, DbError::InvalidInput(_)));
        assert!(err.to_string().contains("different"));
    }

    #[test]
    fn config_toml_roundtrip() {
        let cfg = NodeConfig {
            node_id: NodeId::new(5),
            data_dir: PathBuf::from("/var/lib/msearchdb"),
            http_port: 8080,
            grpc_port: 8081,
            peers: vec![
                NodeAddress::new("10.0.0.1", 9300),
                NodeAddress::new("10.0.0.2", 9300),
            ],
            replication_factor: 3,
        };

        let toml_str = cfg.to_toml_string().unwrap();
        let back = NodeConfig::from_toml_str(&toml_str).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn config_from_toml_str() {
        // Build a config, serialize it to TOML, then parse it back.
        // NodeId serializes as a tuple struct so we use the roundtrip format
        // rather than hand-writing TOML.
        let cfg = NodeConfig {
            node_id: NodeId::new(10),
            data_dir: PathBuf::from("/tmp/test"),
            http_port: 7777,
            grpc_port: 7778,
            peers: vec![NodeAddress::new("peer1", 9300)],
            replication_factor: 2,
        };

        let generated = cfg.to_toml_string().unwrap();
        let parsed = NodeConfig::from_toml_str(&generated).unwrap();
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn config_from_missing_file_is_error() {
        let result = NodeConfig::from_toml_file("/nonexistent/path.toml");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DbError::StorageError(_)));
    }

    #[test]
    fn config_from_invalid_toml_is_error() {
        let result = NodeConfig::from_toml_str("this is not valid TOML {{{{");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DbError::SerializationError(_)
        ));
    }

    #[test]
    fn config_json_roundtrip() {
        let cfg = NodeConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: NodeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }
}
