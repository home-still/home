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
}

pub async fn dispatch(cmd: PipelineCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        PipelineCmd::Rebuild {
            dry_run,
            yes,
            confirm,
        } => cmd_rebuild(dry_run, yes, confirm, reporter).await,
    }
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
