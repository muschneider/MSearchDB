//! Node configuration for MSearchDB.
//!
//! Provides [`NodeConfig`] which can be constructed programmatically, loaded
//! from a TOML file, or fall back to sensible defaults.
//!
//! The configuration supports a nested TOML schema with sections for `node`,
//! `network`, `storage`, `index`, `cluster`, `auth`, and `observability`.
//! CLI flags can override individual fields after loading.
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

// ---------------------------------------------------------------------------
// Nested TOML sections
// ---------------------------------------------------------------------------

/// The `[node]` section of the configuration file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeSection {
    /// Unique node identifier.
    pub id: u64,
    /// Directory for data files, WAL segments, and index shards.
    pub data_dir: String,
}

impl Default for NodeSection {
    fn default() -> Self {
        Self {
            id: 1,
            data_dir: "data".into(),
        }
    }
}

/// The `[network]` section of the configuration file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetworkSection {
    /// HTTP server bind host.
    pub http_host: String,
    /// HTTP server port.
    pub http_port: u16,
    /// gRPC server bind host.
    pub grpc_host: String,
    /// gRPC server port.
    pub grpc_port: u16,
    /// Peer gRPC addresses.
    #[serde(default)]
    pub peers: Vec<String>,
}

impl Default for NetworkSection {
    fn default() -> Self {
        Self {
            http_host: "0.0.0.0".into(),
            http_port: 9200,
            grpc_host: "0.0.0.0".into(),
            grpc_port: 9300,
            peers: Vec::new(),
        }
    }
}

/// The `[storage]` section of the configuration file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StorageSection {
    /// Write buffer size in megabytes.
    pub write_buffer_mb: usize,
    /// Maximum number of open file descriptors.
    pub max_open_files: i32,
    /// Enable Snappy compression.
    pub compression: bool,
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            write_buffer_mb: 64,
            max_open_files: 1000,
            compression: true,
        }
    }
}

/// The `[index]` section of the configuration file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexSection {
    /// Index writer heap size in megabytes.
    pub heap_size_mb: usize,
    /// Merge policy: "log" or "no_merge".
    pub merge_policy: String,
}

impl Default for IndexSection {
    fn default() -> Self {
        Self {
            heap_size_mb: 128,
            merge_policy: "log".into(),
        }
    }
}

/// The `[cluster]` section of the configuration file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClusterSection {
    /// Number of replicas for each shard (including the primary).
    pub replication_factor: u8,
    /// Raft election timeout in milliseconds.
    pub election_timeout_ms: u64,
    /// Raft heartbeat interval in milliseconds.
    pub heartbeat_interval_ms: u64,
}

impl Default for ClusterSection {
    fn default() -> Self {
        Self {
            replication_factor: 1,
            election_timeout_ms: 150,
            heartbeat_interval_ms: 50,
        }
    }
}

/// The `[auth]` section of the configuration file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthSection {
    /// API key for authentication. Empty string means no auth.
    #[serde(default)]
    pub api_key: String,
}

/// The `[observability]` section of the configuration file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservabilitySection {
    /// Log level filter (error, warn, info, debug, trace).
    pub log_level: String,
    /// Prometheus metrics port (0 to disable).
    pub metrics_port: u16,
}

impl Default for ObservabilitySection {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            metrics_port: 9100,
        }
    }
}

// ---------------------------------------------------------------------------
// Full TOML config file structure
// ---------------------------------------------------------------------------

/// The complete TOML configuration file with nested sections.
///
/// This maps 1:1 to the on-disk `node.toml` format. After loading, it is
/// converted into the flattened [`NodeConfig`] for use by the rest of the
/// application.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    /// `[node]` section.
    #[serde(default)]
    pub node: NodeSection,
    /// `[network]` section.
    #[serde(default)]
    pub network: NetworkSection,
    /// `[storage]` section.
    #[serde(default)]
    pub storage: StorageSection,
    /// `[index]` section.
    #[serde(default)]
    pub index: IndexSection,
    /// `[cluster]` section.
    #[serde(default)]
    pub cluster: ClusterSection,
    /// `[auth]` section.
    #[serde(default)]
    pub auth: AuthSection,
    /// `[observability]` section.
    #[serde(default)]
    pub observability: ObservabilitySection,
}

impl ConfigFile {
    /// Load from a TOML file on disk.
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

    /// Parse from a TOML string.
    pub fn from_toml_str(s: &str) -> DbResult<Self> {
        toml::from_str(s)
            .map_err(|e| DbError::SerializationError(format!("failed to parse TOML config: {}", e)))
    }

    /// Serialize to a TOML string.
    pub fn to_toml_string(&self) -> DbResult<String> {
        toml::to_string_pretty(self).map_err(|e| {
            DbError::SerializationError(format!("failed to serialize config to TOML: {}", e))
        })
    }

    /// Convert into the flattened [`NodeConfig`].
    pub fn into_node_config(self) -> NodeConfig {
        let peers = self
            .network
            .peers
            .iter()
            .filter_map(|p| {
                let parts: Vec<&str> = p.rsplitn(2, ':').collect();
                if parts.len() == 2 {
                    parts[0]
                        .parse::<u16>()
                        .ok()
                        .map(|port| NodeAddress::new(parts[1], port))
                } else {
                    None
                }
            })
            .collect();

        NodeConfig {
            node_id: NodeId::new(self.node.id),
            data_dir: PathBuf::from(self.node.data_dir),
            http_host: self.network.http_host,
            http_port: self.network.http_port,
            grpc_host: self.network.grpc_host,
            grpc_port: self.network.grpc_port,
            peers,
            replication_factor: self.cluster.replication_factor,
            write_buffer_mb: self.storage.write_buffer_mb,
            max_open_files: self.storage.max_open_files,
            compression: self.storage.compression,
            heap_size_mb: self.index.heap_size_mb,
            merge_policy: self.index.merge_policy,
            election_timeout_ms: self.cluster.election_timeout_ms,
            heartbeat_interval_ms: self.cluster.heartbeat_interval_ms,
            api_key: if self.auth.api_key.is_empty() {
                None
            } else {
                Some(self.auth.api_key)
            },
            log_level: self.observability.log_level,
            metrics_port: self.observability.metrics_port,
        }
    }
}

// ---------------------------------------------------------------------------
// NodeConfig (flattened runtime config)
// ---------------------------------------------------------------------------

/// Configuration for a single MSearchDB node.
///
/// This is the runtime representation used by all crates. It can be built
/// from a [`ConfigFile`] (TOML), from defaults, or from CLI arg overrides.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Unique identifier for this node in the cluster.
    pub node_id: NodeId,

    /// Directory on disk for data files, WAL segments, and index shards.
    pub data_dir: PathBuf,

    /// Host for the HTTP REST API to bind to.
    #[serde(default = "default_http_host")]
    pub http_host: String,

    /// Port for the HTTP REST API.
    pub http_port: u16,

    /// Host for the gRPC server to bind to.
    #[serde(default = "default_grpc_host")]
    pub grpc_host: String,

    /// Port for gRPC inter-node communication.
    pub grpc_port: u16,

    /// Addresses of peer nodes in the cluster.
    pub peers: Vec<NodeAddress>,

    /// Number of replicas for each shard (including the primary).
    pub replication_factor: u8,

    /// Write buffer size in megabytes.
    #[serde(default = "default_write_buffer_mb")]
    pub write_buffer_mb: usize,

    /// Maximum number of open file descriptors for storage.
    #[serde(default = "default_max_open_files")]
    pub max_open_files: i32,

    /// Enable Snappy compression.
    #[serde(default = "default_compression")]
    pub compression: bool,

    /// Index writer heap size in megabytes.
    #[serde(default = "default_heap_size_mb")]
    pub heap_size_mb: usize,

    /// Merge policy: "log" or "no_merge".
    #[serde(default = "default_merge_policy")]
    pub merge_policy: String,

    /// Raft election timeout in milliseconds.
    #[serde(default = "default_election_timeout_ms")]
    pub election_timeout_ms: u64,

    /// Raft heartbeat interval in milliseconds.
    #[serde(default = "default_heartbeat_interval_ms")]
    pub heartbeat_interval_ms: u64,

    /// Optional API key for authentication.
    /// If `None`, authentication is disabled.
    #[serde(default)]
    pub api_key: Option<String>,

    /// Log level filter (error, warn, info, debug, trace).
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Prometheus metrics port (0 to disable).
    #[serde(default = "default_metrics_port")]
    pub metrics_port: u16,
}

fn default_http_host() -> String {
    "0.0.0.0".into()
}
fn default_grpc_host() -> String {
    "0.0.0.0".into()
}
fn default_write_buffer_mb() -> usize {
    64
}
fn default_max_open_files() -> i32 {
    1000
}
fn default_compression() -> bool {
    true
}
fn default_heap_size_mb() -> usize {
    128
}
fn default_merge_policy() -> String {
    "log".into()
}
fn default_election_timeout_ms() -> u64 {
    150
}
fn default_heartbeat_interval_ms() -> u64 {
    50
}
fn default_log_level() -> String {
    "info".into()
}
fn default_metrics_port() -> u16 {
    9100
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            node_id: NodeId::new(1),
            data_dir: PathBuf::from("data"),
            http_host: default_http_host(),
            http_port: 9200,
            grpc_host: default_grpc_host(),
            grpc_port: 9300,
            peers: Vec::new(),
            replication_factor: 1,
            write_buffer_mb: default_write_buffer_mb(),
            max_open_files: default_max_open_files(),
            compression: default_compression(),
            heap_size_mb: default_heap_size_mb(),
            merge_policy: default_merge_policy(),
            election_timeout_ms: default_election_timeout_ms(),
            heartbeat_interval_ms: default_heartbeat_interval_ms(),
            api_key: None,
            log_level: default_log_level(),
            metrics_port: default_metrics_port(),
        }
    }
}

impl NodeConfig {
    /// Load configuration from a TOML file (nested format).
    ///
    /// Returns [`DbError::StorageError`] if the file cannot be read, or
    /// [`DbError::SerializationError`] if the TOML is malformed.
    pub fn from_toml_file(path: impl AsRef<std::path::Path>) -> DbResult<Self> {
        let cf = ConfigFile::from_toml_file(path)?;
        Ok(cf.into_node_config())
    }

    /// Parse configuration from a TOML string (nested format).
    pub fn from_toml_str(s: &str) -> DbResult<Self> {
        let cf = ConfigFile::from_toml_str(s)?;
        Ok(cf.into_node_config())
    }

    /// Serialize this configuration to a TOML string (flattened format).
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

        // Validate data_dir is not empty.
        if self.data_dir.as_os_str().is_empty() {
            return Err(DbError::InvalidInput("data_dir must not be empty".into()));
        }

        // Validate peers list does not include self.
        let self_http = format!("{}:{}", self.http_host, self.http_port);
        let self_grpc = format!("{}:{}", self.grpc_host, self.grpc_port);
        for peer in &self.peers {
            let peer_addr = format!("{}:{}", peer.host, peer.port);
            if peer_addr == self_http || peer_addr == self_grpc {
                return Err(DbError::InvalidInput(format!(
                    "peers list must not include self address: {}",
                    peer_addr
                )));
            }
        }

        // Validate ports don't conflict with metrics port when enabled.
        if self.metrics_port > 0 {
            if self.metrics_port == self.http_port {
                return Err(DbError::InvalidInput(
                    "metrics_port must not equal http_port".into(),
                ));
            }
            if self.metrics_port == self.grpc_port {
                return Err(DbError::InvalidInput(
                    "metrics_port must not equal grpc_port".into(),
                ));
            }
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
    fn validate_rejects_empty_data_dir() {
        let cfg = NodeConfig {
            data_dir: PathBuf::from(""),
            ..NodeConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, DbError::InvalidInput(_)));
        assert!(err.to_string().contains("data_dir"));
    }

    #[test]
    fn validate_rejects_self_in_peers() {
        let cfg = NodeConfig {
            grpc_host: "0.0.0.0".into(),
            grpc_port: 9300,
            peers: vec![NodeAddress::new("0.0.0.0", 9300)],
            ..NodeConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, DbError::InvalidInput(_)));
        assert!(err.to_string().contains("peers"));
    }

    #[test]
    fn validate_rejects_metrics_port_conflict() {
        let cfg = NodeConfig {
            http_port: 9200,
            metrics_port: 9200,
            ..NodeConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, DbError::InvalidInput(_)));
        assert!(err.to_string().contains("metrics_port"));
    }

    #[test]
    fn config_file_nested_toml_roundtrip() {
        let toml_str = r#"
[node]
id = 1
data_dir = "/var/lib/msearchdb/node1"

[network]
http_host = "0.0.0.0"
http_port = 9200
grpc_host = "0.0.0.0"
grpc_port = 9300
peers = ["node2:9300", "node3:9300"]

[storage]
write_buffer_mb = 64
max_open_files = 1000
compression = true

[index]
heap_size_mb = 128
merge_policy = "log"

[cluster]
replication_factor = 3
election_timeout_ms = 150
heartbeat_interval_ms = 50

[auth]
api_key = ""

[observability]
log_level = "info"
metrics_port = 9100
"#;

        let cfg = NodeConfig::from_toml_str(toml_str).unwrap();
        assert_eq!(cfg.node_id, NodeId::new(1));
        assert_eq!(cfg.data_dir, PathBuf::from("/var/lib/msearchdb/node1"));
        assert_eq!(cfg.http_port, 9200);
        assert_eq!(cfg.grpc_port, 9300);
        assert_eq!(cfg.peers.len(), 2);
        assert_eq!(cfg.replication_factor, 3);
        assert_eq!(cfg.write_buffer_mb, 64);
        assert_eq!(cfg.heap_size_mb, 128);
        assert_eq!(cfg.api_key, None); // empty string maps to None
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.metrics_port, 9100);
    }

    #[test]
    fn config_file_minimal_toml_uses_defaults() {
        let toml_str = r#"
[node]
id = 5
data_dir = "/tmp/test"
"#;

        let cfg = NodeConfig::from_toml_str(toml_str).unwrap();
        assert_eq!(cfg.node_id, NodeId::new(5));
        assert_eq!(cfg.http_port, 9200);
        assert_eq!(cfg.grpc_port, 9300);
        assert!(cfg.peers.is_empty());
        assert_eq!(cfg.replication_factor, 1);
    }

    #[test]
    fn config_file_with_api_key() {
        let toml_str = r#"
[node]
id = 1
data_dir = "data"

[auth]
api_key = "my-secret-key"
"#;

        let cfg = NodeConfig::from_toml_str(toml_str).unwrap();
        assert_eq!(cfg.api_key, Some("my-secret-key".into()));
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

    #[test]
    fn config_file_struct_roundtrip() {
        let cf = ConfigFile::default();
        let toml_str = cf.to_toml_string().unwrap();
        let back = ConfigFile::from_toml_str(&toml_str).unwrap();
        assert_eq!(cf, back);
    }
}
