use std::io;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use hs_common::global_args::{GlobalArgs, OutputFormat};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Padding, Row, Table};

// ── Data model ──────────────────────────────────────────────────

struct DashboardData {
    /// None = still scanning, Some(count, bytes) = done.
    doc_counts: Option<(u64, u64)>,
    markdown_counts: Option<(u64, u64)>,
    catalog_count: Option<u64>,
    corrupted_count: Option<u64>,
    embedded_docs: u64,
    embedded_chunks: u64,
    /// Catalog rows stamped `embedding_skip` (zero-chunk / junk-HTML).
    /// Excluded from the Embedded-% denominator so the bar reflects
    /// embeddable docs, not intentional skips.
    embedding_skipped: u64,

    scribe_servers: Vec<ServiceStatus>,
    distill_servers: Vec<ServiceStatus>,
    qdrant_healthy: bool,
    qdrant_url: String,
    qdrant_version: String,

    watcher: WatcherInfo,
    indexer: IndexerInfo,

    history: Vec<HistoryEvent>,

    /// True before the first data collection completes.
    loading: bool,
}

/// Status of the inbox-sweeper daemon (`hs scribe inbox` cmd_run). Derived
/// from a heartbeat the daemon writes each sweep tick to
/// `hs_common::status::INBOX_HEARTBEAT_KEY`; the MCP server classifies
/// freshness so every CLI host renders the same verdict.
enum WatcherInfo {
    Stopped,
    Running {
        host: String,
        last_tick_seconds_ago: u64,
    },
}

impl From<Option<hs_common::status::InboxHeartbeatSnapshot>> for WatcherInfo {
    fn from(hb: Option<hs_common::status::InboxHeartbeatSnapshot>) -> Self {
        match hb {
            Some(hb) if hb.running => WatcherInfo::Running {
                host: hb.host,
                last_tick_seconds_ago: hb.last_tick_seconds_ago,
            },
            _ => WatcherInfo::Stopped,
        }
    }
}

enum IndexerInfo {
    /// Indexer daemon is actively running
    Running {
        indexed: u64,
        total: u64,
        failed: u64,
        chunks: u64,
        current_file: String,
    },
    /// Indexer finished all files
    Finished { indexed: u64, chunks: u64 },
    /// Not running
    Stopped,
}

struct ServiceStatus {
    url: String,
    healthy: bool,
    detail: String,   // e.g. "(Cpu)" or compute device
    activity: String, // e.g. "idle", "3 converting", "1 embedding"
    version: String,  // server version from /health
}

struct HistoryEvent {
    activity: &'static str, // "Downloaded", "Converted", "Embedded"
    name: String,
    detail: String, // e.g. "12pg 193s" or "27 chunks" or "1.2 MB"
    when: Option<chrono::DateTime<chrono::Utc>>,
}

// ── Data collection ─────────────────────────────────────────────

async fn collect_data() -> DashboardData {
    // Single source of truth: the MCP gateway's `system_status` tool, whose
    // counts come from the Storage trait and therefore work for both LocalFs
    // and S3/Garage backends. On MCP failure we return a blank dashboard —
    // zeros are accurate ("we don't know yet") rather than confidently wrong.
    match collect_data_via_mcp().await {
        Ok(data) => data,
        Err(_) => DashboardData {
            doc_counts: None,
            markdown_counts: None,
            catalog_count: None,
            corrupted_count: None,
            embedded_docs: 0,
            embedded_chunks: 0,
            embedding_skipped: 0,
            scribe_servers: Vec::new(),
            distill_servers: Vec::new(),
            qdrant_healthy: false,
            qdrant_url: String::new(),
            qdrant_version: String::new(),
            watcher: WatcherInfo::Stopped,
            indexer: read_indexer_status(),
            history: Vec::new(),
            loading: false,
        },
    }
}

/// Populate the dashboard from the MCP `system_status` tool — the single
/// source of truth for pipeline counts and service health. Byte counts stay
/// at 0 (system_status reports object counts, not sizes). The `indexer` row
/// reflects the distill-index daemon on the CLI host, not the remote gateway.
async fn collect_data_via_mcp() -> anyhow::Result<DashboardData> {
    use serde_json::Value;

    let client = crate::mcp_client::McpClient::from_default_creds().await?;
    let status_json = client
        .call_tool("system_status", Value::Object(Default::default()))
        .await?;
    let snap: hs_common::status::StatusSnapshot = serde_json::from_value(status_json)?;

    let mut data = snapshot_to_dashboard(snap);
    data.indexer = read_indexer_status();
    Ok(data)
}

/// Map the shared StatusSnapshot into the CLI's TUI-local DashboardData.
/// The caller is responsible for overlaying local-daemon fields
/// (`watcher`, `indexer`) since those reflect the CLI host, not the remote
/// gateway.
fn snapshot_to_dashboard(snap: hs_common::status::StatusSnapshot) -> DashboardData {
    let scribe_servers = snap
        .scribe_instances
        .iter()
        .map(instance_to_status)
        .collect();
    let distill_servers: Vec<ServiceStatus> = snap
        .distill_instances
        .iter()
        .map(instance_to_status)
        .collect();

    let qdrant_healthy = snap.qdrant.is_some();
    let qdrant_url = snap
        .qdrant
        .as_ref()
        .map(|q| {
            // Prefer the real Qdrant endpoint reported by distill's /health;
            // fall back to "gateway" only for older distill servers that
            // don't yet surface qdrant_url (backward-compat via serde default).
            let url_part = if q.qdrant_url.is_empty() {
                "gateway".to_string()
            } else {
                q.qdrant_url.clone()
            };
            format!("{url_part} → {}", q.collection)
        })
        .unwrap_or_default();
    let qdrant_version = snap
        .qdrant
        .as_ref()
        .map(|q| q.qdrant_version.clone())
        .unwrap_or_default();

    let history = snap
        .history
        .into_iter()
        .filter_map(|ev| {
            let activity = match ev.activity.as_str() {
                "Download" => "Download",
                "Convert" => "Convert",
                "Embed" => "Embed",
                _ => return None,
            };
            let when = chrono::DateTime::parse_from_rfc3339(&ev.at)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc));
            // Re-derive human detail from structured fields so the CLI can
            // format byte counts (fmt_bytes) and rich metadata consistently.
            let detail = match activity {
                "Download" => ev.detail_bytes.map(fmt_bytes).unwrap_or(ev.detail),
                _ => ev.detail,
            };
            Some(HistoryEvent {
                activity,
                name: ev.name,
                detail,
                when,
            })
        })
        .collect();

    DashboardData {
        doc_counts: Some((snap.pipeline.documents, 0)),
        markdown_counts: Some((snap.pipeline.markdown, 0)),
        catalog_count: Some(snap.pipeline.catalog_entries),
        corrupted_count: None,
        embedded_docs: snap.pipeline.embedded_documents.unwrap_or(0),
        embedded_chunks: snap.pipeline.embedded_chunks.unwrap_or(0),
        embedding_skipped: snap.pipeline.embedding_skipped.unwrap_or(0),
        scribe_servers,
        distill_servers,
        qdrant_healthy,
        qdrant_url,
        qdrant_version,
        watcher: WatcherInfo::from(snap.inbox_heartbeat),
        indexer: IndexerInfo::Stopped,
        history,
        loading: false,
    }
}

fn instance_to_status(inst: &hs_common::status::ServiceInstance) -> ServiceStatus {
    let detail = if inst.compute_device.is_empty() {
        String::new()
    } else {
        format!("({})", inst.compute_device)
    };
    ServiceStatus {
        url: inst.url.clone(),
        healthy: inst.healthy,
        detail,
        activity: inst.activity.clone(),
        version: inst.version.clone(),
    }
}

fn read_indexer_status() -> IndexerInfo {
    let status = match crate::distill_cmd::read_index_status() {
        Some(s) => s,
        None => return IndexerInfo::Stopped,
    };

    // Cross-host liveness: trust the status file's mtime over local PID check.
    // The indexer updates the status file frequently while running.
    let status_path = crate::distill_cmd::index_status_path();
    let status_is_fresh = std::fs::metadata(&status_path)
        .and_then(|m| m.modified())
        .map(|t| {
            std::time::SystemTime::now()
                .duration_since(t)
                .map(|d| d.as_secs() < 30)
                .unwrap_or(false)
        })
        .unwrap_or(false);

    let is_alive =
        status_is_fresh || (status.pid > 0 && crate::daemon::is_process_alive(status.pid));

    if !is_alive {
        if status.done {
            return IndexerInfo::Finished {
                indexed: status.indexed as u64,
                chunks: status.total_chunks as u64,
            };
        }
        return IndexerInfo::Stopped;
    }

    if status.done {
        return IndexerInfo::Finished {
            indexed: status.indexed as u64,
            chunks: status.total_chunks as u64,
        };
    }

    IndexerInfo::Running {
        indexed: status.indexed as u64,
        total: status.total_files as u64,
        failed: status.failed as u64,
        chunks: status.total_chunks as u64,
        current_file: status.current_file,
    }
}

// ── Formatting helpers ──────────────────────────────────────────

fn fmt_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.0} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.0} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

fn fmt_ago(dt: &chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let secs = (now - *dt).num_seconds();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

// ── TUI rendering ───────────────────────────────────────────────

fn render(frame: &mut Frame, data: &DashboardData) {
    let outer = Layout::vertical([
        Constraint::Length(1), // title
        Constraint::Length(9), // pipeline (includes watcher row)
        Constraint::Length(1), // spacer
        Constraint::Length((data.scribe_servers.len() + data.distill_servers.len() + 3) as u16), // services
        Constraint::Length(1), // spacer
        Constraint::Min(4),    // recent
        Constraint::Length(1), // footer
    ])
    .split(frame.area());

    // Title
    frame.render_widget(
        Line::from(format!(" home-still {} ", env!("HS_VERSION")))
            .bold()
            .centered(),
        outer[0],
    );

    // Pipeline section
    render_pipeline(frame, outer[1], data);

    // Services section
    render_services(frame, outer[3], data);

    // Recent conversions
    render_history(frame, outer[5], data);

    // Footer
    frame.render_widget(
        Line::from(vec![
            " q ".bold().reversed(),
            " quit   ".into(),
            "refresh: 3s".dim(),
        ]),
        outer[6],
    );
}

fn render_pipeline(frame: &mut Frame, area: Rect, data: &DashboardData) {
    let block = Block::new()
        .title(Line::from(" Pipeline "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if data.loading {
        frame.render_widget(Line::from("  Loading...").dim(), inner);
        return;
    }

    let doc_count = data.doc_counts.map(|(c, _)| c).unwrap_or(0);
    let doc_bytes = data.doc_counts.map(|(_, b)| b).unwrap_or(0);
    let markdown_count = data.markdown_counts.map(|(c, _)| c).unwrap_or(0);
    let markdown_bytes = data.markdown_counts.map(|(_, b)| b).unwrap_or(0);
    let catalog_count = data.catalog_count.unwrap_or(0);
    let corrupted_count = data.corrupted_count.unwrap_or(0);

    let convertible = doc_count.saturating_sub(corrupted_count);
    let pdf_to_md = if convertible > 0 {
        (markdown_count as f64 / convertible as f64).min(1.0)
    } else {
        0.0
    };
    // Progress = embedded / embeddable, where embeddable excludes rows
    // stamped `embedding_skip` (zero-chunk HTML stubs etc.). Those docs
    // count toward markdown but will never be embedded, so including
    // them in the denominator makes the bar permanently under 100%.
    let embeddable = markdown_count.saturating_sub(data.embedding_skipped);
    let md_to_embed = if embeddable > 0 {
        (data.embedded_docs as f64 / embeddable as f64).min(1.0)
    } else {
        0.0
    };

    // Helper: show "Scanning..." when None, count when Some.
    let scanning = "  ...".to_string();

    let rows = vec![
        Row::new(vec![
            Cell::from("Documents"),
            Cell::from(if data.doc_counts.is_some() {
                format!("{:>6}", doc_count)
            } else {
                scanning.clone()
            }),
            Cell::from(if data.doc_counts.is_some() {
                format!("{:>8}", fmt_bytes(doc_bytes))
            } else {
                String::new()
            }),
            Cell::from(""),
        ]),
        Row::new(vec![
            Cell::from("Markdown"),
            Cell::from(if data.markdown_counts.is_some() {
                format!("{:>6}", markdown_count)
            } else {
                scanning.clone()
            }),
            Cell::from(if data.markdown_counts.is_some() {
                format!("{:>8}", fmt_bytes(markdown_bytes))
            } else {
                String::new()
            }),
            Cell::from(if data.markdown_counts.is_some() {
                format!("{:>5.1}%", pdf_to_md * 100.0)
            } else {
                String::new()
            }),
        ]),
        Row::new(vec![
            Cell::from("Cataloged"),
            Cell::from(if data.catalog_count.is_some() {
                format!("{:>6}", catalog_count)
            } else {
                scanning.clone()
            }),
            Cell::from(""),
            Cell::from(""),
        ]),
        Row::new(vec![
            Cell::from("Embedded"),
            Cell::from(format!("{:>6}", data.embedded_docs)).style(if data.embedded_docs > 0 {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            }),
            Cell::from(if data.embedding_skipped > 0 {
                format!(
                    "{:>5} chunks · {} skipped",
                    data.embedded_chunks, data.embedding_skipped
                )
            } else {
                format!("{:>5} chunks", data.embedded_chunks)
            }),
            Cell::from(format!("{:>5.1}%", md_to_embed * 100.0)),
        ]),
        Row::new(vec![
            Cell::from("Corrupted PDFs").style(Style::default().fg(Color::Red)),
            Cell::from(if data.corrupted_count.is_some() {
                format!("{:>6}", corrupted_count)
            } else {
                scanning.clone()
            })
            .style(Style::default().fg(Color::Red)),
            Cell::from(""),
            Cell::from(""),
        ]),
        match &data.watcher {
            WatcherInfo::Stopped => Row::new(vec![
                Cell::from("Watcher").style(Style::default().fg(Color::DarkGray)),
                Cell::from("○".to_string()).style(Style::default().fg(Color::DarkGray)),
                Cell::from("stopped"),
                Cell::from(""),
            ]),
            WatcherInfo::Running {
                host,
                last_tick_seconds_ago,
            } => Row::new(vec![
                Cell::from("Watcher").style(Style::default().fg(Color::Green)),
                Cell::from("●".to_string()).style(Style::default().fg(Color::Green)),
                Cell::from(format!("running · {host}")),
                Cell::from(format!("last tick {last_tick_seconds_ago}s ago")),
            ]),
        },
        match &data.indexer {
            IndexerInfo::Running {
                indexed,
                total,
                failed,
                chunks,
                current_file,
            } => {
                let color = Color::Green;
                let pct = if *total > 0 {
                    (*indexed as f64 / *total as f64 * 100.0) as u64
                } else {
                    0
                };
                let file_short = if current_file.len() > 30 {
                    format!("{}...", &current_file[..27])
                } else {
                    current_file.clone()
                };
                let fail_str = if *failed > 0 {
                    format!(" · {failed} failed")
                } else {
                    String::new()
                };
                let detail = format!(
                    "{indexed}/{total} ({pct}%) · {chunks} chunks{fail_str} · {file_short}"
                );
                Row::new(vec![
                    Cell::from("Indexer").style(Style::default().fg(color)),
                    Cell::from("●".to_string()).style(Style::default().fg(color)),
                    Cell::from("indexing"),
                    Cell::from(detail),
                ])
            }
            IndexerInfo::Finished { indexed, chunks } => Row::new(vec![
                Cell::from("Indexer").style(Style::default().fg(Color::Green)),
                Cell::from("●".to_string()).style(Style::default().fg(Color::Green)),
                Cell::from("done"),
                Cell::from(format!("{indexed} indexed · {chunks} chunks")),
            ]),
            IndexerInfo::Stopped => Row::new(vec![
                Cell::from("Indexer").style(Style::default().fg(Color::DarkGray)),
                Cell::from("○".to_string()).style(Style::default().fg(Color::DarkGray)),
                Cell::from("stopped"),
                Cell::from(""),
            ]),
        },
    ];

    let table = Table::new(
        rows,
        [
            Constraint::Length(16), // Label
            Constraint::Length(8),  // Count
            Constraint::Length(14), // Size
            Constraint::Min(8),     // Progress
        ],
    )
    .header(
        Row::new(["", "Count", "Size", "Progress"])
            .style(Style::default().bold().fg(Color::DarkGray)),
    );

    frame.render_widget(table, inner);
}

fn render_services(frame: &mut Frame, area: Rect, data: &DashboardData) {
    let block = Block::new()
        .title(" Services ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if data.loading {
        frame.render_widget(Line::from("  Loading...").dim(), inner);
        return;
    }

    let mut rows = Vec::new();

    for svc in &data.scribe_servers {
        let (indicator, ind_style) = if svc.healthy {
            ("●", Style::default().fg(Color::Green))
        } else {
            ("○", Style::default().fg(Color::Red))
        };
        let status_text = format_status_activity(svc.healthy, "running", &svc.activity);
        rows.push(Row::new(vec![
            Cell::from("Scribe"),
            Cell::from(indicator).style(ind_style),
            Cell::from(status_text),
            Cell::from(svc.url.clone()),
            Cell::from(svc.detail.clone()).style(Style::default().fg(Color::DarkGray)),
            Cell::from(svc.version.clone()).style(Style::default().fg(Color::DarkGray)),
        ]));
    }

    for svc in &data.distill_servers {
        let (indicator, ind_style) = if svc.healthy {
            ("●", Style::default().fg(Color::Green))
        } else {
            ("○", Style::default().fg(Color::Red))
        };
        let status_text = format_status_activity(svc.healthy, "running", &svc.activity);
        rows.push(Row::new(vec![
            Cell::from("Distill"),
            Cell::from(indicator).style(ind_style),
            Cell::from(status_text),
            Cell::from(svc.url.clone()),
            Cell::from(svc.detail.clone()).style(Style::default().fg(Color::DarkGray)),
            Cell::from(svc.version.clone()).style(Style::default().fg(Color::DarkGray)),
        ]));
    }

    let (q_indicator, q_style) = if data.qdrant_healthy {
        ("●", Style::default().fg(Color::Green))
    } else {
        ("○", Style::default().fg(Color::Red))
    };
    let q_status = if data.qdrant_healthy {
        "healthy"
    } else {
        "stopped"
    };
    // Split qdrant_url "http://…:7434 → collection" into url and detail
    let (q_url, q_detail) = match data.qdrant_url.split_once(" → ") {
        Some((u, c)) => (u.to_string(), format!("→ {c}")),
        None => (data.qdrant_url.clone(), String::new()),
    };
    rows.push(Row::new(vec![
        Cell::from("Qdrant"),
        Cell::from(q_indicator).style(q_style),
        Cell::from(q_status.to_string()),
        Cell::from(q_url),
        Cell::from(q_detail).style(Style::default().fg(Color::DarkGray)),
        Cell::from(data.qdrant_version.clone()).style(Style::default().fg(Color::DarkGray)),
    ]));

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),  // Name
            Constraint::Length(2),  // Indicator
            Constraint::Length(18), // Status + Activity
            Constraint::Fill(3),    // URL — gets 3/5 of remaining
            Constraint::Fill(2),    // Detail — gets 2/5 of remaining
            Constraint::Length(16), // Version
        ],
    );

    frame.render_widget(table, inner);
}

fn format_status_activity(healthy: bool, running_label: &str, activity: &str) -> String {
    if !healthy {
        return "stopped".into();
    }
    match activity {
        "" | "idle" => running_label.into(),
        other => format!("{running_label} · {other}"),
    }
}

fn render_history(frame: &mut Frame, area: Rect, data: &DashboardData) {
    let block = Block::new()
        .title(Line::from(" History "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if data.loading {
        frame.render_widget(Line::from("  Loading...").dim(), inner);
        return;
    }
    if data.history.is_empty() {
        frame.render_widget(Line::from("  No activity yet").dim(), inner);
        return;
    }

    let rows: Vec<Row> = data
        .history
        .iter()
        .map(|e| {
            let activity_style = match e.activity {
                "Download" => Style::default().fg(Color::Cyan),
                "Convert" => Style::default().fg(Color::Yellow),
                "Embed" => Style::default().fg(Color::Green),
                _ => Style::default(),
            };
            let ago = e.when.as_ref().map(fmt_ago).unwrap_or_default();
            Row::new(vec![
                Cell::from(e.activity).style(activity_style),
                Cell::from(e.name.clone()),
                Cell::from(e.detail.clone()).style(Style::default().fg(Color::DarkGray)),
                Cell::from(ago),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(9), // Activity
            Constraint::Fill(4),   // Name — gets 4/5 of remaining
            Constraint::Fill(1),   // Detail — gets 1/5 of remaining
            Constraint::Length(9), // Time
        ],
    );

    frame.render_widget(table, inner);
}

// ── One-shot (non-TUI) renderers ───────────────────────────────
//
// `hs status` was TUI-only — `enable_raw_mode()` failed unconditionally on
// any host that couldn't put stdout into raw mode (macOS terminals where
// crossterm sees ENXIO, redirected stdout, log scrapers). These two helpers
// give the same data through a non-TTY path so `hs status --output json` and
// `hs status > status.txt` work everywhere.

async fn run_oneshot_json() -> Result<()> {
    use serde_json::Value;
    let client = crate::mcp_client::McpClient::from_default_creds().await?;
    let snapshot = client
        .call_tool("system_status", Value::Object(Default::default()))
        .await?;
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    Ok(())
}

async fn run_oneshot_text() -> Result<()> {
    let data = collect_data().await;

    // Pipeline counts
    println!("Pipeline:");
    if let Some((docs, _)) = data.doc_counts {
        println!("  Documents : {docs}");
    }
    if let Some((md, _)) = data.markdown_counts {
        println!("  Markdown  : {md}");
    }
    if let Some(cat) = data.catalog_count {
        println!("  Cataloged : {cat}");
    }
    println!(
        "  Embedded  : {} docs, {} chunks",
        data.embedded_docs, data.embedded_chunks
    );
    if let Some(corrupted) = data.corrupted_count {
        println!("  Corrupted : {corrupted}");
    }
    println!();

    // Services
    println!("Services:");
    if data.scribe_servers.is_empty() && data.distill_servers.is_empty() {
        println!("  (none registered)");
    }
    for s in &data.scribe_servers {
        let dot = if s.healthy { "●" } else { "○" };
        println!(
            "  {dot} scribe   {:<35}  {} {}  {}",
            s.url, s.activity, s.detail, s.version
        );
    }
    for s in &data.distill_servers {
        let dot = if s.healthy { "●" } else { "○" };
        println!(
            "  {dot} distill  {:<35}  {} {}  {}",
            s.url, s.activity, s.detail, s.version
        );
    }
    let qdot = if data.qdrant_healthy { "●" } else { "○" };
    println!(
        "  {qdot} qdrant   {:<35}  {}",
        data.qdrant_url, data.qdrant_version
    );
    println!();

    // Recent history
    println!("Recent activity:");
    if data.history.is_empty() {
        println!("  (none)");
    }
    for h in data.history.iter().take(20) {
        let when = h
            .when
            .as_ref()
            .map(fmt_ago)
            .unwrap_or_else(|| "—".to_string());
        println!(
            "  {:<10} {:<60}  {:<20}  {}",
            h.activity, h.name, h.detail, when
        );
    }
    Ok(())
}

// ── Entry point ─────────────────────────────────────────────────

pub async fn run(global: &GlobalArgs) -> Result<()> {
    // Branch on output format + TTY availability so `hs status` works in
    // contexts that can't enter raw mode: SSH-piped commands, macOS
    // terminals where crossterm fails with "Device not configured (os
    // error 6)", redirected stdout, log scrapers calling `--output json`.
    // The TUI is preserved as the default for interactive terminals.
    let interactive = io::stdout().is_terminal() && io::stdin().is_terminal();
    match global.output {
        OutputFormat::Json | OutputFormat::Ndjson => return run_oneshot_json().await,
        OutputFormat::Text if !interactive => return run_oneshot_text().await,
        OutputFormat::Text => {}
    }

    // Install panic hook that restores terminal before printing the panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        original_hook(info);
    }));

    // Install a raw SIGINT handler so Ctrl+C exits even when NFS hangs.
    // crossterm's raw mode masks SIGINT, but we override that here so the
    // process can be killed regardless of NFS/tokio state.
    #[cfg(unix)]
    unsafe {
        extern "C" fn sigint_handler(_sig: libc::c_int) {
            // These are not strictly async-signal-safe, but we're about to
            // exit anyway and this is far better than an unkillable process.
            let _ = disable_raw_mode();
            let _ = io::stdout().execute(LeaveAlternateScreen);
            std::process::exit(130);
        }
        libc::signal(
            libc::SIGINT,
            sigint_handler as *const () as libc::sighandler_t,
        );
    }

    // Setup terminal
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut last_collect = Instant::now() - Duration::from_secs(10); // force immediate collect
    let mut data = DashboardData {
        doc_counts: None,
        markdown_counts: None,
        catalog_count: None,
        corrupted_count: None,
        embedded_docs: 0,
        embedded_chunks: 0,
        embedding_skipped: 0,
        scribe_servers: vec![],
        distill_servers: vec![],
        qdrant_healthy: false,
        qdrant_url: String::new(),
        qdrant_version: String::new(),
        watcher: WatcherInfo::Stopped,
        indexer: IndexerInfo::Stopped,
        history: vec![],
        loading: true,
    };

    let mut collect_task: Option<tokio::task::JoinHandle<DashboardData>> = None;

    loop {
        // Kick off data collection in background (non-blocking)
        if last_collect.elapsed() >= Duration::from_secs(3) && collect_task.is_none() {
            collect_task = Some(tokio::spawn(collect_data()));
            last_collect = Instant::now();
        }

        // Check if background collection finished
        if let Some(ref task) = collect_task {
            if task.is_finished() {
                if let Some(task) = collect_task.take() {
                    if let Ok(new_data) = task.await {
                        // For each directory: if the new scan succeeded, use it;
                        // else keep the previous value so the display doesn't flicker.
                        data.doc_counts = new_data.doc_counts.or(data.doc_counts);
                        data.markdown_counts = new_data.markdown_counts.or(data.markdown_counts);
                        data.catalog_count = new_data.catalog_count.or(data.catalog_count);
                        data.corrupted_count = new_data.corrupted_count.or(data.corrupted_count);
                        // Always update network-sourced fields
                        data.scribe_servers = new_data.scribe_servers;
                        data.distill_servers = new_data.distill_servers;
                        data.qdrant_healthy = new_data.qdrant_healthy;
                        data.qdrant_url = new_data.qdrant_url;
                        data.qdrant_version = new_data.qdrant_version;
                        data.embedded_docs = new_data.embedded_docs;
                        data.embedded_chunks = new_data.embedded_chunks;
                        data.watcher = new_data.watcher;
                        data.indexer = new_data.indexer;
                        data.history = new_data.history;
                        data.loading = false;
                    }
                }
            }
        }

        terminal.draw(|frame| render(frame, &data))?;

        // Poll for events (100ms timeout so we stay responsive)
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if key.code == KeyCode::Char('c')
                        && key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL)
                    {
                        break;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        _ => {}
                    }
                }
            }
        }
    }

    // Cancel any in-flight collection
    if let Some(task) = collect_task {
        task.abort();
    }

    // Restore terminal and panic hook
    let _ = std::panic::take_hook(); // remove our custom hook
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}
