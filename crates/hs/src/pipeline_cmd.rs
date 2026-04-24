//! `hs pipeline` — cross-service pipeline operations.
//!
//! Commands here span scribe + distill + storage and are intentionally
//! CLI-only (not exposed via MCP) because they wipe or mass-republish
//! state that an agent should not invoke.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use dialoguer::Confirm;
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;
use hs_distill::client::DistillClient;
use hs_distill::config::DistillClientConfig;

const CONFIRM_TOKEN: &str = "rebuild-from-papers";
const DEFAULT_DISTILL_URL: &str = "http://localhost:7434";

#[derive(Subcommand, Debug)]
pub enum PipelineCmd {
    /// Wipe derived state (markdown, catalog, Qdrant vectors) and republish
    /// `papers.ingested` for every PDF/HTML under `papers/` so scribe + distill
    /// rebuild the entire pipeline from source. Papers themselves are never
    /// touched.
    Rebuild {
        /// Count what would be deleted / republished without touching anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the interactive confirmation prompt. Still required:
        /// `--confirm rebuild-from-papers` so scripted invocations can't
        /// silently wipe the corpus.
        #[arg(long)]
        yes: bool,
        /// Typed confirmation token. Must equal `rebuild-from-papers`.
        /// Required when `--yes` is set; otherwise the interactive prompt
        /// collects it.
        #[arg(long)]
        confirm: Option<String>,
    },
    /// Republish `papers.ingested` for every paper under `papers/` that does
    /// not yet have a matching markdown file. Use after bringing a new scribe
    /// worker online mid-rebuild so it can pitch in on the remaining queue.
    /// Never deletes.
    CatchUp {
        /// Report what would be republished without touching anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete the JetStream PAPERS and SCRIBE streams — all queued and
    /// in-flight events are discarded. Use when a consumer is stuck
    /// with a stale config (e.g. wrong ack_wait) and `create_or_update`
    /// alone can't recover it. The worker daemons recreate the streams
    /// on their next connect using the current `NatsConfig`. Follow
    /// with `hs pipeline catch-up` to re-queue unconverted papers.
    EventsReset,
    /// Delete HTML paywall / loading-stub artifacts — source `.html`,
    /// derived `.md`, and catalog `.yaml` — for every catalog entry
    /// stamped `embedding_skip.reason = zero_chunks_or_empty` AND
    /// produced by the html-parser. These are known-junk ingests
    /// (PMC "Preparing to download" interstitials etc.) that a
    /// newly-stricter pre-conversion guard now rejects at the door;
    /// this removes the legacy residue so `hs pipeline catch-up`
    /// stops re-queueing them.
    PurgeSkipped {
        /// Report what would be deleted without touching anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

pub async fn dispatch(cmd: PipelineCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        PipelineCmd::Rebuild {
            dry_run,
            yes,
            confirm,
        } => cmd_rebuild(dry_run, yes, confirm, reporter).await,
        PipelineCmd::CatchUp { dry_run } => cmd_catch_up(dry_run, reporter).await,
        PipelineCmd::EventsReset => cmd_events_reset(reporter).await,
        PipelineCmd::PurgeSkipped { dry_run, yes } => {
            cmd_purge_skipped(dry_run, yes, reporter).await
        }
    }
}

async fn cmd_events_reset(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let cfg = DistillClientConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let bus_cfg = cfg.events.clone();
    if bus_cfg.backend != hs_common::event_bus::EventsBackend::Nats {
        reporter.warn("events.backend is not `nats` — nothing to reset.");
        return Ok(());
    }
    let nats =
        hs_common::event_bus::nats::NatsBus::connect(hs_common::event_bus::nats::NatsConfig {
            url: bus_cfg.nats.url.clone(),
            ack_wait: std::time::Duration::from_secs(bus_cfg.nats.ack_wait_secs),
            max_deliver: bus_cfg.nats.max_deliver,
            max_age: std::time::Duration::from_secs(bus_cfg.nats.max_age_secs),
            max_ack_pending: bus_cfg.nats.max_ack_pending,
        })
        .await
        .context("connecting to NATS for stream reset")?;
    nats.reset_streams()
        .await
        .context("reset JetStream streams")?;
    reporter
        .finish("Deleted JetStream streams PAPERS and SCRIBE. Next worker connect recreates them.");
    Ok(())
}

async fn cmd_catch_up(dry_run: bool, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let cfg = DistillClientConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage().context("building storage backend")?;

    let papers = storage.list("papers").await.context("list papers prefix")?;
    let markdown = storage
        .list("markdown")
        .await
        .context("list markdown prefix")?;

    use std::collections::HashSet;
    let md_stems: HashSet<String> = markdown
        .iter()
        .filter_map(|o| {
            let name = o.key.rsplit('/').next()?;
            if name.starts_with("._") || !name.ends_with(".md") {
                return None;
            }
            Some(name.trim_end_matches(".md").to_string())
        })
        .collect();

    let mut to_republish: Vec<String> = Vec::new();
    for obj in &papers {
        let name = match obj.key.rsplit('/').next() {
            Some(n) if !n.starts_with("._") => n,
            _ => continue,
        };
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if e == "pdf" || e == "html" => (s, e),
            _ => continue,
        };
        if md_stems.contains(stem) {
            continue;
        }
        let _ = ext;
        to_republish.push(obj.key.clone());
    }

    reporter.status(
        "Papers",
        &format!(
            "{} total, {} have markdown, {} pending republish",
            papers.len(),
            md_stems.len(),
            to_republish.len()
        ),
    );

    if dry_run {
        reporter.finish("Dry-run complete — no events published.");
        return Ok(());
    }
    if to_republish.is_empty() {
        reporter.finish("Nothing to do — every paper already has markdown.");
        return Ok(());
    }

    let bus = cfg
        .build_event_bus()
        .await
        .context("building event bus for papers.ingested publish")?;

    let mut published = 0u64;
    let mut errors: Vec<String> = Vec::new();
    for (i, key) in to_republish.iter().enumerate() {
        let payload = serde_json::json!({
            "key": key,
            "source": "hs pipeline catch-up",
        });
        match bus
            .publish(
                "papers.ingested",
                serde_json::to_vec(&payload).unwrap_or_default().as_slice(),
            )
            .await
        {
            Ok(()) => published += 1,
            Err(e) => errors.push(format!("publish/{key}: {e}")),
        }
        if (i + 1) % 500 == 0 {
            reporter.status(
                "Republish",
                &format!("published {published}/{}", to_republish.len()),
            );
        }
    }

    reporter.finish(&format!(
        "Catch-up queued — papers_republished={published} errors={}",
        errors.len()
    ));
    for e in errors.iter().take(10) {
        eprintln!("  error: {e}");
    }
    if errors.len() > 10 {
        eprintln!("  ... and {} more", errors.len() - 10);
    }
    Ok(())
}

async fn cmd_rebuild(
    dry_run: bool,
    yes: bool,
    confirm: Option<String>,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let cfg = DistillClientConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage().context("building storage backend")?;
    let server_url = cfg
        .servers
        .first()
        .cloned()
        .unwrap_or_else(|| DEFAULT_DISTILL_URL.to_string());
    let client = DistillClient::new(&server_url);

    // Inventory: used for both dry-run report and live-run "before" snapshot.
    let inv = inventory(&storage, &client).await?;
    print_summary(&inv, reporter);

    if dry_run {
        reporter.finish("Dry-run complete — no state changed.");
        return Ok(());
    }

    // Confirmation gate: either --yes with the right --confirm token, or
    // interactive typed prompt that asks for the same token.
    let confirmed = match (yes, confirm.as_deref()) {
        (true, Some(CONFIRM_TOKEN)) => true,
        (true, Some(other)) => {
            anyhow::bail!("--confirm must be `{CONFIRM_TOKEN}` when --yes is set (got `{other}`)");
        }
        (true, None) => {
            anyhow::bail!("--yes requires --confirm {CONFIRM_TOKEN}");
        }
        (false, _) => {
            eprintln!();
            eprintln!(
                "This will delete {} markdown objects, {} catalog YAMLs, \
                 and drop the Qdrant collection ({} points across {} docs).",
                inv.markdown_count, inv.catalog_count, inv.qdrant_points, inv.qdrant_docs
            );
            eprintln!(
                "{} papers under `papers/` will be republished for re-ingestion.",
                inv.paper_keys.len()
            );
            eprintln!("Papers themselves are NOT touched.");
            eprintln!();
            let accept = Confirm::new()
                .with_prompt("Proceed? (this cannot be undone — expect hours of scribe work)")
                .default(false)
                .interact()?;
            if !accept {
                reporter.finish("Aborted — no state changed.");
                return Ok(());
            }
            // Second gate: typed token.
            let typed: String = dialoguer::Input::new()
                .with_prompt(format!("Type `{CONFIRM_TOKEN}` to confirm"))
                .interact_text()?;
            if typed.trim() != CONFIRM_TOKEN {
                anyhow::bail!("confirmation token mismatch — aborted, no state changed");
            }
            true
        }
    };
    debug_assert!(confirmed);

    let bus = cfg
        .build_event_bus()
        .await
        .context("building event bus for papers.ingested publish")?;

    let started_at = chrono::Utc::now().to_rfc3339();
    reporter.status("Pipeline rebuild", &format!("started at {started_at}"));

    // 1. Drop + recreate Qdrant collection.
    reporter.status("Qdrant", "drop + recreate collection");
    let qdrant_deleted = client
        .reset_collection()
        .await
        .context("distill reset_collection")?;

    // 2. Delete every markdown object.
    let mut markdown_deleted = 0u64;
    let mut errors: Vec<String> = Vec::new();
    for (i, obj) in inv.markdown_objs.iter().enumerate() {
        match storage.delete(&obj.key).await {
            Ok(()) => markdown_deleted += 1,
            Err(e) => errors.push(format!("markdown-delete/{}: {e}", obj.key)),
        }
        if (i + 1) % 500 == 0 {
            reporter.status(
                "Markdown",
                &format!("deleted {markdown_deleted}/{}", inv.markdown_count),
            );
        }
    }

    // 3. Delete every catalog YAML.
    let mut catalog_deleted = 0u64;
    for (i, obj) in inv.catalog_objs.iter().enumerate() {
        match storage.delete(&obj.key).await {
            Ok(()) => catalog_deleted += 1,
            Err(e) => errors.push(format!("catalog-delete/{}: {e}", obj.key)),
        }
        if (i + 1) % 500 == 0 {
            reporter.status(
                "Catalog",
                &format!("deleted {catalog_deleted}/{}", inv.catalog_count),
            );
        }
    }

    // 4. Republish papers.ingested for every paper.
    let mut papers_republished = 0u64;
    for (i, key) in inv.paper_keys.iter().enumerate() {
        let payload = serde_json::json!({
            "key": key,
            "source": "hs pipeline rebuild",
        });
        match bus
            .publish(
                "papers.ingested",
                serde_json::to_vec(&payload).unwrap_or_default().as_slice(),
            )
            .await
        {
            Ok(()) => papers_republished += 1,
            Err(e) => errors.push(format!("publish/{key}: {e}")),
        }
        if (i + 1) % 500 == 0 {
            reporter.status(
                "Republish",
                &format!("published {papers_republished}/{}", inv.paper_keys.len()),
            );
        }
    }

    reporter.finish(&format!(
        "Pipeline rebuild queued — markdown_deleted={markdown_deleted} \
         catalog_deleted={catalog_deleted} qdrant_deleted={qdrant_deleted} \
         papers_republished={papers_republished} errors={} \
         (watch `hs status` for scribe + distill catch-up)",
        errors.len()
    ));

    if !errors.is_empty() {
        for e in errors.iter().take(10) {
            eprintln!("  error: {e}");
        }
        if errors.len() > 10 {
            eprintln!("  ... and {} more", errors.len() - 10);
        }
    }
    Ok(())
}

struct Inventory {
    paper_keys: Vec<String>,
    markdown_objs: Vec<hs_common::storage::ObjectMeta>,
    markdown_count: u64,
    catalog_objs: Vec<hs_common::storage::ObjectMeta>,
    catalog_count: u64,
    qdrant_docs: u64,
    qdrant_points: u64,
}

async fn inventory(storage: &Arc<dyn Storage>, client: &DistillClient) -> Result<Inventory> {
    let papers = storage.list("papers").await.context("list papers prefix")?;
    let paper_keys: Vec<String> = papers
        .into_iter()
        .filter_map(|o| {
            let name = o.key.rsplit('/').next()?;
            if name.starts_with("._") {
                return None;
            }
            let ext = name.rsplit_once('.').map(|(_, e)| e)?;
            if ext == "pdf" || ext == "html" {
                Some(o.key)
            } else {
                None
            }
        })
        .collect();

    let markdown_objs = storage
        .list("markdown")
        .await
        .context("list markdown prefix")?;
    let markdown_count = markdown_objs.len() as u64;

    let catalog_objs = storage
        .list("catalog")
        .await
        .context("list catalog prefix")?;
    let catalog_count = catalog_objs.len() as u64;

    let qdrant_ids = client.list_docs(u64::MAX).await.unwrap_or_default();
    let qdrant_docs = qdrant_ids.len() as u64;
    let qdrant_points = client.status().await.map(|s| s.points_count).unwrap_or(0);

    Ok(Inventory {
        paper_keys,
        markdown_objs,
        markdown_count,
        catalog_objs,
        catalog_count,
        qdrant_docs,
        qdrant_points,
    })
}

fn print_summary(inv: &Inventory, reporter: &Arc<dyn Reporter>) {
    reporter.status("Papers (keep)", &format!("{}", inv.paper_keys.len()));
    reporter.status("Markdown to delete", &format!("{}", inv.markdown_count));
    reporter.status("Catalog to delete", &format!("{}", inv.catalog_count));
    reporter.status(
        "Qdrant to purge",
        &format!("{} points / {} docs", inv.qdrant_points, inv.qdrant_docs),
    );
    reporter.status("Papers to republish", &format!("{}", inv.paper_keys.len()));
}

async fn cmd_purge_skipped(dry_run: bool, yes: bool, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let cfg = DistillClientConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage().context("building storage backend")?;

    reporter.status("Scan", "catalog for embedding_skip stubs");
    let triples = hs_common::catalog::list_catalog_entries_via(&*storage, "catalog")
        .await
        .context("list catalog entries")?;

    // A "junk HTML stub" is an entry whose distill decision was
    // `zero_chunks_or_empty` AND whose converter was `html-parser`.
    // Restricting on both fields keeps this from ever nuking a PDF whose
    // extraction legitimately failed — a PDF path would use `scribe-vlm`.
    struct Victim {
        stem: String,
        catalog_key: String,
    }
    let victims: Vec<Victim> = triples
        .into_iter()
        .filter_map(|(stem, obj, entry)| {
            let skip = entry.embedding_skip.as_ref()?;
            if skip.reason != "zero_chunks_or_empty" {
                return None;
            }
            let conv = entry.conversion.as_ref()?;
            if conv.server != "html-parser" {
                return None;
            }
            Some(Victim {
                stem,
                catalog_key: obj.key,
            })
        })
        .collect();

    reporter.status(
        "Victims",
        &format!("{} HTML stubs identified", victims.len()),
    );
    if victims.is_empty() {
        reporter.finish("Nothing to purge — no HTML stubs stamped `zero_chunks_or_empty`.");
        return Ok(());
    }

    for v in victims.iter().take(5) {
        reporter.status("Sample", &v.stem);
    }
    if victims.len() > 5 {
        reporter.status("...", &format!("+{} more", victims.len() - 5));
    }

    if dry_run {
        reporter.finish("Dry-run complete — no state changed.");
        return Ok(());
    }

    if !yes {
        let accept = Confirm::new()
            .with_prompt(format!(
                "Delete {} HTML stubs (source .html + .md + catalog .yaml)?",
                victims.len()
            ))
            .default(false)
            .interact()?;
        if !accept {
            reporter.finish("Aborted — no state changed.");
            return Ok(());
        }
    }

    let mut md_deleted = 0u64;
    let mut cat_deleted = 0u64;
    let mut src_deleted = 0u64;
    let mut errors: Vec<String> = Vec::new();

    for (i, v) in victims.iter().enumerate() {
        // Catalog yaml — we already have the exact key from the listing.
        match storage.delete(&v.catalog_key).await {
            Ok(()) => cat_deleted += 1,
            Err(e) => errors.push(format!("catalog/{}: {e}", v.stem)),
        }

        // Markdown — always `markdown/{shard}/{stem}.md`.
        let md_key = hs_common::markdown::markdown_storage_key(&v.stem);
        match storage.delete(&md_key).await {
            Ok(()) => md_deleted += 1,
            Err(e) => errors.push(format!("markdown/{}: {e}", v.stem)),
        }

        // Source HTML — extension may be `html` or `htm`. Try both, count
        // one success per stem. `delete` on a missing key is not an error.
        let mut src_hit = false;
        for ext in ["html", "htm"] {
            let key = format!("papers/{}", hs_common::sharded_key(&v.stem, ext));
            if storage.exists(&key).await.unwrap_or(false) {
                match storage.delete(&key).await {
                    Ok(()) => {
                        src_hit = true;
                        break;
                    }
                    Err(e) => errors.push(format!("papers/{}.{ext}: {e}", v.stem)),
                }
            }
        }
        if src_hit {
            src_deleted += 1;
        }

        if (i + 1) % 50 == 0 {
            reporter.status("Progress", &format!("purged {}/{}", i + 1, victims.len()));
        }
    }

    reporter.finish(&format!(
        "Purged HTML stubs — catalog={cat_deleted} markdown={md_deleted} source={src_deleted} errors={}",
        errors.len()
    ));
    for e in errors.iter().take(10) {
        eprintln!("  error: {e}");
    }
    if errors.len() > 10 {
        eprintln!("  ... and {} more", errors.len() - 10);
    }
    Ok(())
}
