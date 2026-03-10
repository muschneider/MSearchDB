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
//! # Examples
//!
//! ```no_run
//! use msearchdb_node::observability::init_tracing;
//!
//! // Development mode (pretty, info level)
//! let _guard = init_tracing("info", false, None);
//!
//! // Production mode (JSON, debug level, file logging to /var/log/msearchdb)
//! let _guard = init_tracing("debug", true, Some("/var/log/msearchdb"));
//! ```

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------------------
// Tracing initialisation
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
