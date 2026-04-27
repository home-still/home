use crate::scribe_pool::ScribePool;
use anyhow::{Context, Result};
use clap::Subcommand;
use hs_common::auth::client::is_cloud_url;
use hs_common::reporter::Reporter;
use hs_scribe::config::ScribeConfig;
use std::path::PathBuf;
use std::sync::Arc;

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
        hs_scribe::client::ScribeClient::new_with_timeout(url, convert_timeout)
    }
}

/// Resolve the server list from CLI flag, config file, or the local
/// default. Config is the sole source of truth — to route through a cloud
/// gateway, set the gateway URL explicitly in config instead of relying
/// on per-request registry discovery with a fallback.
async fn resolve_servers(cli_server: Option<&str>) -> Vec<String> {
    if let Some(s) = cli_server {
        return vec![s.to_string()];
    }
    match ScribeConfig::load() {
        Ok(cfg) if !cfg.servers.is_empty() => cfg.servers,
        _ => vec![DEFAULT_SERVER.to_string()],
    }
}

#[derive(Subcommand, Debug)]
pub enum ScribeCmd {
    /// Convert a PDF to markdown (sends to scribe server).
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
    /// Subscribe to `papers.ingested` on the configured event bus,
    /// convert each PDF via the scribe server, and upload the markdown
    /// back to storage. Event-driven replacement for the filesystem
    /// watcher.
    WatchEvents {
        /// Server URL override
        #[arg(long)]
        server: Option<String>,
    },
    /// Client-side inbox watcher. Sweeps `papers/manually_downloaded/`
    /// on the configured storage, relocates each file to
    /// `papers/<shard>/...`, and publishes `papers.ingested` on NATS so
    /// the server-side scribe can convert.
    Inbox {
        #[command(subcommand)]
        action: Option<InboxAction>,
    },
    /// Backfill catalog entries for markdown files that were converted
    /// before the catalog feature.
    CatalogBackfill,
    /// Clear a stem's `conversion` / `conversion_failed` stamps and
    /// republish `papers.ingested` so the watcher reconverts it. The
    /// only supported escape hatch for rows that the QC gate terminally
    /// rejected (e.g. `vlm_repetition_loop`); without this, those rows
    /// are stuck because the source-scan refuses to re-queue terminal
    /// failures by design.
    Reconvert {
        /// Catalog stem (no extension), e.g. `10.48550_arxiv.2312.10997`.
        stem: String,
    },
    /// Auto-tune `OLLAMA_NUM_PARALLEL` against the local scribe-server's
    /// observed throughput. Install as a root systemd service via
    /// `sudo hs serve scribe-autotune --install`.
    Autotune,
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
        ScribeCmd::WatchEvents { server } => cmd_watch_events(server, reporter).await,
        ScribeCmd::Inbox { action } => {
            crate::scribe_inbox::dispatch(action.unwrap_or(InboxAction::Run), reporter).await
        }
        ScribeCmd::CatalogBackfill => cmd_catalog_backfill(reporter).await,
        ScribeCmd::Reconvert { stem } => cmd_reconvert(&stem, reporter).await,
        ScribeCmd::Autotune => cmd_autotune(reporter).await,
    }
}

/// Clear `conversion` / `conversion_failed` on a single catalog row and
/// republish `papers.ingested` so the watcher reconverts it. The terminal-
/// failure stamp is what stops `list_catalog_stuck_convert` from re-queueing
/// the source on its own — without this command, a row stamped with
/// `vlm_repetition_loop` (or any other terminal reason) is permanently
/// stuck. Operator-driven; CLI-only by design.
async fn cmd_reconvert(stem: &str, reporter: &Arc<dyn Reporter>) -> Result<()> {
    const PAPERS_PREFIX: &str = "papers";
    const CATALOG_PREFIX: &str = "catalog";
    const CANDIDATE_EXTS: &[&str] = &["pdf", "html", "htm", "epub"];

    let cfg = ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage()?;
    let bus = cfg.build_event_bus().await?;

    let entry = hs_common::catalog::read_catalog_entry_via(&*storage, CATALOG_PREFIX, stem)
        .await
        .with_context(|| format!("read catalog row for {stem}"))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no catalog row for stem `{stem}` — use `hs scribe convert` for first-time conversions"
            )
        })?;

    if entry.conversion.is_none() && entry.conversion_failed.is_none() {
        anyhow::bail!(
            "stem `{stem}` has no `conversion` or `conversion_failed` stamp — \
             nothing to retry. The watcher will pick this row up on its next sweep."
        );
    }

    let mut source_key: Option<String> = None;
    for ext in CANDIDATE_EXTS {
        let key = format!("{PAPERS_PREFIX}/{}", hs_common::sharded_key(stem, ext));
        if storage
            .exists(&key)
            .await
            .with_context(|| format!("storage exists check for {key}"))?
        {
            source_key = Some(key);
            break;
        }
    }
    let source_key = source_key.ok_or_else(|| {
        anyhow::anyhow!(
            "no source file (.pdf/.html/.htm/.epub) under `{PAPERS_PREFIX}/` for stem `{stem}`"
        )
    })?;

    let mut cleared = entry;
    cleared.conversion = None;
    cleared.conversion_failed = None;
    hs_common::catalog::write_catalog_entry_via(&*storage, CATALOG_PREFIX, stem, &cleared)
        .await
        .with_context(|| format!("write cleared catalog row for {stem}"))?;

    let payload = serde_json::json!({
        "key": source_key,
        "source": "hs scribe reconvert",
    });
    let bytes = serde_json::to_vec(&payload).context("serialize papers.ingested payload")?;
    bus.publish("papers.ingested", &bytes)
        .await
        .with_context(|| format!("publish papers.ingested for {source_key}"))?;

    reporter.finish(&format!(
        "Reconvert queued: stem={stem} source_key={source_key}"
    ));
    Ok(())
}

async fn cmd_autotune(_reporter: &Arc<dyn Reporter>) -> Result<()> {
    let cfg = hs_scribe::config::ScribeConfig::load()
        .map_err(|e| anyhow::anyhow!("load ScribeConfig: {e}"))?;
    hs_scribe::ollama_tuner::run_forever(cfg.autotune).await
}

/// Strip the path + extension off a NATS `papers.ingested` event key to
/// recover the catalog stem. `"papers/10/10.1007_s001.pdf"` →
/// `Some("10.1007_s001")`. Used when stamping `conversion_failed` on
/// terminal convert failures so the row drops out of the queue.
fn stem_from_event_key(key: &str) -> Option<String> {
    let filename = key.rsplit_once('/').map(|(_, f)| f).unwrap_or(key);
    let (stem, _ext) = filename.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_string())
}

/// Classify a permanent convert error into a short catalog-friendly reason
/// token. The scribe HTTP server returns HTTP 415 + body
/// `unsupported_content_type:{html,binary}` for content-type mismatches
/// (see `hs-scribe/src/server.rs::verify_pdf_content`). PDF parse errors
/// surface as `FormatError` / `Invalid image size` / `PdfiumLibrary` in
/// the error chain. HTML paywall rejection embeds "paywall" in the
/// message. Anything else we tag generically so the operator sees the
/// full chain in the log but the catalog still gets a stamp.
fn classify_permanent_reason(err: &anyhow::Error) -> String {
    let msg = format!("{err:#}");
    if msg.contains("unsupported_content_type:html") {
        "unsupported_content_type:html".to_string()
    } else if msg.contains("unsupported_content_type:binary") {
        "unsupported_content_type:binary".to_string()
    } else if msg.contains("paywall") {
        "paywall_html".to_string()
    } else if msg.contains("FormatError")
        || msg.contains("Invalid image size")
        || msg.contains("PdfiumLibrary")
    {
        "pdf_parse_error".to_string()
    } else if msg.contains("EPUB parse failed") {
        "epub_parse_error".to_string()
    } else if msg.contains("not valid UTF-8") {
        "html_not_utf8".to_string()
    } else if msg.contains("unsupported source type") {
        "unsupported_extension".to_string()
    } else {
        "permanent_convert_failure".to_string()
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
        .collect::<Result<_>>()?;
    let pool = Arc::new(ServicePool::new(clients));
    let timeout_policy = Arc::new(cfg.timeout_policy.clone());

    tracing::info!(
        servers = ?servers,
        convert_timeout_secs = cfg.convert_timeout_secs,
        base_secs = timeout_policy.base_secs,
        per_page_secs = timeout_policy.per_page_secs,
        floor_secs = timeout_policy.floor_secs,
        ceiling_secs = timeout_policy.ceiling_secs,
        "starting event-bus watcher with {}-server pool",
        servers.len()
    );

    let storage_for_handler = storage.clone();
    let bus_for_handler = bus.clone();
    let concurrency = pool.probed_concurrency().await;
    tracing::info!(concurrency, "scribe-watch consumer in-flight cap");
    run_subscriber(bus.clone(), storage.clone(), concurrency, move |event| {
        let storage = storage_for_handler.clone();
        let bus = bus_for_handler.clone();
        let pool = pool.clone();
        let timeout_policy = timeout_policy.clone();
        async move {
            // Dispatch retry: a /convert can fail mid-stream when a
            // single scribe's link flaps. One fast retry on a different
            // host is cheap, and convert_and_upload is idempotent via
            // its head-check on the target markdown key. Permanent
            // failures (VLM repetition, unsupported type, paywall HTML,
            // PDF parse errors) short-circuit the retry — redelivery to
            // a different server would fail identically.
            let max_dispatch_attempts: u32 = 2;
            let mut last_err: Option<hs_scribe::event_watch::HandlerError> = None;
            for attempt in 1..=max_dispatch_attempts {
                let (client, _pick_guard) = match pool.pick_server().await {
                    Ok(t) => t,
                    Err(e) => {
                        return Err(hs_scribe::event_watch::HandlerError::Transient(
                            e.context("no ready scribe servers"),
                        ));
                    }
                };
                tracing::info!(
                    server = %client.url(),
                    key = %event.key,
                    attempt,
                    "dispatching event"
                );
                match convert_and_upload(
                    storage.as_ref(),
                    client,
                    bus.as_ref(),
                    &event,
                    timeout_policy.as_ref(),
                )
                .await
                {
                    Ok(_) => return Ok(()),
                    Err(hs_scribe::event_watch::HandlerError::Permanent(e)) => {
                        // Terminal: source won't convert regardless of
                        // retries. Stamp `conversion_failed` on the
                        // catalog row so catalog_repair's stuck_convert
                        // direction won't re-publish this stem, and
                        // `hs status` surfaces it in the Corrupted PDFs
                        // count. Best-effort — if the stamp itself fails
                        // we still term the NATS message so the daemon
                        // doesn't spin.
                        if let Some(stem) = stem_from_event_key(&event.key) {
                            let reason = classify_permanent_reason(&e);
                            if let Err(stamp_err) =
                                hs_common::catalog::update_conversion_failed_via(
                                    storage.as_ref(),
                                    "catalog",
                                    &stem,
                                    &reason,
                                )
                                .await
                            {
                                tracing::warn!(
                                    stem = %stem,
                                    reason,
                                    error = %stamp_err,
                                    "failed to stamp conversion_failed; terminating anyway"
                                );
                            }
                        }
                        return Err(hs_scribe::event_watch::HandlerError::Permanent(e));
                    }
                    Err(hs_scribe::event_watch::HandlerError::Transient(e))
                        if attempt < max_dispatch_attempts =>
                    {
                        tracing::warn!(
                            server = %client.url(),
                            key = %event.key,
                            attempt,
                            error = %e,
                            "convert failed — retrying on a different server"
                        );
                        last_err = Some(hs_scribe::event_watch::HandlerError::Transient(e));
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(last_err.unwrap_or_else(|| {
                hs_scribe::event_watch::HandlerError::Transient(anyhow::anyhow!(
                    "dispatch retries exhausted"
                ))
            }))
        }
    })
    .await
}

// ── Convert ─────────────────────────────────────────────────────

/// Unpack an EPUB archive's spine to a single HTML string, concatenating
/// each chapter's XHTML in reading order. Used by `scribe_inbox` to turn
/// EPUB drops into HTML so the downstream html-parser path converts them
/// — the scribe VLM pipeline is PDF-only.
pub fn epub_bytes_to_html(bytes: Vec<u8>) -> Result<String> {
    use std::io::Cursor;
    let mut doc = epub::doc::EpubDoc::from_reader(Cursor::new(bytes))
        .context("failed to open EPUB archive")?;
    let mut out = String::new();
    loop {
        if let Some((content, _mime)) = doc.get_current_str() {
            if !content.trim().is_empty() {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&content);
            }
        }
        if !doc.go_next() {
            break;
        }
    }
    Ok(out)
}

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
        let pool = ScribePool::new(&servers, convert_timeout)?;
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
            .convert_with_progress(pdf_bytes, Some(convert_timeout), on_progress)
            .await
            .map(|conv| (servers[0].clone(), conv))
    } else {
        let pool = ScribePool::new(&servers, convert_timeout)?;
        pool.convert_one(pdf_bytes, on_progress).await
    };

    match &result {
        Ok(_) => stage.finish_with_message("done"),
        Err(e) => stage.finish_failed(&format!("{e:#}")),
    }

    let (_server, conversion) = result?;
    let raw_md = conversion.markdown;
    let per_page_region_classes = conversion.per_page_region_classes;
    let per_page_diags = conversion.per_page_diags;
    let qc_started = std::time::Instant::now();
    let longest_run = hs_scribe::postprocess::longest_repeated_run_bytes(&raw_md);
    let (md, per_page_truncations) = hs_scribe::postprocess::clean_repetitions_per_page(&raw_md);
    let truncations: usize = per_page_truncations.iter().map(|t| t.total()).sum();
    if truncations > 0 {
        tracing::info!("Cleaned {} repetition site(s)", truncations);
    }

    let page_offsets = hs_common::catalog::compute_page_offsets(&md);
    let total_pages = page_offsets.len() as u64;
    let per_page_is_bibliography: Vec<bool> = (0..per_page_truncations.len())
        .map(|i| {
            per_page_region_classes
                .get(i)
                .map(|classes| hs_scribe::postprocess::is_bibliography_page(classes))
                .unwrap_or(false)
        })
        .collect();
    let verdict = hs_scribe::postprocess::qc_verdict(
        &per_page_truncations,
        &per_page_is_bibliography,
        longest_run,
    );
    // Optional --diag JSONL: opt-in via HS_SCRIBE_DIAG_DIR env var. Same
    // semantics as the watch-events daemon path — one JSONL per stem.
    let diag_dir = std::env::var_os("HS_SCRIBE_DIAG_DIR").map(PathBuf::from);
    let diag_stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut diag = hs_scribe::diag::DiagWriter::open(diag_dir.as_ref(), &diag_stem);
    for record in &per_page_diags {
        diag.write_page(&diag_stem, record.clone());
    }
    diag.write_document(hs_scribe::diag::DocSummaryRecord {
        stem: diag_stem.clone(),
        total_pages: per_page_truncations.len(),
        per_page_truncation_counts: per_page_truncations.clone(),
        longest_run_bytes: longest_run,
        qc_verdict: format!("{verdict:?}"),
        wall_clock_ms: qc_started.elapsed().as_millis() as u64,
    });
    if verdict == hs_scribe::postprocess::QcVerdict::RejectLoop {
        anyhow::bail!(
            "VLM repetition loop: {truncations} truncation site(s), longest_run={longest_run}B \
             across {total_pages} page(s). Output not persisted; re-run or investigate the source PDF.",
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

/// Pick the native-Ollama path on Apple Silicon and Linux-with-NVIDIA,
/// where we drive Ollama directly (no container). Everywhere else we
/// assume the operator arranged their own Ollama (container, remote, or
/// none — we don't care, we just POST to the configured URL).
fn should_use_native_ollama(has_nvidia: bool) -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
        || (cfg!(target_os = "linux") && has_nvidia)
}

/// Ensure systemd-managed Ollama is active. Called on Linux-with-NVIDIA
/// before the scribe server starts. Best-effort: if systemctl is missing
/// (container, non-systemd distro) we return Ok and let the operator's
/// Ollama setup take over.
async fn ensure_ollama_systemd() -> Result<()> {
    let status = tokio::process::Command::new("systemctl")
        .args(["start", "ollama"])
        .status()
        .await;
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) | Err(_) => {
            // systemctl not available or failed — trust the operator's
            // prior setup. `check_ollama_running` is the next gate.
            Ok(())
        }
    }
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

        hs_common::catalog::write_catalog_entry(catalog_dir, stem, &entry)
            .with_context(|| format!("write catalog {stem}.yaml"))?;
        created += 1;
    }

    reporter.finish(&format!(
        "Backfill complete: {created} created, {skipped} already existed"
    ));
    Ok(())
}

// ── Clean junk HTML papers ────────────────────────────────────
