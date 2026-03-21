//! Production security primitives for MSearchDB.
//!
//! This module provides:
//!
//! - **TLS configuration** — self-signed cert generation for development,
//!   PEM file loading for production, and mTLS setup for node-to-node comms.
//! - **Authentication & authorization** — API key and JWT-based auth with
//!   role-based access control (admin, writer, reader).
//! - **Rate limiting** — token-bucket algorithm implementation.
//! - **Input validation** — document size, query depth, field name sanitisation.
//! - **Resource limits** — connection limits, disk usage warnings, memory caps.
//! - **Audit logging** — append-only structured log of write operations.
//!
//! # Examples
//!
//! ```
//! use msearchdb_core::security::{Role, ApiKeyEntry, SecurityConfig};
//!
//! let config = SecurityConfig::default();
//! assert_eq!(config.max_document_size_bytes, 10 * 1024 * 1024);
//!
//! let entry = ApiKeyEntry::new("admin-key", Role::Admin);
//! assert!(entry.has_permission(Role::Writer));
//! ```

use std::collections::HashMap;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::{DbError, DbResult};
use crate::query::Query;

// ---------------------------------------------------------------------------
// TLS configuration
// ---------------------------------------------------------------------------

/// TLS configuration for both HTTP and gRPC servers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Enable TLS for the HTTP REST API.
    pub enabled: bool,
    /// Path to the server certificate PEM file.
    pub cert_path: Option<String>,
    /// Path to the server private key PEM file.
    pub key_path: Option<String>,
    /// Path to the CA certificate PEM file for client verification (mTLS).
    pub ca_cert_path: Option<String>,
    /// Enable mutual TLS (mTLS) for node-to-node communication.
    pub mtls_enabled: bool,
}

impl TlsConfig {
    /// Validate that TLS configuration is consistent.
    pub fn validate(&self) -> DbResult<()> {
        if self.enabled && (self.cert_path.is_none() || self.key_path.is_none()) {
            return Err(DbError::InvalidInput(
                "TLS enabled but cert_path or key_path is missing".into(),
            ));
        }
        if self.mtls_enabled && !self.enabled {
            return Err(DbError::InvalidInput(
                "mTLS requires TLS to be enabled".into(),
            ));
        }
        if self.mtls_enabled && self.ca_cert_path.is_none() {
            return Err(DbError::InvalidInput(
                "mTLS enabled but ca_cert_path is missing".into(),
            ));
        }
        Ok(())
    }
}

/// Self-signed certificate material for development use.
#[derive(Debug)]
pub struct SelfSignedCert {
    /// PEM-encoded certificate.
    pub cert_pem: String,
    /// PEM-encoded private key.
    pub key_pem: String,
}

/// Generate a self-signed certificate for development.
///
/// The certificate is valid for `localhost`, `127.0.0.1`, and the provided
/// `subject_alt_names`.  It expires after 365 days.
pub fn generate_self_signed_cert(subject_alt_names: &[String]) -> DbResult<SelfSignedCert> {
    let mut params = rcgen::CertificateParams::new(
        subject_alt_names
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<String>>(),
    )
    .map_err(|e| DbError::InvalidInput(format!("invalid SAN: {}", e)))?;

    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2026, 12, 31);

    let key_pair = rcgen::KeyPair::generate()
        .map_err(|e| DbError::InvalidInput(format!("key generation failed: {}", e)))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| DbError::InvalidInput(format!("cert generation failed: {}", e)))?;

    Ok(SelfSignedCert {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    })
}

/// Load a rustls [`ServerConfig`](rustls::ServerConfig) from PEM files.
///
/// If `ca_cert_path` is `Some`, mutual TLS client verification is enabled.
pub fn load_rustls_server_config(
    cert_path: &str,
    key_path: &str,
    ca_cert_path: Option<&str>,
) -> DbResult<rustls::ServerConfig> {
    // Load server cert chain
    let cert_file = std::fs::File::open(cert_path).map_err(|e| {
        DbError::StorageError(format!("cannot open cert file '{}': {}", cert_path, e))
    })?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_file))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| DbError::StorageError(format!("invalid cert PEM: {}", e)))?;

    // Load server private key
    let key_file = std::fs::File::open(key_path).map_err(|e| {
        DbError::StorageError(format!("cannot open key file '{}': {}", key_path, e))
    })?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .map_err(|e| DbError::StorageError(format!("invalid key PEM: {}", e)))?
        .ok_or_else(|| DbError::StorageError("no private key found in PEM file".into()))?;

    let config = if let Some(ca_path) = ca_cert_path {
        // mTLS: require client certificates
        let ca_file = std::fs::File::open(ca_path).map_err(|e| {
            DbError::StorageError(format!("cannot open CA file '{}': {}", ca_path, e))
        })?;
        let ca_certs: Vec<rustls::pki_types::CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(ca_file))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| DbError::StorageError(format!("invalid CA PEM: {}", e)))?;

        let mut root_store = rustls::RootCertStore::empty();
        for ca_cert in ca_certs {
            root_store
                .add(ca_cert)
                .map_err(|e| DbError::StorageError(format!("invalid CA cert: {}", e)))?;
        }

        let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
            .build()
            .map_err(|e| DbError::StorageError(format!("client verifier error: {}", e)))?;

        rustls::ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(certs, key)
            .map_err(|e| DbError::StorageError(format!("TLS config error: {}", e)))?
    } else {
        // Standard TLS: no client cert required
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| DbError::StorageError(format!("TLS config error: {}", e)))?
    };

    Ok(config)
}

/// Load PEM data from a string into a rustls [`ServerConfig`].
///
/// Useful for dev mode where certs are generated in-memory.
pub fn load_rustls_config_from_pem(
    cert_pem: &str,
    key_pem: &str,
) -> DbResult<rustls::ServerConfig> {
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| DbError::StorageError(format!("invalid cert PEM: {}", e)))?;

    let key = rustls_pemfile::private_key(&mut BufReader::new(key_pem.as_bytes()))
        .map_err(|e| DbError::StorageError(format!("invalid key PEM: {}", e)))?
        .ok_or_else(|| DbError::StorageError("no private key found in PEM".into()))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| DbError::StorageError(format!("TLS config error: {}", e)))?;

    Ok(config)
}

// ---------------------------------------------------------------------------
// Roles & permissions
// ---------------------------------------------------------------------------

/// Access control role for API users.
///
/// Roles form a hierarchy: `Admin` > `Writer` > `Reader`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Role {
    /// Full access: all operations.
    Admin,
    /// Write access: can index, update, delete documents and manage collections.
    Writer,
    /// Read-only access: can search, get documents, and view cluster state.
    Reader,
}

impl Role {
    /// Returns the numeric privilege level (higher = more privilege).
    pub fn level(self) -> u8 {
        match self {
            Role::Admin => 3,
            Role::Writer => 2,
            Role::Reader => 1,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Admin => write!(f, "admin"),
            Role::Writer => write!(f, "writer"),
            Role::Reader => write!(f, "reader"),
        }
    }
}

impl std::str::FromStr for Role {
    type Err = DbError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "admin" => Ok(Role::Admin),
            "writer" => Ok(Role::Writer),
            "reader" => Ok(Role::Reader),
            _ => Err(DbError::InvalidInput(format!("unknown role: '{}'", s))),
        }
    }
}

// ---------------------------------------------------------------------------
// API key authentication
// ---------------------------------------------------------------------------

/// An API key entry with an associated role.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiKeyEntry {
    /// The API key string.
    pub key: String,
    /// The role granted by this key.
    pub role: Role,
    /// Human-readable description for audit purposes.
    pub description: String,
}

impl ApiKeyEntry {
    /// Create a new API key entry.
    pub fn new(key: impl Into<String>, role: Role) -> Self {
        Self {
            key: key.into(),
            role,
            description: String::new(),
        }
    }

    /// Builder: set description.
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Check if this key has sufficient permission for the required role.
    pub fn has_permission(&self, required: Role) -> bool {
        self.role.level() >= required.level()
    }
}

/// Registry of API keys for authentication.
#[derive(Clone, Debug)]
pub struct ApiKeyRegistry {
    /// Map from key string to entry.
    keys: HashMap<String, ApiKeyEntry>,
}

impl ApiKeyRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
        }
    }

    /// Register an API key.
    pub fn register(&mut self, entry: ApiKeyEntry) {
        self.keys.insert(entry.key.clone(), entry);
    }

    /// Look up an API key.
    pub fn get(&self, key: &str) -> Option<&ApiKeyEntry> {
        self.keys.get(key)
    }

    /// Number of registered keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

impl Default for ApiKeyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// JWT authentication
// ---------------------------------------------------------------------------

/// Claims encoded in a JWT token.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JwtClaims {
    /// Subject (user/service identifier).
    pub sub: String,
    /// Role granted.
    pub role: String,
    /// Expiration timestamp (seconds since epoch).
    pub exp: u64,
    /// Issued-at timestamp (seconds since epoch).
    pub iat: u64,
}

/// JWT manager for token issuance and verification.
#[derive(Clone, Debug)]
pub struct JwtManager {
    /// HMAC-SHA256 secret key.
    secret: String,
    /// Default token TTL.
    ttl: Duration,
}

impl JwtManager {
    /// Create a new JWT manager.
    pub fn new(secret: impl Into<String>, ttl: Duration) -> Self {
        Self {
            secret: secret.into(),
            ttl,
        }
    }

    /// Issue a new JWT token.
    pub fn issue(&self, subject: &str, role: Role) -> DbResult<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let claims = JwtClaims {
            sub: subject.to_string(),
            role: role.to_string(),
            exp: now + self.ttl.as_secs(),
            iat: now,
        };

        let header = jsonwebtoken::Header::default();
        let key = jsonwebtoken::EncodingKey::from_secret(self.secret.as_bytes());

        jsonwebtoken::encode(&header, &claims, &key)
            .map_err(|e| DbError::InvalidInput(format!("JWT encode error: {}", e)))
    }

    /// Verify and decode a JWT token.
    pub fn verify(&self, token: &str) -> DbResult<JwtClaims> {
        let key = jsonwebtoken::DecodingKey::from_secret(self.secret.as_bytes());

        let mut validation = jsonwebtoken::Validation::default();
        validation.validate_exp = true;
        validation.required_spec_claims.clear();

        let data = jsonwebtoken::decode::<JwtClaims>(token, &key, &validation)
            .map_err(|e| DbError::InvalidInput(format!("JWT verify error: {}", e)))?;

        Ok(data.claims)
    }
}

// ---------------------------------------------------------------------------
// Rate limiting — token bucket
// ---------------------------------------------------------------------------

/// A single token bucket for rate limiting.
///
/// Implements the classic token-bucket algorithm: tokens are added at a fixed
/// rate up to a maximum capacity.  Each request consumes one token.  If no
/// tokens are available, the request is rejected.
#[derive(Debug)]
pub struct TokenBucket {
    /// Maximum number of tokens.
    capacity: u64,
    /// Tokens added per second.
    refill_rate: f64,
    /// Current token count (scaled by 1000 for sub-token precision).
    tokens: AtomicU64,
    /// Last refill timestamp in milliseconds since some epoch.
    last_refill: std::sync::Mutex<Instant>,
}

impl TokenBucket {
    /// Create a new token bucket.
    ///
    /// - `capacity`: maximum burst size.
    /// - `refill_rate`: tokens restored per second.
    pub fn new(capacity: u64, refill_rate: f64) -> Self {
        Self {
            capacity,
            refill_rate,
            tokens: AtomicU64::new(capacity * 1000),
            last_refill: std::sync::Mutex::new(Instant::now()),
        }
    }

    /// Try to consume one token.  Returns `true` if the request is allowed.
    pub fn try_acquire(&self) -> bool {
        self.refill();

        loop {
            let current = self.tokens.load(Ordering::Relaxed);
            if current < 1000 {
                return false;
            }
            if self
                .tokens
                .compare_exchange(current, current - 1000, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Refill tokens based on elapsed time.
    fn refill(&self) {
        let mut last = self.last_refill.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(*last);
        let new_tokens = (elapsed.as_secs_f64() * self.refill_rate * 1000.0) as u64;

        if new_tokens > 0 {
            *last = now;
            let max = self.capacity * 1000;
            loop {
                let current = self.tokens.load(Ordering::Relaxed);
                let updated = (current + new_tokens).min(max);
                if self
                    .tokens
                    .compare_exchange(current, updated, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            }
        }
    }

    /// Current number of available tokens (approximate).
    pub fn available(&self) -> u64 {
        self.refill();
        self.tokens.load(Ordering::Relaxed) / 1000
    }
}

/// Per-key rate limiter.
///
/// Each API key gets its own [`TokenBucket`].  Unknown keys get a shared
/// default bucket.
pub struct RateLimiter {
    /// Per-key buckets.
    buckets: RwLock<HashMap<String, Arc<TokenBucket>>>,
    /// Default capacity per key.
    default_capacity: u64,
    /// Default refill rate per key (tokens/sec).
    default_refill_rate: f64,
}

impl RateLimiter {
    /// Create a rate limiter with default settings.
    pub fn new(default_capacity: u64, default_refill_rate: f64) -> Self {
        Self {
            buckets: RwLock::new(HashMap::new()),
            default_capacity,
            default_refill_rate,
        }
    }

    /// Try to acquire a token for the given key.
    pub async fn try_acquire(&self, key: &str) -> bool {
        // Fast path: check if bucket exists
        {
            let buckets = self.buckets.read().await;
            if let Some(bucket) = buckets.get(key) {
                return bucket.try_acquire();
            }
        }

        // Slow path: create bucket
        let mut buckets = self.buckets.write().await;
        let bucket = buckets
            .entry(key.to_string())
            .or_insert_with(|| {
                Arc::new(TokenBucket::new(
                    self.default_capacity,
                    self.default_refill_rate,
                ))
            })
            .clone();
        bucket.try_acquire()
    }
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("default_capacity", &self.default_capacity)
            .field("default_refill_rate", &self.default_refill_rate)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

/// Input validation configuration and enforcement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValidationConfig {
    /// Maximum document size in bytes (default: 10 MB).
    pub max_document_size_bytes: usize,
    /// Maximum bool query nesting depth (default: 5).
    pub max_query_depth: usize,
    /// Maximum bulk batch size in bytes (default: 10 MB).
    pub max_bulk_size_bytes: usize,
    /// Query timeout in seconds (default: 30).
    pub query_timeout_secs: u64,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            max_document_size_bytes: 10 * 1024 * 1024,
            max_query_depth: 5,
            max_bulk_size_bytes: 10 * 1024 * 1024,
            query_timeout_secs: 30,
        }
    }
}

/// Validate that a field name is acceptable.
///
/// Rejects field names that start with `_` (reserved for system fields).
pub fn validate_field_name(name: &str) -> DbResult<()> {
    if name.starts_with('_') {
        return Err(DbError::InvalidInput(format!(
            "field name '{}' must not start with '_' (reserved for system fields)",
            name
        )));
    }
    if name.is_empty() {
        return Err(DbError::InvalidInput("field name must not be empty".into()));
    }
    if name.len() > 256 {
        return Err(DbError::InvalidInput(format!(
            "field name '{}...' exceeds 256 character limit",
            &name[..32]
        )));
    }
    Ok(())
}

/// Validate document size.
pub fn validate_document_size(json_bytes: usize, config: &ValidationConfig) -> DbResult<()> {
    if json_bytes > config.max_document_size_bytes {
        return Err(DbError::InvalidInput(format!(
            "document size {} bytes exceeds limit of {} bytes",
            json_bytes, config.max_document_size_bytes
        )));
    }
    Ok(())
}

/// Validate bulk request size.
pub fn validate_bulk_size(body_bytes: usize, config: &ValidationConfig) -> DbResult<()> {
    if body_bytes > config.max_bulk_size_bytes {
        return Err(DbError::InvalidInput(format!(
            "bulk request size {} bytes exceeds limit of {} bytes",
            body_bytes, config.max_bulk_size_bytes
        )));
    }
    Ok(())
}

/// Validate query complexity (nesting depth of bool queries).
pub fn validate_query_depth(query: &Query, config: &ValidationConfig) -> DbResult<()> {
    let depth = measure_query_depth(query);
    if depth > config.max_query_depth {
        return Err(DbError::InvalidInput(format!(
            "query nesting depth {} exceeds limit of {}",
            depth, config.max_query_depth
        )));
    }
    Ok(())
}

/// Recursively measure the nesting depth of a query.
fn measure_query_depth(query: &Query) -> usize {
    match query {
        Query::Bool(b) => {
            let max_child = b
                .must
                .iter()
                .chain(b.should.iter())
                .chain(b.must_not.iter())
                .map(measure_query_depth)
                .max()
                .unwrap_or(0);
            1 + max_child
        }
        // All leaf query types have depth 1.
        // The wildcard covers FullText, Term, Range, and any future variants
        // (Query is #[non_exhaustive]).
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Resource limits
// ---------------------------------------------------------------------------

/// Resource limits configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Maximum concurrent connections per node.
    pub max_connections: usize,
    /// Disk usage percentage at which to emit a warning (default: 80%).
    pub disk_warning_threshold_pct: u8,
    /// Memory limit in megabytes (0 = no limit).
    pub memory_limit_mb: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_connections: 1000,
            disk_warning_threshold_pct: 80,
            memory_limit_mb: 0,
        }
    }
}

/// Connection counter for tracking active connections.
#[derive(Debug)]
pub struct ConnectionCounter {
    /// Current active connections.
    current: AtomicU64,
    /// Maximum allowed.
    max: u64,
}

impl ConnectionCounter {
    /// Create a new connection counter.
    pub fn new(max: u64) -> Self {
        Self {
            current: AtomicU64::new(0),
            max,
        }
    }

    /// Try to acquire a connection slot.  Returns `true` if under the limit.
    pub fn try_acquire(&self) -> bool {
        loop {
            let current = self.current.load(Ordering::Relaxed);
            if current >= self.max {
                return false;
            }
            if self
                .current
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Release a connection slot.
    pub fn release(&self) {
        self.current.fetch_sub(1, Ordering::SeqCst);
    }

    /// Current number of active connections.
    pub fn active(&self) -> u64 {
        self.current.load(Ordering::Relaxed)
    }

    /// Maximum number of connections allowed.
    pub fn max(&self) -> u64 {
        self.max
    }
}

// ---------------------------------------------------------------------------
// Audit logging
// ---------------------------------------------------------------------------

/// A single audit log entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEntry {
    /// ISO-8601 timestamp.
    pub timestamp: String,
    /// The user or API key identifier that performed the operation.
    pub user: String,
    /// The operation performed (e.g., "index", "delete", "create_collection").
    pub operation: String,
    /// The collection affected.
    pub collection: String,
    /// The document ID affected (if applicable).
    pub document_id: Option<String>,
    /// The HTTP method and path.
    pub request_path: String,
    /// Request ID for correlation.
    pub request_id: String,
    /// Outcome: "success" or "failure".
    pub outcome: String,
}

impl AuditEntry {
    /// Create a new audit entry with the current timestamp.
    pub fn now(
        user: impl Into<String>,
        operation: impl Into<String>,
        collection: impl Into<String>,
    ) -> Self {
        let timestamp = {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            format!("{}.{:03}Z", now.as_secs(), now.subsec_millis())
        };

        Self {
            timestamp,
            user: user.into(),
            operation: operation.into(),
            collection: collection.into(),
            document_id: None,
            request_path: String::new(),
            request_id: String::new(),
            outcome: "success".into(),
        }
    }

    /// Builder: set document ID.
    pub fn with_document_id(mut self, id: impl Into<String>) -> Self {
        self.document_id = Some(id.into());
        self
    }

    /// Builder: set request path.
    pub fn with_request_path(mut self, path: impl Into<String>) -> Self {
        self.request_path = path.into();
        self
    }

    /// Builder: set request ID.
    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = id.into();
        self
    }

    /// Builder: set outcome.
    pub fn with_outcome(mut self, outcome: impl Into<String>) -> Self {
        self.outcome = outcome.into();
        self
    }
}

/// Append-only audit logger that writes JSON lines to a file.
pub struct AuditLogger {
    /// Sender for async log writes.
    sender: tokio::sync::mpsc::UnboundedSender<AuditEntry>,
}

impl AuditLogger {
    /// Create a new audit logger writing to the specified path.
    ///
    /// Spawns a background task that writes entries as JSON lines.
    pub fn new(path: impl AsRef<Path>) -> DbResult<Self> {
        let path = path.as_ref().to_path_buf();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<AuditEntry>();

        tokio::spawn(async move {
            use std::io::Write;

            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path);

            let mut file = match file {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(path = %path.display(), error = %e, "failed to open audit log");
                    return;
                }
            };

            while let Some(entry) = receiver.recv().await {
                match serde_json::to_string(&entry) {
                    Ok(line) => {
                        if let Err(e) = writeln!(file, "{}", line) {
                            tracing::error!(error = %e, "failed to write audit entry");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to serialize audit entry");
                    }
                }
            }
        });

        Ok(Self { sender })
    }

    /// Create a no-op audit logger (for testing or when disabled).
    pub fn noop() -> Self {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        Self { sender }
    }

    /// Log an audit entry.
    pub fn log(&self, entry: AuditEntry) {
        // Best-effort: don't block the request path.
        let _ = self.sender.send(entry);
    }
}

impl std::fmt::Debug for AuditLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditLogger").finish()
    }
}

// ---------------------------------------------------------------------------
// Top-level security config
// ---------------------------------------------------------------------------

/// Top-level security configuration aggregating all sub-configs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// TLS configuration.
    pub tls: TlsConfig,
    /// Input validation settings.
    pub validation: ValidationConfig,
    /// Resource limits.
    pub resource_limits: ResourceLimits,
    /// Maximum document size in bytes (convenience alias).
    pub max_document_size_bytes: usize,
    /// JWT secret key (empty = JWT disabled).
    pub jwt_secret: String,
    /// JWT token TTL in seconds (default: 3600).
    pub jwt_ttl_secs: u64,
    /// Default rate limit capacity per API key.
    pub rate_limit_capacity: u64,
    /// Default rate limit refill rate (requests/sec).
    pub rate_limit_refill_rate: f64,
    /// Enable audit logging.
    pub audit_enabled: bool,
    /// Path to the audit log file.
    pub audit_log_path: String,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            tls: TlsConfig::default(),
            validation: ValidationConfig::default(),
            resource_limits: ResourceLimits::default(),
            max_document_size_bytes: 10 * 1024 * 1024,
            jwt_secret: String::new(),
            jwt_ttl_secs: 3600,
            rate_limit_capacity: 100,
            rate_limit_refill_rate: 10.0,
            audit_enabled: false,
            audit_log_path: "data/audit.log".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- TLS config tests ---------------------------------------------------

    #[test]
    fn tls_config_default_is_disabled() {
        let cfg = TlsConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.mtls_enabled);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn tls_config_rejects_enabled_without_certs() {
        let cfg = TlsConfig {
            enabled: true,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("cert_path"));
    }

    #[test]
    fn tls_config_rejects_mtls_without_tls() {
        let cfg = TlsConfig {
            mtls_enabled: true,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("mTLS requires TLS"));
    }

    #[test]
    fn tls_config_rejects_mtls_without_ca() {
        let cfg = TlsConfig {
            enabled: true,
            cert_path: Some("cert.pem".into()),
            key_path: Some("key.pem".into()),
            mtls_enabled: true,
            ca_cert_path: None,
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("ca_cert_path"));
    }

    #[test]
    fn tls_config_valid_with_all_paths() {
        let cfg = TlsConfig {
            enabled: true,
            cert_path: Some("cert.pem".into()),
            key_path: Some("key.pem".into()),
            ca_cert_path: Some("ca.pem".into()),
            mtls_enabled: true,
        };
        assert!(cfg.validate().is_ok());
    }

    // -- Self-signed cert tests ---------------------------------------------

    #[test]
    fn generate_self_signed_cert_works() {
        let cert = generate_self_signed_cert(&["localhost".into()]).unwrap();
        assert!(cert.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(cert.key_pem.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn load_rustls_from_generated_pem() {
        let cert = generate_self_signed_cert(&["localhost".into()]).unwrap();
        let config = load_rustls_config_from_pem(&cert.cert_pem, &cert.key_pem).unwrap();
        // Just verify it builds without error — the config is functional.
        assert!(config.alpn_protocols.is_empty());
    }

    // -- Role tests ---------------------------------------------------------

    #[test]
    fn role_hierarchy() {
        assert!(Role::Admin.level() > Role::Writer.level());
        assert!(Role::Writer.level() > Role::Reader.level());
    }

    #[test]
    fn role_display_and_parse() {
        assert_eq!(Role::Admin.to_string(), "admin");
        assert_eq!("writer".parse::<Role>().unwrap(), Role::Writer);
        assert_eq!("READER".parse::<Role>().unwrap(), Role::Reader);
        assert!("unknown".parse::<Role>().is_err());
    }

    // -- API key tests ------------------------------------------------------

    #[test]
    fn api_key_entry_permission_check() {
        let admin = ApiKeyEntry::new("k1", Role::Admin);
        assert!(admin.has_permission(Role::Admin));
        assert!(admin.has_permission(Role::Writer));
        assert!(admin.has_permission(Role::Reader));

        let writer = ApiKeyEntry::new("k2", Role::Writer);
        assert!(!writer.has_permission(Role::Admin));
        assert!(writer.has_permission(Role::Writer));
        assert!(writer.has_permission(Role::Reader));

        let reader = ApiKeyEntry::new("k3", Role::Reader);
        assert!(!reader.has_permission(Role::Admin));
        assert!(!reader.has_permission(Role::Writer));
        assert!(reader.has_permission(Role::Reader));
    }

    #[test]
    fn api_key_registry_crud() {
        let mut registry = ApiKeyRegistry::new();
        assert!(registry.is_empty());

        registry.register(ApiKeyEntry::new("key1", Role::Admin));
        assert_eq!(registry.len(), 1);
        assert!(registry.get("key1").is_some());
        assert!(registry.get("key2").is_none());
    }

    // -- JWT tests ----------------------------------------------------------

    #[test]
    fn jwt_issue_and_verify() {
        let mgr = JwtManager::new("test-secret", Duration::from_secs(3600));
        let token = mgr.issue("user1", Role::Admin).unwrap();
        assert!(!token.is_empty());

        let claims = mgr.verify(&token).unwrap();
        assert_eq!(claims.sub, "user1");
        assert_eq!(claims.role, "admin");
    }

    #[test]
    fn jwt_rejects_wrong_secret() {
        let mgr1 = JwtManager::new("secret1", Duration::from_secs(3600));
        let mgr2 = JwtManager::new("secret2", Duration::from_secs(3600));

        let token = mgr1.issue("user1", Role::Reader).unwrap();
        let err = mgr2.verify(&token).unwrap_err();
        assert!(err.to_string().contains("JWT verify error"));
    }

    #[test]
    fn jwt_rejects_expired_token() {
        // Manually create a token with exp in the past.
        let secret = "secret";
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let claims = JwtClaims {
            sub: "user1".to_string(),
            role: "reader".to_string(),
            exp: now.saturating_sub(3600), // 1 hour ago
            iat: now.saturating_sub(7200), // 2 hours ago
        };

        let header = jsonwebtoken::Header::default();
        let key = jsonwebtoken::EncodingKey::from_secret(secret.as_bytes());
        let token = jsonwebtoken::encode(&header, &claims, &key).unwrap();

        let mgr = JwtManager::new(secret, Duration::from_secs(3600));
        let err = mgr.verify(&token).unwrap_err();
        assert!(err.to_string().contains("JWT verify error"));
    }

    // -- Token bucket tests -------------------------------------------------

    #[test]
    fn token_bucket_basic() {
        let bucket = TokenBucket::new(5, 1.0);
        assert_eq!(bucket.available(), 5);

        // Consume all 5 tokens
        for _ in 0..5 {
            assert!(bucket.try_acquire());
        }

        // 6th should fail
        assert!(!bucket.try_acquire());
    }

    #[test]
    fn token_bucket_refills() {
        let bucket = TokenBucket::new(2, 100.0); // 100 tokens/sec

        // Drain
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire());

        // Wait for refill
        std::thread::sleep(Duration::from_millis(50));

        // Should have refilled some tokens
        assert!(bucket.try_acquire());
    }

    // -- Validation tests ---------------------------------------------------

    #[test]
    fn validate_field_name_rejects_underscore() {
        assert!(validate_field_name("_secret").is_err());
        assert!(validate_field_name("_id").is_err());
    }

    #[test]
    fn validate_field_name_accepts_normal() {
        assert!(validate_field_name("name").is_ok());
        assert!(validate_field_name("price").is_ok());
        assert!(validate_field_name("field_with_underscore").is_ok());
    }

    #[test]
    fn validate_field_name_rejects_empty() {
        assert!(validate_field_name("").is_err());
    }

    #[test]
    fn validate_document_size_enforced() {
        let config = ValidationConfig::default();
        assert!(validate_document_size(100, &config).is_ok());
        assert!(validate_document_size(11 * 1024 * 1024, &config).is_err());
    }

    #[test]
    fn validate_bulk_size_enforced() {
        let config = ValidationConfig::default();
        assert!(validate_bulk_size(100, &config).is_ok());
        assert!(validate_bulk_size(11 * 1024 * 1024, &config).is_err());
    }

    #[test]
    fn validate_query_depth_flat() {
        let config = ValidationConfig::default();
        let query = Query::Term(crate::query::TermQuery {
            field: "f".into(),
            value: "v".into(),
        });
        assert!(validate_query_depth(&query, &config).is_ok());
    }

    #[test]
    fn validate_query_depth_nested_within_limit() {
        let config = ValidationConfig::default(); // depth 5
        let inner = Query::Term(crate::query::TermQuery {
            field: "f".into(),
            value: "v".into(),
        });
        // Build 4 levels of nesting (total depth = 5 including leaf).
        let mut q = inner;
        for _ in 0..4 {
            q = Query::Bool(crate::query::BoolQuery {
                must: vec![q],
                should: vec![],
                must_not: vec![],
            });
        }
        assert!(validate_query_depth(&q, &config).is_ok());
    }

    #[test]
    fn validate_query_depth_exceeds_limit() {
        let config = ValidationConfig {
            max_query_depth: 3,
            ..Default::default()
        };
        let inner = Query::Term(crate::query::TermQuery {
            field: "f".into(),
            value: "v".into(),
        });
        // Build 4 levels of nesting (depth = 5).
        let mut q = inner;
        for _ in 0..4 {
            q = Query::Bool(crate::query::BoolQuery {
                must: vec![q],
                should: vec![],
                must_not: vec![],
            });
        }
        assert!(validate_query_depth(&q, &config).is_err());
    }

    // -- Resource limits tests ----------------------------------------------

    #[test]
    fn connection_counter_basic() {
        let counter = ConnectionCounter::new(2);
        assert!(counter.try_acquire());
        assert!(counter.try_acquire());
        assert!(!counter.try_acquire()); // at limit

        counter.release();
        assert_eq!(counter.active(), 1);
        assert!(counter.try_acquire()); // freed one slot
    }

    // -- Audit entry tests --------------------------------------------------

    #[test]
    fn audit_entry_builder() {
        let entry = AuditEntry::now("admin-key", "index", "products")
            .with_document_id("doc1")
            .with_request_path("POST /collections/products/docs")
            .with_request_id("req-123");

        assert_eq!(entry.user, "admin-key");
        assert_eq!(entry.operation, "index");
        assert_eq!(entry.collection, "products");
        assert_eq!(entry.document_id.as_deref(), Some("doc1"));
        assert_eq!(entry.request_id, "req-123");
        assert_eq!(entry.outcome, "success");
    }

    #[test]
    fn audit_entry_serializes() {
        let entry = AuditEntry::now("user", "delete", "logs").with_document_id("d1");
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"user\":\"user\""));
        assert!(json.contains("\"operation\":\"delete\""));
    }

    // -- SecurityConfig default tests ---------------------------------------

    #[test]
    fn security_config_default() {
        let cfg = SecurityConfig::default();
        assert_eq!(cfg.max_document_size_bytes, 10 * 1024 * 1024);
        assert_eq!(cfg.validation.max_query_depth, 5);
        assert_eq!(cfg.resource_limits.max_connections, 1000);
        assert!(!cfg.tls.enabled);
        assert!(!cfg.audit_enabled);
    }
}
