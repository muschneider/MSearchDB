//! TLS support for gRPC node-to-node communication.
//!
//! Provides helpers to build tonic gRPC servers and clients with TLS and
//! mutual TLS (mTLS) using rustls as the crypto backend.
//!
//! # Usage
//!
//! ```ignore
//! let identity = load_tonic_identity("cert.pem", "key.pem")?;
//! let tls_config = tonic::transport::ServerTlsConfig::new()
//!     .identity(identity);
//! ```

use msearchdb_core::error::{DbError, DbResult};

// ---------------------------------------------------------------------------
// Server-side TLS for tonic gRPC
// ---------------------------------------------------------------------------

/// Load a tonic [`Identity`](tonic::transport::Identity) from PEM files.
///
/// Used to configure a gRPC server with TLS.
pub fn load_tonic_identity(
    cert_path: &str,
    key_path: &str,
) -> DbResult<tonic::transport::Identity> {
    let cert_pem = std::fs::read_to_string(cert_path)
        .map_err(|e| DbError::StorageError(format!("cannot read cert '{}': {}", cert_path, e)))?;
    let key_pem = std::fs::read_to_string(key_path)
        .map_err(|e| DbError::StorageError(format!("cannot read key '{}': {}", key_path, e)))?;

    Ok(tonic::transport::Identity::from_pem(cert_pem, key_pem))
}

/// Load a tonic [`Certificate`](tonic::transport::Certificate) for client CA
/// verification (mTLS).
pub fn load_tonic_ca_cert(ca_path: &str) -> DbResult<tonic::transport::Certificate> {
    let ca_pem = std::fs::read_to_string(ca_path)
        .map_err(|e| DbError::StorageError(format!("cannot read CA cert '{}': {}", ca_path, e)))?;

    Ok(tonic::transport::Certificate::from_pem(ca_pem))
}

/// Build a tonic [`ServerTlsConfig`] from the security TLS configuration.
///
/// Returns `None` if TLS is not enabled.
pub fn build_grpc_server_tls(
    tls: &msearchdb_core::security::TlsConfig,
) -> DbResult<Option<tonic::transport::ServerTlsConfig>> {
    if !tls.enabled {
        return Ok(None);
    }

    let cert_path = tls
        .cert_path
        .as_deref()
        .ok_or_else(|| DbError::InvalidInput("TLS cert_path required".into()))?;
    let key_path = tls
        .key_path
        .as_deref()
        .ok_or_else(|| DbError::InvalidInput("TLS key_path required".into()))?;

    let identity = load_tonic_identity(cert_path, key_path)?;
    let mut config = tonic::transport::ServerTlsConfig::new().identity(identity);

    if tls.mtls_enabled {
        let ca_path = tls
            .ca_cert_path
            .as_deref()
            .ok_or_else(|| DbError::InvalidInput("mTLS ca_cert_path required".into()))?;
        let ca_cert = load_tonic_ca_cert(ca_path)?;
        config = config.client_ca_root(ca_cert);
    }

    Ok(Some(config))
}

// ---------------------------------------------------------------------------
// Client-side TLS for tonic gRPC
// ---------------------------------------------------------------------------

/// Build a tonic [`ClientTlsConfig`] for connecting to TLS-enabled peers.
///
/// If `ca_cert_path` is provided, it is used as the trusted root CA.
/// If `client_cert_path` and `client_key_path` are provided, mutual TLS is
/// used.
pub fn build_grpc_client_tls(
    ca_cert_path: Option<&str>,
    client_cert_path: Option<&str>,
    client_key_path: Option<&str>,
    domain_name: &str,
) -> DbResult<tonic::transport::ClientTlsConfig> {
    let mut config = tonic::transport::ClientTlsConfig::new().domain_name(domain_name);

    if let Some(ca_path) = ca_cert_path {
        let ca_cert = load_tonic_ca_cert(ca_path)?;
        config = config.ca_certificate(ca_cert);
    }

    if let (Some(cert_path), Some(key_path)) = (client_cert_path, client_key_path) {
        let identity = load_tonic_identity(cert_path, key_path)?;
        config = config.identity(identity);
    }

    Ok(config)
}

/// Build a rustls [`ServerConfig`] from PEM file paths for the HTTP server.
///
/// This delegates to [`msearchdb_core::security::load_rustls_server_config`].
pub fn load_http_tls_config(
    tls: &msearchdb_core::security::TlsConfig,
) -> DbResult<Option<rustls::ServerConfig>> {
    if !tls.enabled {
        return Ok(None);
    }

    let cert_path = tls
        .cert_path
        .as_deref()
        .ok_or_else(|| DbError::InvalidInput("TLS cert_path required".into()))?;
    let key_path = tls
        .key_path
        .as_deref()
        .ok_or_else(|| DbError::InvalidInput("TLS key_path required".into()))?;

    let ca_path = if tls.mtls_enabled {
        tls.ca_cert_path.as_deref()
    } else {
        None
    };

    let config = msearchdb_core::security::load_rustls_server_config(cert_path, key_path, ca_path)?;
    Ok(Some(config))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_grpc_server_tls_returns_none_when_disabled() {
        let tls = msearchdb_core::security::TlsConfig::default();
        let result = build_grpc_server_tls(&tls).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn build_grpc_client_tls_basic() {
        let config = build_grpc_client_tls(None, None, None, "localhost").unwrap();
        // Just verify it builds.
        let _ = config;
    }

    #[test]
    fn load_http_tls_returns_none_when_disabled() {
        let tls = msearchdb_core::security::TlsConfig::default();
        let result = load_http_tls_config(&tls).unwrap();
        assert!(result.is_none());
    }
}
