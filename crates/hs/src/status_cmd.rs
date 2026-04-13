use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
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
    /// Per-directory NFS stall flags.
    fs_stalled_docs: bool,
    fs_stalled_markdown: bool,
    fs_stalled_catalog: bool,
}

enum WatcherInfo {
    /// Watcher is running (PID alive)
    Running {
        processing: u64,
        queued: u64,
        completed: u64,
        failed: u64,
    },
    /// PID is dead, exited cleanly (no failures)
    Finished { completed: u64 },
    /// PID is dead but had failures — needs attention
    Failed { completed: u64, failed: u64 },
    /// No status file, no PID file — watcher never started
    Stopped,
}

enum IndexerInfo {
    /// Indexer daemon is actively running
    Running {
        indexed: u64,
        total: u64,
        failed: u64,
        chunks: u64,
        current_file: String,
        gpu_yield: bool,
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

/// Check if a filename is a macOS resource fork (starts with "._")
fn is_macos_resource_fork(path: &Path) -> bool {
    path.file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|name| name.starts_with("._"))
}

/// Recursive version of count_dir — walks subdirectories.
/// Used for watch_dir since scribe processes PDFs recursively.
fn count_dir_recursive(dir: &Path, ext: &str) -> (u64, u64) {
    let mut count = 0u64;
    let mut bytes = 0u64;
    fn walk(dir: &Path, ext: &str, count: &mut u64, bytes: &mut u64) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, ext, count, bytes);
                } else if path.extension().is_some_and(|e| e == ext)
                    && !is_macos_resource_fork(&path)
                {
                    *count += 1;
                    *bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    walk(dir, ext, &mut count, &mut bytes);
    (count, bytes)
}

async fn collect_data() -> DashboardData {
    let scribe_cfg = hs_scribe::config::ScribeConfig::load().unwrap_or_default();
    let distill_cfg = hs_distill::config::DistillClientConfig::load().unwrap_or_default();

    // Run filesystem operations as independent per-directory tasks so the
    // dashboard can show partial results as each directory finishes scanning.
    let cfg_docs = scribe_cfg.clone();
    let docs_task = tokio::task::spawn_blocking(move || {
        let (pdf_count, pdf_bytes) = count_dir_recursive(&cfg_docs.watch_dir, "pdf");
        let (html_count, html_bytes) = count_dir_recursive(&cfg_docs.watch_dir, "html");
        let (htm_count, htm_bytes) = count_dir_recursive(&cfg_docs.watch_dir, "htm");
        (
            pdf_count + html_count + htm_count,
            pdf_bytes + html_bytes + htm_bytes,
        )
    });

    let cfg_md = scribe_cfg.clone();
    let markdown_task =
        tokio::task::spawn_blocking(move || count_dir_recursive(&cfg_md.output_dir, "md"));

    let cfg_cat = scribe_cfg.clone();
    let catalog_task = tokio::task::spawn_blocking(move || {
        let (count, _) = count_dir_recursive(&cfg_cat.catalog_dir, "yaml");
        count
    });

    let cfg_cor = scribe_cfg.clone();
    let corrupted_task = tokio::task::spawn_blocking(move || {
        let (pdf, _) = count_dir_recursive(&cfg_cor.corrupted_dir, "pdf");
        let (html, _) = count_dir_recursive(&cfg_cor.corrupted_dir, "html");
        pdf + html
    });

    // Sharded layout means recursive walks across ~150 subdirs; macOS NFS v3
    // with 32KB blocks needs more time than a flat directory scan.
    let timeout = Duration::from_secs(20);

    let (doc_counts, fs_stalled_docs) = match tokio::time::timeout(timeout, docs_task).await {
        Ok(Ok(counts)) => (Some(counts), false),
        _ => (None, true),
    };
    let (markdown_counts, fs_stalled_markdown) =
        match tokio::time::timeout(timeout, markdown_task).await {
            Ok(Ok(counts)) => (Some(counts), false),
            _ => (None, true),
        };
    let (catalog_count, fs_stalled_catalog) =
        match tokio::time::timeout(timeout, catalog_task).await {
            Ok(Ok(count)) => (Some(count), false),
            _ => (None, true),
        };
    let corrupted_count = match tokio::time::timeout(timeout, corrupted_task).await {
        Ok(Ok(count)) => Some(count),
        _ => None,
    };

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Discover servers from gateway registry, falling back to config
    let scribe_urls =
        hs_common::service::registry::discover_or_fallback("scribe", scribe_cfg.servers.clone())
            .await;
    let distill_urls =
        hs_common::service::registry::discover_or_fallback("distill", distill_cfg.servers.clone())
            .await;

    // Scribe server health + readiness checks
    let mut scribe_servers = Vec::new();
    for url in &scribe_urls {
        let health_version: String = async {
            let resp = http.get(format!("{url}/health")).send().await.ok()?;
            let data: serde_json::Value = resp.json().await.ok()?;
            data["version"].as_str().map(|s| s.to_string())
        }
        .await
        .unwrap_or_default();

        let readiness: Option<serde_json::Value> = async {
            let resp = http.get(format!("{url}/readiness")).send().await.ok()?;
            if !resp.status().is_success() {
                return None;
            }
            resp.json().await.ok()
        }
        .await;

        if let Some(data) = readiness {
            let in_flight = data["in_flight_conversions"].as_u64().unwrap_or(0);
            let activity = if in_flight > 0 {
                format!("{in_flight} converting")
            } else {
                "idle".to_string()
            };
            scribe_servers.push(ServiceStatus {
                url: url.clone(),
                healthy: true,
                detail: String::new(),
                activity,
                version: health_version,
            });
        } else {
            scribe_servers.push(ServiceStatus {
                url: url.clone(),
                healthy: false,
                detail: String::new(),
                activity: String::new(),
                version: String::new(),
            });
        }
    }

    // Distill server health checks + embedded counts
    let mut distill_servers = Vec::new();
    let mut embedded_docs = 0u64;
    let mut embedded_chunks = 0u64;
    let mut qdrant_healthy = false;
    let mut qdrant_url = String::new();
    let mut qdrant_version = String::new();

    for url in &distill_urls {
        let client = if hs_common::auth::client::is_cloud_url(url) {
            match hs_common::auth::client::AuthenticatedClient::from_default_path() {
                Ok(auth) => match auth.build_reqwest_client().await {
                    Ok(http) => hs_distill::client::DistillClient::new_with_client(url, http),
                    Err(_) => hs_distill::client::DistillClient::new(url),
                },
                Err(_) => hs_distill::client::DistillClient::new(url),
            }
        } else {
            hs_distill::client::DistillClient::new(url)
        };
        let health = client.health().await.ok();
        let health_version = health
            .as_ref()
            .map(|h| h.version.clone())
            .unwrap_or_default();
        if qdrant_version.is_empty() {
            qdrant_version = health
                .as_ref()
                .map(|h| h.qdrant_version.clone())
                .unwrap_or_default();
        }
        let healthy = health.is_some();

        // Fetch readiness for in-flight embedding count
        let activity = match client.readiness().await {
            Ok(r) if r.in_flight > 0 => format!("{} embedding", r.in_flight),
            Ok(_) => "idle".into(),
            Err(_) => String::new(),
        };

        // Always try to get status (for embedded counts) even if health is slow
        if let Ok(s) = client.status().await {
            if embedded_docs == 0 {
                embedded_docs = s.documents_count;
                embedded_chunks = s.points_count;
            }
            // If we got a status response, Qdrant must be healthy
            qdrant_healthy = true;
            qdrant_url = format!("{url} → {}", s.collection);
            distill_servers.push(ServiceStatus {
                url: url.clone(),
                healthy: true,
                detail: format!("({})", s.compute_device),
                activity,
                version: health_version,
            });
        } else if healthy {
            distill_servers.push(ServiceStatus {
                url: url.clone(),
                healthy: true,
                detail: String::new(),
                activity,
                version: health_version,
            });
        } else {
            distill_servers.push(ServiceStatus {
                url: url.clone(),
                healthy: false,
                detail: String::new(),
                activity: String::new(),
                version: String::new(),
            });
        }
    }

    // Watcher status + history (filesystem I/O, may stall on NFS)
    let cfg2 = scribe_cfg.clone();
    let fs_result2 = tokio::task::spawn_blocking(move || {
        let watcher = read_watcher_status(&cfg2.output_dir, &cfg2.watch_dir);
        let history = load_history(&cfg2.catalog_dir, 100);
        (watcher, history)
    });
    let (watcher, history) = match tokio::time::timeout(Duration::from_secs(20), fs_result2).await {
        Ok(Ok((w, h))) => (w, h),
        _ => (WatcherInfo::Stopped, vec![]),
    };

    // Distill index daemon status (local JSON file, fast read)
    let indexer = read_indexer_status();

    DashboardData {
        doc_counts,
        markdown_counts,
        catalog_count,
        corrupted_count,
        embedded_docs,
        embedded_chunks,
        scribe_servers,
        distill_servers,
        qdrant_healthy,
        qdrant_url,
        qdrant_version,
        watcher,
        indexer,
        history,
        loading: false,
        fs_stalled_docs,
        fs_stalled_markdown,
        fs_stalled_catalog,
    }
}

fn read_watcher_status(output_dir: &Path, watch_dir: &Path) -> WatcherInfo {
    let status_path = output_dir.join(".scribe-watch-status.json");

    // The watcher writes its status file every ~2s while running. A recent
    // mtime is a cross-host liveness signal — PID checks only work when the
    // dashboard and watcher run on the same machine.
    let status_is_fresh = std::fs::metadata(&status_path)
        .and_then(|m| m.modified())
        .map(|t| {
            std::time::SystemTime::now()
                .duration_since(t)
                .map(|d| d.as_secs() < 30)
                .unwrap_or(false)
        })
        .unwrap_or(false);

    if let Ok(contents) = std::fs::read_to_string(&status_path) {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&contents) {
            let pid = data["pid"].as_u64().unwrap_or(0) as u32;
            let processing = data["processing"].as_u64().unwrap_or(0);
            let queued = data["queued"].as_u64().unwrap_or(0);
            let completed = data["completed"].as_u64().unwrap_or(0);
            let failed = data["failed"].as_u64().unwrap_or(0);

            // Alive if the status file was touched recently OR the pid is local and alive
            let is_alive = status_is_fresh || (pid > 0 && crate::daemon::is_process_alive(pid));
            if is_alive {
                return WatcherInfo::Running {
                    processing,
                    queued,
                    completed,
                    failed,
                };
            } else if failed > 0 {
                return WatcherInfo::Failed { completed, failed };
            } else {
                return WatcherInfo::Finished { completed };
            }
        }
    }

    // No status file — check PID file (local-only fallback)
    let pid_path = crate::daemon::pid_file_path(watch_dir);
    if let Some(pid) = crate::daemon::read_pid(&pid_path) {
        if crate::daemon::is_process_alive(pid) {
            return WatcherInfo::Running {
                processing: 0,
                queued: 0,
                completed: 0,
                failed: 0,
            };
        }
    }

    WatcherInfo::Stopped
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
        gpu_yield: status.gpu_yield,
    }
}

fn load_history(catalog_dir: &Path, limit: usize) -> Vec<HistoryEvent> {
    let mut events = Vec::new();

    // Catalog is sharded under catalog/XX/stem.yaml — walk recursively.
    let mut catalog_paths = hs_common::collect_files_recursive(catalog_dir, "yaml");
    catalog_paths.extend(hs_common::collect_files_recursive(catalog_dir, "yml"));

    let catalog_entries: Vec<_> = catalog_paths
        .into_iter()
        .filter_map(|path| {
            let contents = std::fs::read_to_string(&path).ok()?;
            let entry: hs_common::catalog::CatalogEntry =
                serde_yaml_ng::from_str(&contents).ok()?;
            let stem = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            Some((stem, entry))
        })
        .collect();

    for (stem, entry) in &catalog_entries {
        let name = entry.title.as_deref().unwrap_or(stem);
        let short_name: String = name.to_string();

        // Download event
        if let Some(ref dl_at) = entry.downloaded_at {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(dl_at) {
                let size = entry.file_size_bytes.map(fmt_bytes).unwrap_or_default();
                events.push(HistoryEvent {
                    activity: "Download",
                    name: short_name.clone(),
                    detail: size,
                    when: Some(dt.with_timezone(&chrono::Utc)),
                });
            }
        }

        // Conversion event
        if let Some(ref conv) = entry.conversion {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&conv.converted_at) {
                events.push(HistoryEvent {
                    activity: "Convert",
                    name: short_name.clone(),
                    detail: format!("{}pg {:.0}s", conv.total_pages, conv.duration_secs),
                    when: Some(dt.with_timezone(&chrono::Utc)),
                });
            }
        }

        // Embedding event
        if let Some(ref emb) = entry.embedding {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&emb.embedded_at) {
                events.push(HistoryEvent {
                    activity: "Embed",
                    name: short_name.clone(),
                    detail: format!("{} chunks", emb.chunks_indexed),
                    when: Some(dt.with_timezone(&chrono::Utc)),
                });
            }
        }
    }

    events.sort_by(|a, b| {
        let a_ts = a.when.unwrap_or(chrono::DateTime::UNIX_EPOCH);
        let b_ts = b.when.unwrap_or(chrono::DateTime::UNIX_EPOCH);
        b_ts.cmp(&a_ts)
    });
    events.truncate(limit);
    events
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
    let any_stall = data.fs_stalled_docs || data.fs_stalled_markdown || data.fs_stalled_catalog;
    let title = if any_stall {
        Line::from(vec![" Pipeline ".into(), "· NFS stall ".fg(Color::Red)])
    } else {
        Line::from(" Pipeline ")
    };
    let block = Block::new()
        .title(title)
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
    let md_to_embed = if markdown_count > 0 {
        (data.embedded_docs as f64 / markdown_count as f64).min(1.0)
    } else {
        0.0
    };

    // Helper: show "Scanning..." when None, count when Some, "NFS stall" suffix when stalled
    let scanning = "  ...".to_string();
    let stall_style = Style::default().fg(Color::Yellow);

    let doc_label = if data.fs_stalled_docs {
        "Documents (stall)"
    } else {
        "Documents"
    };
    let md_label = if data.fs_stalled_markdown {
        "Markdown (stall)"
    } else {
        "Markdown"
    };
    let cat_label = if data.fs_stalled_catalog {
        "Cataloged (stall)"
    } else {
        "Cataloged"
    };

    let rows = vec![
        Row::new(vec![
            Cell::from(doc_label).style(if data.fs_stalled_docs {
                stall_style
            } else {
                Style::default()
            }),
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
            Cell::from(md_label).style(if data.fs_stalled_markdown {
                stall_style
            } else {
                Style::default()
            }),
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
            Cell::from(cat_label).style(if data.fs_stalled_catalog {
                stall_style
            } else {
                Style::default()
            }),
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
            Cell::from(format!("{:>5} chunks", data.embedded_chunks)),
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
            WatcherInfo::Running {
                processing,
                queued,
                completed,
                failed,
            } => {
                let (status_label, status_color) = if *processing > 0 || *queued > 0 {
                    ("working", Color::Green)
                } else if *completed > 0 && *failed == 0 {
                    ("complete", Color::Green)
                } else if *completed > 0 || *failed > 0 {
                    ("idle", Color::Green)
                } else {
                    ("idle", Color::DarkGray)
                };
                let detail = if *processing > 0 || *queued > 0 {
                    format!("{processing} active, {queued} queued")
                } else {
                    format!("{completed} done, {failed} failed")
                };
                Row::new(vec![
                    Cell::from("Watcher").style(Style::default().fg(status_color)),
                    Cell::from("●".to_string()).style(Style::default().fg(status_color)),
                    Cell::from(status_label),
                    Cell::from(detail),
                ])
            }
            WatcherInfo::Finished { completed } => Row::new(vec![
                Cell::from("Watcher").style(Style::default().fg(Color::Green)),
                Cell::from("○".to_string()).style(Style::default().fg(Color::Green)),
                Cell::from("done"),
                Cell::from(format!("{completed} done")),
            ]),
            WatcherInfo::Failed { completed, failed } => Row::new(vec![
                Cell::from("Watcher").style(Style::default().fg(Color::Red)),
                Cell::from("○".to_string()).style(Style::default().fg(Color::Red)),
                Cell::from("failed"),
                Cell::from(format!("{completed} done, {failed} failed")),
            ]),
            WatcherInfo::Stopped => Row::new(vec![
                Cell::from("Watcher").style(Style::default().fg(Color::DarkGray)),
                Cell::from("○".to_string()).style(Style::default().fg(Color::DarkGray)),
                Cell::from("stopped"),
                Cell::from(""),
            ]),
        },
        match &data.indexer {
            IndexerInfo::Running {
                indexed,
                total,
                failed,
                chunks,
                current_file,
                gpu_yield,
            } => {
                let (label, color) = if *gpu_yield {
                    ("yielding", Color::Yellow)
                } else {
                    ("indexing", Color::Green)
                };
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
                let detail = if *gpu_yield {
                    format!("{indexed}/{total} ({pct}%) · GPU yield")
                } else {
                    let fail_str = if *failed > 0 {
                        format!(" · {failed} failed")
                    } else {
                        String::new()
                    };
                    format!("{indexed}/{total} ({pct}%) · {chunks} chunks{fail_str} · {file_short}")
                };
                Row::new(vec![
                    Cell::from("Indexer").style(Style::default().fg(color)),
                    Cell::from("●".to_string()).style(Style::default().fg(color)),
                    Cell::from(label),
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
    let any_stall = data.fs_stalled_docs || data.fs_stalled_markdown || data.fs_stalled_catalog;
    let title = if any_stall {
        Line::from(vec![" History ".into(), "· NFS stall ".fg(Color::Red)])
    } else {
        Line::from(" History ")
    };
    let block = Block::new()
        .title(title)
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

// ── Entry point ─────────────────────────────────────────────────

pub async fn run() -> Result<()> {
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
        scribe_servers: vec![],
        distill_servers: vec![],
        qdrant_healthy: false,
        qdrant_url: String::new(),
        qdrant_version: String::new(),
        watcher: WatcherInfo::Stopped,
        indexer: IndexerInfo::Stopped,
        history: vec![],
        loading: true,
        fs_stalled_docs: false,
        fs_stalled_markdown: false,
        fs_stalled_catalog: false,
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
                        // if it stalled and we have previous data, keep the old value.
                        data.doc_counts = new_data.doc_counts.or(data.doc_counts);
                        data.markdown_counts = new_data.markdown_counts.or(data.markdown_counts);
                        data.catalog_count = new_data.catalog_count.or(data.catalog_count);
                        data.corrupted_count = new_data.corrupted_count.or(data.corrupted_count);
                        data.fs_stalled_docs = new_data.fs_stalled_docs;
                        data.fs_stalled_markdown = new_data.fs_stalled_markdown;
                        data.fs_stalled_catalog = new_data.fs_stalled_catalog;
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
