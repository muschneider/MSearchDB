//! Structured logging and observability setup for MSearchDB.
//!
//! This module configures [`tracing_subscriber`] with two output modes:
//!
//! - **JSON** (production): structured JSON lines suitable for log aggregation
//!   (ELK, Loki, Datadog).
//! - **Pretty** (development): human-readable coloured output for local
//!   development.
//!
//! An optional file appender with daily rotation writes logs to the data
//! directory alongside console output.
//!
//! When an OTLP endpoint is provided, an [`OpenTelemetryLayer`] is added to
//! the subscriber stack so that spans are exported as distributed traces to
//! any OpenTelemetry-compatible collector (Jaeger, Tempo, etc.).
//!
//! # Examples
//!
//! ```no_run
//! use msearchdb_node::observability::{init_tracing, init_tracing_with_otel, shutdown_otel};
//!
//! // Development mode (pretty, info level)
//! let _guard = init_tracing("info", false, None);
//!
//! // Production mode (JSON, debug level, file logging to /var/log/msearchdb)
//! let _guard = init_tracing("debug", true, Some("/var/log/msearchdb"));
//!
//! // Production mode with OTLP export
//! let (_guard, provider) = init_tracing_with_otel(
//!     "info",
//!     true,
//!     None,
//!     Some("http://localhost:4317"),
//!     "msearchdb-node-1",
//! );
//!
//! // On shutdown:
//! shutdown_otel(provider);
//! ```

use opentelemetry::trace::TracerProvider;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::TracerProvider as SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------------------
// OpenTelemetry tracer provider
// ---------------------------------------------------------------------------

/// Create an OpenTelemetry [`SdkTracerProvider`] that exports spans over OTLP/gRPC.
///
/// # Arguments
///
/// * `endpoint` — OTLP collector endpoint (e.g. `"http://localhost:4317"`).
/// * `service_name` — the `service.name` resource attribute attached to every span.
///
/// # Panics
///
/// Panics if the OTLP exporter cannot be constructed (e.g. invalid endpoint).
fn init_otel_tracer(endpoint: &str, service_name: &str) -> SdkTracerProvider {
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .expect("failed to create OTLP span exporter");

    let resource = Resource::new(vec![KeyValue::new(
        "service.name",
        service_name.to_string(),
    )]);

    SdkTracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(resource)
        .build()
}

// ---------------------------------------------------------------------------
// Tracing initialisation (without OpenTelemetry)
// ---------------------------------------------------------------------------

/// Initialise the global tracing subscriber.
///
/// # Arguments
///
/// * `log_level` — filter string passed to [`EnvFilter`] (e.g. `"info"`,
///   `"debug"`, `"msearchdb=trace,tower=warn"`).
/// * `json_format` — if `true`, emit structured JSON lines; otherwise use
///   the human-readable pretty formatter.
/// * `log_dir` — optional directory for a daily-rotating log file.  When
///   `Some`, a non-blocking file appender is created alongside the console
///   layer.
///
/// # Returns
///
/// An optional [`WorkerGuard`] that **must** be held alive for the lifetime
/// of the process.  Dropping the guard flushes buffered log entries.  If no
/// file appender is configured, `None` is returned.
///
/// # Panics
///
/// Panics if the global subscriber has already been set.
pub fn init_tracing(
    log_level: &str,
    json_format: bool,
    log_dir: Option<&str>,
) -> Option<WorkerGuard> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    match (json_format, log_dir) {
        // JSON + file appender
        (true, Some(dir)) => {
            let file_appender = tracing_appender::rolling::daily(dir, "msearchdb.log");
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_thread_ids(true)
                        .with_file(true)
                        .with_line_number(true),
                )
                .with(
                    fmt::layer()
                        .json()
                        .with_writer(non_blocking)
                        .with_target(true)
                        .with_ansi(false),
                )
                .init();

            Some(guard)
        }
        // JSON without file appender
        (true, None) => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_thread_ids(true)
                        .with_file(true)
                        .with_line_number(true),
                )
                .init();

            None
        }
        // Pretty + file appender
        (false, Some(dir)) => {
            let file_appender = tracing_appender::rolling::daily(dir, "msearchdb.log");
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    fmt::layer()
                        .with_target(true)
                        .with_thread_ids(true)
                        .with_file(true)
                        .with_line_number(true),
                )
                .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
                .init();

            Some(guard)
        }
        // Pretty without file appender (default for development)
        (false, None) => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    fmt::layer()
                        .with_target(true)
                        .with_thread_ids(true)
                        .with_file(true)
                        .with_line_number(true),
                )
                .init();

            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tracing initialisation (with OpenTelemetry)
// ---------------------------------------------------------------------------

/// Initialise the global tracing subscriber with optional OpenTelemetry export.
///
/// This is the preferred entry point for production deployments.  When
/// `otlp_endpoint` is [`Some`], an [`OpenTelemetryLayer`] is composed into
/// the subscriber stack so that every [`tracing::Span`] is also exported as
/// an OpenTelemetry span to the configured OTLP collector.
///
/// When `otlp_endpoint` is [`None`], the behaviour is identical to
/// [`init_tracing`].
///
/// # Arguments
///
/// * `log_level` — filter string passed to [`EnvFilter`].
/// * `json_format` — if `true`, emit structured JSON lines.
/// * `log_dir` — optional directory for a daily-rotating log file.
/// * `otlp_endpoint` — optional OTLP/gRPC collector endpoint
///   (e.g. `"http://localhost:4317"`).
/// * `service_name` — the `service.name` resource attribute attached to
///   exported spans.
///
/// # Returns
///
/// A tuple of:
/// - An optional [`WorkerGuard`] for the file appender (must be held alive).
/// - An optional [`SdkTracerProvider`] for graceful shutdown via
///   [`shutdown_otel`].
///
/// # Panics
///
/// Panics if the global subscriber has already been set or if the OTLP
/// exporter cannot be created.
pub fn init_tracing_with_otel(
    log_level: &str,
    json_format: bool,
    log_dir: Option<&str>,
    otlp_endpoint: Option<&str>,
    service_name: &str,
) -> (Option<WorkerGuard>, Option<SdkTracerProvider>) {
    // If no OTLP endpoint, fall back to the regular init.
    let Some(endpoint) = otlp_endpoint else {
        let guard = init_tracing(log_level, json_format, log_dir);
        return (guard, None);
    };

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    let provider = init_otel_tracer(endpoint, service_name);
    let tracer = provider.tracer("msearchdb");
    let otel_layer = OpenTelemetryLayer::new(tracer);

    match (json_format, log_dir) {
        // JSON + file appender + OTel
        (true, Some(dir)) => {
            let file_appender = tracing_appender::rolling::daily(dir, "msearchdb.log");
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(otel_layer)
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_thread_ids(true)
                        .with_file(true)
                        .with_line_number(true),
                )
                .with(
                    fmt::layer()
                        .json()
                        .with_writer(non_blocking)
                        .with_target(true)
                        .with_ansi(false),
                )
                .init();

            (Some(guard), Some(provider))
        }
        // JSON + OTel (no file)
        (true, None) => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(otel_layer)
                .with(
                    fmt::layer()
                        .json()
                        .with_target(true)
                        .with_thread_ids(true)
                        .with_file(true)
                        .with_line_number(true),
                )
                .init();

            (None, Some(provider))
        }
        // Pretty + file appender + OTel
        (false, Some(dir)) => {
            let file_appender = tracing_appender::rolling::daily(dir, "msearchdb.log");
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(otel_layer)
                .with(
                    fmt::layer()
                        .with_target(true)
                        .with_thread_ids(true)
                        .with_file(true)
                        .with_line_number(true),
                )
                .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
                .init();

            (Some(guard), Some(provider))
        }
        // Pretty + OTel (no file, development with collector)
        (false, None) => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(otel_layer)
                .with(
                    fmt::layer()
                        .with_target(true)
                        .with_thread_ids(true)
                        .with_file(true)
                        .with_line_number(true),
                )
                .init();

            (None, Some(provider))
        }
    }
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

/// Gracefully shut down the OpenTelemetry tracer provider.
///
/// This flushes any buffered spans to the collector.  Should be called during
/// application shutdown before the process exits.
///
/// If `provider` is `None`, this is a no-op.
pub fn shutdown_otel(provider: Option<SdkTracerProvider>) {
    if let Some(provider) = provider {
        if let Err(e) = provider.shutdown() {
            tracing::error!(error = %e, "failed to shutdown OpenTelemetry provider");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // Note: We cannot test `init_tracing` in unit tests because it calls
    // `tracing_subscriber::registry().init()` which sets a global subscriber.
    // Multiple tests calling it would panic. Integration tests should verify
    // the logging output.

    #[test]
    fn env_filter_parses_valid_levels() {
        use tracing_subscriber::EnvFilter;

        for level in &["error", "warn", "info", "debug", "trace"] {
            let filter = EnvFilter::new(level);
            // Just verify it doesn't panic.
            let _ = format!("{:?}", filter);
        }
    }

    #[test]
    fn env_filter_parses_compound_directives() {
        use tracing_subscriber::EnvFilter;

        let filter = EnvFilter::new("msearchdb=debug,tower=warn,hyper=info");
        let _ = format!("{:?}", filter);
    }
}
