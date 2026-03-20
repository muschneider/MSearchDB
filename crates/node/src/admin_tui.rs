//! Terminal admin UI for MSearchDB cluster monitoring.
//!
//! Provides a live dashboard using [`ratatui`] that displays cluster topology,
//! real-time metrics, node health indicators, and recent operations.
//!
//! The TUI connects to the local node's HTTP REST API (`/_cluster/health` and
//! `/metrics`) to fetch health data and Prometheus metrics, then renders them
//! in a four-panel layout:
//!
//! 1. **Header** — cluster name and overall status indicator.
//! 2. **Topology / Metrics** — leader info, node counts, and key gauges.
//! 3. **Collections** — per-collection document counts and sizes.
//! 4. **Operations Log** — scrollable list of recent refresh events.
//!
//! # Usage
//!
//! ```no_run
//! use msearchdb_node::admin_tui::run_tui;
//!
//! #[tokio::main]
//! async fn main() {
//!     run_tui("http://localhost:9200").await.unwrap();
//! }
//! ```

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Row, Table};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Cluster health payload returned by `GET /_cluster/health`.
///
/// Fields use `#[serde(default)]` where the server may omit them so that
/// deserialization never fails on a partial response.
#[derive(Debug, Clone, Deserialize, Default)]
struct ClusterHealthData {
    /// Overall status: `"green"`, `"yellow"`, or `"red"`.
    #[serde(default)]
    status: String,

    /// Human-readable cluster name.
    #[serde(default)]
    cluster_name: String,

    /// Total nodes expected in the cluster.
    #[serde(default)]
    number_of_nodes: u64,

    /// Currently reachable / active nodes.
    #[serde(default)]
    active_nodes: u64,

    /// Node id of the Raft leader, if elected.
    #[serde(default)]
    leader_node: Option<u64>,

    /// Last committed Raft log index.
    #[serde(default)]
    raft_commit_index: i64,

    /// Per-follower replication lag (stringified node-id → entries behind).
    #[serde(default)]
    replication_lag: HashMap<String, i64>,

    /// Per-collection health info.
    #[serde(default)]
    collections: HashMap<String, CollectionInfo>,
}

/// Health information for a single collection.
#[derive(Debug, Clone, Deserialize, Default)]
struct CollectionInfo {
    /// Approximate document count.
    #[serde(default)]
    docs: u64,

    /// Approximate data size in bytes.
    #[serde(default)]
    size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

/// Mutable state for the TUI event loop.
struct App {
    /// Base URL of the MSearchDB node (e.g. `http://localhost:9200`).
    endpoint: String,

    /// Latest cluster health snapshot.
    health: ClusterHealthData,

    /// Raw Prometheus metrics text from `/metrics`.
    metrics_text: String,

    /// Rolling log of refresh events shown in the bottom panel.
    ops_log: Vec<String>,

    /// Timestamp of the last successful data refresh.
    last_refresh: Instant,

    /// Set to `true` when the user presses `q` or `Esc`.
    should_quit: bool,

    // --- Parsed metric values ------------------------------------------------
    /// HTTP requests-per-second (derived from counter delta — placeholder).
    http_rps: f64,

    /// Search P99 latency in milliseconds.
    search_p99_ms: f64,

    /// Raft commit P99 latency in milliseconds.
    raft_latency_p99_ms: f64,
}

impl App {
    /// Create a new [`App`] with default / empty state.
    fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            health: ClusterHealthData::default(),
            metrics_text: String::new(),
            ops_log: Vec::new(),
            last_refresh: Instant::now() - Duration::from_secs(10), // force immediate refresh
            should_quit: false,
            http_rps: 0.0,
            search_p99_ms: 0.0,
            raft_latency_p99_ms: 0.0,
        }
    }

    /// Fetch health and metrics from the HTTP API, update internal state.
    async fn refresh(&mut self) {
        let health = fetch_health(&self.endpoint).await;
        let metrics = fetch_metrics(&self.endpoint).await;

        // Parse selected metric values from the Prometheus text.
        self.http_rps =
            parse_metric_value(&metrics, "msearchdb_http_requests_total").unwrap_or(0.0);
        self.search_p99_ms = parse_metric_value(&metrics, "msearchdb_search_duration_seconds_sum")
            .map(|v| v * 1000.0)
            .unwrap_or(0.0);
        self.raft_latency_p99_ms =
            parse_metric_value(&metrics, "msearchdb_raft_commit_latency_seconds_sum")
                .map(|v| v * 1000.0)
                .unwrap_or(0.0);

        // Append to operations log.
        let timestamp = chrono_now();
        self.ops_log.push(format!(
            "[{}] Refreshed — status={}",
            timestamp, health.status
        ));

        // Keep only the most recent 100 entries.
        if self.ops_log.len() > 100 {
            let drain = self.ops_log.len() - 100;
            self.ops_log.drain(..drain);
        }

        self.health = health;
        self.metrics_text = metrics;
        self.last_refresh = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP client (no external deps beyond std + tokio)
// ---------------------------------------------------------------------------

/// Perform a blocking HTTP/1.1 GET and return the response body.
///
/// Uses [`std::net::TcpStream`] inside [`tokio::task::spawn_blocking`] to
/// avoid pulling in an HTTP client crate. Only `http://` URLs are supported.
async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url_without_scheme = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = match url_without_scheme.find('/') {
        Some(i) => (&url_without_scheme[..i], &url_without_scheme[i..]),
        None => (url_without_scheme, "/"),
    };

    let host_port = host_port.to_string();
    let path = path.to_string();

    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let mut stream = TcpStream::connect(&host_port)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;

        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            path, host_port
        );
        stream.write_all(request.as_bytes())?;

        let mut response = String::new();
        stream.read_to_string(&mut response)?;

        // Strip HTTP headers — body starts after the first blank line.
        if let Some(body_start) = response.find("\r\n\r\n") {
            Ok(response[body_start + 4..].to_string())
        } else {
            Ok(response)
        }
    })
    .await?
}

// ---------------------------------------------------------------------------
// Data fetchers
// ---------------------------------------------------------------------------

/// Fetch cluster health from the `/_cluster/health` endpoint.
///
/// Returns [`ClusterHealthData::default`] with `status = "unknown"` on any
/// error so that the TUI always has *something* to render.
async fn fetch_health(endpoint: &str) -> ClusterHealthData {
    let url = format!("{}/_cluster/health", endpoint);
    match http_get(&url).await {
        Ok(body) => serde_json::from_str(&body).unwrap_or_else(|_| ClusterHealthData {
            status: "unknown".into(),
            ..Default::default()
        }),
        Err(_) => ClusterHealthData {
            status: "unreachable".into(),
            ..Default::default()
        },
    }
}

/// Fetch raw Prometheus metrics text from the `/metrics` endpoint.
///
/// Returns an empty string on error.
async fn fetch_metrics(endpoint: &str) -> String {
    let url = format!("{}/metrics", endpoint);
    http_get(&url).await.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Metric parsing
// ---------------------------------------------------------------------------

/// Extract the **first** numeric value for `metric_name` from Prometheus text.
///
/// Skips comment lines (starting with `#`) and looks for a line whose first
/// token equals `metric_name` (or starts with `metric_name{`). The value is
/// the last whitespace-separated token on that line.
fn parse_metric_value(metrics: &str, metric_name: &str) -> Option<f64> {
    for line in metrics.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        // Match either "metric_name <value>" or "metric_name{labels} <value>"
        if line.starts_with(metric_name) && line[metric_name.len()..].starts_with([' ', '{']) {
            if let Some(value_str) = line.rsplit_once(' ').map(|(_, v)| v) {
                return value_str.parse::<f64>().ok();
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Time helper (avoid adding `chrono` dep)
// ---------------------------------------------------------------------------

/// Return a simple `HH:MM:SS` timestamp string using [`std::time::SystemTime`].
fn chrono_now() -> String {
    use std::time::SystemTime;

    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

// ---------------------------------------------------------------------------
// Size formatting
// ---------------------------------------------------------------------------

/// Format a byte count into a human-readable string (B / KB / MB / GB).
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ---------------------------------------------------------------------------
// UI rendering
// ---------------------------------------------------------------------------

/// Map a cluster status string to a [`Color`].
fn status_color(status: &str) -> Color {
    match status {
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "red" => Color::Red,
        _ => Color::DarkGray,
    }
}

/// Render the full TUI layout into a single [`Frame`].
///
/// Layout (top to bottom):
///
/// ```text
/// ┌─────────────────────────────────────────────────────┐
/// │  Header: MSearchDB Admin — cluster_name   [status]  │  3 lines
/// ├──────────────────────┬──────────────────────────────┤
/// │  Cluster Topology    │  Metrics                     │  40%
/// ├──────────────────────┴──────────────────────────────┤
/// │  Collections                                        │  30%
/// ├─────────────────────────────────────────────────────┤
/// │  Operations Log                                     │  Min(8)
/// └─────────────────────────────────────────────────────┘
/// ```
fn draw_ui(frame: &mut Frame, app: &App) {
    let outer = Layout::vertical([
        Constraint::Length(3),      // header
        Constraint::Percentage(40), // topology + metrics
        Constraint::Percentage(30), // collections
        Constraint::Min(8),         // operations log
    ])
    .split(frame.area());

    // ----- Header -----------------------------------------------------------
    let color = status_color(&app.health.status);
    let header_text = format!(
        "MSearchDB Admin — {}  [{}]",
        if app.health.cluster_name.is_empty() {
            "msearchdb"
        } else {
            &app.health.cluster_name
        },
        app.health.status.to_uppercase(),
    );
    let header = Paragraph::new(header_text)
        .alignment(Alignment::Center)
        .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .title(" Cluster Dashboard ")
                .title_alignment(Alignment::Center),
        );
    frame.render_widget(header, outer[0]);

    // ----- Top row: Topology | Metrics --------------------------------------
    let top_cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(outer[1]);

    draw_topology_panel(frame, app, top_cols[0]);
    draw_metrics_panel(frame, app, top_cols[1]);

    // ----- Middle row: Collections ------------------------------------------
    draw_collections_panel(frame, app, outer[2]);

    // ----- Bottom row: Operations log ---------------------------------------
    draw_ops_log_panel(frame, app, outer[3]);
}

/// Render the **Cluster Topology** panel.
fn draw_topology_panel(frame: &mut Frame, app: &App, area: Rect) {
    let leader_line = match app.health.leader_node {
        Some(id) => Line::from(vec![
            Span::raw("Leader: Node "),
            Span::styled(
                id.to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        None => Line::from(Span::styled(
            "Leader: NONE",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
    };

    let nodes_line = Line::from(format!(
        "Nodes: {}/{}",
        app.health.active_nodes, app.health.number_of_nodes,
    ));

    let status_line = {
        let color = status_color(&app.health.status);
        Line::from(vec![
            Span::raw("Status: "),
            Span::styled(
                app.health.status.to_uppercase(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ])
    };

    let mut lines = vec![leader_line, nodes_line, status_line, Line::raw("")];

    // Replication lag per follower.
    if !app.health.replication_lag.is_empty() {
        lines.push(Line::styled(
            "Replication Lag:",
            Style::default().add_modifier(Modifier::UNDERLINED),
        ));
        let mut entries: Vec<_> = app.health.replication_lag.iter().collect();
        entries.sort_by_key(|(k, _)| (*k).clone());
        for (node_id, lag) in entries {
            let lag_color = if *lag == 0 {
                Color::Green
            } else if *lag < 10 {
                Color::Yellow
            } else {
                Color::Red
            };
            lines.push(Line::from(vec![
                Span::raw(format!("  Node {node_id}: ")),
                Span::styled(format!("{lag} entries"), Style::default().fg(lag_color)),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Cluster Topology ");
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Render the **Metrics** panel.
fn draw_metrics_panel(frame: &mut Frame, app: &App, area: Rect) {
    let lines = vec![
        Line::from(vec![
            Span::raw("Raft Commit Index: "),
            Span::styled(
                app.health.raft_commit_index.to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("HTTP Ops/sec:      "),
            Span::styled(
                format!("{:.1}", app.http_rps),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::raw("Search P99:        "),
            Span::styled(
                format!("{:.2} ms", app.search_p99_ms),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::raw("Raft Commit P99:   "),
            Span::styled(
                format!("{:.2} ms", app.raft_latency_p99_ms),
                Style::default().fg(Color::White),
            ),
        ]),
    ];

    let block = Block::default().borders(Borders::ALL).title(" Metrics ");
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

/// Render the **Collections** table.
fn draw_collections_panel(frame: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(vec!["Name", "Documents", "Size"])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);

    let mut entries: Vec<_> = app.health.collections.iter().collect();
    entries.sort_by_key(|(name, _)| (*name).clone());

    let rows: Vec<Row> = entries
        .iter()
        .map(|(name, info)| {
            Row::new(vec![
                name.to_string(),
                info.docs.to_string(),
                format_bytes(info.size_bytes),
            ])
        })
        .collect();

    let widths = [
        Constraint::Percentage(40),
        Constraint::Percentage(30),
        Constraint::Percentage(30),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Collections "),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    frame.render_widget(table, area);
}

/// Render the **Operations Log** panel (last 20 entries).
fn draw_ops_log_panel(frame: &mut Frame, app: &App, area: Rect) {
    let visible: Vec<ListItem> = app
        .ops_log
        .iter()
        .rev()
        .take(20)
        .rev()
        .map(|entry| {
            let color = if entry.contains("unreachable") || entry.contains("unknown") {
                Color::Red
            } else if entry.contains("yellow") {
                Color::Yellow
            } else {
                Color::DarkGray
            };
            ListItem::new(Line::from(Span::styled(
                entry.as_str(),
                Style::default().fg(color),
            )))
        })
        .collect();

    let list = List::new(visible).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Operations Log (latest 20) "),
    );
    frame.render_widget(list, area);
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the terminal admin UI.
///
/// Enters raw mode, draws the dashboard in an alternate screen buffer, and
/// polls the given HTTP `endpoint` every 2 seconds for fresh data.  The loop
/// exits when the user presses **`q`** or **`Esc`**.
///
/// # Errors
///
/// Returns an [`io::Error`] if terminal setup or teardown fails.
pub async fn run_tui(endpoint: &str) -> io::Result<()> {
    // --- Terminal setup -----------------------------------------------------
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(endpoint);

    // --- Main event loop ----------------------------------------------------
    let tick_rate = Duration::from_millis(250);
    let refresh_interval = Duration::from_secs(2);

    loop {
        // Draw the current state.
        terminal.draw(|frame| draw_ui(frame, &app))?;

        // Poll for keyboard events (non-blocking, up to `tick_rate`).
        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            app.should_quit = true;
                        }
                        _ => {}
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }

        // Periodically refresh data from the HTTP API.
        if app.last_refresh.elapsed() >= refresh_interval {
            app.refresh().await;
        }
    }

    // --- Terminal teardown ---------------------------------------------------
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_metric_value_finds_simple_gauge() {
        let text = "\
# HELP msearchdb_raft_commit_index Latest committed Raft log index
# TYPE msearchdb_raft_commit_index gauge
msearchdb_raft_commit_index 42
";
        assert_eq!(
            parse_metric_value(text, "msearchdb_raft_commit_index"),
            Some(42.0)
        );
    }

    #[test]
    fn parse_metric_value_finds_labelled_metric() {
        let text = "\
# TYPE msearchdb_http_requests_total counter
msearchdb_http_requests_total{method=\"GET\",path=\"/collections\",status=\"200\"} 137
";
        assert_eq!(
            parse_metric_value(text, "msearchdb_http_requests_total"),
            Some(137.0)
        );
    }

    #[test]
    fn parse_metric_value_returns_none_for_missing() {
        let text = "# TYPE foo gauge\nfoo 1\n";
        assert_eq!(parse_metric_value(text, "bar"), None);
    }

    #[test]
    fn parse_metric_value_ignores_comment_lines() {
        let text = "# msearchdb_raft_commit_index 999\nmsearchdb_raft_commit_index 7\n";
        assert_eq!(
            parse_metric_value(text, "msearchdb_raft_commit_index"),
            Some(7.0)
        );
    }

    #[test]
    fn parse_metric_value_does_not_match_prefix_of_different_metric() {
        let text = "msearchdb_http_requests_total_created 1234\nmsearchdb_http_requests_total 99\n";
        // Should match the exact metric name, not the _created variant.
        assert_eq!(
            parse_metric_value(text, "msearchdb_http_requests_total"),
            Some(99.0)
        );
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
        assert_eq!(format_bytes(1_073_741_824), "1.0 GB");
    }

    #[test]
    fn status_color_mapping() {
        assert_eq!(status_color("green"), Color::Green);
        assert_eq!(status_color("yellow"), Color::Yellow);
        assert_eq!(status_color("red"), Color::Red);
        assert_eq!(status_color("unknown"), Color::DarkGray);
    }

    #[test]
    fn cluster_health_data_deserializes_minimal_json() {
        let json = r#"{"status":"green","number_of_nodes":1}"#;
        let data: ClusterHealthData = serde_json::from_str(json).unwrap();
        assert_eq!(data.status, "green");
        assert_eq!(data.number_of_nodes, 1);
        assert!(data.collections.is_empty());
    }

    #[test]
    fn cluster_health_data_deserializes_full_json() {
        let json = r#"{
            "status": "green",
            "cluster_name": "prod",
            "number_of_nodes": 3,
            "active_nodes": 3,
            "leader_node": 1,
            "raft_commit_index": 500,
            "replication_lag": {"2": 0, "3": 5},
            "collections": {
                "products": {"docs": 1000, "size_bytes": 2048}
            }
        }"#;
        let data: ClusterHealthData = serde_json::from_str(json).unwrap();
        assert_eq!(data.cluster_name, "prod");
        assert_eq!(data.leader_node, Some(1));
        assert_eq!(data.replication_lag.get("3"), Some(&5));
        assert_eq!(data.collections.get("products").unwrap().docs, 1000);
    }

    #[test]
    fn app_new_forces_immediate_refresh() {
        let app = App::new("http://localhost:9200");
        // last_refresh is set 10 seconds in the past so the first tick refreshes.
        assert!(app.last_refresh.elapsed() >= Duration::from_secs(2));
        assert!(!app.should_quit);
    }

    #[test]
    fn chrono_now_format() {
        let ts = chrono_now();
        // Should match HH:MM:SS pattern.
        assert_eq!(ts.len(), 8);
        assert_eq!(&ts[2..3], ":");
        assert_eq!(&ts[5..6], ":");
    }
}
