use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hs_common::auth::client::is_cloud_url;
use hs_common::compose::{check_command, wait_for_url, ComposeCmd};
use hs_common::global_args::{GlobalArgs, OutputFormat};
use hs_common::reporter::Reporter;
use hs_distill::cli::DistillCmd;
use hs_distill::client::DistillClient;
use hs_distill::config::{DistillClientConfig, DistillServerConfig};

/// Create a DistillClient, with auth headers if the URL is a cloud gateway.
pub(crate) async fn make_distill_client(url: &str) -> Result<DistillClient> {
    if is_cloud_url(url) {
        let auth = hs_common::auth::client::AuthenticatedClient::from_default_path()
            .context("Cloud credentials not found. Run `hs cloud enroll` first.")?;
        let http = hs_common::auth::client::AuthedHttp::with_auth(
            auth,
            std::time::Duration::from_secs(900),
        )?;
        Ok(DistillClient::new_with_client(url, http))
    } else {
        DistillClient::new(url)
    }
}
const QDRANT_REST_PORT: u16 = 6333;
const QDRANT_GRPC_PORT: u16 = 6334;

pub(crate) async fn resolve_servers(cli_server: Option<&str>) -> Result<Vec<String>> {
    if let Some(s) = cli_server {
        return Ok(vec![s.to_string()]);
    }
    // Config is the sole source of truth — to route through a cloud
    // gateway, set the gateway URL explicitly in config. No local default:
    // an empty `distill.servers` is an error naming that key.
    Ok(DistillClientConfig::load()?.require_servers()?.to_vec())
}

fn hidden_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(hs_common::HIDDEN_DIR)
}

fn distill_compose_path() -> PathBuf {
    hidden_dir().join("docker-compose-distill.yml")
}

fn distill_pid_path() -> PathBuf {
    hidden_dir().join("distill-server.pid")
}

/// Convert Qdrant gRPC URL (:6334) to REST URL (:6333) for health checks.
fn qdrant_rest_from_grpc(grpc_url: &str) -> String {
    grpc_url.replace(
        &format!(":{QDRANT_GRPC_PORT}"),
        &format!(":{QDRANT_REST_PORT}"),
    )
}

/// Qdrant's REST base URL, from the distill server's configured gRPC URL.
pub(crate) fn qdrant_rest_url() -> Result<String> {
    Ok(qdrant_rest_from_grpc(
        &DistillServerConfig::load()?.qdrant_url,
    ))
}

fn distill_compose_yaml(data_dir: &std::path::Path) -> String {
    format!(
        r#"services:
  qdrant:
    image: docker.io/qdrant/qdrant:latest
    ports:
      - "{QDRANT_REST_PORT}:{QDRANT_REST_PORT}"
      - "{QDRANT_GRPC_PORT}:{QDRANT_GRPC_PORT}"
    volumes:
      - {}:/qdrant/storage
    restart: on-failure:3
"#,
        data_dir.display()
    )
}

fn find_distill_binary() -> Result<Option<PathBuf>> {
    // Check ~/.local/bin (install script location)
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".local/bin/hs-distill-server");
        if path.exists() {
            return Ok(Some(path));
        }
    }
    // Check next to the current binary (same install dir)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join("hs-distill-server");
            if path.exists() {
                return Ok(Some(path));
            }
        }
    }
    // Check cargo target dirs (dev builds)
    let project = hs_common::resolve_project_dir()?;
    for profile in ["release", "debug"] {
        let path = project
            .join("target")
            .join(profile)
            .join("hs-distill-server");
        if path.exists() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Find the ort cache directory containing CUDA provider .so files.
fn find_ort_cuda_libs() -> Option<String> {
    let cache = dirs::home_dir()?.join(".cache/ort.pyke.io/dfbin");
    if !cache.exists() {
        return None;
    }
    // Walk into the platform-specific subdir to find libonnxruntime_providers_cuda.so
    for entry in std::fs::read_dir(&cache).ok()?.flatten() {
        let platform_dir = entry.path();
        for hash_entry in std::fs::read_dir(&platform_dir).ok()?.flatten() {
            let dir = hash_entry.path();
            if dir.join("libonnxruntime_providers_cuda.so").exists() {
                return Some(dir.to_string_lossy().to_string());
            }
        }
    }
    None
}

pub async fn dispatch(
    cmd: DistillCmd,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    match cmd {
        DistillCmd::Init { force } => cmd_init(force, reporter).await,
        DistillCmd::Index {
            force,
            file,
            server,
            daemon_child,
        } => {
            if daemon_child {
                cmd_index_daemon(file, server.as_deref(), force).await
            } else {
                cmd_index(file, server.as_deref(), force, reporter).await
            }
        }
        DistillCmd::Search {
            query,
            limit,
            year,
            topic,
            server,
        } => cmd_search(&query, limit, year, topic, server.as_deref(), global).await,
        DistillCmd::Status { server } => cmd_status(server.as_deref(), reporter).await,
        DistillCmd::Hnsw {
            action:
                hs_distill::cli::HnswCmd::Enable {
                    collection,
                    yes,
                    server,
                },
        } => cmd_hnsw_enable(&collection, yes, server.as_deref(), reporter).await,
        DistillCmd::WatchEvents { server } => cmd_watch_events(server, reporter).await,
        DistillCmd::Diagnose { stem, verbose } => cmd_diagnose(&stem, verbose, reporter).await,
        DistillCmd::Reconcile {
            fix_stamps,
            reembed,
            server,
        } => cmd_reconcile(fix_stamps, reembed, server.as_deref(), reporter).await,
        DistillCmd::Purge { doc_id, server } => {
            cmd_purge(&doc_id, server.as_deref(), reporter).await
        }
        DistillCmd::Abstracts(sub) => match sub {
            hs_distill::cli::AbstractsCmd::Build { force, server } => {
                cmd_abstracts_build(server.as_deref(), force, reporter).await
            }
            hs_distill::cli::AbstractsCmd::Reconcile { server } => {
                cmd_abstracts_build(server.as_deref(), false, reporter).await
            }
            hs_distill::cli::AbstractsCmd::Status => cmd_abstracts_status(reporter).await,
        },
    }
}

async fn cmd_purge(doc_id: &str, server: Option<&str>, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let servers = resolve_servers(server).await?;
    let client = DistillClient::new(&servers[0])?;
    reporter.status("Purging", doc_id);
    let deleted = client
        .delete_doc(doc_id)
        .await
        .with_context(|| format!("delete_doc({doc_id})"))?;
    reporter.finish(&format!("Deleted {deleted} chunk(s) for {doc_id}"));
    Ok(())
}

/// Open the local OpenAlex DuckDB read-only using the path from
/// `~/.home-still/config.yaml`. Mirrors `open_openalex_readonly` in hs-mcp;
/// keeps the same memory cap so a stray query can't OOM the CLI process
/// the way it used to OOM hs-mcp.
fn open_openalex_readonly_for_cli() -> Result<duckdb::Connection> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no $HOME"))?;
    let cfg_path = home.join(".home-still").join("config.yaml");
    let raw = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("read {}", cfg_path.display()))?;
    let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&raw)?;
    let oa = v
        .get("openalex")
        .ok_or_else(|| anyhow::anyhow!("missing `openalex:` section in config.yaml"))?;
    let db_path_str = oa
        .get("db_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("openalex.db_path missing"))?;
    let db_path = if let Some(rest) = db_path_str.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(db_path_str)
    };
    let cfg = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
    let conn = duckdb::Connection::open_with_flags(&db_path, cfg)?;
    conn.execute_batch("SET memory_limit='4GB';")?;
    Ok(conn)
}

/// Shared read-only OpenAlex handle. DuckDB is blocking and `!Sync`, so every
/// query goes through `spawn_blocking` behind this lock.
type OpenAlexConn = Arc<std::sync::Mutex<duckdb::Connection>>;

/// Look `doi` up in the local OpenAlex catalog off the async threads. `None`
/// means the work is not in the snapshot; a query failure is an `Err` — it is
/// not "not found", and treating it as such silently embeds a worse abstract.
async fn lookup_openalex(
    conn: &OpenAlexConn,
    doi: &str,
) -> Result<Option<openalex_ingest::WorkAbstract>> {
    let conn = Arc::clone(conn);
    let doi = doi.to_string();
    tokio::task::spawn_blocking(move || {
        let guard = conn
            .lock()
            .map_err(|_| anyhow::anyhow!("OpenAlex connection lock poisoned"))?;
        openalex_ingest::lookup_work_abstract_by_doi(&guard, &doi)
            .map_err(|e| anyhow::anyhow!("OpenAlex lookup for {doi}: {e:#}"))
    })
    .await
    .context("OpenAlex lookup task")?
}

/// How one catalog row ended.
enum AbstractRow {
    /// No source had a usable abstract: not embedded.
    NoAbstract,
    Indexed(hs_distill::abstracts::AbstractSource),
}

/// Coalesce, embed and stamp the abstract of one catalog row. Every failure
/// (OpenAlex query, markdown read, distill, catalog stamp) is an `Err` for
/// THIS row; the caller counts it and fails the command at the end.
async fn build_abstract_row(
    stem: &str,
    entry: &hs_common::catalog::CatalogEntry,
    oa_conn: &OpenAlexConn,
    storage: &dyn hs_common::storage::Storage,
    client: &DistillClient,
) -> Result<AbstractRow> {
    use hs_distill::abstracts::{build_embed_input, coalesce_abstract};

    const COLLECTION: &str = "paper_abstracts";
    const CATALOG_PREFIX: &str = "catalog";

    // 1. OpenAlex DOI lookup. The catalog only has metadata that was
    //    available at download time, which is often nothing — many
    //    DOIs land in the catalog with `title=None` from a metadata
    //    provider that gave a bare PDF URL. The local OA catalog is
    //    the canonical source for both title AND abstract.
    let oa_row = match entry.doi.as_deref() {
        Some(doi) => lookup_openalex(oa_conn, doi).await?,
        None => None,
    };
    let openalex_abstract = oa_row.as_ref().and_then(|w| w.abstract_text.clone());
    // Title coalesce: catalog > OpenAlex > stem. The stem is a degraded
    // signal for pure-DOI filenames but a useful one for
    // author_year_topic personal-corpus naming.
    let title = entry
        .title
        .clone()
        .or_else(|| oa_row.as_ref().and_then(|w| w.title.clone()))
        .unwrap_or_else(|| stem.to_string());

    // 2. Catalog-stored abstract — captured at `paper_download` time
    //    from whichever provider produced the hit (often Crossref,
    //    Semantic Scholar, or arxiv for DOIs not in the OpenAlex
    //    snapshot). Cheap to read — already on the entry we just
    //    loaded.
    let catalog_abstract = entry.abstract_text.clone();

    // 3. Markdown fallback — only fetched if neither structured
    //    source has an abstract, since every fetch is an S3
    //    round-trip and many papers' converted markdown also lacks a
    //    detectable Abstract section. A markdown object that is absent is
    //    "no markdown"; one that cannot be read is this row's error.
    let need_markdown = openalex_abstract.is_none()
        && catalog_abstract
            .as_deref()
            .map(|s| s.trim().chars().count() < hs_distill::abstracts::MIN_ABSTRACT_CHARS)
            .unwrap_or(true);
    let markdown_text = match entry.markdown_path.as_deref().filter(|_| need_markdown) {
        Some(md_key) => match storage.get(md_key).await {
            Ok(bytes) => Some(
                String::from_utf8(bytes)
                    .with_context(|| format!("markdown {md_key} is not valid UTF-8"))?,
            ),
            Err(e) if hs_common::storage::is_not_found(&e) => None,
            Err(e) => return Err(e.context(format!("read markdown {md_key}"))),
        },
        None => None,
    };

    // No usable abstract from any source: skip the paper. A title-only
    // vector would be a degraded stand-in indistinguishable from a real
    // abstract hit in search results.
    let Some(coalesced) = coalesce_abstract(
        openalex_abstract,
        catalog_abstract,
        markdown_text.as_deref(),
    ) else {
        tracing::debug!("{stem}: no usable abstract — not embedded");
        return Ok(AbstractRow::NoAbstract);
    };
    let abstract_chars = coalesced.abstract_chars();
    let embed_input = build_embed_input(Some(&title), &coalesced);

    // 4. POST to the existing /distill endpoint with a synthetic path
    //    so the doc_id resolves to the catalog stem.
    let path_hint = format!("{stem}.md");
    let result = client
        .index_content_in(&path_hint, &embed_input, Some(entry), Some(COLLECTION))
        .await
        .context("embed abstract")?;
    if result.chunks_indexed == 0 {
        // Server returned success but produced 0 chunks (the pipeline's
        // quality filter dropped them, or the chunker emitted nothing
        // usable). Don't stamp — the catalog must never lie about Qdrant
        // state.
        anyhow::bail!("distill produced 0 chunks — not stamping");
    }
    hs_common::catalog::update_abstract_embed_catalog_via(
        storage,
        CATALOG_PREFIX,
        stem,
        coalesced.source.as_str(),
        abstract_chars,
    )
    .await
    .context("stamp catalog after embedding")?;
    Ok(AbstractRow::Indexed(coalesced.source))
}

/// Build the `paper_abstracts` Qdrant collection from every catalog entry.
///
/// Per the abstracts plan: coalesce (OpenAlex DuckDB, then the catalog's
/// provider abstract, then markdown `## Abstract`), embed `title + abstract` via the
/// existing /distill route targeting `collection_name="paper_abstracts"`,
/// then stamp the catalog so reconcile runs are idempotent. Rows that fail
/// are counted and listed, the run continues, and the command exits non-zero.
async fn cmd_abstracts_build(
    server: Option<&str>,
    force: bool,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    use hs_distill::abstracts::AbstractSource;

    const CATALOG_PREFIX: &str = "catalog";

    let stop = crate::shutdown::cooperative();
    let cfg = DistillClientConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage()?;
    let servers = resolve_servers(server).await?;
    let client = DistillClient::new(&servers[0])?;

    reporter.status("Init", "opening OpenAlex DuckDB (read-only)");
    let oa_conn: OpenAlexConn = Arc::new(std::sync::Mutex::new(
        open_openalex_readonly_for_cli()
            .context("open OpenAlex DuckDB — the abstracts pipeline needs the local OA catalog as the canonical source")?,
    ));

    reporter.status("Init", "listing catalog entries");
    let entries = hs_common::catalog::list_catalog_entries_via(&*storage, CATALOG_PREFIX)
        .await
        .context("list catalog entries")?;
    let total = entries.len();
    reporter.status("Catalog", &format!("{total} entries"));

    let mut count_openalex = 0u32;
    let mut count_catalog = 0u32;
    let mut count_markdown = 0u32;
    let mut count_no_abstract = 0u32;
    let mut count_skipped = 0u32;
    let mut errors: Vec<String> = Vec::new();
    let mut interrupted = false;

    for (idx, (stem, _meta, entry)) in entries.into_iter().enumerate() {
        if stop.requested() {
            interrupted = true;
            break;
        }
        if !force && entry.abstract_embed.is_some() {
            count_skipped += 1;
            continue;
        }

        reporter.status(&format!("[{}/{}]", idx + 1, total), &stem);

        match build_abstract_row(&stem, &entry, &oa_conn, &*storage, &client).await {
            Ok(AbstractRow::NoAbstract) => count_no_abstract += 1,
            Ok(AbstractRow::Indexed(AbstractSource::Openalex)) => count_openalex += 1,
            Ok(AbstractRow::Indexed(AbstractSource::Catalog)) => count_catalog += 1,
            Ok(AbstractRow::Indexed(AbstractSource::Markdown)) => count_markdown += 1,
            Err(e) => {
                tracing::warn!("{stem}: {e:#}");
                errors.push(format!("{stem}: {e:#}"));
            }
        }
    }

    reporter.finish(&format!(
        "abstracts indexed: openalex={count_openalex} catalog={count_catalog} markdown={count_markdown} no_abstract={count_no_abstract} skipped={count_skipped} errors={}",
        errors.len()
    ));
    for e in errors.iter().take(10) {
        reporter.warn(e);
    }
    if interrupted {
        anyhow::bail!(
            "abstracts build interrupted with {} error(s); re-run to continue (stamped rows are skipped)",
            errors.len()
        );
    }
    if !errors.is_empty() {
        anyhow::bail!("abstracts build: {} row(s) failed", errors.len());
    }
    Ok(())
}

/// Report `paper_abstracts` coverage from catalog stamps. Does not touch
/// Qdrant — pure catalog-walk so it's safe to run while distill is busy.
async fn cmd_abstracts_status(reporter: &Arc<dyn Reporter>) -> Result<()> {
    const CATALOG_PREFIX: &str = "catalog";
    let cfg = DistillClientConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage()?;

    let entries = hs_common::catalog::list_catalog_entries_via(&*storage, CATALOG_PREFIX)
        .await
        .context("list catalog entries")?;
    let total = entries.len();

    let mut count_openalex = 0u32;
    let mut count_catalog = 0u32;
    let mut count_markdown = 0u32;
    let mut count_title_only = 0u32;
    let mut count_unstamped = 0u32;

    for (_stem, _meta, entry) in entries {
        match entry.abstract_embed.as_ref().map(|s| s.source.as_str()) {
            Some("openalex") => count_openalex += 1,
            Some("catalog") => count_catalog += 1,
            Some("markdown") => count_markdown += 1,
            Some("title_only") => count_title_only += 1,
            _ => count_unstamped += 1,
        }
    }

    reporter.finish(&format!(
        "catalog={total} indexed={} (openalex={count_openalex} catalog={count_catalog} markdown={count_markdown} title_only={count_title_only}) unstamped={count_unstamped}",
        count_openalex + count_catalog + count_markdown + count_title_only
    ));
    Ok(())
}

pub(crate) async fn cmd_watch_events(
    server_override: Option<String>,
    _reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    use hs_distill::client::DistillClient;
    use hs_distill::config::DistillClientConfig;
    use hs_distill::event_watch::{index_and_publish, run_subscriber};

    let cfg = DistillClientConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage()?;
    let bus = cfg.build_event_bus().await?;
    let drain_timeout = cfg
        .events
        .as_ref()
        .context("events section (checked by build_event_bus)")?
        .drain_timeout();

    let server_url = match server_override {
        Some(s) => s,
        None => cfg.require_servers()?[0].clone(),
    };
    let distill =
        Arc::new(DistillClient::new(&server_url)?.with_index_timeout(cfg.index_timeout()));

    let concurrency = cfg.resolved_concurrency();
    tracing::info!(%server_url, concurrency, "starting distill event-bus watcher");

    // A rejected HS_BACKEND_TOKEN stops the watcher here, before it pulls an event.
    hs_distill::event_watch::preflight(&distill).await?;

    let storage_for_handler = storage.clone();
    let bus_for_handler = bus.clone();
    run_subscriber(
        bus.clone(),
        storage.clone(),
        concurrency,
        drain_timeout,
        move |event| {
            let storage = storage_for_handler.clone();
            let bus = bus_for_handler.clone();
            let distill = distill.clone();
            async move {
                index_and_publish(storage.as_ref(), distill.as_ref(), bus.as_ref(), &event).await
            }
        },
    )
    .await
}

// ── Init ────────────────────────────────────────────────────────

async fn cmd_init(force: bool, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let config = DistillServerConfig::load()?;
    let qdrant_rest = qdrant_rest_from_grpc(&config.qdrant_url);

    // Step 1: Check Qdrant availability
    reporter.status("Step 1/3", "Checking Qdrant availability");
    let qdrant_reachable = reqwest::get(&format!("{qdrant_rest}/healthz"))
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    if qdrant_reachable && !force {
        reporter.status("Qdrant", &format!("already reachable at {qdrant_rest}"));
    } else {
        // Need Docker for Qdrant
        let compose = ComposeCmd::detect().await;
        if compose.is_none() {
            if cfg!(target_os = "macos") && check_command("brew", &["--version"]).await {
                anyhow::bail!(
                    "No container runtime found. Install with:\n  \
                     brew install podman docker-compose\n  \
                     podman machine init && podman machine start"
                );
            }
            anyhow::bail!(
                "No container runtime found. Install Docker or Podman:\n  \
                 https://docs.docker.com/get-docker/"
            );
        }
        let compose = compose.unwrap();
        reporter.status(
            "Runtime",
            &format!("{} {}", compose.bin, compose.args_prefix.join(" ")),
        );

        // Step 2: Write compose config
        reporter.status("Step 2/3", "Docker Compose config");
        let compose_path = distill_compose_path();

        if compose_path.exists() && !force {
            reporter.status("Config", "already exists");
        } else {
            std::fs::create_dir_all(hidden_dir())?;
            std::fs::create_dir_all(&config.qdrant_data_dir)?;
            std::fs::write(&compose_path, distill_compose_yaml(&config.qdrant_data_dir))?;

            reporter.status("Written", &format!("{}", compose_path.display()));
        }

        // Step 3: Start Qdrant
        reporter.status("Step 3/3", "Starting Qdrant");
        let cf = compose_path.to_string_lossy().to_string();
        compose.run_capture(&["-f", &cf, "up", "-d"]).await?;
        wait_for_url(&format!("{qdrant_rest}/healthz"), 60, "Qdrant").await?;
        reporter.status("Qdrant", "OK");
    }

    // Check for distill binary
    if find_distill_binary()?.is_none() {
        reporter.warn(
            "hs-distill-server binary not found. Build with:\n  \
             HS_RELEASE_TAG=<tag> cargo build --release -p hs-distill --features server,cuda\n  \
             (the `cuda` feature is required — the binary refuses to compile without it).",
        );
    } else {
        reporter.status("Binary", "hs-distill-server found");
    }

    reporter.finish("Ready! Run: hs distill server start");
    Ok(())
}

// ── Server ──────────────────────────────────────────────────────

pub async fn cmd_server_start(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let config = DistillServerConfig::load()?;
    let qdrant_rest = qdrant_rest_from_grpc(&config.qdrant_url);

    // 1. Start Qdrant container if compose file exists
    let compose_path = distill_compose_path();
    if compose_path.exists() {
        let compose = ComposeCmd::detect()
            .await
            .ok_or_else(|| anyhow::anyhow!("No container runtime found. Run: hs distill init"))?;
        let cf = compose_path.to_string_lossy().to_string();
        compose.run_capture(&["-f", &cf, "up", "-d"]).await?;
        wait_for_url(&format!("{qdrant_rest}/healthz"), 60, "Qdrant").await?;
        reporter.status("Qdrant", "OK");
    } else {
        // No compose file — check if Qdrant is reachable anyway
        let reachable = reqwest::get(&format!("{qdrant_rest}/healthz"))
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if !reachable {
            anyhow::bail!(
                "Qdrant not reachable at {qdrant_rest} and no compose config found.\n\
                 Run: hs distill init"
            );
        }
        reporter.status("Qdrant", &format!("reachable at {qdrant_rest}"));
    }

    // 2. Stop any existing distill server (e.g. orphaned from a previous run)
    let pid_path = distill_pid_path();
    if let Some(pid) = crate::daemon::read_pid(&pid_path) {
        if crate::daemon::is_process_alive(pid) {
            reporter.status("Distill", &format!("stopping old process (PID {pid})"));
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            for _ in 0..50 {
                if !crate::daemon::is_process_alive(pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            #[cfg(unix)]
            if crate::daemon::is_process_alive(pid) {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            crate::daemon::remove_pid_file(&pid_path);
        }
    }

    let binary = find_distill_binary()?.ok_or_else(|| {
        anyhow::anyhow!(
            "hs-distill-server binary not found. Build with:\n  \
             HS_RELEASE_TAG=<tag> cargo build --release -p hs-distill --features server,cuda"
        )
    })?;

    let log_dir = hs_common::resolve_log_dir()?;
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("distill-server.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_file.try_clone()?;

    // Ensure ONNX Runtime CUDA provider .so files can be found at runtime.
    // They live in the ort cache dir alongside the static lib.
    let mut ld_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    if let Some(cache_dir) = find_ort_cuda_libs() {
        if !ld_path.contains(&cache_dir) {
            if !ld_path.is_empty() {
                ld_path.push(':');
            }
            ld_path.push_str(&cache_dir);
        }
    }
    // Include standard CUDA and local lib paths
    for extra in [
        "/usr/local/lib",
        "/opt/cuda/lib64",
        "/opt/cuda/targets/x86_64-linux/lib",
    ] {
        if !ld_path.contains(extra) {
            if !ld_path.is_empty() {
                ld_path.push(':');
            }
            ld_path.push_str(extra);
        }
    }
    // Include ~/.local/lib for user-created compat symlinks and the
    // home-still CUDA-12 runtime-compat dir (rc.267: the pyke cu12 ort
    // bundle needs cublas/cudart/cufft .so.12/.so.11 that a CUDA-13-only
    // host lacks; NVIDIA's runtime-only wheels drop into this dir).
    if let Some(home) = dirs::home_dir() {
        for rel in [".local/lib", ".home-still/cuda12-libs"] {
            let dir = home.join(rel);
            let s = dir.to_string_lossy().to_string();
            if !ld_path.contains(&s) {
                if !ld_path.is_empty() {
                    ld_path.push(':');
                }
                ld_path.push_str(&s);
            }
        }
    }

    // fastembed defaults cache_dir to CWD/.fastembed_cache, which breaks
    // when launched from systemd (CWD = /). Use a stable absolute path.
    let fastembed_cache = std::env::var("FASTEMBED_CACHE_DIR").unwrap_or_else(|_| {
        // Check next to the binary first (where old versions cached the model)
        let beside_binary = binary
            .parent()
            .unwrap_or(binary.as_ref())
            .join(".fastembed_cache");
        if beside_binary.exists() {
            return beside_binary.to_string_lossy().to_string();
        }
        // Otherwise use ~/.home-still/fastembed_cache
        hidden_dir()
            .join("fastembed_cache")
            .to_string_lossy()
            .to_string()
    });

    let child = std::process::Command::new(&binary)
        .env("LD_LIBRARY_PATH", &ld_path)
        .env("FASTEMBED_CACHE_DIR", &fastembed_cache)
        .stdout(log_file)
        .stderr(log_err)
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("Failed to start distill server")?;

    let pid = child.id();
    std::fs::write(&pid_path, pid.to_string())?;

    let distill_url = format!("http://{}:{}/health", config.host, config.port);
    // Server binds to 0.0.0.0 but we check on localhost
    let check_url = format!("http://localhost:{}/health", config.port);
    wait_for_url(&check_url, 300, "distill server")
        .await
        .context(format!(
            "Distill server started (PID {pid}) but health check failed.\n\
         Check logs: {}",
            log_path.display()
        ))?;

    reporter.status("Distill", &format!("OK (PID {pid})"));

    // Auto-start index daemon to process any pending documents
    if ensure_index_running().await? {
        reporter.status("Pipeline", "index daemon started");
    }

    reporter.finish(&format!(
        "Listening on {distill_url}\nLogs: {}",
        log_path.display()
    ));
    Ok(())
}

pub async fn cmd_server_stop(reporter: &Arc<dyn Reporter>) -> Result<()> {
    // 1. Stop native distill server
    let pid_path = distill_pid_path();
    if let Some(pid) = crate::daemon::read_pid(&pid_path) {
        if crate::daemon::is_process_alive(pid) {
            #[cfg(unix)]
            {
                // SIGTERM first, then wait, then SIGKILL
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
                for _ in 0..50 {
                    if !crate::daemon::is_process_alive(pid) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                if crate::daemon::is_process_alive(pid) {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
            }
            crate::daemon::remove_pid_file(&pid_path);
            reporter.status("Distill", &format!("stopped (PID {pid})"));
        } else {
            crate::daemon::remove_pid_file(&pid_path);
            reporter.status("Distill", "not running (stale PID removed)");
        }
    } else {
        reporter.status("Distill", "not running");
    }

    // 2. Stop Qdrant container
    let compose_path = distill_compose_path();
    if compose_path.exists() {
        if let Some(compose) = ComposeCmd::detect().await {
            let cf = compose_path.to_string_lossy().to_string();
            compose.run_capture(&["-f", &cf, "down"]).await?;
            reporter.status("Qdrant", "stopped");
        }
    }

    Ok(())
}

// ── Public API for `hs serve` ──────────────────────────────────

/// Idempotent init: ensures Qdrant and compose config are ready.
/// Skips steps that are already done. Does NOT start the distill server.
pub async fn ensure_init(reporter: &Arc<dyn Reporter>) -> Result<()> {
    cmd_init(false, reporter).await
}

/// Start the distill server in the foreground (blocks until shutdown).
/// Runs the native binary directly instead of as a background daemon.
pub async fn start_server_foreground(port: u16, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let config = DistillServerConfig::load()?;
    let qdrant_rest = qdrant_rest_from_grpc(&config.qdrant_url);

    // Ensure Qdrant is running
    let compose_path = distill_compose_path();
    if compose_path.exists() {
        let compose = ComposeCmd::detect()
            .await
            .ok_or_else(|| anyhow::anyhow!("No container runtime found. Run: hs distill init"))?;
        let cf = compose_path.to_string_lossy().to_string();
        compose.run_capture(&["-f", &cf, "up", "-d"]).await?;
        hs_common::compose::wait_for_url(&format!("{qdrant_rest}/healthz"), 60, "Qdrant").await?;
        reporter.status("Qdrant", "OK");
    } else {
        let reachable = reqwest::get(&format!("{qdrant_rest}/healthz"))
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if !reachable {
            anyhow::bail!(
                "Qdrant not reachable at {qdrant_rest} and no compose config found.\n\
                 Run: hs distill init"
            );
        }
    }

    let binary = find_distill_binary()?.ok_or_else(|| {
        anyhow::anyhow!(
            "hs-distill-server binary not found. Build with:\n  \
             HS_RELEASE_TAG=<tag> cargo build --release -p hs-distill --features server,cuda"
        )
    })?;

    // Build environment (same logic as cmd_server_start)
    let mut ld_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    if let Some(cache_dir) = find_ort_cuda_libs() {
        if !ld_path.contains(&cache_dir) {
            if !ld_path.is_empty() {
                ld_path.push(':');
            }
            ld_path.push_str(&cache_dir);
        }
    }
    for extra in [
        "/usr/local/lib",
        "/opt/cuda/lib64",
        "/opt/cuda/targets/x86_64-linux/lib",
    ] {
        if !ld_path.contains(extra) {
            if !ld_path.is_empty() {
                ld_path.push(':');
            }
            ld_path.push_str(extra);
        }
    }
    if let Some(home) = dirs::home_dir() {
        for rel in [".local/lib", ".home-still/cuda12-libs"] {
            let dir = home.join(rel);
            let s = dir.to_string_lossy().to_string();
            if !ld_path.contains(&s) {
                if !ld_path.is_empty() {
                    ld_path.push(':');
                }
                ld_path.push_str(&s);
            }
        }
    }

    let fastembed_cache = std::env::var("FASTEMBED_CACHE_DIR").unwrap_or_else(|_| {
        let beside_binary = binary
            .parent()
            .unwrap_or(binary.as_ref())
            .join(".fastembed_cache");
        if beside_binary.exists() {
            return beside_binary.to_string_lossy().to_string();
        }
        hidden_dir()
            .join("fastembed_cache")
            .to_string_lossy()
            .to_string()
    });

    reporter.status(
        "Distill",
        &format!("running on port {port} (Ctrl+C to stop)"),
    );

    // Auto-start index daemon once the server is healthy
    let index_port = port;
    tokio::spawn(async move {
        let check_url = format!("http://localhost:{index_port}/health");
        // Wait for the server to become healthy (up to 5 minutes for model load)
        for _ in 0..300 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if reqwest::get(&check_url)
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false)
            {
                match ensure_index_running().await {
                    Ok(true) => {
                        tracing::info!("Auto-started index daemon after distill server ready")
                    }
                    Ok(false) => {}
                    Err(e) => tracing::error!("Failed to start index daemon: {e:#}"),
                }
                return;
            }
        }
    });

    // Run in foreground — inherit stdout/stderr, block until exit
    let status = tokio::process::Command::new(&binary)
        .env("LD_LIBRARY_PATH", &ld_path)
        .env("FASTEMBED_CACHE_DIR", &fastembed_cache)
        .env("HS_DISTILL_PORT", port.to_string())
        .kill_on_drop(true)
        .status()
        .await
        .context("Failed to start distill server")?;

    if !status.success() {
        anyhow::bail!("distill server exited with {status}");
    }

    Ok(())
}

// ── Status ──────────────────────────────────────────────────────

/// Ensure the index daemon is running. Spawns it if not already active.
/// Used by the pipeline auto-trigger: scribe watch → distill index.
///
/// `Ok(true)`: a daemon is running (already, or just spawned). `Ok(false)`:
/// not started because its prerequisites are absent (no distill server
/// binary, server unreachable) — the caller decides whether that matters.
/// `Err`: the prerequisites were present but the daemon could not be spawned.
pub async fn ensure_index_running() -> Result<bool> {
    let pid_path = index_pid_path();
    if let Some(pid) = crate::daemon::read_pid(&pid_path) {
        if crate::daemon::is_process_alive(pid) {
            return Ok(true); // already running
        }
        crate::daemon::remove_pid_file(&pid_path);
    }

    if find_distill_binary()?.is_none() {
        tracing::debug!("Skipping auto-index: hs-distill-server binary not found");
        return Ok(false);
    }

    // Uses an HTTP health check so it works for both local and remote
    // servers (e.g. big_mac → big).
    let server_url = DistillClientConfig::load()?.require_servers()?[0].clone();
    let client = DistillClient::new(&server_url)
        .with_context(|| format!("building distill client for {server_url}"))?;
    if client.health().await.is_err() {
        tracing::debug!("Skipping auto-index: distill server not reachable at {server_url}");
        return Ok(false);
    }

    // Spawn index daemon with defaults (no specific files, no force)
    let pid = spawn_index_daemon(&None, None, false).context("spawning index daemon")?;
    tracing::info!("Auto-started index daemon (PID {pid})");
    Ok(true)
}

async fn cmd_status(server: Option<&str>, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let config = DistillServerConfig::load()?;
    let qdrant_rest = qdrant_rest_from_grpc(&config.qdrant_url);

    // Qdrant health
    let qdrant_ok = reqwest::get(&format!("{qdrant_rest}/healthz"))
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if qdrant_ok {
        reporter.status("Qdrant", &format!("OK ({qdrant_rest})"));
    } else {
        reporter.error(&format!("Qdrant: not reachable at {qdrant_rest}"));
    }

    // Distill server PID
    let pid_path = distill_pid_path();
    match crate::daemon::read_pid(&pid_path) {
        Some(pid) if crate::daemon::is_process_alive(pid) => {
            reporter.status("Server", &format!("running (PID {pid})"));
        }
        _ => {
            reporter.status("Server", "not running");
        }
    }

    // Collection info (if server is reachable)
    let servers = resolve_servers(server).await?;
    let client = DistillClient::new(&servers[0])?;
    match client.status().await {
        Ok(status) => {
            reporter.status("Collection", &status.collection);
            reporter.status("Points", &status.points_count.to_string());
            reporter.status("Device", &status.compute_device);
        }
        Err(_) => {
            reporter.status(
                "Collection",
                &format!("unavailable (server at {} not reachable)", servers[0]),
            );
        }
    }

    // Compose status if available
    let compose_path = distill_compose_path();
    if compose_path.exists() {
        if let Some(compose) = ComposeCmd::detect().await {
            let cf = compose_path.to_string_lossy().to_string();
            let _ = compose.run_capture(&["-f", &cf, "ps"]).await;
        }
    }

    Ok(())
}

// ── Index ───────────────────────────────────────────────────────

const INDEX_STATUS_FILE: &str = "distill-index-status.json";

fn index_pid_path() -> PathBuf {
    hidden_dir().join("distill-index.pid")
}

pub fn index_status_path() -> PathBuf {
    hidden_dir().join(INDEX_STATUS_FILE)
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct IndexStatus {
    pub pid: u32,
    pub total_files: usize,
    pub indexed: usize,
    pub failed: usize,
    pub total_chunks: u32,
    pub current_file: String,
    pub done: bool,
}

pub fn read_index_status() -> Option<IndexStatus> {
    let contents = std::fs::read_to_string(index_status_path()).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Write the status file. It is advisory (a dashboard reads it), so a failed
/// write is logged and must not kill the indexing run — but it is logged.
fn write_index_status(status: &IndexStatus) {
    if let Err(e) = write_index_status_to(&index_status_path(), status) {
        tracing::warn!(error = %e, "could not write the distill index status file");
    }
}

/// Replace `path` with the JSON of `status` atomically: the daemon, the
/// foreground indexer and `hs status` all touch this file, and a plain
/// `fs::write` truncates in place, so a reader (or a second writer) saw it
/// empty or half-written. Write a temp file in the same directory, then
/// rename it over the target.
fn write_index_status_to(path: &Path, status: &IndexStatus) -> std::io::Result<()> {
    use std::io::Write as _;

    let json = serde_json::to_vec(status)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&json)?;
    // `std::fs::rename`, not `persist`: on Windows it retries with a
    // POSIX-semantics rename when a reader holds the target open, where
    // tempfile's bare `MoveFileExW` fails with access denied. The `TempPath`
    // drop removes the temp file if the rename failed.
    let tmp = tmp.into_temp_path();
    std::fs::rename(&tmp, path)
}

/// Spawn the index daemon as a background process.
fn spawn_index_daemon(
    files: &Option<Vec<PathBuf>>,
    server: Option<&str>,
    force: bool,
) -> Result<u32> {
    let exe = std::env::current_exe().context("Cannot find current executable")?;

    let mut args = vec![
        "distill".to_string(),
        "index".to_string(),
        "--daemon-child".to_string(),
    ];
    if force {
        args.push("--force".to_string());
    }
    if let Some(s) = server {
        args.push("--server".to_string());
        args.push(s.to_string());
    }
    if let Some(file_list) = files {
        for f in file_list {
            args.push("--file".to_string());
            args.push(f.to_string_lossy().to_string());
        }
    }

    let log_path = hs_common::resolve_log_dir()?.join("distill-index.log");
    let _ = std::fs::create_dir_all(log_path.parent().unwrap_or(std::path::Path::new(".")));

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("Cannot open daemon log file")?;
    let log_err = log_file.try_clone()?;

    let child = std::process::Command::new(exe)
        .args(&args)
        .stdout(log_file)
        .stderr(log_err)
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn index daemon")?;

    Ok(child.id())
}

/// Foreground: spawn daemon, then attach to its progress.
async fn cmd_index(
    files: Option<Vec<PathBuf>>,
    server: Option<&str>,
    force: bool,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    // Check if daemon already running
    let pid_path = index_pid_path();
    if let Some(pid) = crate::daemon::read_pid(&pid_path) {
        if crate::daemon::is_process_alive(pid) {
            reporter.status(
                "Index",
                &format!("already running (PID {pid}). Attaching..."),
            );
            return attach_index(reporter).await;
        }
        crate::daemon::remove_pid_file(&pid_path);
    }

    // Health check before spawning
    let servers = resolve_servers(server).await?;
    let client = DistillClient::new(&servers[0])?;
    match client.health().await {
        Ok(h) => reporter.status(
            "Connected",
            &format!("{} ({})", servers[0], h.compute_device),
        ),
        Err(e) => {
            return Err(e).context(format!("Is hs-distill-server running at {}?", servers[0]));
        }
    };

    // Spawn daemon
    let pid = spawn_index_daemon(&files, server, force)?;
    reporter.status(
        "Index",
        &format!("daemon started (PID {pid}). Press q to detach."),
    );

    // Wait briefly for status file to appear
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    attach_index(reporter).await
}

/// Attach to a running index daemon — display progress, q to detach.
async fn attach_index(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let raw_enabled = crossterm::terminal::enable_raw_mode().is_ok();
    let mut last_indexed = 0usize;

    loop {
        // Poll for keypress with short timeout (stays responsive)
        if raw_enabled {
            if crossterm::event::poll(std::time::Duration::from_millis(200)).unwrap_or(false) {
                if let Ok(crossterm::event::Event::Key(key)) = crossterm::event::read() {
                    if key.kind == crossterm::event::KeyEventKind::Press
                        && matches!(
                            key.code,
                            crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc
                        )
                    {
                        let _ = crossterm::terminal::disable_raw_mode();
                        reporter.status("Index", "detached. Daemon continues in background.");
                        return Ok(());
                    }
                }
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        // Read status
        if let Some(status) = read_index_status() {
            if status.indexed > last_indexed {
                let _ = crossterm::terminal::disable_raw_mode();
                eprintln!(
                    "  [{}/{}] {} — {} chunks total",
                    status.indexed, status.total_files, status.current_file, status.total_chunks
                );
                if raw_enabled {
                    let _ = crossterm::terminal::enable_raw_mode();
                }
                last_indexed = status.indexed;
            }

            if status.done {
                let _ = crossterm::terminal::disable_raw_mode();
                reporter.finish(&format!(
                    "Indexed {}/{} files, {} chunks ({} failed)",
                    status.indexed, status.total_files, status.total_chunks, status.failed
                ));
                let _ = std::fs::remove_file(index_status_path());
                crate::daemon::remove_pid_file(&index_pid_path());
                return Ok(());
            }

            if !crate::daemon::is_process_alive(status.pid) {
                let _ = crossterm::terminal::disable_raw_mode();
                reporter.error("Index daemon exited unexpectedly. Check logs.");
                return Ok(());
            }
        }
    }
}

/// Daemon child: run the actual indexing loop, write status file.
async fn cmd_index_daemon(
    files: Option<Vec<PathBuf>>,
    server: Option<&str>,
    force: bool,
) -> Result<()> {
    // Write PID
    let pid_path = index_pid_path();
    crate::daemon::write_pid_file(&pid_path)?;

    let servers = resolve_servers(server).await?;
    let client = DistillClient::new(&servers[0])?;

    // Health check
    client
        .health()
        .await
        .context(format!("Is hs-distill-server running at {}?", servers[0]))?;

    // Determine files
    let config = DistillClientConfig::load()?;
    let catalog_dir = config.catalog_dir.clone();
    let markdown_dir = config.markdown_dir;

    let paths: Vec<PathBuf> = if let Some(files) = files {
        files
    } else {
        hs_common::collect_files_recursive(&markdown_dir, "md")
    };

    let mut status = IndexStatus {
        pid: std::process::id(),
        total_files: paths.len(),
        ..Default::default()
    };
    write_index_status(&status);

    for path in &paths {
        let path_str = path.to_string_lossy().to_string();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        status.current_file = stem.to_string();
        write_index_status(&status);

        // Skip if already indexed (unless --force). A failed probe is a
        // failure of this file, not "not indexed".
        if !force {
            match client.doc_exists(stem).await {
                Ok(true) => {
                    status.indexed += 1;
                    write_index_status(&status);
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    status.failed += 1;
                    tracing::error!("{stem}: could not check whether it is indexed: {e}");
                    write_index_status(&status);
                    continue;
                }
            }
        }

        match client.index_file_with_progress(&path_str, |_| {}).await {
            Ok(result) => {
                status.total_chunks += result.chunks_indexed;
                status.indexed += 1;
                if let Err(e) = hs_common::catalog::update_embedding_catalog_via(
                    &hs_common::storage::LocalFsStorage::new(&catalog_dir),
                    "",
                    stem,
                    &servers[0],
                    result.chunks_indexed,
                    &result.embedding_device,
                )
                .await
                {
                    tracing::warn!("{stem}: embedding stamp write failed: {e}");
                }
            }
            Err(e) => {
                status.failed += 1;
                tracing::error!("{stem}: {e}");
            }
        }
        write_index_status(&status);
    }

    status.done = true;
    write_index_status(&status);

    crate::daemon::remove_pid_file(&pid_path);
    if status.failed > 0 {
        anyhow::bail!(
            "{} of {} file(s) failed to index (see the log)",
            status.failed,
            status.total_files
        );
    }
    Ok(())
}

// ── Search ──────────────────────────────────────────────────────

async fn cmd_search(
    query: &str,
    limit: u64,
    year: Option<String>,
    topic: Option<String>,
    server: Option<&str>,
    global: &GlobalArgs,
) -> Result<()> {
    if query.trim().is_empty() {
        anyhow::bail!("Search query cannot be empty");
    }

    let servers = resolve_servers(server).await?;
    let client = make_distill_client(&servers[0]).await?;

    let filters = hs_distill::client::SearchFilters {
        year,
        topic,
        category: None,
    };
    let hits = client
        .search(query, limit, filters)
        .await
        .context(format!("Is hs-distill-server running at {}?", servers[0]))?;

    match global.output {
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(&hits)?;
            println!("{json}");
        }
        OutputFormat::Ndjson => {
            for hit in &hits {
                let line = serde_json::to_string(hit)?;
                println!("{line}");
            }
        }
        OutputFormat::Text => {
            if hits.is_empty() {
                println!("No results found.");
                return Ok(());
            }

            for (i, hit) in hits.iter().enumerate() {
                let title = hit.title.as_deref().unwrap_or(&hit.doc_id);
                let authors = if hit.authors.is_empty() {
                    String::new()
                } else {
                    format!(" by {}", hit.authors.join(", "))
                };
                let year_str = hit.year.map(|y| format!(" ({})", y)).unwrap_or_default();
                let page_info = hit
                    .page
                    .map(|p| format!(" (page {})", p))
                    .unwrap_or_default();
                let pdf = hit.pdf_path.as_deref().unwrap_or("?");

                println!(
                    "\n{}. {}{} [score: {:.3}]{}",
                    i + 1,
                    title,
                    authors,
                    hit.score,
                    year_str
                );
                println!(
                    "   {}:{}-{}{}",
                    hit.doc_id, hit.line_start, hit.line_end, page_info
                );
                println!("   PDF: {pdf}");

                let preview: String = hit.chunk_text.chars().take(200).collect();
                println!("   {preview}...");
            }
        }
    }

    Ok(())
}

// ── Diagnose ────────────────────────────────────────────────────

async fn cmd_diagnose(stem: &str, verbose: bool, reporter: &Arc<dyn Reporter>) -> Result<()> {
    use hs_distill::chunker::{chunk_markdown, ChunkerConfig};
    use hs_distill::quality;
    use hs_distill::types::DocumentMeta;

    let client_cfg = DistillClientConfig::load().context("loading distill client config")?;
    let server_cfg = DistillServerConfig::load().context("loading distill server config")?;
    let storage = client_cfg
        .build_storage()
        .context("building storage backend")?;

    let markdown = hs_common::markdown::read_markdown_via(&*storage, "markdown", stem)
        .await
        .with_context(|| format!("reading markdown for stem '{stem}'"))?
        .ok_or_else(|| anyhow::anyhow!("markdown not found for stem '{stem}'"))?;
    let non_ws = markdown.chars().filter(|c| !c.is_whitespace()).count();
    reporter.status(
        "Markdown",
        &format!("{} bytes ({} non-ws chars)", markdown.len(), non_ws),
    );
    if markdown.trim().is_empty() {
        reporter.error(
            "Markdown is empty after trim — pipeline would return Ok(0) with no catalog write.",
        );
        return Ok(());
    }

    let catalog_entry = hs_common::catalog::read_catalog_entry_via(&*storage, "catalog", stem)
        .await
        .with_context(|| format!("catalog read for {stem}"))?;
    let page_offsets = catalog_entry
        .as_ref()
        .and_then(|e| e.conversion.as_ref())
        .map(|c| c.pages.clone())
        .unwrap_or_default();
    let title = catalog_entry
        .as_ref()
        .and_then(|e| e.title.clone())
        .unwrap_or_else(|| stem.to_string());

    let doc_meta = DocumentMeta {
        doc_id: stem.to_string(),
        title: Some(title),
        markdown_path: format!("markdown/{stem}.md"),
        ..Default::default()
    };
    let chunker_config = ChunkerConfig {
        max_tokens: server_cfg.chunk_max_tokens,
        overlap_tokens: server_cfg.chunk_overlap,
        ..Default::default()
    };
    let chunks = chunk_markdown(&markdown, &doc_meta, &page_offsets, &chunker_config)
        .context("chunking markdown (check distill_server.chunk_max_tokens / chunk_overlap)")?;
    reporter.status(
        "Chunked",
        &format!("{} chunk(s) before filter", chunks.len()),
    );

    let mut accepted = 0usize;
    let mut rejected_too_short = 0usize;
    let mut rejected_dominant_char = 0usize;
    let mut rejected_low_diversity = 0usize;
    let mut rejected_dominant_ngram = 0usize;

    for chunk in &chunks {
        let reason = quality::explain(&chunk.raw_text);
        let non_ws = chunk
            .raw_text
            .chars()
            .filter(|c: &char| !c.is_whitespace())
            .count();
        let page = chunk
            .page
            .map(|p| format!("p{p}"))
            .unwrap_or_else(|| "p?".into());
        let header = format!(
            "[{:02}] L{}-{} {} len={} non-ws={}",
            chunk.chunk_index,
            chunk.span.line_start,
            chunk.span.line_end,
            page,
            chunk.raw_text.len(),
            non_ws
        );
        match &reason {
            None => {
                accepted += 1;
                reporter.status("Accept", &header);
            }
            Some(r) => {
                match r {
                    quality::RejectReason::TooShort { .. } => rejected_too_short += 1,
                    quality::RejectReason::DominantChar { .. } => rejected_dominant_char += 1,
                    quality::RejectReason::LowDiversity { .. } => rejected_low_diversity += 1,
                    quality::RejectReason::DominantNgram { .. } => rejected_dominant_ngram += 1,
                }
                reporter.error(&format!("{header} — reject: {r}"));
            }
        }
        let preview_source: &str = if verbose {
            chunk.raw_text.as_str()
        } else {
            let end = chunk.raw_text.len().min(160);
            let cut = chunk
                .raw_text
                .char_indices()
                .take_while(|(i, _): &(usize, char)| *i < end)
                .last()
                .map(|(i, c): (usize, char)| i + c.len_utf8())
                .unwrap_or(0);
            &chunk.raw_text[..cut]
        };
        let oneline: String = preview_source
            .chars()
            .map(|c| if c == '\n' { ' ' } else { c })
            .collect();
        println!("    {oneline}");
    }

    reporter.status(
        "Summary",
        &format!(
            "accept={accepted} reject={} (short={rejected_too_short}, dom_char={rejected_dominant_char}, low_div={rejected_low_diversity}, dom_ngram={rejected_dominant_ngram})",
            chunks.len() - accepted,
        ),
    );
    if accepted == 0 && !chunks.is_empty() {
        reporter.error("All chunks rejected — pipeline would return Ok(0) with no catalog write.");
    }
    Ok(())
}

// ── HNSW ────────────────────────────────────────────────────────

/// `hs distill hnsw enable`: ask the distill server to enable HNSW on one
/// collection. This starts a background index build over the whole
/// collection on the shared Qdrant host, so it needs an explicit `--yes`.
async fn cmd_hnsw_enable(
    collection: &str,
    yes: bool,
    server: Option<&str>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let servers = resolve_servers(server).await?;
    let client = DistillClient::new(&servers[0])?;
    hnsw_enable(&client, &servers[0], collection, yes, reporter).await
}

async fn hnsw_enable(
    client: &DistillClient,
    server_url: &str,
    collection: &str,
    yes: bool,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    if !yes {
        reporter.status(
            "Would enable",
            &format!(
                "HNSW on collection `{collection}` via {server_url}: Qdrant would build the \
                 graph in the background over the whole collection (CPU/disk load on the \
                 shared host, capped by the server's hnsw.max_indexing_threads)"
            ),
        );
        anyhow::bail!("not submitted: re-run with --yes to start the index build");
    }
    let result = client.enable_hnsw(Some(collection)).await.map_err(|e| {
        match e.downcast_ref::<hs_distill::client::ServerError>() {
            Some(se) if se.status == reqwest::StatusCode::UNAUTHORIZED => anyhow::anyhow!(
                "distill rejected the request (401): the backend token is missing or wrong; \
                 set HS_BACKEND_TOKEN to the token the server was started with"
            ),
            _ => e,
        }
    })?;
    if result.submitted {
        reporter.finish(&format!(
            "submitted: HNSW enable on `{}` (m={}, ef_construct={}, max_indexing_threads={}); {}",
            result.collection,
            result.m,
            result.ef_construct,
            result.max_indexing_threads,
            result.message
        ));
    } else {
        reporter.finish(&format!(
            "already enabled: `{}` (m={}, ef_construct={}); nothing submitted",
            result.collection, result.m, result.ef_construct
        ));
    }
    Ok(())
}

// ── Reconcile (driver) ──────────────────────────────────────────

/// Walk markdown, Qdrant and catalog; heal the divergences that let
/// phantom "unembedded" docs accumulate. See `hs_distill::reconcile` for
/// the pure classification logic; this function is the IO driver.
async fn cmd_reconcile(
    fix_stamps: bool,
    reembed: bool,
    server_override: Option<&str>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    use hs_distill::reconcile::{partition, CatalogState, Classification, ReconcileCounts};
    use std::collections::{HashMap, HashSet};

    let cfg = DistillClientConfig::load().context("loading distill client config")?;
    let storage = cfg.build_storage().context("building storage backend")?;

    let server_url = match server_override {
        Some(s) => s.to_string(),
        None => cfg.require_servers()?[0].clone(),
    };
    let distill = make_distill_client(&server_url).await?;

    reporter.status("Scan", "listing markdown stems from storage");
    let markdown_objects = storage
        .list("markdown")
        .await
        .context("listing markdown prefix")?;
    let markdown_stems: Vec<String> = markdown_objects
        .iter()
        .filter_map(|o| {
            let name = o.key.rsplit('/').next()?;
            if name.starts_with("._") || !name.ends_with(".md") {
                return None;
            }
            Some(name.trim_end_matches(".md").to_string())
        })
        .collect();
    reporter.status("Markdown", &format!("{} stems", markdown_stems.len()));

    reporter.status("Scan", "fetching indexed doc_ids from Qdrant");
    // `list_docs` returns the distinct doc_id facet over the entire
    // collection. 500k is well above our expected corpus size for years.
    let indexed: HashSet<String> = distill
        .list_docs(500_000)
        .await
        .context("listing Qdrant doc_ids")?
        .into_iter()
        .collect();
    reporter.status("Qdrant", &format!("{} indexed docs", indexed.len()));

    reporter.status("Scan", "reading catalog entries");
    let catalog_triples = hs_common::catalog::list_catalog_entries_via(&*storage, "catalog")
        .await
        .context("listing catalog entries")?;
    let catalog: HashMap<String, CatalogState> = catalog_triples
        .into_iter()
        .map(|(stem, _, entry)| {
            (
                stem,
                CatalogState {
                    has_embedding_stamp: entry.embedding.is_some(),
                    embedding_skip_reason: entry.embedding_skip.map(|s| s.reason),
                },
            )
        })
        .collect();
    reporter.status("Catalog", &format!("{} entries", catalog.len()));

    let parts = partition(&markdown_stems, &indexed, &catalog);
    let counts = ReconcileCounts::from_partitions(&parts);
    reporter.status(
        "Partition",
        &format!(
            "ok={}  stamp_missing={}  embed_missing={}",
            counts.ok, counts.stamp_missing, counts.embed_missing
        ),
    );

    // Collect the two actionable buckets so we can drive them in order.
    let stamp_missing: Vec<&str> = parts
        .iter()
        .filter_map(|(s, c)| matches!(c, Classification::StampMissing).then_some(*s))
        .collect();
    let embed_missing: Vec<&str> = parts
        .iter()
        .filter_map(|(s, c)| matches!(c, Classification::EmbedMissing).then_some(*s))
        .collect();

    if !fix_stamps && !reembed {
        reporter.status(
            "Dry run",
            "no changes. Pass --fix-stamps to backfill stamps, --reembed to re-index.",
        );
        if !stamp_missing.is_empty() {
            reporter.status(
                "Sample stamp_missing",
                &stamp_missing
                    .iter()
                    .take(5)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        if !embed_missing.is_empty() {
            reporter.status(
                "Sample embed_missing",
                &embed_missing
                    .iter()
                    .take(5)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        return Ok(());
    }

    // ── Fix stamps ────────────────────────────────────────────────
    let mut stamp_done = 0usize;
    let mut stamp_failed = 0usize;
    if fix_stamps && !stamp_missing.is_empty() {
        // Pull compute_device from the distill health so the backfilled
        // stamp reflects reality (Cpu vs Cuda) instead of a placeholder.
        let device = distill
            .health()
            .await
            .ok()
            .map(|h| h.compute_device)
            .unwrap_or_else(|| "unknown".to_string());

        reporter.status(
            "Fix stamps",
            &format!("backfilling {} embedding stamps", stamp_missing.len()),
        );
        for (i, stem) in stamp_missing.iter().enumerate() {
            if i.is_multiple_of(25) && i > 0 {
                reporter.status("Progress", &format!("{i}/{} stamped", stamp_missing.len()));
            }
            let chunks = match distill.doc_chunks(stem).await {
                Ok((true, n)) => n as u32,
                Ok((false, _)) => {
                    // Doc vanished between list_docs and doc_chunks — rare
                    // race. Skip.
                    stamp_failed += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(%stem, error = %e, "doc_chunks failed during reconcile");
                    stamp_failed += 1;
                    continue;
                }
            };
            match hs_common::catalog::update_embedding_catalog_via(
                &*storage,
                "catalog",
                stem,
                "reconciler-backfill",
                chunks,
                &device,
            )
            .await
            {
                Ok(()) => stamp_done += 1,
                Err(e) => {
                    tracing::warn!(%stem, error = %e, "stamp write failed during reconcile");
                    stamp_failed += 1;
                }
            }
        }
        reporter.status(
            "Stamps",
            &format!("done={stamp_done}  failed={stamp_failed}"),
        );
    }

    // ── Re-embed ──────────────────────────────────────────────────
    let mut embed_done = 0usize;
    let mut embed_failed = 0usize;
    if reembed && !embed_missing.is_empty() {
        reporter.status(
            "Re-embed",
            &format!("indexing {} markdown stems", embed_missing.len()),
        );
        for (i, stem) in embed_missing.iter().enumerate() {
            if i.is_multiple_of(10) && i > 0 {
                reporter.status(
                    "Progress",
                    &format!("{i}/{} re-embedded", embed_missing.len()),
                );
            }
            // Prefer the exact key scribe wrote (stored on the catalog row
            // as `markdown_path`). Re-deriving via `sharded_key(stem, "md")`
            // is only safe when the stem has no normalization drift relative
            // to the original filename — stems with apostrophes or
            // percent-encoded bytes silently orphan the markdown otherwise.
            // Fall back to re-derivation for pre-rc.241 rows that predate
            // the `markdown_path` field.
            let catalog_entry =
                hs_common::catalog::read_catalog_entry_via(&*storage, "catalog", stem)
                    .await
                    .with_context(|| format!("catalog read for {stem}"))?;
            let md_key = catalog_entry
                .as_ref()
                .and_then(|e| e.markdown_path.clone())
                .unwrap_or_else(|| format!("markdown/{}", hs_common::sharded_key(stem, "md")));
            match distill
                .index_from_storage_with_catalog(&*storage, &md_key, catalog_entry.as_ref())
                .await
            {
                Ok(result) => {
                    // Also write the stamp — if we don't, the next
                    // reconcile run will see this as StampMissing and
                    // redo the work.
                    if let Err(e) = hs_common::catalog::record_embedding_outcome_via(
                        &*storage,
                        "catalog",
                        stem,
                        "reconciler-reembed",
                        result.chunks_indexed,
                        &result.embedding_device,
                    )
                    .await
                    {
                        tracing::warn!(%stem, error = %e, "stamp after reembed failed");
                    }
                    embed_done += 1;
                }
                Err(e) => {
                    tracing::warn!(%stem, error = %e, "reembed failed");
                    // Record the failure so the next reconcile cycle
                    // doesn't keep retrying a doc the server can't
                    // handle.
                    let reason = format!("embed_failed: {e}");
                    let _ = hs_common::catalog::update_embedding_skip_via(
                        &*storage, "catalog", stem, &reason,
                    )
                    .await;
                    embed_failed += 1;
                }
            }
        }
        reporter.status(
            "Re-embed",
            &format!("done={embed_done}  failed={embed_failed}"),
        );
    }

    reporter.finish(&format!(
        "Reconcile complete: ok={} stamps_fixed={} embeds_done={} (failures: stamps={} embeds={})",
        counts.ok, stamp_done, embed_done, stamp_failed, embed_failed
    ));
    if stamp_failed + embed_failed > 0 {
        anyhow::bail!(
            "reconcile left {} stamp failure(s) and {} embed failure(s) (see the log)",
            stamp_failed,
            embed_failed
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{FakeServer, Response};
    use async_trait::async_trait;
    use hs_common::catalog::CatalogEntry;
    use hs_common::storage::{LocalFsStorage, ObjectMeta, Storage};

    fn long_abstract() -> String {
        "A sufficiently long abstract about vector search. ".repeat(4)
    }

    fn in_memory_openalex() -> OpenAlexConn {
        Arc::new(std::sync::Mutex::new(
            duckdb::Connection::open_in_memory().unwrap(),
        ))
    }

    async fn distill_indexing(chunks: u32) -> FakeServer {
        FakeServer::start(move |req| match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/distill") => Response::json(
                200,
                &serde_json::json!({
                    "doc_id": "10.1_abc", "chunks_indexed": chunks, "embedding_device": "cuda"
                }),
            ),
            _ => Response::json(404, &serde_json::json!({})),
        })
        .await
    }

    async fn catalog_row(storage: &LocalFsStorage, stem: &str, entry: &CatalogEntry) {
        hs_common::catalog::write_catalog_entry_via(storage, "catalog", stem, entry)
            .await
            .unwrap();
    }

    /// RA-110: `lookup_work_abstract_by_doi(..).ok().flatten()` turned a
    /// broken OpenAlex database into "work not found", and the row was
    /// embedded from a worse source without a word.
    #[tokio::test]
    async fn an_openalex_query_failure_fails_the_row_instead_of_reading_as_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let server = distill_indexing(1).await;
        let client = DistillClient::new(&server.base).unwrap();
        let entry = CatalogEntry {
            doi: Some("10.1/abc".into()),
            abstract_text: Some(long_abstract()),
            ..Default::default()
        };
        catalog_row(&storage, "10.1_abc", &entry).await;

        // An in-memory database has no `works` table: the query itself fails.
        let err = build_abstract_row("10.1_abc", &entry, &in_memory_openalex(), &storage, &client)
            .await
            .err()
            .expect("a failing OpenAlex query must fail the row");

        assert!(format!("{err:#}").contains("OpenAlex"), "{err:#}");
        assert!(
            server.requests().is_empty(),
            "nothing was embedded from a fallback source"
        );
    }

    #[tokio::test]
    async fn a_catalog_abstract_is_embedded_and_stamped() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let server = distill_indexing(1).await;
        let client = DistillClient::new(&server.base).unwrap();
        let entry = CatalogEntry {
            abstract_text: Some(long_abstract()),
            ..Default::default()
        };
        catalog_row(&storage, "10.1_abc", &entry).await;

        let row = build_abstract_row("10.1_abc", &entry, &in_memory_openalex(), &storage, &client)
            .await
            .unwrap();

        assert!(matches!(
            row,
            AbstractRow::Indexed(hs_distill::abstracts::AbstractSource::Catalog)
        ));
        let stamped = hs_common::catalog::read_catalog_entry_via(&storage, "catalog", "10.1_abc")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stamped.abstract_embed.unwrap().source, "catalog");
    }

    #[tokio::test]
    async fn zero_chunks_is_a_row_failure_and_is_not_stamped() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let server = distill_indexing(0).await;
        let client = DistillClient::new(&server.base).unwrap();
        let entry = CatalogEntry {
            abstract_text: Some(long_abstract()),
            ..Default::default()
        };
        catalog_row(&storage, "10.1_abc", &entry).await;

        let err = build_abstract_row("10.1_abc", &entry, &in_memory_openalex(), &storage, &client)
            .await
            .err()
            .expect("0 chunks is a failure");

        assert!(format!("{err:#}").contains("0 chunks"), "{err:#}");
        let row = hs_common::catalog::read_catalog_entry_via(&storage, "catalog", "10.1_abc")
            .await
            .unwrap()
            .unwrap();
        assert!(row.abstract_embed.is_none(), "the catalog must not lie");
    }

    #[tokio::test]
    async fn a_row_without_any_abstract_is_skipped_not_embedded_from_its_title() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let server = distill_indexing(1).await;
        let client = DistillClient::new(&server.base).unwrap();
        let entry = CatalogEntry {
            title: Some("Only A Title".into()),
            ..Default::default()
        };

        let row = build_abstract_row("10.1_abc", &entry, &in_memory_openalex(), &storage, &client)
            .await
            .unwrap();

        assert!(matches!(row, AbstractRow::NoAbstract));
        assert!(server.requests().is_empty());
    }

    /// `get` fails with a transport error for one key; every other call works.
    struct FailingGet(LocalFsStorage, &'static str);

    #[async_trait]
    impl Storage for FailingGet {
        async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
            if key == self.1 {
                anyhow::bail!("503 Slow Down");
            }
            self.0.get(key).await
        }
        async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
            self.0.put(key, bytes).await
        }
        async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
            self.0.head(key).await
        }
        async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
            self.0.list(prefix).await
        }
        async fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.0.delete(key).await
        }
    }

    #[tokio::test]
    async fn an_unreadable_markdown_fails_the_row_but_a_missing_one_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = FailingGet(LocalFsStorage::new(tmp.path()), "markdown/10/10.1_abc.md");
        let server = distill_indexing(1).await;
        let client = DistillClient::new(&server.base).unwrap();
        let broken = CatalogEntry {
            markdown_path: Some("markdown/10/10.1_abc.md".into()),
            ..Default::default()
        };
        let err = build_abstract_row(
            "10.1_abc",
            &broken,
            &in_memory_openalex(),
            &storage,
            &client,
        )
        .await
        .err()
        .expect("a storage error is a row failure");
        assert!(format!("{err:#}").contains("503"), "{err:#}");

        let missing = CatalogEntry {
            markdown_path: Some("markdown/10/never-written.md".into()),
            ..Default::default()
        };
        let row = build_abstract_row(
            "10.1_xyz",
            &missing,
            &in_memory_openalex(),
            &storage,
            &client,
        )
        .await
        .unwrap();
        assert!(matches!(row, AbstractRow::NoAbstract));
    }

    fn sample_status(i: usize) -> IndexStatus {
        IndexStatus {
            pid: 1000 + i as u32,
            total_files: 1000,
            indexed: i,
            failed: 0,
            total_chunks: 5,
            // Large enough that a non-atomic write is observable mid-way.
            current_file: format!("file-{i}-{}", "x".repeat(64 * 1024)),
            done: false,
        }
    }

    /// RA-86: the daemon and the foreground indexer both write this file and
    /// `hs status` reads it; `fs::write` truncates in place, so a reader saw
    /// an empty or partial document (and read it as "no indexer").
    #[test]
    fn the_index_status_file_is_never_observable_half_written() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("distill-index-status.json");
        write_index_status_to(&path, &sample_status(0)).unwrap();

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..3)
            .map(|w| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..150 {
                        write_index_status_to(&path, &sample_status(w * 1000 + i)).unwrap();
                    }
                })
            })
            .collect();
        let reader = {
            let path = path.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut reads = 0u32;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let text = std::fs::read_to_string(&path).expect("the file always exists");
                    serde_json::from_str::<IndexStatus>(&text)
                        .unwrap_or_else(|e| panic!("observed a torn status file: {e}"));
                    reads += 1;
                }
                reads
            })
        };
        for w in writers {
            w.join().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);

        // No temp files are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    }
}

#[cfg(test)]
mod hnsw_tests {
    use super::*;
    use crate::test_http::{FakeServer, Response};

    fn reporter() -> Arc<dyn Reporter> {
        Arc::new(hs_common::reporter::SilentReporter)
    }

    async fn server(status: u16, body: serde_json::Value) -> FakeServer {
        FakeServer::start(move |_| Response::json(status, &body)).await
    }

    fn ok_body(submitted: bool) -> serde_json::Value {
        serde_json::json!({"collection": "academic_papers", "submitted": submitted, "m": 16,
            "ef_construct": 100, "max_indexing_threads": 4, "message": "building"})
    }

    #[tokio::test]
    async fn without_yes_nothing_is_sent_and_the_command_fails() {
        let s = server(200, ok_body(true)).await;
        let c = DistillClient::new(&s.base).unwrap();
        let err = hnsw_enable(&c, &s.base, "academic_papers", false, &reporter())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("--yes"));
        assert!(s.requests().is_empty());
    }

    #[tokio::test]
    async fn with_yes_the_named_collection_is_posted() {
        for submitted in [true, false] {
            let s = server(200, ok_body(submitted)).await;
            let c = DistillClient::new(&s.base).unwrap();
            hnsw_enable(&c, &s.base, "academic_papers", true, &reporter())
                .await
                .unwrap();
            let reqs = s.requests();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "POST");
            assert_eq!(reqs[0].path, "/collection/hnsw?collection=academic_papers");
        }
    }

    #[tokio::test]
    async fn a_401_names_the_backend_token_and_a_400_is_shown_verbatim() {
        let s = server(401, serde_json::json!({"error": "unauthorized"})).await;
        let c = DistillClient::new(&s.base).unwrap();
        let err = hnsw_enable(&c, &s.base, "x", true, &reporter())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("HS_BACKEND_TOKEN"), "{err:#}");

        let s = server(400, serde_json::json!({"error": "unknown collection nope"})).await;
        let c = DistillClient::new(&s.base).unwrap();
        let err = hnsw_enable(&c, &s.base, "nope", true, &reporter())
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown collection nope"),
            "{err:#}"
        );
    }
}
