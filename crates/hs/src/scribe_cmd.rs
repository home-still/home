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
        Ok(cfg) if !cfg.servers.is_empty() => cfg.servers.into_iter().map(|e| e.url).collect(),
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

/// Reset every derived artifact for a stem and republish `papers.ingested`
/// so the watcher reconverts it from the source bytes:
///
/// 1. purge the stem's chunks from Qdrant (re-indexing upserts by
///    `(doc_id, chunk_index)`, so a shorter new conversion would leave
///    orphaned tail chunks from the old one),
/// 2. delete the stale markdown object (the watcher's idempotency guard
///    — "markdown already present; skipping" — would otherwise skip the
///    reconvert entirely),
/// 3. clear `conversion` / `conversion_failed` / `embedding` /
///    `embedding_skip` (the stamps gate the source-scan and the distill
///    reconcile; left in place they pin the old derived state forever).
///
/// Remote purge runs first so a failure aborts before any local state is
/// destroyed. The terminal-failure stamp is what stops
/// `list_catalog_stuck_convert` from re-queueing the source on its own —
/// without this command, a row stamped with `vlm_repetition_loop` (or any
/// other terminal reason) is permanently stuck. Operator-driven; CLI-only
/// by design.
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

    // Purge old vectors before touching anything else: if the distill
    // server is unreachable this aborts with nothing mutated, instead of
    // leaving a half-reset row whose stale chunks keep matching searches.
    if entry.embedding.is_some() || entry.embedding_skip.is_some() {
        let servers = crate::distill_cmd::resolve_servers(None).await;
        let client = crate::distill_cmd::make_distill_client(&servers[0]).await?;
        let deleted = client.delete_doc(stem).await.with_context(|| {
            format!(
                "purge old Qdrant chunks for {stem} via {} — start the distill \
                 server (or fix the URL in config) and re-run",
                servers[0]
            )
        })?;
        reporter.status("Purged", &format!("{deleted} stale chunk(s) for {stem}"));
    }

    // Delete the stale markdown so the watcher actually reconverts.
    let md_key = hs_common::markdown::markdown_storage_key(stem);
    if storage
        .exists(&md_key)
        .await
        .with_context(|| format!("storage exists check for {md_key}"))?
    {
        storage
            .delete(&md_key)
            .await
            .with_context(|| format!("delete stale markdown {md_key}"))?;
    }

    let mut cleared = entry;
    cleared.conversion = None;
    cleared.conversion_failed = None;
    cleared.embedding = None;
    cleared.embedding_skip = None;
    cleared.markdown_path = None;
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

/// Classified outcome of a convert failure, consumed by the scribe-chain
/// dispatcher in `cmd_watch_events`.
///
/// `Permanent` means the source content is intrinsically unconvertable —
/// HTML masquerading as PDF, a structurally broken PDF, a paywall. No
/// other VLM backend will succeed on the same input, so the chain stops
/// and `conversion_failed` is stamped immediately.
///
/// `Escalate` means a VLM-class failure: the current backend rejected
/// the content (e.g. GLM-OCR's per-region repetition detector aborted on
/// code-dense pages) but a different backend may handle it (e.g. olmocr
/// with text-layer anchoring). The dispatcher should try the next
/// backend in `ScribeConfig.servers` and only stamp `conversion_failed`
/// when the chain is fully exhausted.
///
/// Both variants carry the catalog-friendly reason token so the stamp
/// is consistent whether it lands immediately (Permanent) or after the
/// chain runs dry (Escalate).
pub(crate) enum ConvertClassification {
    Permanent(String),
    Escalate(String),
}

impl ConvertClassification {
    /// Catalog-friendly reason token, regardless of variant. Used when
    /// the caller needs the string but doesn't care about chain semantics
    /// (e.g. when logging or when the chain has only one backend so
    /// Escalate degenerates to Permanent).
    pub(crate) fn reason(&self) -> &str {
        match self {
            Self::Permanent(r) | Self::Escalate(r) => r,
        }
    }
}

/// Classify a convert failure as Permanent (stop chain) or Escalate
/// (try next backend). The scribe HTTP server returns HTTP 415 + body
/// `unsupported_content_type:{html,binary}` for content-type mismatches
/// (see `hs-scribe/src/server.rs::verify_pdf_content`). PDF parse errors
/// surface as `FormatError` / `Invalid image size` / `PdfiumLibrary` in
/// the error chain. HTML paywall rejection embeds "paywall" in the
/// message. VLM-class failures (`VLM repetition loop`, mid-stream
/// `connection closed before message completed`) are Escalate so the
/// next backend can take a swing.
pub(crate) fn classify_convert_failure(err: &anyhow::Error) -> ConvertClassification {
    let msg = format!("{err:#}");
    match hs_scribe::classify::classify_failure(&msg) {
        hs_scribe::classify::FailureClass::Permanent(reason) => {
            ConvertClassification::Permanent(reason.to_string())
        }
        hs_scribe::classify::FailureClass::Escalate(reason) => {
            ConvertClassification::Escalate(reason.to_string())
        }
        // This arm only fires for errors that arrived as
        // HandlerError::Permanent yet match no table entry — the handler
        // positively identified them as non-retriable, so escalating is
        // the safe default: the next backend may succeed for a reason we
        // haven't catalogued yet, and the chain naturally terminates if
        // every backend rejects.
        hs_scribe::classify::FailureClass::Transient => {
            ConvertClassification::Escalate("permanent_convert_failure".to_string())
        }
    }
}

/// One backend's HTTP client plus the operator-visible name from
/// `ScribeConfig.servers[].backend`. Built once at startup so per-event
/// dispatch isn't reconstructing reqwest clients.
struct ChainEntry {
    backend: String,
    client: hs_scribe::client::ScribeClient,
    /// Per-backend conversion concurrency cap. Sized from
    /// `scribe.servers[].concurrency` so a heavy model (olmocr) is throttled
    /// independently of a light one (glm). The handler holds a permit across
    /// each backend's convert attempt.
    sem: Arc<tokio::sync::Semaphore>,
}

pub(crate) async fn cmd_watch_events(
    server_override: Option<String>,
    _reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    use hs_scribe::client::ScribeClient;
    use hs_scribe::config::ScribeConfig;
    use hs_scribe::event_watch::{convert_and_upload, run_subscriber};

    let cfg = ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage()?;
    let bus = cfg.build_event_bus().await?;

    let convert_timeout = std::time::Duration::from_secs(cfg.convert_timeout_secs);
    // Build the chain in operator-specified order. CLI `--server` override
    // collapses to a singleton chain with an unknown backend label —
    // catalog will record `converted_by: "unknown"` so an operator can
    // still tell at a glance that the chain was bypassed.
    let chain: Vec<ChainEntry> = match server_override {
        Some(url) => vec![ChainEntry {
            backend: "unknown".to_string(),
            client: ScribeClient::new_with_timeout(&url, convert_timeout)?,
            // Manual single-backend override: one operator convert, modest cap.
            sem: Arc::new(tokio::sync::Semaphore::new(4)),
        }],
        None if !cfg.servers.is_empty() => cfg
            .servers
            .iter()
            .map(|e| {
                Ok(ChainEntry {
                    backend: e.backend.clone(),
                    client: ScribeClient::new_with_timeout(&e.url, convert_timeout)?,
                    sem: Arc::new(tokio::sync::Semaphore::new(e.concurrency.max(1))),
                })
            })
            .collect::<Result<_>>()?,
        None => vec![ChainEntry {
            backend: "glm_ocr".to_string(),
            client: ScribeClient::new_with_timeout(DEFAULT_SERVER, convert_timeout)?,
            sem: Arc::new(tokio::sync::Semaphore::new(4)),
        }],
    };
    let chain = Arc::new(chain);
    let timeout_policy = Arc::new(cfg.timeout_policy.clone());

    let chain_summary: Vec<String> = chain
        .iter()
        .map(|e| {
            format!(
                "{}@{} (concurrency={})",
                e.backend,
                e.client.url(),
                e.sem.available_permits()
            )
        })
        .collect();
    tracing::info!(
        chain = ?chain_summary,
        convert_timeout_secs = cfg.convert_timeout_secs,
        base_secs = timeout_policy.base_secs,
        per_page_secs = timeout_policy.per_page_secs,
        floor_secs = timeout_policy.floor_secs,
        ceiling_secs = timeout_policy.ceiling_secs,
        "starting event-bus watcher with {}-backend chain",
        chain.len()
    );

    let storage_for_handler = storage.clone();
    let bus_for_handler = bus.clone();
    // Chain processing is sequential per event; concurrency cap stays
    // modest. The legacy `ServicePool::probed_concurrency` was sized for
    // pool-wide fan-out which no longer matches the chain model — fall
    // back to a fixed 8-in-flight cap so the watcher can still pipeline
    // independent events through long chains without blowing up.
    let concurrency = 8usize;
    tracing::info!(concurrency, "scribe-watch consumer in-flight cap");
    run_subscriber(bus.clone(), storage.clone(), concurrency, move |event| {
        let storage = storage_for_handler.clone();
        let bus = bus_for_handler.clone();
        let chain = chain.clone();
        let timeout_policy = timeout_policy.clone();
        async move {
            // Walk the chain in order. On `Escalate` or `Transient`, log
            // the attempt and continue to the next backend; the next
            // backend may handle the content this one rejected. On
            // `Permanent`, short-circuit immediately — the source is
            // intrinsically unconvertable and no other backend will help.
            // On success, the backend's own catalog stamp captures the
            // chain context (converted_by + attempts_log).
            let mut attempts_log: Vec<hs_common::catalog::AttemptEntry> = Vec::new();
            let mut last_err: Option<hs_scribe::event_watch::HandlerError> = None;

            // Fetch + parse the source ONCE per event. Every backend
            // attempt reuses the same Arc'd bytes and page count —
            // escalation used to re-download and re-parse the whole
            // book from storage per backend. prepare_source handles
            // its own failure stamping (source_missing) and
            // classification: Permanent → TERM, Transient → NAK.
            let source =
                match hs_scribe::event_watch::prepare_source(storage.as_ref(), &event).await {
                    Ok(hs_scribe::event_watch::SourcePrep::AlreadyConverted(_)) => return Ok(()),
                    Ok(hs_scribe::event_watch::SourcePrep::Fetched(src)) => src,
                    Err(e) => return Err(e),
                };

            for entry in chain.iter() {
                tracing::info!(
                    server = %entry.client.url(),
                    backend = %entry.backend,
                    key = %event.key,
                    chain_pos = attempts_log.len() + 1,
                    "chain attempt"
                );
                // Per-backend concurrency cap: hold a permit for THIS backend
                // across its convert attempt (released on escalation or return).
                // Sized from `scribe.servers[].concurrency` so a heavy model
                // (olmocr) is throttled to a few while a light one (glm) runs
                // more — instead of one global cap fanning out onto olmocr.
                let _permit = Arc::clone(&entry.sem)
                    .acquire_owned()
                    .await
                    .expect("scribe chain concurrency semaphore is never closed");
                let result = convert_and_upload(
                    storage.as_ref(),
                    &entry.client,
                    bus.as_ref(),
                    &event,
                    timeout_policy.as_ref(),
                    &source,
                    Some(entry.backend.clone()),
                    attempts_log.clone(),
                )
                .await;

                let now = chrono::Utc::now().to_rfc3339();
                match result {
                    Ok(_) => return Ok(()),
                    Err(hs_scribe::event_watch::HandlerError::Permanent(e)) => {
                        let classification = classify_convert_failure(&e);
                        let reason = classification.reason().to_string();
                        let outcome = match &classification {
                            ConvertClassification::Permanent(_) => "permanent",
                            ConvertClassification::Escalate(_) => "escalate",
                        };
                        attempts_log.push(hs_common::catalog::AttemptEntry {
                            backend: entry.backend.clone(),
                            outcome: outcome.to_string(),
                            reason: Some(reason.clone()),
                            at: now,
                        });
                        match classification {
                            ConvertClassification::Permanent(_) => {
                                tracing::warn!(
                                    backend = %entry.backend,
                                    key = %event.key,
                                    reason = %reason,
                                    "chain short-circuit: permanent failure"
                                );
                                if let Some(stem) = stem_from_event_key(&event.key) {
                                    if let Err(stamp_err) =
                                        hs_common::catalog::update_conversion_failed_via(
                                            storage.as_ref(),
                                            "catalog",
                                            &stem,
                                            &reason,
                                            attempts_log.clone(),
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            stem = %stem,
                                            reason = %reason,
                                            error = %stamp_err,
                                            "failed to stamp conversion_failed; terminating anyway"
                                        );
                                    }
                                }
                                return Err(hs_scribe::event_watch::HandlerError::Permanent(e));
                            }
                            ConvertClassification::Escalate(_) => {
                                tracing::info!(
                                    backend = %entry.backend,
                                    key = %event.key,
                                    reason = %reason,
                                    "chain escalating to next backend"
                                );
                                last_err = Some(hs_scribe::event_watch::HandlerError::Permanent(e));
                            }
                        }
                    }
                    Err(hs_scribe::event_watch::HandlerError::Transient(e)) => {
                        // Transient failures on this backend (network
                        // flake, scribe 5xx) — try the next backend
                        // before NAKing the event back to JetStream.
                        // Different backend = different process =
                        // different network path, so it may not share
                        // the transient condition.
                        attempts_log.push(hs_common::catalog::AttemptEntry {
                            backend: entry.backend.clone(),
                            outcome: "transient".to_string(),
                            reason: Some(format!("{e:#}")),
                            at: now,
                        });
                        tracing::warn!(
                            backend = %entry.backend,
                            key = %event.key,
                            error = %e,
                            "chain transient: trying next backend"
                        );
                        last_err = Some(hs_scribe::event_watch::HandlerError::Transient(e));
                    }
                }
            }

            // Chain exhausted. Whatever the last error was, surface it
            // — `last_err` is `Some` because the loop ran at least once
            // (the chain is non-empty by construction). If the last
            // surviving error was Permanent, stamp it; if it was
            // Transient, NAK and let JetStream redeliver.
            match last_err {
                Some(hs_scribe::event_watch::HandlerError::Permanent(e)) => {
                    let reason = classify_convert_failure(&e).reason().to_string();
                    if let Some(stem) = stem_from_event_key(&event.key) {
                        if let Err(stamp_err) = hs_common::catalog::update_conversion_failed_via(
                            storage.as_ref(),
                            "catalog",
                            &stem,
                            &reason,
                            attempts_log,
                        )
                        .await
                        {
                            tracing::warn!(
                                stem = %stem,
                                reason = %reason,
                                error = %stamp_err,
                                "failed to stamp chain-exhausted conversion_failed"
                            );
                        }
                    }
                    Err(hs_scribe::event_watch::HandlerError::Permanent(e))
                }
                Some(e) => Err(e),
                None => Err(hs_scribe::event_watch::HandlerError::Transient(
                    anyhow::anyhow!("chain was empty"),
                )),
            }
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
            .convert_with_progress(pdf_bytes, Some(convert_timeout), None, on_progress)
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
                converted_by: None,
                attempts_log: Vec::new(),
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

#[cfg(test)]
mod classify_convert_failure_tests {
    use super::{classify_convert_failure, ConvertClassification};

    #[test]
    fn vlm_repetition_loop_escalates_with_specific_reason() {
        // Verbatim shape of the error event_watch.rs constructs at the
        // RejectLoop arm. Classification must preserve the specific
        // reason so the catalog stamp lands as `vlm_repetition_loop`
        // (not the generic clobber) when the chain runs out of backends.
        // VLM-class failure → Escalate so a different backend gets a
        // shot before the chain stamps failure.
        let err = anyhow::anyhow!(
            "VLM repetition loop on papers/10/x.pdf (truncations=23, longest_run=9009B)"
        );
        match classify_convert_failure(&err) {
            ConvertClassification::Escalate(r) => assert_eq!(r, "vlm_repetition_loop"),
            other => panic!("expected Escalate, got {:?}", other.reason()),
        }
    }

    #[test]
    fn vlm_transport_error_escalates() {
        // llama-server slot eviction mid-stream. Same VLM-class family
        // as the repetition loop — escalate to next backend.
        let err = anyhow::anyhow!(
            "scribe convert failed: client error (SendRequest): connection closed before message completed"
        );
        match classify_convert_failure(&err) {
            ConvertClassification::Escalate(r) => assert_eq!(r, "vlm_transport_error"),
            other => panic!("expected Escalate, got {:?}", other.reason()),
        }
    }

    #[test]
    fn unrecognized_message_escalates_with_generic_reason() {
        // Unknown failure → Escalate so the next backend gets a chance.
        // If every backend rejects with the same unknown reason, the
        // chain terminates naturally and stamps `permanent_convert_failure`.
        let err = anyhow::anyhow!("scribe convert failed: some novel failure mode");
        match classify_convert_failure(&err) {
            ConvertClassification::Escalate(r) => assert_eq!(r, "permanent_convert_failure"),
            other => panic!("expected Escalate, got {:?}", other.reason()),
        }
    }

    #[test]
    fn olmocr_zero_pages_escalates() {
        // The mcconnell shape: olmocr ran 45 min on a Cyrillic book with
        // a clean text layer, then reported 0 completed pages. Must
        // Escalate so GLM-OCR gets its shot — the original Permanent
        // classification short-circuited the chain incorrectly.
        let err =
            anyhow::anyhow!("Server error: olmocr reported 0 completed pages (failed=0); content may need a different backend");
        match classify_convert_failure(&err) {
            ConvertClassification::Escalate(r) => assert_eq!(r, "olmocr_zero_pages"),
            other => panic!("expected Escalate, got {:?}", other.reason()),
        }
    }

    #[test]
    fn pdf_parse_error_is_permanent() {
        // FormatError is a hard PDF-parse problem; no VLM backend will
        // succeed because the PDF itself is unreadable to scribe's
        // parser. Stop the chain immediately, don't burn other backends'
        // compute on a hopeless input.
        let err = anyhow::anyhow!("scribe convert failed: FormatError on page 3");
        match classify_convert_failure(&err) {
            ConvertClassification::Permanent(r) => assert_eq!(r, "pdf_parse_error"),
            other => panic!("expected Permanent, got {:?}", other.reason()),
        }
    }

    #[test]
    fn unsupported_content_type_is_permanent() {
        let err = anyhow::anyhow!("scribe rejected: unsupported_content_type:html");
        match classify_convert_failure(&err) {
            ConvertClassification::Permanent(r) => assert_eq!(r, "unsupported_content_type:html"),
            other => panic!("expected Permanent, got {:?}", other.reason()),
        }
    }

    #[test]
    fn paywall_is_permanent() {
        let err = anyhow::anyhow!("paywall HTML detected on download");
        match classify_convert_failure(&err) {
            ConvertClassification::Permanent(r) => assert_eq!(r, "paywall_html"),
            other => panic!("expected Permanent, got {:?}", other.reason()),
        }
    }
}
