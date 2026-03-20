//! Prometheus metrics for MSearchDB.
//!
//! This module provides a [`Metrics`] struct that registers all application
//! metrics with a dedicated [`prometheus::Registry`].
//!
//! Metrics are exposed at `GET /metrics` in the Prometheus text format.
//!
//! # Metrics Catalogue
//!
//! | Metric | Type | Description |
//! |--------|------|-------------|
//! | `msearchdb_documents_total` | Counter | Total documents indexed per collection |
//! | `msearchdb_index_operations_total` | Counter | Index operations per type and collection |
//! | `msearchdb_search_duration_seconds` | Histogram | Search latency distribution |
//! | `msearchdb_storage_bytes_used` | Gauge | Approximate storage bytes used |
//! | `msearchdb_raft_commit_index` | Gauge | Latest committed Raft log index |
//! | `msearchdb_raft_leader_id` | Gauge | Node ID of the current Raft leader |
//! | `msearchdb_raft_commit_latency_seconds` | Histogram | Raft commit latency in seconds |
//! | `msearchdb_replication_lag` | Gauge | Replication lag per node in committed log entries |
//! | `msearchdb_node_health` | Gauge | Node health (1=up, 0=down) |
//! | `msearchdb_http_requests_total` | Counter | HTTP requests per method, path, status |
//! | `msearchdb_http_request_duration_seconds` | Histogram | HTTP request latency distribution |
//! | `msearchdb_cache_hits_total` | Counter | Total cache hits by type |
//! | `msearchdb_cache_misses_total` | Counter | Total cache misses by type |
//! | `msearchdb_active_connections` | Gauge | Number of active HTTP connections |
//! | `msearchdb_collections_total` | Gauge | Total number of collections |

use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Central metrics registry for MSearchDB.
///
/// All metrics are registered on construction and can be updated throughout
/// the application lifetime. The [`gather`](Metrics::gather) method produces
/// the Prometheus text format for scraping.
#[derive(Clone)]
pub struct Metrics {
    /// The Prometheus registry.
    pub registry: Registry,

    /// Total documents indexed per collection.
    pub documents_total: IntCounterVec,

    /// Index operations per (operation, collection).
    pub index_operations_total: IntCounterVec,

    /// Search duration histogram (seconds).
    pub search_duration_seconds: HistogramVec,

    /// Approximate storage bytes used.
    pub storage_bytes_used: IntGauge,

    /// Raft commit index.
    pub raft_commit_index: IntGauge,

    /// Raft leader node ID.
    pub raft_leader_id: IntGauge,

    /// Raft commit latency histogram (seconds).
    pub raft_commit_latency_seconds: HistogramVec,

    /// Replication lag per node in committed log entries.
    pub replication_lag: GaugeVec,

    /// Per-node health gauge (1=up, 0=down).
    pub node_health: GaugeVec,

    /// HTTP requests counter by (method, path, status).
    pub http_requests_total: IntCounterVec,

    /// HTTP request duration histogram (seconds).
    pub http_request_duration_seconds: HistogramVec,

    /// Total cache hits by type.
    pub cache_hits_total: IntCounterVec,

    /// Total cache misses by type.
    pub cache_misses_total: IntCounterVec,

    /// Number of active HTTP connections.
    pub active_connections: IntGauge,

    /// Total number of collections.
    pub collections_total: IntGauge,
}

impl Metrics {
    /// Create and register all metrics.
    ///
    /// # Panics
    ///
    /// Panics if metric registration fails (should never happen unless
    /// duplicate names are registered).
    pub fn new() -> Self {
        let registry = Registry::new();

        let documents_total = IntCounterVec::new(
            Opts::new(
                "msearchdb_documents_total",
                "Total documents indexed per collection",
            ),
            &["collection"],
        )
        .expect("metric: documents_total");

        let index_operations_total = IntCounterVec::new(
            Opts::new(
                "msearchdb_index_operations_total",
                "Index operations per type and collection",
            ),
            &["operation", "collection"],
        )
        .expect("metric: index_operations_total");

        let search_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "msearchdb_search_duration_seconds",
                "Search latency distribution",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ]),
            &["collection"],
        )
        .expect("metric: search_duration_seconds");

        let storage_bytes_used = IntGauge::new(
            "msearchdb_storage_bytes_used",
            "Approximate storage bytes used",
        )
        .expect("metric: storage_bytes_used");

        let raft_commit_index = IntGauge::new(
            "msearchdb_raft_commit_index",
            "Latest committed Raft log index",
        )
        .expect("metric: raft_commit_index");

        let raft_leader_id = IntGauge::new(
            "msearchdb_raft_leader_id",
            "Node ID of the current Raft leader",
        )
        .expect("metric: raft_leader_id");

        let raft_commit_latency_seconds = HistogramVec::new(
            HistogramOpts::new(
                "msearchdb_raft_commit_latency_seconds",
                "Raft commit latency in seconds",
            )
            .buckets(vec![
                0.0005, 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
            ]),
            &[],
        )
        .expect("metric: raft_commit_latency_seconds");

        let replication_lag = GaugeVec::new(
            Opts::new(
                "msearchdb_replication_lag",
                "Replication lag per node in committed log entries",
            ),
            &["node_id"],
        )
        .expect("metric: replication_lag");

        let node_health = GaugeVec::new(
            Opts::new("msearchdb_node_health", "Node health (1=up, 0=down)"),
            &["node_id"],
        )
        .expect("metric: node_health");

        let http_requests_total = IntCounterVec::new(
            Opts::new(
                "msearchdb_http_requests_total",
                "HTTP requests by method, path, and status",
            ),
            &["method", "path", "status"],
        )
        .expect("metric: http_requests_total");

        let http_request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "msearchdb_http_request_duration_seconds",
                "HTTP request latency distribution",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ]),
            &["method", "path"],
        )
        .expect("metric: http_request_duration_seconds");

        let cache_hits_total = IntCounterVec::new(
            Opts::new("msearchdb_cache_hits_total", "Total cache hits by type"),
            &["cache_type"],
        )
        .expect("metric: cache_hits_total");

        let cache_misses_total = IntCounterVec::new(
            Opts::new("msearchdb_cache_misses_total", "Total cache misses by type"),
            &["cache_type"],
        )
        .expect("metric: cache_misses_total");

        let active_connections = IntGauge::new(
            "msearchdb_active_connections",
            "Number of active HTTP connections",
        )
        .expect("metric: active_connections");

        let collections_total =
            IntGauge::new("msearchdb_collections_total", "Total number of collections")
                .expect("metric: collections_total");

        // Register all metrics.
        registry
            .register(Box::new(documents_total.clone()))
            .expect("register: documents_total");
        registry
            .register(Box::new(index_operations_total.clone()))
            .expect("register: index_operations_total");
        registry
            .register(Box::new(search_duration_seconds.clone()))
            .expect("register: search_duration_seconds");
        registry
            .register(Box::new(storage_bytes_used.clone()))
            .expect("register: storage_bytes_used");
        registry
            .register(Box::new(raft_commit_index.clone()))
            .expect("register: raft_commit_index");
        registry
            .register(Box::new(raft_leader_id.clone()))
            .expect("register: raft_leader_id");
        registry
            .register(Box::new(raft_commit_latency_seconds.clone()))
            .expect("register: raft_commit_latency_seconds");
        registry
            .register(Box::new(replication_lag.clone()))
            .expect("register: replication_lag");
        registry
            .register(Box::new(node_health.clone()))
            .expect("register: node_health");
        registry
            .register(Box::new(http_requests_total.clone()))
            .expect("register: http_requests_total");
        registry
            .register(Box::new(http_request_duration_seconds.clone()))
            .expect("register: http_request_duration_seconds");
        registry
            .register(Box::new(cache_hits_total.clone()))
            .expect("register: cache_hits_total");
        registry
            .register(Box::new(cache_misses_total.clone()))
            .expect("register: cache_misses_total");
        registry
            .register(Box::new(active_connections.clone()))
            .expect("register: active_connections");
        registry
            .register(Box::new(collections_total.clone()))
            .expect("register: collections_total");

        Self {
            registry,
            documents_total,
            index_operations_total,
            search_duration_seconds,
            storage_bytes_used,
            raft_commit_index,
            raft_leader_id,
            raft_commit_latency_seconds,
            replication_lag,
            node_health,
            http_requests_total,
            http_request_duration_seconds,
            cache_hits_total,
            cache_misses_total,
            active_connections,
            collections_total,
        }
    }

    /// Gather all metrics and encode them in the Prometheus text exposition
    /// format.
    pub fn gather(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .expect("encode metrics");
        String::from_utf8(buffer).expect("metrics should be valid UTF-8")
    }
}

impl Default for Metrics {
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
    fn metrics_registration_does_not_panic() {
        let _metrics = Metrics::new();
    }

    #[test]
    fn metrics_default_does_not_panic() {
        let _metrics = Metrics::default();
    }

    #[test]
    fn metrics_gather_returns_valid_prometheus_format() {
        let metrics = Metrics::new();

        // Increment some counters.
        metrics
            .documents_total
            .with_label_values(&["products"])
            .inc();
        metrics
            .index_operations_total
            .with_label_values(&["index", "products"])
            .inc();
        metrics
            .http_requests_total
            .with_label_values(&["GET", "/collections", "200"])
            .inc();

        // Set some gauges.
        metrics.storage_bytes_used.set(1024);
        metrics.raft_commit_index.set(42);
        metrics.raft_leader_id.set(1);
        metrics.node_health.with_label_values(&["node-1"]).set(1.0);

        // Observe histogram values.
        metrics
            .search_duration_seconds
            .with_label_values(&["products"])
            .observe(0.025);
        metrics
            .http_request_duration_seconds
            .with_label_values(&["GET", "/collections"])
            .observe(0.010);

        let output = metrics.gather();

        // Verify Prometheus format markers.
        assert!(
            output.contains("msearchdb_documents_total"),
            "should contain documents_total"
        );
        assert!(
            output.contains("msearchdb_index_operations_total"),
            "should contain index_operations_total"
        );
        assert!(
            output.contains("msearchdb_search_duration_seconds"),
            "should contain search_duration_seconds"
        );
        assert!(
            output.contains("msearchdb_storage_bytes_used"),
            "should contain storage_bytes_used"
        );
        assert!(
            output.contains("msearchdb_raft_commit_index"),
            "should contain raft_commit_index"
        );
        assert!(
            output.contains("msearchdb_http_requests_total"),
            "should contain http_requests_total"
        );
        assert!(
            output.contains("msearchdb_http_request_duration_seconds"),
            "should contain http_request_duration_seconds"
        );

        // Verify it contains TYPE declarations (Prometheus format).
        assert!(output.contains("# TYPE msearchdb_documents_total counter"));
        assert!(output.contains("# TYPE msearchdb_search_duration_seconds histogram"));
        assert!(output.contains("# TYPE msearchdb_storage_bytes_used gauge"));
    }

    #[test]
    fn metrics_counters_increment_correctly() {
        let metrics = Metrics::new();

        metrics
            .documents_total
            .with_label_values(&["test"])
            .inc_by(5);
        let val = metrics.documents_total.with_label_values(&["test"]).get();
        assert_eq!(val, 5);

        metrics
            .index_operations_total
            .with_label_values(&["index", "test"])
            .inc_by(3);
        let val = metrics
            .index_operations_total
            .with_label_values(&["index", "test"])
            .get();
        assert_eq!(val, 3);
    }

    #[test]
    fn metrics_gauges_set_correctly() {
        let metrics = Metrics::new();

        metrics.raft_commit_index.set(100);
        assert_eq!(metrics.raft_commit_index.get(), 100);

        metrics.raft_leader_id.set(3);
        assert_eq!(metrics.raft_leader_id.get(), 3);

        metrics.storage_bytes_used.set(1_000_000);
        assert_eq!(metrics.storage_bytes_used.get(), 1_000_000);
    }

    #[test]
    fn new_metrics_gather_contains_all_new_metrics() {
        let metrics = Metrics::new();

        // Exercise the new metrics so they appear in the output.
        metrics
            .raft_commit_latency_seconds
            .with_label_values(&[])
            .observe(0.003);
        metrics
            .replication_lag
            .with_label_values(&["node-2"])
            .set(5.0);
        metrics
            .cache_hits_total
            .with_label_values(&["query"])
            .inc_by(10);
        metrics
            .cache_misses_total
            .with_label_values(&["query"])
            .inc_by(2);
        metrics.active_connections.set(42);
        metrics.collections_total.set(7);

        let output = metrics.gather();

        assert!(
            output.contains("msearchdb_raft_commit_latency_seconds"),
            "should contain raft_commit_latency_seconds"
        );
        assert!(
            output.contains("msearchdb_replication_lag"),
            "should contain replication_lag"
        );
        assert!(
            output.contains("msearchdb_cache_hits_total"),
            "should contain cache_hits_total"
        );
        assert!(
            output.contains("msearchdb_cache_misses_total"),
            "should contain cache_misses_total"
        );
        assert!(
            output.contains("msearchdb_active_connections"),
            "should contain active_connections"
        );
        assert!(
            output.contains("msearchdb_collections_total"),
            "should contain collections_total"
        );

        // Verify TYPE declarations for new metrics.
        assert!(output.contains("# TYPE msearchdb_raft_commit_latency_seconds histogram"));
        assert!(output.contains("# TYPE msearchdb_replication_lag gauge"));
        assert!(output.contains("# TYPE msearchdb_cache_hits_total counter"));
        assert!(output.contains("# TYPE msearchdb_cache_misses_total counter"));
        assert!(output.contains("# TYPE msearchdb_active_connections gauge"));
        assert!(output.contains("# TYPE msearchdb_collections_total gauge"));
    }

    #[test]
    fn new_metrics_counters_increment_correctly() {
        let metrics = Metrics::new();

        metrics
            .cache_hits_total
            .with_label_values(&["document"])
            .inc_by(15);
        assert_eq!(
            metrics
                .cache_hits_total
                .with_label_values(&["document"])
                .get(),
            15
        );

        metrics
            .cache_misses_total
            .with_label_values(&["document"])
            .inc_by(3);
        assert_eq!(
            metrics
                .cache_misses_total
                .with_label_values(&["document"])
                .get(),
            3
        );
    }

    #[test]
    fn new_metrics_gauges_set_correctly() {
        let metrics = Metrics::new();

        metrics.active_connections.set(128);
        assert_eq!(metrics.active_connections.get(), 128);

        metrics.active_connections.inc();
        assert_eq!(metrics.active_connections.get(), 129);

        metrics.active_connections.dec();
        assert_eq!(metrics.active_connections.get(), 128);

        metrics.collections_total.set(5);
        assert_eq!(metrics.collections_total.get(), 5);

        metrics
            .replication_lag
            .with_label_values(&["node-3"])
            .set(12.0);
        assert!(
            (metrics.replication_lag.with_label_values(&["node-3"]).get() - 12.0).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn raft_commit_latency_uses_custom_buckets() {
        let metrics = Metrics::new();

        // Observe a value that falls in the sub-millisecond bucket.
        metrics
            .raft_commit_latency_seconds
            .with_label_values(&[])
            .observe(0.0003);

        let output = metrics.gather();

        // The 0.0005 bucket should exist (custom bucket boundary).
        assert!(
            output.contains("msearchdb_raft_commit_latency_seconds_bucket{le=\"0.0005\"}"),
            "should contain the 0.0005 bucket boundary"
        );
        // The default 0.005 bucket should also be present.
        assert!(
            output.contains("msearchdb_raft_commit_latency_seconds_bucket{le=\"0.005\"}"),
            "should contain the 0.005 bucket boundary"
        );
    }
}
