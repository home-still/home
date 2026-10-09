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
    /// Files sitting in `papers/manually_downloaded/` waiting for the
    /// next sweep. `None` on the loading frame before the first snapshot
    /// arrives; `Some(0)` once a real snapshot confirms empty.
    inbox_pending: Option<u64>,
    /// Sum of `in_flight` across all scribes. Rendered as the `In-flight`
    /// row so a busy pool is visible in the Pipeline panel, not just in
    /// per-scribe Services rows.
    in_flight_conversions: Option<u64>,
    /// Outstanding work on the scribe consumer (waiting + unacked). `None`
    /// with `queue_error` set means the broker could not say.
    queued_conversions: Option<u64>,
    /// Unconverted papers nothing is queued to convert.
    stalled_conversions: Option<u64>,
    queue_error: Option<String>,
    /// True once a real server snapshot produced this value (not the
    /// loading frame or the "status unavailable" frame).
    snapshot_received: bool,

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

    /// Why the pipeline counts are unknown (storage listing failed), if so.
    counts_error: Option<String>,
}

/// Shown for Queued/Stalled when the MCP server answered but sent neither a
/// queue depth nor a queue error.
const QUEUE_SERVER_TOO_OLD: &str = "MCP server predates queue reporting — upgrade it";

impl DashboardData {
    /// A server that reports the queue always sets `queued_conversions` or
    /// `queue_error` once it has built a snapshot, so neither being set on a
    /// received snapshot means the server predates the queue fields.
    /// Before the first snapshot this is false and the rows say "scanning".
    fn queue_server_too_old(&self) -> bool {
        self.snapshot_received && self.queued_conversions.is_none() && self.queue_error.is_none()
    }

    /// Fold a freshly collected frame into the one on screen. A count the new
    /// collection could not produce keeps its previous value so the display
    /// does not flicker; everything that must reflect "now" is replaced.
    fn absorb(&mut self, new: DashboardData) {
        self.doc_counts = new.doc_counts.or(self.doc_counts);
        self.markdown_counts = new.markdown_counts.or(self.markdown_counts);
        self.catalog_count = new.catalog_count.or(self.catalog_count);
        self.corrupted_count = new.corrupted_count.or(self.corrupted_count);
        self.inbox_pending = new.inbox_pending.or(self.inbox_pending);
        self.in_flight_conversions = new.in_flight_conversions.or(self.in_flight_conversions);
        // Not carried over: a stale "queue empty" would hide a broker outage.
        self.queued_conversions = new.queued_conversions;
        self.stalled_conversions = new.stalled_conversions;
        self.queue_error = new.queue_error;
        // Also replaced: `queue_server_too_old` keys on it, so a frame that
        // never saw a server snapshot must not keep an older frame's verdict.
        self.snapshot_received = new.snapshot_received;
        // Always update network-sourced fields
        self.scribe_servers = new.scribe_servers;
        self.distill_servers = new.distill_servers;
        self.qdrant_healthy = new.qdrant_healthy;
        self.qdrant_url = new.qdrant_url;
        self.qdrant_version = new.qdrant_version;
        self.embedded_docs = new.embedded_docs;
        self.embedded_chunks = new.embedded_chunks;
        self.embedding_skipped = new.embedding_skipped;
        self.watcher = new.watcher;
        self.indexer = new.indexer;
        self.history = new.history;
        self.counts_error = new.counts_error;
        self.loading = false;
    }

    /// Nothing known yet: `loading` before the first collection, or the
    /// "could not collect" frame (`counts_error` says why).
    fn blank(loading: bool, counts_error: Option<String>) -> Self {
        Self {
            doc_counts: None,
            markdown_counts: None,
            catalog_count: None,
            corrupted_count: None,
            embedded_docs: 0,
            embedded_chunks: 0,
            embedding_skipped: 0,
            inbox_pending: None,
            in_flight_conversions: None,
            queued_conversions: None,
            stalled_conversions: None,
            queue_error: None,
            snapshot_received: false,
            scribe_servers: vec![],
            distill_servers: vec![],
            qdrant_healthy: false,
            qdrant_url: String::new(),
            qdrant_version: String::new(),
            watcher: WatcherInfo::Stopped,
            indexer: IndexerInfo::Stopped,
            history: vec![],
            loading,
            counts_error,
        }
    }
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
        /// Last-completed sweep's relocated count. `None` when the daemon
        /// has written its boot-time heartbeat but hasn't finished a
        /// sweep yet, or when upgrading from a pre-rc.301 daemon.
        last_sweep_relocated: Option<u64>,
        last_sweep_found: Option<u64>,
        /// Per-file error count from the most recent sweep. Nonzero
        /// colors the row yellow in the renderer.
        last_sweep_errors: Option<u64>,
    },
}

impl From<Option<hs_common::status::InboxHeartbeatSnapshot>> for WatcherInfo {
    fn from(hb: Option<hs_common::status::InboxHeartbeatSnapshot>) -> Self {
        match hb {
            Some(hb) if hb.running => WatcherInfo::Running {
                host: hb.host,
                last_tick_seconds_ago: hb.last_tick_seconds_ago,
                last_sweep_relocated: hb.last_sweep_relocated,
                last_sweep_found: hb.last_sweep_found,
                last_sweep_errors: hb.last_sweep_errors,
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
    detail: String,        // e.g. "(Cpu)" or compute device
    activity: String,      // e.g. "idle", "3 converting", "1 embedding"
    version: String,       // server version from /health
    error: Option<String>, // why a health/readiness/status call failed
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
    // and S3/Garage backends. On MCP failure the TUI shows a blank dashboard
    // that SAYS why (the counts-unavailable banner) — zeros are accurate
    // ("we don't know yet") rather than confidently wrong, and the cause is
    // never discarded.
    match collect_data_via_mcp().await {
        Ok(data) => data,
        Err(e) => {
            let (indexer, note) = match read_indexer_status() {
                Ok(indexer) => (indexer, String::new()),
                Err(ie) => (
                    IndexerInfo::Stopped,
                    format!("; indexer status unavailable: {ie:#}"),
                ),
            };
            DashboardData {
                indexer,
                ..DashboardData::blank(false, Some(format!("status unavailable: {e:#}{note}")))
            }
        }
    }
}

/// Populate the dashboard from the MCP `system_status` tool — the single
/// source of truth for pipeline counts and service health. Byte counts stay
/// at 0 (system_status reports object counts, not sizes). The `indexer` row
/// reflects the distill-index daemon on the CLI host, not the remote gateway.
async fn collect_data_via_mcp() -> anyhow::Result<DashboardData> {
    use serde_json::Value;

    let client = crate::mcp_client::McpClient::from_default_creds().await?;
    let called = client
        .call_tool("system_status", Value::Object(Default::default()))
        .await;
    // End the session whether or not the call worked.
    client.close_logged().await;
    let snap: hs_common::status::StatusSnapshot = serde_json::from_value(called?)?;

    let mut data = snapshot_to_dashboard(snap);
    data.indexer = read_indexer_status()?;
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

    // Counts the storage layer could not take are unknown, not zero.
    let counts_ok = snap.pipeline.counts_error.is_none();
    DashboardData {
        doc_counts: counts_ok.then_some((snap.pipeline.documents, 0)),
        markdown_counts: counts_ok.then_some((snap.pipeline.markdown, 0)),
        catalog_count: counts_ok.then_some(snap.pipeline.catalog_entries),
        counts_error: snap.pipeline.counts_error.clone(),
        corrupted_count: snap.pipeline.corrupted_pdfs,
        embedded_docs: snap.pipeline.embedded_documents.unwrap_or(0),
        embedded_chunks: snap.pipeline.embedded_chunks.unwrap_or(0),
        embedding_skipped: snap.pipeline.embedding_skipped.unwrap_or(0),
        inbox_pending: snap.pipeline.inbox_pending,
        in_flight_conversions: snap.pipeline.in_flight_conversions,
        queued_conversions: snap.pipeline.queued_conversions,
        stalled_conversions: snap.pipeline.stalled_conversions,
        queue_error: snap.pipeline.queue_error.clone(),
        snapshot_received: true,
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
        error: inst.error.clone(),
    }
}

fn read_indexer_status() -> anyhow::Result<IndexerInfo> {
    let status = match crate::distill_cmd::read_index_status()? {
        Some(s) => s,
        None => return Ok(IndexerInfo::Stopped),
    };

    // Cross-host liveness: trust the status file's mtime over local PID check.
    // The indexer updates the status file frequently while running.
    let status_path = crate::distill_cmd::index_status_path()?;
    let status_is_fresh = std::fs::metadata(&status_path)
        .and_then(|m| m.modified())
        .map(|t| {
            std::time::SystemTime::now()
                .duration_since(t)
                .map(|d| d.as_secs() < 30)
                .unwrap_or(false)
        })
        .unwrap_or(false);

    // A bare "is this PID alive" names whatever process recycled the PID of a
    // crashed indexer; only the real index daemon counts.
    let is_alive =
        status_is_fresh || (status.pid > 0 && crate::distill_cmd::is_index_daemon(status.pid)?);

    if !is_alive {
        if status.done {
            return Ok(IndexerInfo::Finished {
                indexed: status.indexed as u64,
                chunks: status.total_chunks as u64,
            });
        }
        return Ok(IndexerInfo::Stopped);
    }

    if status.done {
        return Ok(IndexerInfo::Finished {
            indexed: status.indexed as u64,
            chunks: status.total_chunks as u64,
        });
    }

    Ok(IndexerInfo::Running {
        indexed: status.indexed as u64,
        total: status.total_files as u64,
        failed: status.failed as u64,
        chunks: status.total_chunks as u64,
        current_file: status.current_file,
    })
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
    let secs = (now - *dt).num_seconds().max(0);
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
        Constraint::Length(1),  // title
        Constraint::Length(14), // pipeline: header + Documents/Markdown/Cataloged/Embedded/In-flight/Queued/Stalled/Inbox/Corrupted/Watcher/Indexer rows + 2 border rows
        Constraint::Length(1),  // spacer
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
        .title(match &data.counts_error {
            Some(why) => Line::from(format!(" Pipeline — {} ", ellipsize(why, 100)))
                .style(Style::default().fg(Color::Red)),
            None => Line::from(" Pipeline "),
        })
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
    let scanning = if data.counts_error.is_some() {
        "   n/a".to_string()
    } else {
        "  ...".to_string()
    };
    let too_old = data.queue_server_too_old();

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
            Cell::from("In-flight"),
            Cell::from(match data.in_flight_conversions {
                Some(n) => format!("{n:>6}"),
                None => scanning.clone(),
            })
            .style(match data.in_flight_conversions {
                Some(n) if n > 0 => Style::default().fg(Color::Green),
                _ => Style::default().fg(Color::DarkGray),
            }),
            Cell::from(match data.in_flight_conversions {
                Some(n) if n > 0 => format!(
                    "converting across {} scribe{}",
                    data.scribe_servers.len(),
                    if data.scribe_servers.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
                Some(_) => "idle".to_string(),
                None => String::new(),
            }),
            Cell::from(""),
        ]),
        Row::new(vec![
            Cell::from("Queued"),
            Cell::from(match (data.queued_conversions, &data.queue_error) {
                (Some(n), _) => format!("{n:>6}"),
                (None, Some(_)) => "     ?".to_string(),
                (None, None) if too_old => "   n/a".to_string(),
                (None, None) => scanning.clone(),
            })
            .style(match data.queued_conversions {
                Some(n) if n > 0 => Style::default().fg(Color::Green),
                _ => Style::default().fg(Color::DarkGray),
            }),
            Cell::from(match (data.queued_conversions, &data.queue_error) {
                (Some(n), _) if n > 0 => "waiting for or held by a scribe".to_string(),
                (Some(_), _) => "queue empty".to_string(),
                (None, Some(reason)) => format!("unavailable: {reason}"),
                (None, None) if too_old => QUEUE_SERVER_TOO_OLD.to_string(),
                (None, None) => String::new(),
            })
            .style(if data.queue_error.is_some() {
                Style::default().fg(Color::Red)
            } else if too_old {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            }),
            Cell::from(""),
        ]),
        Row::new(vec![
            Cell::from("Stalled"),
            Cell::from(match data.stalled_conversions {
                Some(n) => format!("{n:>6}"),
                None if too_old => "   n/a".to_string(),
                None => "     ?".to_string(),
            })
            .style(match data.stalled_conversions {
                Some(n) if n > 0 => Style::default().fg(Color::Yellow),
                _ => Style::default().fg(Color::DarkGray),
            }),
            Cell::from(match data.stalled_conversions {
                Some(n) if n > 0 => {
                    "no markdown and not queued — run `hs pipeline catch-up`".to_string()
                }
                Some(_) => "none".to_string(),
                None if too_old => QUEUE_SERVER_TOO_OLD.to_string(),
                None => String::new(),
            })
            .style(if too_old {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            }),
            Cell::from(""),
        ]),
        Row::new(vec![
            Cell::from("Inbox"),
            Cell::from(match data.inbox_pending {
                Some(n) => format!("{n:>6}"),
                None => scanning.clone(),
            })
            .style(match data.inbox_pending {
                Some(n) if n > 0 => Style::default().fg(Color::Yellow),
                _ => Style::default().fg(Color::DarkGray),
            }),
            Cell::from(match data.inbox_pending {
                Some(n) if n > 0 => format!("pending in manually_downloaded/ ({n})"),
                Some(_) => "manually_downloaded/ empty".to_string(),
                None => String::new(),
            }),
            Cell::from(""),
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
                last_sweep_relocated,
                last_sweep_found,
                last_sweep_errors,
            } => {
                let sweep_frag = match (last_sweep_relocated, last_sweep_found) {
                    (Some(r), Some(f)) => format!(" · swept {r} / {f}"),
                    _ => String::new(),
                };
                let errs = last_sweep_errors.unwrap_or(0);
                let color = if errs > 0 {
                    Color::Yellow
                } else {
                    Color::Green
                };
                Row::new(vec![
                    Cell::from("Watcher").style(Style::default().fg(color)),
                    Cell::from("●".to_string()).style(Style::default().fg(color)),
                    Cell::from(format!("running · {host}{sweep_frag}")),
                    Cell::from(if errs > 0 {
                        format!("last tick {last_tick_seconds_ago}s ago · {errs} err")
                    } else {
                        format!("last tick {last_tick_seconds_ago}s ago")
                    }),
                ])
            }
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
                let file_short = ellipsize(current_file, 30);
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
            // Detail column: holds byte counts (Documents/Markdown),
            // "X chunks · Y skipped" (Embedded), "converting across N
            // scribes" (In-flight), "pending in manually_downloaded/"
            // (Inbox), and the Watcher "running · host · swept N / M"
            // string. Widened from 14 → Min(38) because the rc.301
            // rows overflowed the 14-char slot.
            Constraint::Min(38),
            // Trailing column: % for Documents/Markdown/Embedded, or
            // "last tick Ns ago · K err" for Watcher.
            Constraint::Min(8),
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
            Constraint::Length(24), // Status + Activity ("running · 12 converting", "backend unavailable")
            Constraint::Fill(3),    // URL — gets 3/5 of remaining
            Constraint::Fill(2),    // Detail — gets 2/5 of remaining
            Constraint::Length(16), // Version
        ],
    );

    frame.render_widget(table, inner);
}

/// An unhealthy server is not necessarily a stopped one: the server's own
/// activity label ("backend unavailable", "unhealthy: <why>") says what is
/// actually wrong. "stopped" is only the answer when nothing was reported.
fn format_status_activity(healthy: bool, running_label: &str, activity: &str) -> String {
    if !healthy {
        return if activity.is_empty() {
            "stopped".into()
        } else {
            activity.into()
        };
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

async fn run_oneshot_json(ndjson: bool) -> Result<()> {
    use serde_json::Value;
    let client = crate::mcp_client::McpClient::from_default_creds().await?;
    let called = client
        .call_tool("system_status", Value::Object(Default::default()))
        .await;
    client.close_logged().await;
    println!("{}", status_json_text(&called?, ndjson)?);
    Ok(())
}

/// `--output json` is pretty-printed; `--output ndjson` is one record per
/// line, so its single record must not span lines.
fn status_json_text(snapshot: &serde_json::Value, ndjson: bool) -> Result<String> {
    Ok(if ndjson {
        serde_json::to_string(snapshot)?
    } else {
        serde_json::to_string_pretty(snapshot)?
    })
}

async fn run_oneshot_text() -> Result<()> {
    // A status command that cannot get the status fails (non-zero exit); only
    // the interactive dashboard keeps going with a "status unavailable" banner.
    let data = collect_data_via_mcp().await?;

    // Pipeline counts
    println!("Pipeline:");
    if let Some(reason) = &data.counts_error {
        println!("  Counts unavailable: {reason}");
    }
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
    if data.queue_server_too_old() {
        println!("  Queued    : n/a ({QUEUE_SERVER_TOO_OLD})");
        println!("  Stalled   : n/a ({QUEUE_SERVER_TOO_OLD})");
    } else {
        if let Some(reason) = &data.queue_error {
            println!("  Queued    : unavailable ({reason})");
        } else if let Some(q) = data.queued_conversions {
            println!("  Queued    : {q}");
        }
        if let Some(s) = data.stalled_conversions {
            println!("  Stalled   : {s}");
        }
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
        if let Some(e) = &s.error {
            println!("      ! {e}");
        }
    }
    for s in &data.distill_servers {
        let dot = if s.healthy { "●" } else { "○" };
        println!(
            "  {dot} distill  {:<35}  {} {}  {}",
            s.url, s.activity, s.detail, s.version
        );
        if let Some(e) = &s.error {
            println!("      ! {e}");
        }
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
        OutputFormat::Json => return run_oneshot_json(false).await,
        OutputFormat::Ndjson => return run_oneshot_json(true).await,
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

    // Ctrl+C / SIGTERM request a graceful stop (`shutdown`): the loop below
    // notices within one poll interval, restores the terminal and returns.
    // No signal handler of our own, and the process still exits promptly if
    // a collection task is wedged on a hung mount (`main` abandons workers
    // after a short grace period).
    let stop = crate::shutdown::cooperative();

    // Setup terminal. From the moment raw mode is on, every exit — including
    // a failed draw or poll below — restores the terminal (`TerminalRestore`).
    enable_raw_mode()?;
    let mut restore = TerminalRestore { done: false };
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut last_collect = Instant::now() - Duration::from_secs(10); // force immediate collect
    let mut data = DashboardData::blank(true, None);

    let mut collect_task: Option<tokio::task::JoinHandle<DashboardData>> = None;

    loop {
        if stop.requested() {
            break;
        }
        // Kick off data collection in background (non-blocking)
        if last_collect.elapsed() >= Duration::from_secs(3) && collect_task.is_none() {
            collect_task = Some(tokio::spawn(collect_data()));
            last_collect = Instant::now();
        }

        // Check if background collection finished
        if let Some(task) = &collect_task {
            if task.is_finished() {
                if let Some(task) = collect_task.take() {
                    if let Ok(new_data) = task.await {
                        data.absorb(new_data);
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
    restore.restore()?;
    Ok(())
}

/// Puts the terminal back (raw mode off, alternate screen left) exactly once,
/// on the normal path via [`restore`](Self::restore) (which reports failure)
/// and on every early `?` return via `Drop`.
struct TerminalRestore {
    done: bool,
}

impl TerminalRestore {
    fn restore(&mut self) -> Result<()> {
        if std::mem::replace(&mut self.done, true) {
            return Ok(());
        }
        // Both are attempted: a failed raw-mode reset must not skip leaving
        // the alternate screen.
        let raw = disable_raw_mode();
        let screen = io::stdout().execute(LeaveAlternateScreen).map(|_| ());
        raw?;
        screen?;
        Ok(())
    }
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Shorten `s` to at most `max` characters, ending in `...` when it was cut.
/// Counts characters, not bytes: a byte slice through a multi-byte character
/// in a file name panicked the whole dashboard.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(3)).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn an_unhealthy_server_shows_why_instead_of_stopped() {
        assert_eq!(
            format_status_activity(false, "running", "backend unavailable"),
            "backend unavailable"
        );
        assert_eq!(format_status_activity(false, "running", ""), "stopped");
        assert_eq!(format_status_activity(true, "running", "idle"), "running");
        assert_eq!(
            format_status_activity(true, "running", "2 converting"),
            "running · 2 converting"
        );
    }

    #[test]
    fn ellipsize_counts_characters_and_leaves_short_names_alone() {
        assert_eq!(ellipsize("short.md", 30), "short.md");
        let ascii = "a".repeat(40);
        assert_eq!(ellipsize(&ascii, 30), format!("{}...", "a".repeat(27)));
        // 30 two-byte characters are 60 bytes but exactly 30 characters.
        let thirty_e_acute = "é".repeat(30);
        assert_eq!(ellipsize(&thirty_e_acute, 30), thirty_e_acute);
        let long = "日本語".repeat(20);
        let cut = ellipsize(&long, 30);
        assert_eq!(cut.chars().count(), 30);
        assert!(cut.ends_with("..."));
    }

    /// The indexer row sliced `current_file` at byte 27: a multi-byte
    /// character straddling it panicked the render (and, in release builds,
    /// aborted the process with the terminal in raw mode). The row is built
    /// eagerly even when the panel clips it, so a completed draw is the proof.
    #[test]
    fn the_dashboard_renders_a_non_ascii_file_name_longer_than_the_column() {
        for name in [
            format!("{}é{}", "a".repeat(26), "b".repeat(20)),
            format!("{}日本語{}", "a".repeat(25), "b".repeat(20)),
        ] {
            let mut data = DashboardData::blank(false, None);
            data.indexer = IndexerInfo::Running {
                indexed: 1,
                total: 2,
                failed: 0,
                chunks: 3,
                current_file: name,
            };
            let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();

            terminal.draw(|frame| render(frame, &data)).unwrap();

            let screen: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(screen.contains("Pipeline"), "the dashboard drew");
        }
    }

    fn draw(data: &DashboardData) -> String {
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
        terminal.draw(|frame| render(frame, data)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// The Pipeline panel was one row short of its content, so the last row
    /// (Indexer) never drew; and the Services status cell was narrower than
    /// the "backend unavailable" label it exists to show.
    #[test]
    fn the_dashboard_shows_the_indexer_row_and_the_whole_unhealthy_label() {
        let mut data = DashboardData::blank(false, None);
        data.scribe_servers.push(ServiceStatus {
            url: "http://scribe.example.local:7435".into(),
            healthy: false,
            detail: String::new(),
            activity: "backend unavailable".into(),
            version: String::new(),
            error: None,
        });
        let screen = draw(&data);
        assert!(screen.contains("Indexer"), "Indexer row clipped");
        assert!(screen.contains("backend unavailable"), "label truncated");
    }

    /// A failed collection used to show only blank counts: the cause was
    /// dropped on the floor.
    #[test]
    fn the_dashboard_says_why_the_status_is_unavailable() {
        let data = DashboardData::blank(false, Some("status unavailable: gateway refused".into()));
        assert!(draw(&data).contains("gateway refused"));
    }

    fn snapshot_with(
        queued: Option<u64>,
        stalled: Option<u64>,
        queue_error: Option<&str>,
    ) -> hs_common::status::StatusSnapshot {
        let mut snap = hs_common::status::StatusSnapshot::default();
        snap.pipeline.queued_conversions = queued;
        snap.pipeline.stalled_conversions = stalled;
        snap.pipeline.queue_error = queue_error.map(String::from);
        snap
    }

    /// An older MCP server omits the queue fields (serde default: both
    /// `None`). A received snapshot with neither set can never come from a
    /// current server, so the rows say so instead of "scanning" forever.
    #[test]
    fn a_snapshot_from_a_server_without_queue_fields_says_to_upgrade_it() {
        let data = snapshot_to_dashboard(snapshot_with(None, None, None));
        assert!(data.queue_server_too_old());
        let screen = draw(&data);
        assert_eq!(
            screen.matches(QUEUE_SERVER_TOO_OLD).count(),
            2,
            "both the Queued and Stalled rows explain: {screen}"
        );
    }

    /// Before the first snapshot (and on the "status unavailable" frame)
    /// nothing has been received, so the old-server verdict must not fire.
    #[test]
    fn before_the_first_snapshot_the_queue_rows_still_say_scanning() {
        for data in [
            DashboardData::blank(true, None),
            DashboardData::blank(false, Some("status unavailable: gateway refused".into())),
        ] {
            assert!(!data.queue_server_too_old());
            assert!(!draw(&data).contains(QUEUE_SERVER_TOO_OLD));
        }
    }

    /// A current server always sets the depth or the error, so neither of
    /// those readings is mistaken for an old server.
    #[test]
    fn a_current_server_snapshot_is_never_called_too_old() {
        for snap in [
            snapshot_with(Some(0), Some(0), None),
            snapshot_with(Some(4), None, None),
            snapshot_with(None, None, Some("broker down")),
        ] {
            let data = snapshot_to_dashboard(snap);
            assert!(!data.queue_server_too_old());
            assert!(!draw(&data).contains(QUEUE_SERVER_TOO_OLD));
        }
    }

    #[test]
    fn stalled_rows_point_at_catch_up_without_blaming_scribe() {
        let screen = draw(&snapshot_to_dashboard(snapshot_with(
            Some(0),
            Some(7),
            None,
        )));
        assert!(
            screen.contains("no markdown and not queued — run `hs pipeline catch-up`"),
            "{screen}"
        );
        assert!(!screen.contains("refused"), "{screen}");
    }

    /// The dashboard on screen is the loading frame with every collection
    /// folded into it, so what a collection sets must survive `absorb`.
    #[test]
    fn the_old_server_verdict_reaches_the_screen_and_does_not_outlive_its_frame() {
        let mut shown = DashboardData::blank(true, None);
        shown.absorb(snapshot_to_dashboard(snapshot_with(None, None, None)));
        assert!(shown.queue_server_too_old());
        assert_eq!(draw(&shown).matches(QUEUE_SERVER_TOO_OLD).count(), 2);

        shown.absorb(DashboardData::blank(
            false,
            Some("status unavailable: down".into()),
        ));
        assert!(!shown.queue_server_too_old());
    }

    /// Skipped rows are not embeddable: a fully embedded corpus reads 100%,
    /// not permanently short by the skip count.
    #[test]
    fn skipped_embeddings_reach_the_screen_and_leave_the_percentage_denominator() {
        let mut snap = snapshot_with(Some(0), Some(0), None);
        snap.pipeline.documents = 200;
        snap.pipeline.markdown = 100;
        snap.pipeline.embedded_documents = Some(90);
        snap.pipeline.embedding_skipped = Some(10);
        let mut shown = DashboardData::blank(true, None);
        shown.absorb(snapshot_to_dashboard(snap));
        let screen = draw(&shown);
        assert!(screen.contains("10 skipped"), "{screen}");
        assert!(screen.contains("100.0%"), "{screen}");
    }

    #[test]
    fn a_timestamp_ahead_of_this_clock_reads_as_now_not_negative() {
        let ahead = chrono::Utc::now() + chrono::Duration::seconds(45);
        assert_eq!(fmt_ago(&ahead), "0s ago");
    }

    /// NDJSON is one record per line: pretty-printing the status record
    /// split it across lines for any line-oriented consumer.
    #[test]
    fn ndjson_status_is_one_line_and_json_status_is_pretty() {
        let snapshot = serde_json::json!({"pipeline": {"documents": 1}, "history": []});
        let ndjson = status_json_text(&snapshot, true).unwrap();
        assert_eq!(ndjson.lines().count(), 1, "{ndjson}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&ndjson).unwrap(),
            snapshot
        );
        assert!(status_json_text(&snapshot, false).unwrap().lines().count() > 1);
    }
}
