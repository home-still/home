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
    pdf_count: u64,
    pdf_bytes: u64,
    markdown_count: u64,
    markdown_bytes: u64,
    catalog_count: u64,
    corrupted_count: u64,
    embedded_docs: u64,
    embedded_chunks: u64,

    scribe_servers: Vec<ServiceStatus>,
    distill_servers: Vec<ServiceStatus>,
    qdrant_healthy: bool,
    qdrant_url: String,

    recent: Vec<RecentConversion>,
}

struct ServiceStatus {
    url: String,
    healthy: bool,
    detail: String,  // e.g. "(Cpu)" or model info
    version: String, // server version from /health
}

struct RecentConversion {
    title: String,
    pages: u64,
    duration_secs: f64,
    converted_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ── Data collection ─────────────────────────────────────────────

fn count_dir(dir: &Path, ext: &str) -> (u64, u64) {
    let mut count = 0u64;
    let mut bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == ext) {
                count += 1;
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (count, bytes)
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
                } else if path.extension().is_some_and(|e| e == ext) {
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
    let (pdf_count, pdf_bytes) = count_dir_recursive(&scribe_cfg.watch_dir, "pdf");
    let (markdown_count, markdown_bytes) = count_dir(&scribe_cfg.output_dir, "md");
    let (catalog_count, _) = count_dir(&scribe_cfg.catalog_dir, "yaml");
    let (corrupted_count, _) = count_dir(&scribe_cfg.corrupted_dir, "pdf");

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Scribe server health + readiness checks
    let mut scribe_servers = Vec::new();
    for url in &scribe_cfg.servers {
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
            let detail = if in_flight > 0 {
                format!("{in_flight} converting")
            } else {
                "idle".to_string()
            };
            scribe_servers.push(ServiceStatus {
                url: url.clone(),
                healthy: true,
                detail,
                version: health_version,
            });
        } else {
            scribe_servers.push(ServiceStatus {
                url: url.clone(),
                healthy: false,
                detail: String::new(),
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

    for url in &distill_cfg.servers {
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
        let healthy = health.is_some();

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
                version: health_version,
            });
        } else if healthy {
            distill_servers.push(ServiceStatus {
                url: url.clone(),
                healthy: true,
                detail: String::new(),
                version: health_version,
            });
        } else {
            distill_servers.push(ServiceStatus {
                url: url.clone(),
                healthy: false,
                detail: String::new(),
                version: String::new(),
            });
        }
    }

    // Recent conversions from catalog
    let recent = load_recent_conversions(&scribe_cfg.catalog_dir, 5);

    DashboardData {
        pdf_count,
        pdf_bytes,
        markdown_count,
        markdown_bytes,
        catalog_count,
        corrupted_count,
        embedded_docs,
        embedded_chunks,
        scribe_servers,
        distill_servers,
        qdrant_healthy,
        qdrant_url,
        recent,
    }
}

fn load_recent_conversions(catalog_dir: &Path, limit: usize) -> Vec<RecentConversion> {
    let mut entries: Vec<_> = std::fs::read_dir(catalog_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "yaml" || ext == "yml")
        })
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), mtime))
        })
        .collect();

    entries.sort_by(|a, b| b.1.cmp(&a.1));
    entries.truncate(limit);

    entries
        .into_iter()
        .filter_map(|(path, _)| {
            let contents = std::fs::read_to_string(&path).ok()?;
            let entry: hs_common::catalog::CatalogEntry =
                serde_yaml_ng::from_str(&contents).ok()?;
            let conversion = entry.conversion.as_ref()?;
            let title = entry.title.unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into()
            });
            let converted_at = chrono::DateTime::parse_from_rfc3339(&conversion.converted_at)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc));
            Some(RecentConversion {
                title,
                pages: conversion.total_pages,
                duration_secs: conversion.duration_secs as f64,
                converted_at,
            })
        })
        .collect()
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
        Constraint::Length(8), // pipeline
        Constraint::Length(1), // spacer
        Constraint::Length((data.scribe_servers.len() + data.distill_servers.len() + 3) as u16), // services
        Constraint::Length(1), // spacer
        Constraint::Min(4),    // recent
        Constraint::Length(1), // footer
    ])
    .split(frame.area());

    // Title
    frame.render_widget(Line::from(" home-still ").bold().centered(), outer[0]);

    // Pipeline section
    render_pipeline(frame, outer[1], data);

    // Services section
    render_services(frame, outer[3], data);

    // Recent conversions
    render_recent(frame, outer[5], data);

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
        .title(" Pipeline ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let pdf_to_md = if data.pdf_count > 0 {
        data.markdown_count as f64 / data.pdf_count as f64
    } else {
        0.0
    };
    let md_to_embed = if data.markdown_count > 0 {
        data.embedded_docs as f64 / data.markdown_count as f64
    } else {
        0.0
    };

    let rows = vec![
        Row::new(vec![
            Cell::from("PDFs"),
            Cell::from(format!("{:>6}", data.pdf_count)),
            Cell::from(format!("{:>8}", fmt_bytes(data.pdf_bytes))),
            Cell::from(""),
        ]),
        Row::new(vec![
            Cell::from("Markdown"),
            Cell::from(format!("{:>6}", data.markdown_count)),
            Cell::from(format!("{:>8}", fmt_bytes(data.markdown_bytes))),
            Cell::from(format!("{:>5.1}%", pdf_to_md * 100.0)),
        ]),
        Row::new(vec![
            Cell::from("Cataloged"),
            Cell::from(format!("{:>6}", data.catalog_count)),
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
            Cell::from(format!("{:>6}", data.corrupted_count))
                .style(Style::default().fg(Color::Red)),
            Cell::from(""),
            Cell::from(""),
        ]),
    ];

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(8),
            Constraint::Length(14),
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

    let mut rows = Vec::new();

    for svc in &data.scribe_servers {
        let (indicator, style) = if svc.healthy {
            ("●", Style::default().fg(Color::Green))
        } else {
            ("○", Style::default().fg(Color::Red))
        };
        rows.push(Row::new(vec![
            Cell::from("Scribe".to_string()),
            Cell::from(indicator).style(style),
            Cell::from(if svc.healthy { "running" } else { "stopped" }.to_string()),
            Cell::from(svc.url.clone()),
            Cell::from(svc.version.clone()).style(Style::default().fg(Color::DarkGray)),
        ]));
    }

    for svc in &data.distill_servers {
        let (indicator, style) = if svc.healthy {
            ("●", Style::default().fg(Color::Green))
        } else {
            ("○", Style::default().fg(Color::Red))
        };
        let detail = if svc.detail.is_empty() {
            svc.url.clone()
        } else {
            format!("{} {}", svc.url, svc.detail)
        };
        rows.push(Row::new(vec![
            Cell::from("Distill".to_string()),
            Cell::from(indicator).style(style),
            Cell::from(if svc.healthy { "running" } else { "stopped" }.to_string()),
            Cell::from(detail),
            Cell::from(svc.version.clone()).style(Style::default().fg(Color::DarkGray)),
        ]));
    }

    let (q_indicator, q_style) = if data.qdrant_healthy {
        ("●", Style::default().fg(Color::Green))
    } else {
        ("○", Style::default().fg(Color::Red))
    };
    rows.push(Row::new(vec![
        Cell::from("Qdrant".to_string()),
        Cell::from(q_indicator).style(q_style),
        Cell::from(
            if data.qdrant_healthy {
                "healthy"
            } else {
                "stopped"
            }
            .to_string(),
        ),
        Cell::from(data.qdrant_url.clone()),
        Cell::from(""),
    ]));

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(2),
            Constraint::Length(8),
            Constraint::Length(30),
            Constraint::Min(10),
        ],
    );

    frame.render_widget(table, inner);
}

fn render_recent(frame: &mut Frame, area: Rect, data: &DashboardData) {
    let block = Block::new()
        .title(" Recent conversions ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if data.recent.is_empty() {
        frame.render_widget(Line::from("  No conversions yet").dim(), inner);
        return;
    }

    let rows: Vec<Row> = data
        .recent
        .iter()
        .map(|r| {
            let title: String = r.title.chars().take(30).collect();
            let ago = r.converted_at.as_ref().map(fmt_ago).unwrap_or_default();
            Row::new(vec![
                Cell::from(title),
                Cell::from(format!("{:>3}pg", r.pages)),
                Cell::from(format!("{:.1}s", r.duration_secs)),
                Cell::from(ago),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(10),
        ],
    );

    frame.render_widget(table, inner);
}

// ── Entry point ─────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut last_collect = Instant::now() - Duration::from_secs(10); // force immediate collect
    let mut data = DashboardData {
        pdf_count: 0,
        pdf_bytes: 0,
        markdown_count: 0,
        markdown_bytes: 0,
        catalog_count: 0,
        corrupted_count: 0,
        embedded_docs: 0,
        embedded_chunks: 0,
        scribe_servers: vec![],
        distill_servers: vec![],
        qdrant_healthy: false,
        qdrant_url: String::new(),
        recent: vec![],
    };

    loop {
        // Refresh data every 3 seconds
        if last_collect.elapsed() >= Duration::from_secs(3) {
            data = collect_data().await;
            last_collect = Instant::now();
        }

        terminal.draw(|frame| render(frame, &data))?;

        // Poll for events (100ms timeout so we stay responsive)
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        _ => {}
                    }
                }
            }
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}
