use anyhow::{Context, Result};
use clap::Subcommand;
use hs_common::auth::client::is_cloud_url;
use hs_common::reporter::Reporter;
use hs_scribe::config::ScribeConfig;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use crate::scribe_pool::ScribePool;

const DEFAULT_SERVER: &str = "http://localhost:7433";

/// Create a ScribeClient, with auth headers if the URL is a cloud gateway.
/// `convert_timeout` caps each PDF conversion so a stuck server can't
/// pin the caller. Cloud path uses the auth-injected reqwest client; the
/// caller is responsible for configuring its timeouts (see
/// `AuthenticatedClient::build_reqwest_client`).
async fn make_scribe_client(
    url: &str,
    convert_timeout: std::time::Duration,
) -> Result<hs_scribe::client::ScribeClient> {
    if is_cloud_url(url) {
        let auth = hs_common::auth::client::AuthenticatedClient::from_default_path()
            .context("Cloud credentials not found. Run `hs cloud enroll` first.")?;
        let http = auth.build_reqwest_client().await?;
        Ok(hs_scribe::client::ScribeClient::new_with_client(url, http))
    } else {
        Ok(hs_scribe::client::ScribeClient::new_with_timeout(
            url,
            convert_timeout,
        ))
    }
}

/// Resolve the server list from CLI flag, gateway registry, config file, or default.
async fn resolve_servers(cli_server: Option<&str>) -> Vec<String> {
    if let Some(s) = cli_server {
        return vec![s.to_string()];
    }
    let config_servers = match ScribeConfig::load() {
        Ok(cfg) if !cfg.servers.is_empty() => cfg.servers,
        _ => vec![DEFAULT_SERVER.to_string()],
    };
    hs_common::service::registry::discover_or_fallback("scribe", config_servers).await
}
const LAYOUT_MODEL_URL: &str =
    "https://github.com/home-still/home/releases/download/v0.0.1-rc.39/pp-doclayoutv3.onnx";
const TABLE_MODEL_URL: &str =
    "https://github.com/home-still/home/releases/download/v0.0.1-rc.39/slanet-plus.onnx";

fn compose_yaml(has_gpu: bool) -> String {
    let gpu_section = if has_gpu {
        "    devices:\n      - nvidia.com/gpu=all\n"
    } else {
        ""
    };
    format!(
        r#"services:
  scribe:
    image: ghcr.io/home-still/hs-scribe-server:latest
    ports:
      - "7433:7433"
    volumes:
      - ${{MODELS_DIR}}:/models:ro
    environment:
      HS_SCRIBE_LAYOUT_MODEL_PATH: /models/pp-doclayoutv3.onnx
      HS_SCRIBE_TABLE_MODEL_PATH: /models/slanet-plus.onnx
      HS_SCRIBE_BACKEND: Ollama
      HS_SCRIBE_OLLAMA_URL: http://vlm:11434
      HS_SCRIBE_USE_CUDA: "{has_gpu}"
    command: ["hs-scribe-server", "--host", "0.0.0.0", "--port", "7433"]
    depends_on:
      vlm:
        condition: service_healthy
    restart: on-failure:3

  vlm:
    image: docker.io/ollama/ollama
{gpu_section}    volumes:
      - ${{OLLAMA_DATA}}:/root/.ollama
    healthcheck:
      test: ["CMD", "ollama", "list"]
      interval: 10s
      timeout: 5s
      retries: 30
      start_period: 30s
    restart: on-failure:3
"#
    )
}

/// Compose config for macOS Apple Silicon: native Ollama (Metal GPU) on host,
/// only the scribe server runs in a container.
fn compose_yaml_native_ollama(use_cuda: bool) -> String {
    format!(
        r#"services:
  scribe:
    image: ghcr.io/home-still/hs-scribe-server:latest
    ports:
      - "7433:7433"
    volumes:
      - ${{MODELS_DIR}}:/models:ro
    environment:
      HS_SCRIBE_LAYOUT_MODEL_PATH: /models/pp-doclayoutv3.onnx
      HS_SCRIBE_TABLE_MODEL_PATH: /models/slanet-plus.onnx
      HS_SCRIBE_BACKEND: Ollama
      HS_SCRIBE_OLLAMA_URL: http://host.docker.internal:11434
      HS_SCRIBE_USE_CUDA: "{use_cuda}"
    command: ["hs-scribe-server", "--host", "0.0.0.0", "--port", "7433"]
    extra_hosts:
      - "host.docker.internal:host-gateway"
    restart: on-failure:3
"#
    )
}

#[derive(Subcommand, Debug)]
pub enum ScribeCmd {
    /// Convert a PDF to markdown (sends to scribe server)
    Convert {
        /// Input PDF file
        input: PathBuf,
        /// Write markdown to file (default: stdout)
        #[arg(long = "out")]
        out_file: Option<PathBuf>,
        /// Server URL override
        #[arg(long)]
        server: Option<String>,
    },
    /// Set up everything: download models, start Docker services
    Init {
        /// Re-download model and recreate compose config
        #[arg(long)]
        force: bool,
        /// Dry run: report what's present/missing without changing anything
        #[arg(long)]
        check: bool,
    },
    /// Watch a directory for new PDFs and auto-convert to markdown
    Watch {
        #[command(subcommand)]
        action: Option<WatchAction>,
        /// Directory to watch for PDFs (default: current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Output directory for markdown files (default: <dir>/markdown)
        #[arg(long = "outdir")]
        outdir: Option<PathBuf>,
        /// Server URL override
        #[arg(long)]
        server: Option<String>,
        /// Internal: daemon child process (hidden)
        #[arg(long, hide = true)]
        daemon_child: bool,
    },
    /// Show status of a running watch service
    Status {
        /// Output directory to read status from
        #[arg(long = "dir")]
        status_dir: Option<PathBuf>,
    },
    /// Subscribe to `papers.ingested` on the configured event bus, convert
    /// each PDF via the scribe server, and upload the markdown back to
    /// storage. Event-driven replacement for the filesystem watcher.
    WatchEvents {
        /// Server URL override
        #[arg(long)]
        server: Option<String>,
    },
    /// Client-side inbox watcher. Sweeps `papers/manually_downloaded/` on
    /// the configured storage, relocates each file to `papers/<shard>/...`,
    /// and publishes `papers.ingested` on NATS so the server-side scribe
    /// can convert. Runs purely against the Storage trait — works on any
    /// host with S3 creds (or a local/NFS-mounted papers/ directory).
    ///
    /// No local conversion. No dependency on a reachable scribe server.
    /// Pair with `hs scribe watch-events` on the scribe host for the
    /// full ingest pipeline.
    Inbox {
        #[command(subcommand)]
        action: Option<InboxAction>,
    },
    /// Backfill catalog entries for markdown files that were converted before the catalog feature
    CatalogBackfill,
    /// Remove junk HTML papers (paywall pages, landing pages) from the collection
    CleanJunk {
        /// Dry run: show what would be removed without deleting
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum WatchAction {
    /// Start the watch daemon without showing the panel
    Start,
    /// Stop a running watch daemon
    Stop,
}

#[derive(Subcommand, Debug)]
pub enum InboxAction {
    /// Run the inbox watcher in the foreground (Ctrl+C to exit).
    /// Default when no subcommand is given.
    Run,
    /// Do one sweep of the inbox prefix and exit. Useful for testing.
    Sweep,
    /// Install a user-level daemon that runs the inbox watcher at login.
    /// macOS: LaunchAgent plist. Linux: systemd --user unit.
    Install,
    /// Remove the user-level daemon.
    Uninstall,
    /// Report the daemon's running state.
    Status,
    /// Internal: daemon child process (hidden).
    #[command(hide = true)]
    DaemonChild,
}

/// Internal actions for scribe server management.
/// Use `hs serve scribe start/stop` from the CLI.
pub enum ServerAction {
    Start,
    Stop,
}

pub async fn dispatch(cmd: ScribeCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        ScribeCmd::Convert {
            input,
            out_file,
            server,
        } => cmd_convert(input, out_file, server, reporter).await,
        ScribeCmd::Watch {
            action: Some(WatchAction::Start),
            dir,
            outdir,
            server,
            ..
        } => cmd_daemon_start(dir, outdir, server, reporter).await,
        ScribeCmd::Watch {
            action: Some(WatchAction::Stop),
            dir,
            ..
        } => cmd_daemon_stop(dir, reporter).await,
        ScribeCmd::Watch {
            action: None,
            dir,
            outdir,
            server,
            daemon_child,
        } => {
            if daemon_child {
                // Internal: running as daemon child process
                let watch_dir =
                    resolve_watch_dir(dir.as_ref().map(|p| p.to_str().unwrap_or_default()));
                crate::daemon::setup_daemon_child(&watch_dir)?;
                let result = cmd_watch(Some(watch_dir.clone()), outdir, server, reporter).await;
                crate::daemon::cleanup_daemon(&watch_dir);
                result
            } else {
                // Default: start daemon + attach live panel
                cmd_watch_attach(dir, outdir, server, reporter).await
            }
        }
        ScribeCmd::Status { status_dir } => cmd_status(status_dir, reporter).await,
        ScribeCmd::Init { force, check } => cmd_init(force, check).await,
        ScribeCmd::WatchEvents { server } => cmd_watch_events(server, reporter).await,
        ScribeCmd::Inbox { action } => {
            crate::scribe_inbox::dispatch(action.unwrap_or(InboxAction::Run), reporter).await
        }
        ScribeCmd::CatalogBackfill => cmd_catalog_backfill(reporter).await,
        ScribeCmd::CleanJunk { dry_run } => cmd_clean_junk(dry_run, reporter).await,
    }
}

pub(crate) async fn cmd_watch_events(
    server_override: Option<String>,
    _reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    use hs_common::service::pool::ServicePool;
    use hs_scribe::client::ScribeClient;
    use hs_scribe::config::ScribeConfig;
    use hs_scribe::event_watch::{convert_and_upload, run_subscriber};

    let cfg = ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage()?;
    let bus = cfg.build_event_bus().await?;

    let servers: Vec<String> = match server_override {
        Some(s) => vec![s],
        None if !cfg.servers.is_empty() => cfg.servers.clone(),
        None => vec![DEFAULT_SERVER.to_string()],
    };
    let convert_timeout = std::time::Duration::from_secs(cfg.convert_timeout_secs);
    let clients: Vec<ScribeClient> = servers
        .iter()
        .map(|u| ScribeClient::new_with_timeout(u, convert_timeout))
        .collect();
    let pool = Arc::new(ServicePool::new(clients));

    tracing::info!(
        servers = ?servers,
        convert_timeout_secs = cfg.convert_timeout_secs,
        "starting event-bus watcher with {}-server pool",
        servers.len()
    );

    let storage_for_handler = storage.clone();
    let bus_for_handler = bus.clone();
    let concurrency = pool.concurrency();
    run_subscriber(bus.clone(), storage.clone(), concurrency, move |event| {
        let storage = storage_for_handler.clone();
        let bus = bus_for_handler.clone();
        let pool = pool.clone();
        async move {
            // Dispatch retry: a /convert can fail mid-stream when a scribe's
            // link flaps (Wi-Fi jitter on big_mac). One fast retry on a
            // different host is cheap, and convert_and_upload is idempotent
            // via its head-check on the target markdown key. If both
            // attempts fail, the subscriber logs the full error chain and
            // gives up — operator reconciles via `hs pipeline catch-up`.
            let max_dispatch_attempts: u32 = 2;
            let mut last_err: Option<anyhow::Error> = None;
            for attempt in 1..=max_dispatch_attempts {
                let client = pool
                    .pick_server()
                    .await
                    .context("no ready scribe servers")?;
                tracing::info!(
                    server = %client.url(),
                    key = %event.key,
                    attempt,
                    "dispatching event"
                );
                match convert_and_upload(storage.as_ref(), client, bus.as_ref(), &event).await {
                    Ok(_) => return Ok(()),
                    Err(e) if attempt < max_dispatch_attempts => {
                        tracing::warn!(
                            server = %client.url(),
                            key = %event.key,
                            attempt,
                            error = %e,
                            "convert failed — retrying on a different server"
                        );
                        last_err = Some(e);
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(last_err.unwrap_or_else(|| anyhow::anyhow!("dispatch retries exhausted")))
        }
    })
    .await
}

// ── Convert ─────────────────────────────────────────────────────

async fn cmd_convert(
    input: PathBuf,
    out_file: Option<PathBuf>,
    server: Option<String>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let servers = resolve_servers(server.as_deref()).await;
    let convert_timeout = std::time::Duration::from_secs(
        ScribeConfig::load()
            .map(|c| c.convert_timeout_secs)
            .unwrap_or(900),
    );

    // Health check
    let check_stage = reporter.begin_stage("Connecting", None);
    if servers.len() == 1 {
        let url = &servers[0];
        check_stage.set_message(&format!("server at {url}"));
        let client = make_scribe_client(url, convert_timeout).await?;
        match client.health().await {
            Ok(_) => check_stage.finish_and_clear(),
            Err(e) => {
                check_stage.finish_failed("server not reachable");
                anyhow::bail!(
                    "Cannot reach scribe server at {url}: {e:#}\n\nRun `hs scribe init` to set up the server."
                );
            }
        }
    } else {
        check_stage.set_message(&format!("{} servers", servers.len()));
        let pool = ScribePool::new(&servers, convert_timeout);
        let results = pool.check_all().await;
        let reachable = results.iter().filter(|(_, ok)| *ok).count();
        if reachable == 0 {
            check_stage.finish_failed("no servers reachable");
            anyhow::bail!("No scribe servers are reachable. Check your config.");
        }
        check_stage.finish_and_clear();
    }

    let pdf_bytes =
        std::fs::read(&input).with_context(|| format!("Cannot read {}", input.display()))?;

    let stage: Arc<Box<dyn hs_common::reporter::StageHandle>> =
        Arc::new(reporter.begin_counted_stage("Converting", None));
    stage.set_message("sending PDF to server...");
    let stage_cb = Arc::clone(&stage);

    let on_progress = move |event: hs_scribe::client::ProgressEvent| {
        if event.total_pages > 0 {
            stage_cb.set_length(event.total_pages);
            stage_cb.set_position(event.page);
        }
        stage_cb.set_message(&format!("[{}] {}", event.stage, event.message));
    };

    let result = if servers.len() == 1 {
        let client = make_scribe_client(&servers[0], convert_timeout).await?;
        client
            .convert_with_progress(pdf_bytes, on_progress)
            .await
            .map(|md| (servers[0].clone(), md))
    } else {
        let pool = ScribePool::new(&servers, convert_timeout);
        pool.convert_one(pdf_bytes, on_progress).await
    };

    match &result {
        Ok(_) => stage.finish_with_message("done"),
        Err(e) => stage.finish_failed(&format!("{e:#}")),
    }

    let (_server, md) = result?;
    let (md, truncations) = hs_scribe::postprocess::clean_repetitions(&md);
    if truncations > 0 {
        tracing::info!("Cleaned {} repetition site(s)", truncations);
    }

    let page_offsets = hs_common::catalog::compute_page_offsets(&md);
    let total_pages = page_offsets.len() as u64;
    if hs_scribe::postprocess::qc_verdict(truncations, total_pages)
        == hs_scribe::postprocess::QcVerdict::RejectLoop
    {
        anyhow::bail!(
            "VLM repetition loop: {truncations} truncation site(s) across {total_pages} page(s). Output not persisted; re-run or investigate the source PDF.",
        );
    }

    // Resolve output: CLI flag > config output_dir > stdout
    let out = out_file.or_else(|| {
        ScribeConfig::load().ok().and_then(|cfg| {
            let dir = &cfg.output_dir;
            if dir.as_os_str().is_empty() || dir == std::path::Path::new(".") {
                None
            } else {
                let stem = input.file_stem()?;
                let path = hs_common::sharded_path(dir, &stem.to_string_lossy(), "md");
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).ok()?;
                }
                Some(path)
            }
        })
    });

    match out {
        Some(path) => std::fs::write(&path, &md)?,
        None => print!("{md}"),
    }
    Ok(())
}

// ── Watch Daemon ────────────────────────────────────────────────

const STATUS_FILE: &str = ".scribe-watch-status.json";

fn resolve_watch_dir(dir: Option<&str>) -> PathBuf {
    dir.map(PathBuf::from).unwrap_or_else(|| {
        let cfg = ScribeConfig::load().unwrap_or_default();
        cfg.watch_dir
    })
}

async fn cmd_daemon_start(
    dir: Option<PathBuf>,
    outdir: Option<PathBuf>,
    server: Option<String>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let watch_dir = resolve_watch_dir(dir.as_ref().map(|p| p.to_str().unwrap_or_default()));

    match crate::daemon::acquire_instance_lock(&watch_dir) {
        Ok(()) => {}
        Err(pid) => {
            reporter.status("Watch", &format!("daemon already running (PID {pid})"));
            return Ok(());
        }
    }

    let pid = crate::daemon::spawn_daemon(
        dir.as_ref().map(|p| p.to_str().unwrap_or_default()),
        outdir.as_ref().map(|p| p.to_str().unwrap_or_default()),
        server.as_deref(),
    )?;

    // Wait for PID file to appear (confirms child started)
    let pid_path = crate::daemon::pid_file_path(&watch_dir);
    for _ in 0..30 {
        if pid_path.exists() {
            reporter.status("Watch", &format!("daemon started (PID {pid})"));
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    reporter.warn("Daemon may not have started. Check logs at ~/.home-still/scribe-watch.log");
    Ok(())
}

async fn cmd_daemon_stop(dir: Option<PathBuf>, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let watch_dir = resolve_watch_dir(dir.as_ref().map(|p| p.to_str().unwrap_or_default()));

    match crate::daemon::stop_daemon(&watch_dir)? {
        Some(pid) => reporter.status("Watch", &format!("daemon stopped (PID {pid})")),
        None => reporter.warn("No watch daemon running"),
    }
    Ok(())
}

async fn cmd_watch_attach(
    dir: Option<PathBuf>,
    outdir: Option<PathBuf>,
    server: Option<String>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let watch_dir = resolve_watch_dir(dir.as_ref().map(|p| p.to_str().unwrap_or_default()));

    // Start daemon if not running
    let daemon_pid = match crate::daemon::acquire_instance_lock(&watch_dir) {
        Ok(()) => {
            let pid = crate::daemon::spawn_daemon(
                dir.as_ref().map(|p| p.to_str().unwrap_or_default()),
                outdir.as_ref().map(|p| p.to_str().unwrap_or_default()),
                server.as_deref(),
            )?;
            // Wait for PID file
            let pid_path = crate::daemon::pid_file_path(&watch_dir);
            for _ in 0..30 {
                if pid_path.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            pid
        }
        Err(pid) => pid, // already running
    };

    reporter.status(
        "Watch",
        &format!("attached to daemon (PID {daemon_pid}). Press q to detach, CTRL+C to stop."),
    );

    // Read and display status in a loop
    let scribe_cfg = ScribeConfig::load().unwrap_or_default();
    let output_dir = outdir.unwrap_or(scribe_cfg.output_dir);
    let status_path = output_dir.join(STATUS_FILE);

    // Enable raw mode for keypress detection
    let raw_enabled = crossterm::terminal::enable_raw_mode().is_ok();

    let result = loop {
        // Poll for keypress with short timeout (stays responsive)
        if raw_enabled {
            if crossterm::event::poll(std::time::Duration::from_millis(200)).unwrap_or(false) {
                if let Ok(crossterm::event::Event::Key(key)) = crossterm::event::read() {
                    if key.kind != crossterm::event::KeyEventKind::Press {
                        continue;
                    }
                    use crossterm::event::KeyCode;
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            let _ = crossterm::terminal::disable_raw_mode();
                            reporter.status(
                                "Watch",
                                &format!(
                                    "detached. Daemon running in background (PID {daemon_pid})"
                                ),
                            );
                            break Ok(());
                        }
                        KeyCode::Char('c')
                            if key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            let _ = crossterm::terminal::disable_raw_mode();
                            reporter.status("Watch", "stopping daemon...");
                            let _ = crate::daemon::stop_daemon(&watch_dir);
                            reporter.status("Watch", "daemon stopped");
                            break Ok(());
                        }
                        _ => {}
                    }
                }
            }
        } else {
            // No raw mode — just sleep
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        // Check if daemon is still alive
        if !crate::daemon::is_process_alive(daemon_pid) {
            reporter.status("Watch", "daemon exited");
            break Ok(());
        }

        // Read and display status — only show updates from our daemon
        if let Ok(contents) = std::fs::read_to_string(&status_path) {
            if let Ok(status) = serde_json::from_str::<serde_json::Value>(&contents) {
                let file_pid = status["pid"].as_u64().unwrap_or(0) as u32;
                if file_pid == daemon_pid {
                    let p = status["processing"].as_u64().unwrap_or(0);
                    let q = status["queued"].as_u64().unwrap_or(0);
                    let c = status["completed"].as_u64().unwrap_or(0);
                    let f = status["failed"].as_u64().unwrap_or(0);
                    let _ = crossterm::terminal::disable_raw_mode();
                    reporter.status(
                        "Watch",
                        &format!("{p} processing, {q} queued, {c} completed, {f} failed"),
                    );
                    if raw_enabled {
                        let _ = crossterm::terminal::enable_raw_mode();
                    }
                }
            }
        }
    };

    // Always restore terminal mode
    if raw_enabled {
        let _ = crossterm::terminal::disable_raw_mode();
    }

    result
}

/// Check if a path is a document to process (PDF or HTML, not a macOS resource fork or temp file).
fn is_processable_document(path: &std::path::Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext != "pdf" && ext != "html" && ext != "htm" && ext != "epub" {
        return false;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    // Skip macOS resource forks (._filename.pdf)
    if name.starts_with("._") {
        return false;
    }
    // Skip temp files from atomic downloads
    if name.ends_with(".tmp") {
        return false;
    }
    // Skip if .tmp companion exists (download in progress)
    if path.with_extension("pdf.tmp").exists() {
        return false;
    }
    // Skip files already in a corrupted/ directory
    if path.to_string_lossy().contains("/corrupted/") {
        return false;
    }
    true
}

/// Quick validation: check PDF magic bytes and minimum size.
fn validate_pdf_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 100 && bytes.starts_with(b"%PDF")
}

/// Check if bytes look like HTML content (for .pdf files that are actually HTML).
fn looks_like_html(bytes: &[u8]) -> bool {
    let prefix = &bytes[..bytes.len().min(512)];
    let lower = String::from_utf8_lossy(prefix).to_lowercase();
    lower.contains("<!doctype html") || lower.contains("<html") || lower.contains("<head")
}

/// Move a corrupt/invalid file to the corrupted directory.
fn quarantine_file(path: &std::path::Path, corrupted_dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(corrupted_dir);
    let dest = corrupted_dir.join(path.file_name().unwrap_or_default());
    if let Err(e) = std::fs::rename(path, &dest) {
        eprintln!("warning: Failed to quarantine {}: {e}", path.display());
    }
}

/// Read an EPUB as concatenated spine-ordered HTML.
/// EPUB is a zip of XHTML chapters; we walk the spine (reading order) and
/// combine the chapter bodies so the existing HTML→markdown path can handle it.
fn epub_to_html(epub_path: &std::path::Path) -> std::io::Result<String> {
    use epub::doc::EpubDoc;
    let mut doc =
        EpubDoc::new(epub_path).map_err(|e| std::io::Error::other(format!("invalid EPUB: {e}")))?;
    Ok(epub_doc_to_html(&mut doc))
}

/// In-memory variant: unpack an EPUB from its raw bytes. Used by the
/// client-side inbox watcher, which reads the source from S3 and has
/// no path to pass. Shares the spine-walking logic with `epub_to_html`.
pub(crate) fn epub_bytes_to_html(bytes: Vec<u8>) -> std::io::Result<String> {
    use epub::doc::EpubDoc;
    let mut doc = EpubDoc::from_reader(std::io::Cursor::new(bytes))
        .map_err(|e| std::io::Error::other(format!("invalid EPUB: {e}")))?;
    Ok(epub_doc_to_html(&mut doc))
}

fn epub_doc_to_html<R: std::io::Read + std::io::Seek>(doc: &mut epub::doc::EpubDoc<R>) -> String {
    let mut combined = String::from("<html><body>");
    loop {
        if let Some(content) = doc.get_current_str() {
            combined.push_str(&content.0);
        }
        if !doc.go_next() {
            break;
        }
    }
    combined.push_str("</body></html>");
    combined
}

/// Read a file as HTML, transparently handling `.epub` by unpacking its spine.
fn read_as_html(path: &std::path::Path) -> std::io::Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "epub" {
        epub_to_html(path)
    } else {
        std::fs::read_to_string(path)
    }
}

/// Convert an HTML paper and save the result, updating catalog.
/// No server needed — runs locally.
async fn convert_html_and_save(
    html_path: &std::path::Path,
    storage: &dyn hs_common::storage::Storage,
    markdown_prefix: &str,
    catalog_prefix: &str,
    papers_prefix: &str,
    reporter: &Arc<dyn Reporter>,
    stats: &WatchStats,
) {
    use std::sync::atomic::Ordering::Relaxed;

    let start = std::time::Instant::now();
    let stem = html_path.file_stem().unwrap_or_default().to_string_lossy();
    let md_key = format!(
        "{}/{}",
        markdown_prefix.trim_end_matches('/'),
        hs_common::sharded_key(&stem, "md")
    );

    stats.queued.fetch_sub(1, Relaxed);
    stats.processing.fetch_add(1, Relaxed);

    let html = match read_as_html(html_path) {
        Ok(h) => h,
        Err(e) => {
            reporter.warn(&format!("{stem}: Cannot read input: {e}"));
            stats.processing.fetch_sub(1, Relaxed);
            stats.failed.fetch_add(1, Relaxed);
            return;
        }
    };

    let md = hs_scribe::html::convert_html_to_markdown(&html);
    let page_offsets = crate::catalog::compute_page_offsets(&md);
    let total_pages = page_offsets.len() as u64;

    stats.processing.fetch_sub(1, Relaxed);

    let md_bytes = md.clone().into_bytes();
    if let Err(e) = storage.put(&md_key, md_bytes).await {
        reporter.warn(&format!("{stem}: Write failed ({md_key}): {e}"));
        stats.failed.fetch_add(1, Relaxed);
        return;
    }

    stats.completed.fetch_add(1, Relaxed);

    if let Err(e) = hs_common::catalog::update_conversion_catalog_via(
        storage,
        catalog_prefix,
        &stem,
        "html-parser",
        start.elapsed().as_secs_f64(),
        total_pages.max(1),
        page_offsets,
        &md_key,
    )
    .await
    {
        reporter.warn(&format!("{stem}: catalog update failed: {e}"));
    }

    // After successful conversion, move source out of manually_downloaded/ (if applicable)
    relocate_from_manual_dir(html_path, storage, papers_prefix).await;
}

/// If `source_path` sits in a `manually_downloaded/` folder, ensure the file
/// is in S3 under `{papers_prefix}/{shard}/{stem}.{ext}` (so remote consumers
/// like mac_air see it via rclone-NFS), then move the local copy into the
/// sharded layout (e.g. `papers/10/stem.pdf`). Source files that are already
/// in the sharded layout are left alone.
///
/// Files that fail to convert stay in `manually_downloaded/` so users can see
/// what hasn't been processed yet.
async fn relocate_from_manual_dir(
    source_path: &std::path::Path,
    storage: &dyn hs_common::storage::Storage,
    papers_prefix: &str,
) {
    // Only relocate if the parent directory is "manually_downloaded"
    let is_manual = source_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        == Some("manually_downloaded");
    if !is_manual {
        return;
    }

    // Papers root is the grandparent (papers/manually_downloaded/file.pdf)
    let papers_root = match source_path.parent().and_then(|p| p.parent()) {
        Some(p) => p,
        None => return,
    };

    let stem = match source_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return,
    };
    let ext = match source_path.extension().and_then(|e| e.to_str()) {
        Some(e) => e,
        None => return,
    };

    // Mirror to S3 so remote consumers see this file. Skip if already present.
    let s3_key = format!(
        "{}/{}",
        papers_prefix.trim_end_matches('/'),
        hs_common::sharded_key(stem, ext)
    );
    if !storage.exists(&s3_key).await.unwrap_or(false) {
        match std::fs::read(source_path) {
            Ok(bytes) => {
                if let Err(e) = storage.put(&s3_key, bytes).await {
                    tracing::warn!(
                        "Failed to upload {} to {s3_key}: {e}",
                        source_path.display()
                    );
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to read {} for S3 upload: {e}",
                    source_path.display()
                );
                return;
            }
        }
    }

    let target = hs_common::sharded_path(papers_root, stem, ext);
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("Failed to create shard dir {}: {e}", parent.display());
            return;
        }
    }
    // Don't overwrite if target already exists (original is still the canonical copy).
    // But if source and target are byte-identical (same size), the drop-zone copy is a
    // redundant duplicate — remove it so the folder stays clean. Different sizes mean
    // the user dropped a distinct version; leave it for manual inspection.
    if target.exists() {
        let src_size = std::fs::metadata(source_path).map(|m| m.len()).ok();
        let tgt_size = std::fs::metadata(&target).map(|m| m.len()).ok();
        if src_size.is_some() && src_size == tgt_size {
            if let Err(e) = std::fs::remove_file(source_path) {
                tracing::warn!("Failed to remove duplicate {}: {e}", source_path.display());
            }
        }
        return;
    }
    if let Err(e) = std::fs::rename(source_path, &target) {
        tracing::warn!(
            "Failed to move {} → {}: {e}",
            source_path.display(),
            target.display()
        );
    }
}

/// Atomic file write: write to temp file in same directory, then rename.
/// Safe on NFS — rename within same directory is atomic.
fn atomic_write(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content)?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

struct WatchStats {
    processing: std::sync::atomic::AtomicUsize,
    queued: std::sync::atomic::AtomicUsize,
    completed: std::sync::atomic::AtomicUsize,
    failed: std::sync::atomic::AtomicUsize,
}

impl WatchStats {
    fn new() -> Self {
        use std::sync::atomic::AtomicUsize;
        Self {
            processing: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
        }
    }

    fn summary(&self) -> String {
        use std::sync::atomic::Ordering::Relaxed;
        let p = self.processing.load(Relaxed);
        let q = self.queued.load(Relaxed);
        let c = self.completed.load(Relaxed);
        let f = self.failed.load(Relaxed);
        format!("{p} processing, {q} queued, {c} completed, {f} failed")
    }

    fn write_status_file(&self, path: &std::path::Path, watch_dir: &str, output_dir: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        let json = serde_json::json!({
            "pid": std::process::id(),
            "processing": self.processing.load(Relaxed),
            "queued": self.queued.load(Relaxed),
            "completed": self.completed.load(Relaxed),
            "failed": self.failed.load(Relaxed),
            "watch_dir": watch_dir,
            "output_dir": output_dir,
        });
        let _ = atomic_write(
            path,
            serde_json::to_string_pretty(&json)
                .unwrap_or_default()
                .as_bytes(),
        );
    }
}

async fn cmd_watch(
    dir: Option<PathBuf>,
    output: Option<PathBuf>,
    server: Option<String>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    use notify::{PollWatcher, RecursiveMode, Watcher};
    use std::time::Duration;

    let servers = resolve_servers(server.as_deref()).await;
    let scribe_cfg = ScribeConfig::load().unwrap_or_default();
    let corrupted_dir = scribe_cfg.corrupted_dir.clone();
    let storage = scribe_cfg.build_storage()?;
    // Matches the prefixes MCP and distill expect — see hs-mcp/src/main.rs and
    // distill_cmd.rs' read_catalog_entry_via(storage, "catalog", …).
    let markdown_prefix: Arc<str> = Arc::from("markdown");
    let catalog_prefix: Arc<str> = Arc::from("catalog");
    let papers_prefix: Arc<str> = Arc::from("papers");

    // Resolve dirs: CLI flag > config > defaults
    let watch_dir = dir
        .or_else(|| {
            let d = &scribe_cfg.watch_dir;
            if d.as_os_str().is_empty() || d == std::path::Path::new(".") {
                None
            } else {
                Some(d.clone())
            }
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let output_dir = output
        .or_else(|| {
            let d = &scribe_cfg.output_dir;
            if d.as_os_str().is_empty() || d == std::path::Path::new("markdown") {
                None
            } else {
                Some(d.clone())
            }
        })
        .unwrap_or_else(|| watch_dir.join("markdown"));
    std::fs::create_dir_all(&output_dir)?;

    // Create a drop folder for manually downloaded papers
    let manual_dir = watch_dir.join("manually_downloaded");
    std::fs::create_dir_all(&manual_dir)?;

    // Health check
    for s in &servers {
        reporter.status("Server", s);
    }
    let pool = Arc::new(ScribePool::new(
        &servers,
        std::time::Duration::from_secs(scribe_cfg.convert_timeout_secs),
    ));
    let spawn_sem = Arc::new(tokio::sync::Semaphore::new(pool.concurrency()));
    let results = pool.check_all().await;
    let reachable = results.iter().filter(|(_, ok)| *ok).count();
    if reachable == 0 {
        anyhow::bail!("No scribe servers reachable. Run `hs scribe server start` first.");
    }

    // Auto-trigger: ensure distill index daemon is running
    if crate::distill_cmd::ensure_index_running().await {
        reporter.status("Pipeline", "distill indexer running");
    }

    let stats = Arc::new(WatchStats::new());
    let status_path = output_dir.join(STATUS_FILE);
    let watch_dir_str = watch_dir.display().to_string();
    let output_dir_str = output_dir.display().to_string();

    reporter.status(
        "Watching",
        &format!(
            "{} → {} ({} server{})",
            watch_dir.display(),
            output_dir.display(),
            reachable,
            if reachable == 1 { "" } else { "s" }
        ),
    );

    // CTRL+C handler — sets flag so the blocking recv_timeout loop can exit
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_flag = Arc::clone(&shutdown);
    let _ = ctrlc::set_handler(move || {
        shutdown_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    // Initial scan: queue existing PDFs whose markdown isn't already in storage.
    // The existence check moved into each spawned task since storage.exists is async.
    {
        let all_docs = {
            let mut docs = hs_common::collect_files_recursive(&watch_dir, "pdf");
            docs.extend(hs_common::collect_files_recursive(&watch_dir, "html"));
            docs.extend(hs_common::collect_files_recursive(&watch_dir, "htm"));
            docs.extend(hs_common::collect_files_recursive(&watch_dir, "epub"));
            docs
        };
        for path in all_docs {
            if !is_processable_document(&path) {
                continue;
            }
            let is_html = path
                .extension()
                .is_some_and(|e| e == "html" || e == "htm" || e == "epub");
            let pool = Arc::clone(&pool);
            let corrupted_dir = corrupted_dir.clone();
            let storage = storage.clone();
            let markdown_prefix = markdown_prefix.clone();
            let catalog_prefix = catalog_prefix.clone();
            let papers_prefix = papers_prefix.clone();
            let reporter = Arc::clone(reporter);
            let stats = Arc::clone(&stats);
            let sem = Arc::clone(&spawn_sem);
            tokio::spawn(async move {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                let md_key = format!(
                    "{}/{}",
                    markdown_prefix.trim_end_matches('/'),
                    hs_common::sharded_key(&stem, "md")
                );
                if storage.exists(&md_key).await.unwrap_or(false) {
                    // Already converted — still relocate from manually_downloaded/ if applicable.
                    relocate_from_manual_dir(&path, &*storage, &papers_prefix).await;
                    return;
                }
                stats
                    .queued
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if is_html {
                    convert_html_and_save(
                        &path,
                        &*storage,
                        &markdown_prefix,
                        &catalog_prefix,
                        &papers_prefix,
                        &reporter,
                        &stats,
                    )
                    .await;
                } else {
                    let _permit = sem.acquire_owned().await;
                    convert_and_save_pool(
                        &pool,
                        &path,
                        &corrupted_dir,
                        &*storage,
                        &markdown_prefix,
                        &catalog_prefix,
                        &papers_prefix,
                        &reporter,
                        &stats,
                    )
                    .await;
                }
            });
        }
    }

    // Write initial status after scan so counts are populated
    stats.write_status_file(&status_path, &watch_dir_str, &output_dir_str);

    // PollWatcher works on NFS/CIFS/FUSE — inotify only works on local filesystems
    let (tx, rx) = std::sync::mpsc::channel();
    let poll_config = notify::Config::default().with_poll_interval(Duration::from_secs(5));
    let mut watcher = PollWatcher::new(tx, poll_config)?;
    watcher.watch(&watch_dir, RecursiveMode::Recursive)?;

    let mut last_summary = String::new();
    let mut ticks_since_status_write: u32 = 0;

    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            reporter.status("Watch", "shutting down...");
            break;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(event)) => {
                for path in &event.paths {
                    if !is_processable_document(path) {
                        continue;
                    }
                    let is_html = path
                        .extension()
                        .is_some_and(|e| e == "html" || e == "htm" || e == "epub");
                    let pool = Arc::clone(&pool);
                    let path = path.clone();
                    let corrupted_dir = corrupted_dir.clone();
                    let storage = storage.clone();
                    let markdown_prefix = markdown_prefix.clone();
                    let catalog_prefix = catalog_prefix.clone();
                    let papers_prefix = papers_prefix.clone();
                    let reporter = Arc::clone(reporter);
                    let stats = Arc::clone(&stats);
                    let sem = Arc::clone(&spawn_sem);
                    tokio::spawn(async move {
                        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                        let md_key = format!(
                            "{}/{}",
                            markdown_prefix.trim_end_matches('/'),
                            hs_common::sharded_key(&stem, "md")
                        );
                        if storage.exists(&md_key).await.unwrap_or(false) {
                            // Already converted — still relocate from manually_downloaded/ if applicable.
                            relocate_from_manual_dir(&path, &*storage, &papers_prefix).await;
                            return;
                        }
                        stats
                            .queued
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if is_html {
                            convert_html_and_save(
                                &path,
                                &*storage,
                                &markdown_prefix,
                                &catalog_prefix,
                                &papers_prefix,
                                &reporter,
                                &stats,
                            )
                            .await;
                        } else {
                            let _permit = sem.acquire_owned().await;
                            convert_and_save_pool(
                                &pool,
                                &path,
                                &corrupted_dir,
                                &*storage,
                                &markdown_prefix,
                                &catalog_prefix,
                                &papers_prefix,
                                &reporter,
                                &stats,
                            )
                            .await;
                        }
                    });
                }
            }
            Ok(Err(e)) => {
                reporter.warn(&format!("Watch error: {e}"));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Update summary line if stats changed
        let summary = stats.summary();
        if summary != last_summary {
            reporter.status("Watch", &summary);
            last_summary = summary;
            ticks_since_status_write = 0;
        }

        // Write status file every ~2 seconds (4 ticks at 500ms)
        ticks_since_status_write += 1;
        if ticks_since_status_write >= 4 {
            stats.write_status_file(&status_path, &watch_dir_str, &output_dir_str);
            ticks_since_status_write = 0;
        }
    }

    // Final status write
    stats.write_status_file(&status_path, &watch_dir_str, &output_dir_str);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn convert_and_save_pool(
    pool: &ScribePool,
    pdf_path: &std::path::Path,
    corrupted_dir: &std::path::Path,
    storage: &dyn hs_common::storage::Storage,
    markdown_prefix: &str,
    catalog_prefix: &str,
    papers_prefix: &str,
    reporter: &Arc<dyn Reporter>,
    stats: &WatchStats,
) {
    use std::sync::atomic::Ordering::Relaxed;

    let start_time = std::time::Instant::now();
    let stem = pdf_path.file_stem().unwrap_or_default().to_string_lossy();
    let md_key = format!(
        "{}/{}",
        markdown_prefix.trim_end_matches('/'),
        hs_common::sharded_key(&stem, "md")
    );

    // Read and validate while still in "queued" state (no progress bar yet)
    let pdf_bytes = match std::fs::read(pdf_path) {
        Ok(b) => b,
        Err(e) => {
            reporter.warn(&format!("{stem}: Cannot read: {e}"));
            stats.queued.fetch_sub(1, Relaxed);
            stats.failed.fetch_add(1, Relaxed);
            return;
        }
    };

    if !validate_pdf_bytes(&pdf_bytes) {
        // Check if it's actually an HTML paper saved with .pdf extension
        if looks_like_html(&pdf_bytes) {
            let html_path = pdf_path.with_extension("html");
            if std::fs::rename(pdf_path, &html_path).is_ok() {
                reporter.warn(&format!(
                    "{stem}: HTML in .pdf → renamed to .html (will convert on next scan)"
                ));
                stats.queued.fetch_sub(1, Relaxed);
                return;
            }
        }
        reporter.warn(&format!("{stem}: invalid PDF → quarantined"));
        quarantine_file(pdf_path, corrupted_dir);
        stats.queued.fetch_sub(1, Relaxed);
        stats.failed.fetch_add(1, Relaxed);
        return;
    }

    // Queued → processing happens here (before pool.convert_one blocks on semaphore)
    // But we DON'T create a progress bar — it's created lazily on first progress event
    stats.queued.fetch_sub(1, Relaxed);
    stats.processing.fetch_add(1, Relaxed);

    let stage: Arc<std::sync::Mutex<Option<Box<dyn hs_common::reporter::StageHandle>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let server_tag: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
    let total_pages_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stage_cb = Arc::clone(&stage);
    let server_tag_cb = Arc::clone(&server_tag);
    let total_pages_cb = Arc::clone(&total_pages_counter);
    let reporter_cb = Arc::clone(reporter);
    let stem_cb = stem.to_string();

    let result = pool
        .convert_one(pdf_bytes, move |event| {
            let mut guard = stage_cb.lock().unwrap();
            // Capture server assignment from the first "server" event
            if event.stage == "server" {
                let mut tag = server_tag_cb.lock().unwrap();
                *tag = event.message.clone(); // "→ host:port"
            }
            if guard.is_none() {
                *guard = Some(reporter_cb.begin_counted_stage(&stem_cb, None));
            }
            if let Some(ref s) = *guard {
                if event.total_pages > 0 {
                    s.set_length(event.total_pages);
                    s.set_position(event.page);
                    total_pages_cb.store(event.total_pages, std::sync::atomic::Ordering::Relaxed);
                }
                let tag = server_tag_cb.lock().unwrap();
                // Keep message short to avoid wrapping: server + stage only
                let short_stage = match event.stage.as_str() {
                    "layout" => "layout",
                    "vlm" => "vlm",
                    "parse" => "parse",
                    "done" => "done",
                    "server" => "",
                    other => other,
                };
                if tag.is_empty() {
                    s.set_message(short_stage);
                } else if short_stage.is_empty() {
                    s.set_message(&tag);
                } else {
                    s.set_message(&format!("{tag} {short_stage}"));
                }
            }
        })
        .await;

    stats.processing.fetch_sub(1, Relaxed);
    // Take the stage handle out of the shared Mutex so we can .await freely —
    // MutexGuard isn't Send and holding it across an await breaks tokio::spawn.
    let stage_handle = stage.lock().unwrap().take();
    match result {
        Ok((server_url, md)) => {
            let (md, truncations) = hs_scribe::postprocess::clean_repetitions(&md);
            if truncations > 0 {
                tracing::info!("{md_key}: cleaned {truncations} repetition site(s)");
            }

            let qc_page_offsets = crate::catalog::compute_page_offsets(&md);
            let qc_total_pages = qc_page_offsets.len() as u64;
            if hs_scribe::postprocess::qc_verdict(truncations, qc_total_pages)
                == hs_scribe::postprocess::QcVerdict::RejectLoop
            {
                reporter.warn(&format!(
                    "{stem}: VLM repetition loop ({truncations} site(s), {qc_total_pages} pg) → quarantined"
                ));
                quarantine_file(pdf_path, corrupted_dir);
                stats.failed.fetch_add(1, Relaxed);
                if let Some(ref s) = stage_handle {
                    s.finish_failed("repetition_loop");
                }
                return;
            }

            let md_bytes = md.clone().into_bytes();
            if let Err(e) = storage.put(&md_key, md_bytes).await {
                if let Some(ref s) = stage_handle {
                    s.finish_failed(&format!("Write failed ({md_key}): {e}"));
                }
                stats.failed.fetch_add(1, Relaxed);
            } else {
                let short_server = server_url
                    .strip_prefix("http://")
                    .or_else(|| server_url.strip_prefix("https://"))
                    .unwrap_or(&server_url);
                if let Some(ref s) = stage_handle {
                    let elapsed = start_time.elapsed();
                    let secs = elapsed.as_secs();
                    let duration = if secs >= 60 {
                        format!("{}m{}s", secs / 60, secs % 60)
                    } else {
                        format!("{secs}s")
                    };
                    let out_name = md_key.rsplit('/').next().unwrap_or(&md_key);
                    s.finish_with_message(&format!("→ {out_name} [{short_server}] ({duration})"));
                }
                stats.completed.fetch_add(1, Relaxed);

                // Write catalog entry with conversion metadata
                let page_offsets = crate::catalog::compute_page_offsets(&md);
                let total_pages = total_pages_counter.load(std::sync::atomic::Ordering::Relaxed);
                let duration_secs = start_time.elapsed().as_secs_f64();

                if let Err(e) = hs_common::catalog::update_conversion_catalog_via(
                    storage,
                    catalog_prefix,
                    &stem,
                    short_server,
                    duration_secs,
                    total_pages,
                    page_offsets,
                    &md_key,
                )
                .await
                {
                    reporter.warn(&format!("{stem}: catalog update failed: {e}"));
                }

                // After successful conversion, move source out of manually_downloaded/
                relocate_from_manual_dir(pdf_path, storage, papers_prefix).await;
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            stats.failed.fetch_add(1, Relaxed);
            if msg.contains("FormatError") || msg.contains("PdfiumLibrary") {
                if let Some(ref s) = stage_handle {
                    s.finish_and_clear();
                }
                reporter.warn(&format!("{stem}: server rejected PDF → quarantined"));
                quarantine_file(pdf_path, corrupted_dir);
            } else if let Some(ref s) = stage_handle {
                s.finish_failed(&msg);
            }
        }
    }
}

// ── Status ──────────────────────────────────────────────────────

async fn cmd_status(output: Option<PathBuf>, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let scribe_cfg = ScribeConfig::load().unwrap_or_default();
    let output_dir = output.unwrap_or(scribe_cfg.output_dir);
    let status_path = output_dir.join(STATUS_FILE);

    if !status_path.exists() {
        reporter.warn("No watch service status found.");
        reporter.warn(&format!(
            "Expected status file at: {}",
            status_path.display()
        ));
        reporter.warn("Start a watch service with: hs scribe watch");
        return Ok(());
    }

    let contents = std::fs::read_to_string(&status_path)
        .with_context(|| format!("Cannot read {}", status_path.display()))?;
    let status: serde_json::Value =
        serde_json::from_str(&contents).context("Invalid status file")?;

    let processing = status["processing"].as_u64().unwrap_or(0);
    let queued = status["queued"].as_u64().unwrap_or(0);
    let completed = status["completed"].as_u64().unwrap_or(0);
    let failed = status["failed"].as_u64().unwrap_or(0);
    let watch_dir = status["watch_dir"].as_str().unwrap_or("?");
    let out_dir = status["output_dir"].as_str().unwrap_or("?");

    reporter.status("Watch dir", watch_dir);
    reporter.status("Output dir", out_dir);
    reporter.status(
        "Status",
        &format!(
            "{processing} processing, {queued} queued, {completed} completed, {failed} failed"
        ),
    );

    Ok(())
}

// ── Init ────────────────────────────────────────────────────────

async fn cmd_init(force: bool, check: bool) -> Result<()> {
    cmd_init_inner(force, check, false).await
}

async fn cmd_init_inner(force: bool, check: bool, prereqs_only: bool) -> Result<()> {
    // Step 1: Check container runtime (auto-install on macOS)
    eprintln!("[1/5] Checking container runtime...");
    let mut compose = ComposeCmd::detect().await;

    if compose.is_none() && cfg!(target_os = "macos") && check_command("brew", &["--version"]).await
    {
        eprintln!("       Not found — installing via Homebrew...");
        let steps: &[(&str, &[&str])] = &[
            ("brew", &["install", "podman", "docker-compose"]),
            ("podman", &["machine", "init", "--now"]),
        ];
        for (cmd, args) in steps {
            eprintln!("       Running: {} {}", cmd, args.join(" "));
            let status = tokio::process::Command::new(cmd)
                .args(*args)
                .status()
                .await?;
            if !status.success() {
                anyhow::bail!("{} {} failed", cmd, args[0]);
            }
        }
        compose = ComposeCmd::detect().await;
    }

    if compose.is_none() {
        let instructions = if cfg!(target_os = "macos") {
            "Container runtime not found. Install Homebrew first:\n  \
             https://brew.sh\n\n\
             Then re-run: hs scribe init\n"
        } else if cfg!(target_os = "linux") {
            "Docker/Podman not found. Install with:\n\n  \
             Arch:   sudo pacman -S podman podman-compose podman-docker\n  \
             Ubuntu: https://docs.docker.com/get-docker/\n  \
             Fedora: sudo dnf install podman podman-compose podman-docker\n"
        } else {
            "Docker not found.\n  Install: https://docs.docker.com/get-docker/\n"
        };
        anyhow::bail!("{}", instructions);
    }
    let compose = compose.unwrap();

    // On macOS, ensure podman machine is running
    if cfg!(target_os = "macos") && check_command("podman", &["--version"]).await {
        let machine_running = check_command("podman", &["machine", "info"]).await;
        if !machine_running {
            eprintln!("       Starting podman machine...");
            let _ = tokio::process::Command::new("podman")
                .args(["machine", "init", "--now"])
                .status()
                .await;
        }
    }

    eprintln!(
        "       OK ({} {})",
        compose.bin,
        compose.args_prefix.join(" ")
    );

    // Step 2: Detect GPU / Apple Silicon / Linux NVIDIA
    let has_nvidia = check_command("nvidia-smi", &[]).await;
    let use_native_ollama = should_use_native_ollama(has_nvidia);
    let has_gpu;

    if use_native_ollama {
        if cfg!(target_os = "linux") {
            // Linux + NVIDIA GPU: native Ollama with CUDA
            has_gpu = true;
            eprintln!("[2/5] NVIDIA GPU detected — using native Ollama (CUDA)...");

            // Check VRAM
            if let Some(free_vram) = check_nvidia_vram_mib().await {
                if free_vram < 2500 {
                    anyhow::bail!(
                        "Insufficient GPU VRAM: {} MiB free, need ≥2500 MiB for GLM-OCR.\n\
                         Free VRAM by closing other GPU applications or use a larger GPU.",
                        free_vram
                    );
                }
                eprintln!("       {} MiB VRAM available", free_vram);
            }

            // Install Ollama if not present
            if !check_command("ollama", &["--version"]).await {
                install_ollama_linux().await?;
            }

            // Configure auto-unload
            configure_ollama_keepalive().await?;

            // Start systemd service
            if !check_ollama_running().await {
                ensure_ollama_systemd().await?;
            }
            eprintln!("       OK (native Ollama with CUDA)");
        } else {
            // macOS Apple Silicon: native Ollama with Metal
            has_gpu = false;
            eprintln!("[2/5] Apple Silicon detected — using native Ollama (Metal GPU)...");

            if !check_command("ollama", &["--version"]).await {
                if check_command("brew", &["--version"]).await {
                    eprintln!("       Installing Ollama via Homebrew...");
                    let status = tokio::process::Command::new("brew")
                        .args(["install", "ollama"])
                        .status()
                        .await?;
                    if !status.success() {
                        anyhow::bail!("Failed to install Ollama via Homebrew");
                    }
                } else {
                    anyhow::bail!(
                        "Ollama not found. Install from https://ollama.com or via:\n  brew install ollama"
                    );
                }
            }

            if !check_ollama_running().await {
                eprintln!("       Starting Ollama...");
                tokio::process::Command::new("ollama")
                    .arg("serve")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .context("Failed to start ollama serve")?;
                wait_for_ollama_native(30).await?;
            }
            eprintln!("       OK (native Ollama with Metal)");
        }
    } else {
        eprintln!("[2/5] Detecting GPU...");
        has_gpu = has_nvidia;
        if has_gpu {
            eprintln!("       NVIDIA GPU detected (CUDA enabled, containerized Ollama)");
        } else {
            eprintln!("       No NVIDIA GPU (CPU mode)");
        }
    }

    // Step 3: Download models (layout + table structure)
    let models_dir = hidden_dir().join("models");
    let layout_path = models_dir.join("pp-doclayoutv3.onnx");
    let table_path = models_dir.join("slanet-plus.onnx");

    eprintln!("[3/5] Models...");
    if !check {
        std::fs::create_dir_all(&models_dir)?;
    }
    for (label, size_hint, url, path) in [
        ("layout", "~125MB", LAYOUT_MODEL_URL, &layout_path),
        ("table", "~8MB", TABLE_MODEL_URL, &table_path),
    ] {
        if path.exists() && !force {
            eprintln!("       {label}: OK (already downloaded)");
        } else if check {
            eprintln!("       {label}: MISSING ({})", path.display());
        } else {
            eprintln!("       {label}: downloading ({size_hint})...");
            download_file(url, path).await?;
            eprintln!("       {label}: saved to {}", path.display());
        }
    }

    // Step 4: Write compose config
    let config_dir = hidden_dir();
    let compose_path = config_dir.join("docker-compose.yml");
    let env_path = config_dir.join(".env");

    eprintln!("[4/5] Docker Compose config...");
    if compose_path.exists() && !force {
        eprintln!("       OK (already exists)");
    } else if check {
        if compose_path.exists() {
            eprintln!("       OK ({})", compose_path.display());
        } else {
            eprintln!("       MISSING");
        }
    } else {
        std::fs::create_dir_all(&config_dir)?;
        let compose_content = if use_native_ollama {
            compose_yaml_native_ollama(has_gpu)
        } else {
            compose_yaml(has_gpu)
        };
        std::fs::write(&compose_path, compose_content)?;

        let env_contents = format!(
            "MODELS_DIR={}\nOLLAMA_DATA={}\nUSE_CUDA={}\n",
            models_dir.display(),
            hidden_dir().join("ollama").display(),
            has_gpu,
        );
        std::fs::write(&env_path, env_contents)?;
        eprintln!("       Written to {}", compose_path.display());
    }

    if check {
        // Step 5 (check only): report service status
        eprintln!("[5/5] Service status...");
        match health_check(DEFAULT_SERVER).await {
            Ok(h) => eprintln!(
                "       Scribe server: OK (layout={}, tables={})",
                h.layout_model, h.table_model
            ),
            Err(_) => eprintln!("       Scribe server: NOT RUNNING"),
        }
        return Ok(());
    }

    if prereqs_only {
        eprintln!("[5/5] Prerequisites ready (skipping service start)");
        return Ok(());
    }

    // Step 5: Start services
    eprintln!("[5/5] Starting services...");
    let cf = compose_path.to_str().unwrap_or_default();
    let output = compose.run_capture(&["-f", cf, "up", "-d"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("docker-credential-") && stderr.contains("not found") {
            anyhow::bail!(
                "Docker credential helper not found.\n\n\
                 Fix: edit ~/.docker/config.json and change\n\
                 \x20 \"credsStore\": \"desktop\"\n\
                 to\n\
                 \x20 \"credsStore\": \"\"\n\n\
                 Then re-run: hs scribe init"
            );
        }
        if stderr.contains("CDI") && stderr.contains("nvidia") {
            anyhow::bail!(
                "Docker tried to attach an NVIDIA GPU that doesn't exist on this machine.\n\n\
                 Fix: re-run with --force to regenerate the compose config without GPU:\n\
                 \x20 hs scribe init --force"
            );
        }
        if stderr.contains("certificate signed by unknown authority")
            || stderr.contains("x509: certificate")
        {
            anyhow::bail!(
                "TLS certificate error — likely a corporate VPN/proxy (e.g. Netskope, Zscaler)\n\
                 doing SSL inspection.\n\n\
                 Fix for Docker Desktop (macOS):\n\
                 \x20 1. Open Docker Desktop → Settings → General\n\
                 \x20 2. Enable \"Use system certificates\"\n\
                 \x20 3. Restart Docker Desktop\n\n\
                 If that option isn't available, manually trust the proxy CA:\n\
                 \x20 1. Find the CA cert (Netskope: /Library/Application Support/Netskope/STAgent/data/nscacert.pem)\n\
                 \x20 2. mkdir -p ~/.docker/certs.d/ghcr.io && cp <ca.pem> ~/.docker/certs.d/ghcr.io/ca.crt\n\
                 \x20 3. Repeat for docker.io if needed\n\n\
                 Then re-run: hs scribe init --force"
            );
        }
        // Print captured output so the user sees what happened
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.is_empty() {
            eprint!("{stdout}");
        }
        if !stderr.is_empty() {
            eprint!("{stderr}");
        }
        anyhow::bail!("compose up failed");
    }

    // Wait for Ollama and pull model
    if use_native_ollama {
        // Native Ollama should already be running from Step 2
        eprintln!("       Pulling GLM-OCR model (first run downloads ~2.5GB)...");
        let pull_status = tokio::process::Command::new("ollama")
            .args(["pull", "glm-ocr"])
            .status()
            .await?;
        if !pull_status.success() {
            anyhow::bail!("Failed to pull glm-ocr model");
        }
    } else {
        eprintln!("       Waiting for Ollama...");
        wait_for_ollama(&compose, cf, 60).await?;
        eprintln!("       Pulling GLM-OCR model (first run downloads ~2.5GB)...");
        let pull_status = compose
            .exec_run(cf, "vlm", &["ollama", "pull", "glm-ocr"])
            .await?;
        if !pull_status.success() {
            anyhow::bail!("Failed to pull glm-ocr model into Ollama");
        }
    }
    eprintln!("       Waiting for scribe server...");
    wait_for_health(DEFAULT_SERVER, 120).await?;
    eprintln!("       Scribe server: OK");

    eprintln!();
    eprintln!("Ready! Try: hs scribe convert paper.pdf");
    eprintln!();
    eprintln!("To stop:  hs scribe server stop");
    eprintln!("To check: hs scribe server list");
    Ok(())
}

// ── Server ──────────────────────────────────────────────────────

pub async fn cmd_server(action: ServerAction) -> Result<()> {
    let compose_path = hidden_dir().join("docker-compose.yml");
    if !compose_path.exists() {
        anyhow::bail!("No compose config found. Run `hs scribe init` first.");
    }
    let compose = ComposeCmd::detect()
        .await
        .ok_or_else(|| anyhow::anyhow!("No container runtime found"))?;
    let cf = compose_path.to_str().unwrap_or_default();

    match action {
        ServerAction::Start => {
            let has_nvidia = check_command("nvidia-smi", &[]).await;
            if should_use_native_ollama(has_nvidia) && !check_ollama_running().await {
                if cfg!(target_os = "linux") {
                    eprintln!("Starting Ollama systemd service...");
                    ensure_ollama_systemd().await?;
                } else {
                    eprintln!("Starting native Ollama...");
                    let _ = tokio::process::Command::new("ollama")
                        .arg("serve")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                    wait_for_ollama_native(30).await?;
                }
            }
            compose.run_capture(&["-f", cf, "up", "-d"]).await?;
            eprintln!("Waiting for services...");
            wait_for_health(DEFAULT_SERVER, 300).await?;
            eprintln!("Ready.");
        }
        ServerAction::Stop => {
            compose.run_capture(&["-f", cf, "down"]).await?;
            let has_nvidia = check_command("nvidia-smi", &[]).await;
            if should_use_native_ollama(has_nvidia) {
                eprintln!("Unloading model from VRAM...");
                unload_ollama_model("glm-ocr").await;
            }
            eprintln!("Stopped.");
        }
    }
    Ok(())
}

// ── Public API ─────────────────────────────────────────────────

/// Ensure the scribe watch daemon is running. Spawns it if not already active.
/// Used by the pipeline auto-trigger: paper download → scribe watch.
/// Skips silently if scribe is not initialized (compose config missing).
pub fn ensure_watcher_running(reporter: &Arc<dyn Reporter>) {
    // Don't auto-start if scribe hasn't been initialized
    let compose_path = hidden_dir().join("docker-compose.yml");
    if !compose_path.exists() {
        reporter.warn("Scribe not initialized — run `hs scribe init` to enable auto-conversion");
        return;
    }

    let watch_dir = resolve_watch_dir(None);

    match crate::daemon::acquire_instance_lock(&watch_dir) {
        Ok(()) => {
            // Not running — start it
            match crate::daemon::spawn_daemon(None, None, None) {
                Ok(pid) => {
                    reporter.status("Pipeline", &format!("scribe watcher started (PID {pid})"));
                }
                Err(e) => {
                    reporter.warn(&format!("Could not auto-start scribe watcher: {e}"));
                }
            }
        }
        Err(pid) => {
            // Already running
            tracing::debug!("Scribe watcher already running (PID {pid})");
        }
    }
}

/// Idempotent prereq check: ensures container runtime, models, and compose
/// config exist. Does NOT start services — `start_server_foreground` handles that.
pub async fn ensure_init(force: bool) -> Result<()> {
    let cfg = hs_scribe::config::ScribeConfig::load().unwrap_or_default();
    if !cfg.local_server {
        return Ok(());
    }
    cmd_init_inner(force, false, true).await
}

/// Resolve the `hs-scribe-server` binary location. Preference order: user
/// install (`~/.local/bin`), alongside the current `hs` binary, then
/// development-build targets. Mirrors `find_distill_binary`.
fn find_scribe_server_binary() -> Option<PathBuf> {
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".local/bin/hs-scribe-server");
        if path.exists() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join("hs-scribe-server");
            if path.exists() {
                return Some(path);
            }
        }
    }
    let project = hs_common::resolve_project_dir();
    for profile in ["release", "debug"] {
        let path = project
            .join("target")
            .join(profile)
            .join("hs-scribe-server");
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Start the scribe server in the foreground (blocks until shutdown).
/// Launches the native `hs-scribe-server` binary directly — one path, no
/// container indirection. `lib_bootstrap` in the binary handles the
/// platform-specific library-path setup (CUDA on Linux, pdfium on macOS).
pub async fn start_server_foreground(port: u16, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let binary = find_scribe_server_binary().ok_or_else(|| {
        anyhow::anyhow!(
            "hs-scribe-server binary not found. Build with:\n  \
             cargo build --release -p hs-scribe --features server,cuda   (Linux with CUDA)\n  \
             cargo build --release -p hs-scribe --features server         (macOS / CPU)"
        )
    })?;

    // Ensure Ollama (the VLM backend) is running — same logic as before.
    let has_nvidia = check_command("nvidia-smi", &[]).await;
    if should_use_native_ollama(has_nvidia) && !check_ollama_running().await {
        if cfg!(target_os = "linux") {
            reporter.status("Ollama", "starting systemd service");
            ensure_ollama_systemd().await?;
        } else {
            reporter.status("Ollama", "starting native");
            let _ = tokio::process::Command::new("ollama")
                .arg("serve")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            wait_for_ollama_native(30).await?;
        }
    }

    reporter.status(
        "Scribe",
        &format!("running on port {port} (Ctrl+C to stop)"),
    );

    // Run in foreground — inherit stdout/stderr, block until exit.
    let status = tokio::process::Command::new(&binary)
        .arg("--host")
        .arg("0.0.0.0")
        .arg("--port")
        .arg(port.to_string())
        .stdin(std::process::Stdio::null())
        .status()
        .await
        .context("Failed to spawn hs-scribe-server")?;

    if !status.success() {
        anyhow::bail!("hs-scribe-server exited with {status}");
    }

    if should_use_native_ollama(has_nvidia) {
        unload_ollama_model("glm-ocr").await;
    }

    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────

/// Hidden directory for config, cache, models, compose (~/.home-still)
fn hidden_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(hs_common::HIDDEN_DIR)
}

use hs_common::compose::{check_command, ComposeCmd};

/// Determine if we should use native Ollama (macOS Apple Silicon or Linux with NVIDIA GPU).
fn should_use_native_ollama(has_nvidia: bool) -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
        || (cfg!(target_os = "linux") && has_nvidia)
}

/// Check available GPU VRAM in MiB via nvidia-smi.
async fn check_nvidia_vram_mib() -> Option<u64> {
    let output = tokio::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().lines().next()?.trim().parse().ok()
}

/// Install Ollama on Linux via the official install script.
async fn install_ollama_linux() -> Result<()> {
    eprintln!("       Installing Ollama...");
    let status = tokio::process::Command::new("sh")
        .args(["-c", "curl -fsSL https://ollama.com/install.sh | sh"])
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("Failed to install Ollama. Install manually: https://ollama.com");
    }
    Ok(())
}

/// Configure Ollama systemd service for 5-minute auto-unload.
async fn configure_ollama_keepalive() -> Result<()> {
    eprintln!("       Configuring auto-unload (OLLAMA_KEEP_ALIVE=5m)...");
    let override_dir = "/etc/systemd/system/ollama.service.d";
    let override_path = format!("{override_dir}/override.conf");
    let override_content =
        "[Service]\nEnvironment=\"OLLAMA_KEEP_ALIVE=5m\"\nEnvironment=\"OLLAMA_HOST=0.0.0.0\"";

    let status = tokio::process::Command::new("sudo")
        .args(["mkdir", "-p", override_dir])
        .status()
        .await?;
    if !status.success() {
        eprintln!("       warning: Could not create systemd override (no sudo?)");
        eprintln!("       Set OLLAMA_KEEP_ALIVE=5m manually in {override_path}");
        return Ok(());
    }

    let status = tokio::process::Command::new("sudo")
        .args([
            "sh",
            "-c",
            &format!("echo '{override_content}' > {override_path}"),
        ])
        .status()
        .await?;
    if status.success() {
        let _ = tokio::process::Command::new("sudo")
            .args(["systemctl", "daemon-reload"])
            .status()
            .await;
    }
    Ok(())
}

/// Start Ollama via systemd (Linux).
async fn ensure_ollama_systemd() -> Result<()> {
    let status = tokio::process::Command::new("sudo")
        .args(["systemctl", "start", "ollama"])
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!(
            "Failed to start Ollama systemd service.\n\
             Check: sudo systemctl status ollama"
        );
    }
    wait_for_ollama_native(30).await
}

/// Unload a model from Ollama VRAM by setting keep_alive=0.
async fn unload_ollama_model(model: &str) {
    let _ = reqwest::Client::new()
        .post("http://localhost:11434/api/generate")
        .json(&serde_json::json!({
            "model": model,
            "keep_alive": 0
        }))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
}

async fn check_ollama_running() -> bool {
    reqwest::Client::new()
        .get("http://localhost:11434/api/tags")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

async fn wait_for_ollama_native(timeout_secs: u64) -> Result<()> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!(
                "Timed out waiting for native Ollama to start.\n\
                 Try running manually: ollama serve"
            );
        }
        if check_ollama_running().await {
            return Ok(());
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }
}

async fn download_file(url: &str, dest: &std::path::Path) -> Result<()> {
    let resp = reqwest::get(url).await?;
    if !resp.status().is_success() {
        anyhow::bail!("Download failed ({}): {}", resp.status(), url);
    }
    let bytes = resp.bytes().await?;
    let mut file = tokio::fs::File::create(dest).await?;
    file.write_all(&bytes).await?;
    Ok(())
}

async fn health_check(server_url: &str) -> Result<hs_scribe::client::HealthResponse> {
    let client = hs_scribe::client::ScribeClient::new(server_url);
    client.health().await
}

async fn wait_for_ollama(
    compose: &ComposeCmd,
    compose_file: &str,
    timeout_secs: u64,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("Timed out waiting for Ollama to start");
        }
        if compose
            .run_silent(&["-f", compose_file, "exec", "vlm", "ollama", "list"])
            .await
        {
            return Ok(());
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
}

async fn wait_for_health(server_url: &str, timeout_secs: u64) -> Result<()> {
    let url = format!("{server_url}/health");
    hs_common::compose::wait_for_url(&url, timeout_secs, "scribe server").await
}

const PAGE_SEPARATOR: &str = "\n\n---\n\n";

async fn cmd_catalog_backfill(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let scribe_cfg = ScribeConfig::load().unwrap_or_default();
    let markdown_dir = &scribe_cfg.output_dir;
    let catalog_dir = &scribe_cfg.catalog_dir;
    let papers_dir = &scribe_cfg.watch_dir;

    let entries = hs_common::collect_files_recursive(markdown_dir, "md");

    let mut created = 0u32;
    let mut skipped = 0u32;

    for md_path in &entries {
        let stem = md_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();

        // Skip if catalog entry already exists
        if hs_common::catalog::read_catalog_entry(catalog_dir, stem).is_some() {
            skipped += 1;
            continue;
        }

        // Read markdown to extract metadata
        let content = match std::fs::read_to_string(md_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Extract title from first line if it looks like a heading
        let title = content
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim_start_matches('#').trim().to_string())
            .filter(|t| !t.is_empty());

        // Count pages
        let total_pages = content.split(PAGE_SEPARATOR).count() as u64;

        // Look for matching PDF
        let pdf_path = papers_dir.join(format!("{stem}.pdf"));
        let pdf_exists = pdf_path.exists();

        let entry = hs_common::catalog::CatalogEntry {
            title,
            pdf_path: if pdf_exists {
                Some(pdf_path.to_string_lossy().to_string())
            } else {
                None
            },
            markdown_path: Some(md_path.to_string_lossy().to_string()),
            conversion: Some(hs_common::catalog::ConversionMeta {
                server: "backfill".to_string(),
                duration_secs: 0.0,
                total_pages,
                converted_at: chrono::Utc::now().to_rfc3339(),
                pages: hs_common::catalog::compute_page_offsets(&content),
            }),
            ..Default::default()
        };

        hs_common::catalog::write_catalog_entry(catalog_dir, stem, &entry);
        created += 1;
    }

    reporter.finish(&format!(
        "Backfill complete: {created} created, {skipped} already existed"
    ));
    Ok(())
}

// ── Clean junk HTML papers ────────────────────────────────────

async fn cmd_clean_junk(dry_run: bool, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let scribe_cfg = hs_scribe::config::ScribeConfig::load().unwrap_or_default();
    let catalog_dir = &scribe_cfg.catalog_dir;
    let output_dir = &scribe_cfg.output_dir;
    let watch_dir = &scribe_cfg.watch_dir;

    reporter.status(
        "Scan",
        "looking for junk HTML papers (1-page, low quality)...",
    );

    let catalog_files = hs_common::collect_files_recursive(catalog_dir, "yaml");
    let mut junk_count = 0u64;
    let mut kept_count = 0u64;

    for catalog_path in &catalog_files {
        let stem = match catalog_path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        let entry = match crate::catalog::read_catalog_entry(catalog_dir, &stem) {
            Some(e) => e,
            None => continue,
        };

        // Only check HTML-converted papers (1 page, local-html server)
        let is_html_convert = entry
            .conversion
            .as_ref()
            .is_some_and(|c| c.server == "local-html" && c.total_pages <= 1);
        if !is_html_convert {
            continue;
        }

        // Read the markdown and check quality
        let md_path = hs_common::sharded_path(output_dir, &stem, "md");
        let markdown = match std::fs::read_to_string(&md_path) {
            Ok(m) => m,
            Err(_) => continue, // markdown missing, skip
        };

        // If the entire content fails the quality check, it's junk.
        // Also catch known loading stubs (PMC "Preparing to download" etc.)
        // that aren't short enough to trip the quality filter's 50-char floor.
        let lower = markdown.to_lowercase();
        let is_junk = hs_distill::quality::is_low_quality(markdown.trim())
            || lower.contains("preparing to download")
            || lower.contains("please wait while the document loads")
            || (lower.contains("hhs vulnerability disclosure") && markdown.len() < 2000);

        if !is_junk {
            kept_count += 1;
            continue;
        }

        junk_count += 1;

        if dry_run {
            let title = entry.title.as_deref().unwrap_or(&stem);
            reporter.status("Junk", &format!("{title} ({stem})"));
            continue;
        }

        // Delete markdown, catalog, and source HTML
        let _ = std::fs::remove_file(&md_path);
        let _ = std::fs::remove_file(catalog_path);
        // Try both .html and .htm extensions in the sharded papers dir
        for ext in ["html", "htm"] {
            let src = hs_common::sharded_path(watch_dir, &stem, ext);
            let _ = std::fs::remove_file(&src);
        }
    }

    if dry_run {
        reporter.finish(&format!(
            "Dry run: {junk_count} junk files would be removed, {kept_count} HTML papers kept"
        ));
    } else {
        reporter.finish(&format!(
            "Cleaned {junk_count} junk files, kept {kept_count} valid HTML papers"
        ));
    }

    Ok(())
}
