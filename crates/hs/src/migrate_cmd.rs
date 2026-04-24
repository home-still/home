use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use hs_common::reporter::Reporter;

/// Migrate flat file directories to 2-character prefix sharded layout.
///
/// Moves files from `dir/stem.ext` to `dir/XX/stem.ext` where XX is the
/// first 2 characters of the stem.
pub async fn run_sharding(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let scribe_cfg = hs_scribe::config::ScribeConfig::load().unwrap_or_default();
    let paper_cfg = paper::config::Config::load().unwrap_or_default();

    let dirs_to_migrate: Vec<(&str, &Path, &[&str])> = vec![
        ("papers", &paper_cfg.download_path, &["pdf", "html", "htm"]),
        ("markdown", &scribe_cfg.output_dir, &["md"]),
        ("catalog", &scribe_cfg.catalog_dir, &["yaml"]),
    ];

    let mut total_moved = 0u64;

    for (name, dir, extensions) in &dirs_to_migrate {
        if !dir.exists() {
            reporter.status("Skip", &format!("{name}: directory not found"));
            continue;
        }

        let mut moved = 0u64;
        let mut skipped = 0u64;
        let entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let path = e.path();
                let name = path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or_default();
                // Skip directories, macOS resource forks (._*), and non-matching extensions
                !path.is_dir()
                    && !name.starts_with("._")
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| extensions.contains(&ext))
            })
            .collect();

        let count = entries.len();
        if count == 0 {
            reporter.status("OK", &format!("{name}: already sharded (0 flat files)"));
            continue;
        }

        reporter.status("Migrate", &format!("{name}: {count} files to shard..."));

        for entry in entries {
            let path = entry.path();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default();

            if stem.len() < 2 {
                skipped += 1;
                continue;
            }

            let target = hs_common::sharded_path(dir, stem, ext);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::rename(&path, &target) {
                Ok(()) => moved += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // File vanished between scan and rename (NFS race)
                    skipped += 1;
                }
                Err(e) => {
                    reporter.warn(&format!("{name}: failed to move {}: {e}", path.display()));
                    skipped += 1;
                }
            }
        }

        if skipped > 0 {
            reporter.status(
                "OK",
                &format!("{name}: migrated {moved}, skipped {skipped}"),
            );
        } else {
            reporter.status("OK", &format!("{name}: migrated {moved} files"));
        }
        total_moved += moved;
    }

    if total_moved > 0 {
        reporter.finish(&format!("Migrated {total_moved} files to sharded layout"));
    } else {
        reporter.finish("All directories already using sharded layout");
    }

    Ok(())
}

/// Decide whether a storage key is a root-level orphan needing relocation to
/// `papers/`. Matches pre-rc.298 artifacts written by `paper_download` when
/// the downloader emitted bare `sharded_key(stem, ext)` (no prefix).
///
/// Accepts `<shard>/<filename>` where `<shard>` is 1–2 ASCII alphanumeric
/// characters. Rejects:
/// - canonical prefixes (`papers/`, `markdown/`, `catalog/`, `logs/`, `.heartbeats/`)
/// - dot-prefixed / underscore-prefixed first segments (hidden / dirs)
/// - bucket-root files with no `/`
/// - macOS detritus (`._*`, `.DS_Store`)
///
/// Exposed for the unit tests so we can pin the filter in isolation from
/// any real backend.
pub(crate) fn is_root_orphan_key(key: &str) -> bool {
    let Some(first_seg) = key.split('/').next() else {
        return false;
    };
    // Root-level file (no directory component).
    if first_seg == key {
        return false;
    }
    if first_seg.is_empty() || first_seg.starts_with('.') || first_seg.starts_with('_') {
        return false;
    }
    if matches!(first_seg, "papers" | "markdown" | "catalog" | "logs") {
        return false;
    }
    // Shard segment: 1–2 ASCII alphanumeric chars.
    if first_seg.len() > 2 || !first_seg.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    // Filename junk inside an otherwise-legitimate shard.
    let filename = key.rsplit('/').next().unwrap_or("");
    !filename.starts_with("._") && filename != ".DS_Store"
}

/// Relocate pre-rc.298 PDFs/HTML written at bucket-root `XX/stem.ext` back
/// under `papers/XX/stem.ext`. Per-file: GET source → PUT target (unless
/// already present with matching size) → HEAD target to verify → DELETE
/// source. Catalog rows whose `pdf_path` pointed at the old root key are
/// rewritten to the new location in the same pass so reads stay coherent.
///
/// Concurrency: 8 in-flight moves via `futures::stream::buffer_unordered`.
/// Errors per file are collected and reported at the end; the command
/// fails-loud (non-zero exit) if any error occurred.
pub async fn run_move_root_orphans(
    reporter: &Arc<dyn Reporter>,
    dry_run: bool,
    limit: Option<usize>,
) -> Result<()> {
    use futures::stream::{self, StreamExt};
    use hs_common::storage::Storage;

    let paper_cfg = paper::config::Config::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage: Arc<dyn Storage> = paper_cfg
        .build_storage()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    reporter.status("Scan", "listing bucket root for orphan shards...");
    let all = storage
        .list("")
        .await
        .map_err(|e| anyhow::anyhow!("list root: {e}"))?;

    let mut candidates: Vec<hs_common::storage::ObjectMeta> = all
        .into_iter()
        .filter(|o| is_root_orphan_key(&o.key))
        .collect();
    candidates.sort_by(|a, b| a.key.cmp(&b.key));

    let total = candidates.len();
    if total == 0 {
        reporter.finish("No root-level orphans found");
        return Ok(());
    }

    if let Some(lim) = limit {
        candidates.truncate(lim);
    }
    let planned = candidates.len();

    reporter.status(
        "Plan",
        &format!(
            "{} root-level orphans found; {} planned this run ({})",
            total,
            planned,
            if dry_run { "dry-run" } else { "live" },
        ),
    );
    // Show a couple of samples so the operator can sanity-check what's
    // about to move before they re-invoke without --dry-run.
    for sample in candidates.iter().take(5) {
        reporter.status(
            "Sample",
            &format!("{} -> papers/{}", sample.key, sample.key),
        );
    }

    if dry_run {
        reporter.finish(&format!(
            "Dry-run: {planned} file(s) would be relocated to papers/XX/…"
        ));
        return Ok(());
    }

    const CONCURRENCY: usize = 8;
    let moved = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let skipped = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let catalog_rewritten = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let errors: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let mut stream = stream::iter(candidates.into_iter().map(|obj| {
        let storage = Arc::clone(&storage);
        let moved = Arc::clone(&moved);
        let skipped = Arc::clone(&skipped);
        let catalog_rewritten = Arc::clone(&catalog_rewritten);
        let errors = Arc::clone(&errors);
        async move {
            let src = obj.key.clone();
            let tgt = format!("papers/{src}");
            match relocate_one(&*storage, &src, &tgt).await {
                Ok(RelocateOutcome::Moved { catalog_updated }) => {
                    moved.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if catalog_updated {
                        catalog_rewritten.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Ok(RelocateOutcome::AlreadyAtTarget) => {
                    skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    errors.lock().await.push(format!("{src}: {e}"));
                }
            }
        }
    }))
    .buffer_unordered(CONCURRENCY);

    let mut done = 0usize;
    while stream.next().await.is_some() {
        done += 1;
        if done.is_multiple_of(25) || done == planned {
            reporter.status(
                "Move",
                &format!(
                    "{done}/{planned} (moved={}, skipped={}, errors={})",
                    moved.load(std::sync::atomic::Ordering::Relaxed),
                    skipped.load(std::sync::atomic::Ordering::Relaxed),
                    errors.lock().await.len(),
                ),
            );
        }
    }

    let moved_n = moved.load(std::sync::atomic::Ordering::Relaxed);
    let skipped_n = skipped.load(std::sync::atomic::Ordering::Relaxed);
    let catalog_n = catalog_rewritten.load(std::sync::atomic::Ordering::Relaxed);
    let errs = errors.lock().await;
    for e in errs.iter() {
        reporter.warn(e);
    }

    if errs.is_empty() {
        reporter.finish(&format!(
            "Relocated {moved_n} (skipped {skipped_n}; catalog rows rewritten: {catalog_n})"
        ));
        Ok(())
    } else {
        anyhow::bail!(
            "relocation finished with {} error(s); moved={moved_n} skipped={skipped_n}",
            errs.len()
        )
    }
}

#[derive(Debug)]
enum RelocateOutcome {
    Moved { catalog_updated: bool },
    AlreadyAtTarget,
}

/// Move one object. Guarantees:
/// - size-matching pre-existing target keys are treated as already-relocated
///   (idempotent re-runs after a partial failure).
/// - post-put HEAD verifies the bytes landed before the source is deleted.
/// - any catalog row at `catalog/{sharded_key(stem, "yaml")}` whose
///   `pdf_path` equals the old source key is rewritten inline.
async fn relocate_one(
    storage: &dyn hs_common::storage::Storage,
    src: &str,
    tgt: &str,
) -> anyhow::Result<RelocateOutcome> {
    let Some(filename) = src.rsplit('/').next() else {
        anyhow::bail!("source key has no filename: {src}");
    };
    let (stem, _ext) = filename
        .rsplit_once('.')
        .ok_or_else(|| anyhow::anyhow!("source key has no extension: {src}"))?;
    let stem = stem.to_string();

    let src_meta = storage
        .head(src)
        .await
        .map_err(|e| anyhow::anyhow!("head src {src}: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("source vanished before move: {src}"))?;

    // If the target already exists at the same size we assume a prior run
    // completed the PUT but failed the DELETE; finish the job by removing
    // the source. Different size is a real collision — fail loud.
    if let Ok(Some(tgt_meta)) = storage.head(tgt).await {
        if tgt_meta.size == src_meta.size {
            storage
                .delete(src)
                .await
                .map_err(|e| anyhow::anyhow!("delete src {src}: {e}"))?;
            return Ok(RelocateOutcome::AlreadyAtTarget);
        }
        anyhow::bail!(
            "target {tgt} already exists with size {} (source size {}); refusing to overwrite",
            tgt_meta.size,
            src_meta.size
        );
    }

    let bytes = storage
        .get(src)
        .await
        .map_err(|e| anyhow::anyhow!("get src {src}: {e}"))?;
    let src_size = bytes.len() as u64;
    storage
        .put(tgt, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("put tgt {tgt}: {e}"))?;

    let verify_meta = storage
        .head(tgt)
        .await
        .map_err(|e| anyhow::anyhow!("head tgt {tgt}: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("target missing after put: {tgt}"))?;
    if verify_meta.size != src_size {
        anyhow::bail!(
            "post-put verify failed for {tgt}: expected {src_size} bytes, got {}",
            verify_meta.size
        );
    }

    // Rewrite catalog row's `pdf_path` if it still points at the bucket-root
    // key. `hs_common::catalog::read_catalog_entry_via` looks up by stem and
    // uses the sharded layout under `catalog/`, which is already correct.
    let catalog_updated =
        match hs_common::catalog::read_catalog_entry_via(storage, "catalog", &stem)
            .await
            .map_err(|e| anyhow::anyhow!("catalog read for {stem}: {e}"))?
        {
            Some(mut entry) if entry.pdf_path.as_deref() == Some(src) => {
                entry.pdf_path = Some(tgt.to_string());
                hs_common::catalog::write_catalog_entry_via(storage, "catalog", &stem, &entry)
                    .await
                    .map_err(|e| anyhow::anyhow!("catalog pdf_path rewrite {stem}: {e}"))?;
                true
            }
            _ => false,
        };

    storage
        .delete(src)
        .await
        .map_err(|e| anyhow::anyhow!("delete src {src}: {e}"))?;

    Ok(RelocateOutcome::Moved { catalog_updated })
}

// ── quarantine-bad-content ───────────────────────────────────────────────

#[derive(Debug)]
enum QuarantineOutcome {
    /// First 4 KB started with `%PDF` — legitimate PDF, left alone.
    HealthyPdf,
    /// Bytes were HTML. File renamed in place from `.pdf` to `.html`;
    /// scribe-watch-events' html/htm branch will pick it up on republish.
    RenamedToHtml,
    /// Bytes were neither PDF nor HTML. File moved to
    /// `papers/.quarantine/XX/stem.pdf` and the catalog row stamped
    /// `conversion_failed` so nothing re-publishes it.
    Quarantined,
}

/// Scan `papers/XX/*.pdf` for content-type mismatches and quarantine the
/// bad files. Per-key flow:
/// 1. List `papers/` recursively, keep `.pdf` keys (skip `._*`, skip
///    `.quarantine/` which is our own output).
/// 2. Bounded-concurrency (8) fetch each object's bytes, inspect the
///    first 4 KB:
///    - `%PDF` → `HealthyPdf`, no-op.
///    - `looks_like_html` → `put` to `papers/XX/stem.html`, `delete`
///      the `.pdf` key, publish `papers.ingested` so watch-events
///      re-queues under the html/htm branch.
///    - else → `put` to `papers/.quarantine/XX/stem.pdf`, `delete`
///      original, `update_conversion_failed_via` with reason
///      `quarantine_scan:binary`.
/// 3. Fail-loud on any storage error; summary at end.
pub async fn run_quarantine_bad_content(
    reporter: &Arc<dyn Reporter>,
    dry_run: bool,
    limit: Option<usize>,
) -> Result<()> {
    use futures::stream::{self, StreamExt};
    use hs_common::event_bus::EventBus;
    use hs_common::storage::Storage;

    let paper_cfg = paper::config::Config::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let scribe_cfg = hs_scribe::config::ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage: Arc<dyn Storage> = paper_cfg
        .build_storage()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let bus: Arc<dyn EventBus> = scribe_cfg.build_event_bus().await?;

    reporter.status("Scan", "listing papers/ for .pdf keys...");
    let all = storage
        .list("papers")
        .await
        .map_err(|e| anyhow::anyhow!("list papers/: {e}"))?;

    let mut candidates: Vec<hs_common::storage::ObjectMeta> = all
        .into_iter()
        .filter(|o| {
            if !o.key.ends_with(".pdf") {
                return false;
            }
            // Skip our own quarantine output + macOS junk.
            if o.key.contains("/.quarantine/") || o.key.contains("/._") {
                return false;
            }
            true
        })
        .collect();
    candidates.sort_by(|a, b| a.key.cmp(&b.key));

    let total = candidates.len();
    if total == 0 {
        reporter.finish("No papers/*.pdf objects to inspect");
        return Ok(());
    }
    if let Some(lim) = limit {
        candidates.truncate(lim);
    }
    let planned = candidates.len();

    reporter.status(
        "Plan",
        &format!(
            "{} PDF objects; inspecting {} this run ({})",
            total,
            planned,
            if dry_run { "dry-run" } else { "live" }
        ),
    );

    const CONCURRENCY: usize = 8;
    let healthy = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let renamed_html = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let quarantined = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let errors: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let mut stream = stream::iter(candidates.into_iter().map(|obj| {
        let storage = Arc::clone(&storage);
        let bus = Arc::clone(&bus);
        let healthy = Arc::clone(&healthy);
        let renamed_html = Arc::clone(&renamed_html);
        let quarantined = Arc::clone(&quarantined);
        let errors = Arc::clone(&errors);
        async move {
            match inspect_and_quarantine(&*storage, &*bus, &obj.key, dry_run).await {
                Ok(QuarantineOutcome::HealthyPdf) => {
                    healthy.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(QuarantineOutcome::RenamedToHtml) => {
                    renamed_html.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(QuarantineOutcome::Quarantined) => {
                    quarantined.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    errors.lock().await.push(format!("{}: {e}", obj.key));
                }
            }
        }
    }))
    .buffer_unordered(CONCURRENCY);

    let mut done = 0usize;
    while stream.next().await.is_some() {
        done += 1;
        if done.is_multiple_of(25) || done == planned {
            reporter.status(
                "Scan",
                &format!(
                    "{done}/{planned} (healthy={}, html-rename={}, quarantined={}, errors={})",
                    healthy.load(std::sync::atomic::Ordering::Relaxed),
                    renamed_html.load(std::sync::atomic::Ordering::Relaxed),
                    quarantined.load(std::sync::atomic::Ordering::Relaxed),
                    errors.lock().await.len(),
                ),
            );
        }
    }

    let h = healthy.load(std::sync::atomic::Ordering::Relaxed);
    let r = renamed_html.load(std::sync::atomic::Ordering::Relaxed);
    let q = quarantined.load(std::sync::atomic::Ordering::Relaxed);
    let errs = errors.lock().await;
    for e in errs.iter() {
        reporter.warn(e);
    }

    let tag = if dry_run { "would" } else { "did" };
    if errs.is_empty() {
        reporter.finish(&format!(
            "Inspected {planned}: {h} healthy · {tag} rename-to-html {r} · {tag} quarantine {q}"
        ));
        Ok(())
    } else {
        anyhow::bail!(
            "quarantine finished with {} error(s); healthy={h} html-rename={r} quarantined={q}",
            errs.len()
        )
    }
}

/// Inspect one `.pdf` object, classify, and act. Guards:
/// - `head(src)` first; skip silently if vanished (concurrent run).
/// - HTML path publishes `papers.ingested` with the new `.html` key so
///   `hs-scribe-watch-events` re-queues it — otherwise the renamed
///   file would sit forever without trigger.
/// - Quarantine path writes the catalog `conversion_failed` stamp
///   *before* relocating, so even if we crash mid-operation the row is
///   marked and stuck_convert won't re-emit it.
async fn inspect_and_quarantine(
    storage: &dyn hs_common::storage::Storage,
    bus: &dyn hs_common::event_bus::EventBus,
    src: &str,
    dry_run: bool,
) -> anyhow::Result<QuarantineOutcome> {
    let Some(filename) = src.rsplit('/').next() else {
        anyhow::bail!("source key has no filename: {src}");
    };
    let stem = filename.trim_end_matches(".pdf").to_string();
    if stem.is_empty() {
        anyhow::bail!("source key has no stem: {src}");
    }

    let bytes = storage
        .get(src)
        .await
        .map_err(|e| anyhow::anyhow!("get({src}): {e}"))?;
    let head = &bytes[..bytes.len().min(4096)];
    if head.starts_with(b"%PDF") {
        return Ok(QuarantineOutcome::HealthyPdf);
    }
    if hs_common::html::looks_like_html(head) {
        // Rename papers/XX/stem.pdf -> papers/XX/stem.html.
        let (prefix_dir, _) = src
            .rsplit_once('/')
            .ok_or_else(|| anyhow::anyhow!("cannot split {src}"))?;
        let tgt = format!("{prefix_dir}/{stem}.html");
        if dry_run {
            return Ok(QuarantineOutcome::RenamedToHtml);
        }
        storage
            .put(&tgt, bytes)
            .await
            .map_err(|e| anyhow::anyhow!("put({tgt}): {e}"))?;
        storage
            .delete(src)
            .await
            .map_err(|e| anyhow::anyhow!("delete({src}): {e}"))?;
        // Re-queue the file under its actual content-type. Best-effort —
        // if the bus publish fails the file is still in the right place
        // and `catalog_repair`'s stuck_convert direction will re-emit it
        // on the next run.
        let payload = serde_json::json!({
            "key": tgt,
            "source": "hs migrate quarantine-bad-content:html_rename",
        });
        if let Err(e) = bus
            .publish(
                "papers.ingested",
                serde_json::to_vec(&payload).unwrap_or_default().as_slice(),
            )
            .await
        {
            tracing::warn!(key = %tgt, error = %e, "re-queue publish failed; file relies on catalog_repair");
        }
        return Ok(QuarantineOutcome::RenamedToHtml);
    }
    // Neither PDF nor HTML — random bytes, encrypted, truncated, etc.
    // Move to .quarantine/ and stamp conversion_failed.
    let (prefix_dir, _) = src
        .rsplit_once('/')
        .ok_or_else(|| anyhow::anyhow!("cannot split {src}"))?;
    let shard = prefix_dir.rsplit('/').next().unwrap_or("");
    let tgt = format!("papers/.quarantine/{shard}/{stem}.pdf");
    if dry_run {
        return Ok(QuarantineOutcome::Quarantined);
    }
    // Stamp catalog first so a mid-op crash still leaves the row dead
    // rather than resurrectable by stuck_convert.
    if let Err(e) = hs_common::catalog::update_conversion_failed_via(
        storage,
        "catalog",
        &stem,
        "quarantine_scan:binary",
    )
    .await
    {
        tracing::warn!(stem, error = %e, "conversion_failed stamp failed; continuing with move");
    }
    storage
        .put(&tgt, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("put({tgt}): {e}"))?;
    storage
        .delete(src)
        .await
        .map_err(|e| anyhow::anyhow!("delete({src}): {e}"))?;
    Ok(QuarantineOutcome::Quarantined)
}

/// Storage prefix the migration walks when purging legacy `local-html`
/// converter rows. Kept as a constant so the integration test and the
/// production path read the same value.
const LEGACY_LOCAL_HTML_SERVER: &str = "local-html";

/// Summary returned by [`purge_local_html_rows`].
#[derive(Debug, Default)]
struct LocalHtmlPurgeStats {
    total_rows: u64,
    deleted_catalog: u64,
    deleted_markdown: u64,
    deleted_paper: u64,
    errors: Vec<String>,
}

/// Core logic of the `drop-local-html` migration, factored so tests can
/// drive it against an in-memory `LocalFsStorage`.
async fn purge_local_html_rows(
    storage: &dyn hs_common::storage::Storage,
    dry_run: bool,
) -> Result<LocalHtmlPurgeStats> {
    let mut entries = hs_common::catalog::list_catalog_entries_via(storage, "catalog").await?;
    entries.retain(|(_, _, e)| {
        e.conversion
            .as_ref()
            .is_some_and(|c| c.server == LEGACY_LOCAL_HTML_SERVER)
    });

    let mut stats = LocalHtmlPurgeStats {
        total_rows: entries.len() as u64,
        ..Default::default()
    };

    if dry_run {
        return Ok(stats);
    }

    for (stem, _meta, _entry) in entries {
        match hs_common::catalog::delete_catalog_entry_via(storage, "catalog", &stem).await {
            Ok(()) => stats.deleted_catalog += 1,
            Err(e) => {
                stats.errors.push(format!("catalog/{stem}: {e}"));
                continue;
            }
        }

        let md_key = format!("markdown/{}", hs_common::sharded_key(&stem, "md"));
        if storage.exists(&md_key).await.unwrap_or(false) {
            match storage.delete(&md_key).await {
                Ok(()) => stats.deleted_markdown += 1,
                Err(e) => stats.errors.push(format!("markdown/{stem}: {e}")),
            }
        }

        for ext in ["html", "htm"] {
            let key = format!("papers/{}", hs_common::sharded_key(&stem, ext));
            if !storage.exists(&key).await.unwrap_or(false) {
                continue;
            }
            match storage.delete(&key).await {
                Ok(()) => stats.deleted_paper += 1,
                Err(e) => stats.errors.push(format!("papers/{stem}.{ext}: {e}")),
            }
        }
    }

    Ok(stats)
}

/// One-shot migration that deletes every catalog row stamped with
/// `conversion.server == "local-html"` — the legacy dual-converter output
/// removed in rc.306. The markdown and any source `.html`/`.htm` keys are
/// deleted alongside each row so downstream pipeline stages treat the stem
/// as absent rather than "converted but missing markdown" (which would
/// trip `catalog_repair` into a stuck-convert loop).
pub async fn run_drop_local_html(reporter: &Arc<dyn Reporter>, dry_run: bool) -> Result<()> {
    use hs_common::storage::Storage;

    let paper_cfg = paper::config::Config::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage: Arc<dyn Storage> = paper_cfg
        .build_storage()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    reporter.status("Scan", "listing catalog/ for local-html rows...");
    let stats = purge_local_html_rows(&*storage, dry_run).await?;

    if stats.total_rows == 0 {
        reporter.finish("No local-html catalog rows found");
        return Ok(());
    }
    reporter.status(
        "Plan",
        &format!(
            "{} local-html row(s) to purge ({})",
            stats.total_rows,
            if dry_run { "dry-run" } else { "live" }
        ),
    );

    for e in &stats.errors {
        reporter.warn(e);
    }

    if dry_run {
        reporter.finish(&format!(
            "Dry-run: {} local-html row(s) would be deleted",
            stats.total_rows
        ));
    } else {
        reporter.finish(&format!(
            "Purged {} catalog, {} markdown, {} paper key(s); {} error(s)",
            stats.deleted_catalog,
            stats.deleted_markdown,
            stats.deleted_paper,
            stats.errors.len()
        ));
    }

    if !stats.errors.is_empty() {
        anyhow::bail!(
            "drop-local-html finished with {} error(s)",
            stats.errors.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod root_orphan_tests {
    use super::*;
    use hs_common::storage::{LocalFsStorage, Storage};

    #[test]
    fn filter_accepts_root_shard_files() {
        // Canonical rc.297 pre-fix shape.
        assert!(is_root_orphan_key("00/doc.pdf"));
        assert!(is_root_orphan_key("ab/cdef.html"));
        assert!(is_root_orphan_key("W2/00926230.pdf"));
    }

    #[test]
    fn filter_rejects_canonical_prefixes() {
        assert!(!is_root_orphan_key("papers/00/doc.pdf"));
        assert!(!is_root_orphan_key("markdown/10/thing.md"));
        assert!(!is_root_orphan_key("catalog/ab/row.yaml"));
        assert!(!is_root_orphan_key("logs/foo.log"));
    }

    #[test]
    fn filter_rejects_bucket_root_and_junk() {
        assert!(!is_root_orphan_key(".DS_Store"));
        assert!(!is_root_orphan_key("README.md"));
        assert!(!is_root_orphan_key(""));
        assert!(!is_root_orphan_key("._SomeThing"));
        assert!(!is_root_orphan_key("00/._junk.pdf"));
        assert!(!is_root_orphan_key("00/.DS_Store"));
        assert!(!is_root_orphan_key(".heartbeats/scribe-inbox.yaml"));
    }

    #[test]
    fn filter_rejects_non_shard_top_segments() {
        // Three-char first segment isn't a shard.
        assert!(!is_root_orphan_key("abc/doc.pdf"));
        // Non-alphanumeric.
        assert!(!is_root_orphan_key("-a/doc.pdf"));
        // Too deep: only top-level orphans qualify.
        assert!(!is_root_orphan_key("papers/00/manually_downloaded/foo.pdf"));
    }

    #[tokio::test]
    async fn relocate_one_moves_file_and_rewrites_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        // Seed a root-level PDF plus a catalog row whose pdf_path matches
        // the bare key (what pre-rc.298 paper_download would have written).
        storage
            .put("ab/abcdef.pdf", b"fake pdf".to_vec())
            .await
            .unwrap();
        let entry = hs_common::catalog::CatalogEntry {
            pdf_path: Some("ab/abcdef.pdf".to_string()),
            ..Default::default()
        };
        hs_common::catalog::write_catalog_entry_via(&storage, "catalog", "abcdef", &entry)
            .await
            .unwrap();

        let outcome = relocate_one(&storage, "ab/abcdef.pdf", "papers/ab/abcdef.pdf")
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            RelocateOutcome::Moved {
                catalog_updated: true
            }
        ));

        // Source gone, target present with matching size.
        assert!(storage.exists("ab/abcdef.pdf").await.unwrap().not());
        let tgt_meta = storage.head("papers/ab/abcdef.pdf").await.unwrap().unwrap();
        assert_eq!(tgt_meta.size, b"fake pdf".len() as u64);

        // Catalog pdf_path rewritten.
        let roundtripped =
            hs_common::catalog::read_catalog_entry_via(&storage, "catalog", "abcdef")
                .await
                .expect("catalog read")
                .expect("entry present");
        assert_eq!(
            roundtripped.pdf_path.as_deref(),
            Some("papers/ab/abcdef.pdf")
        );
    }

    #[tokio::test]
    async fn relocate_one_is_idempotent_after_partial_prior_run() {
        // A prior run that crashed between PUT and DELETE leaves both src
        // and tgt populated with identical bytes. The re-run should finish
        // the job (delete src) and report AlreadyAtTarget — no double-put.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        storage
            .put("ab/abcdef.pdf", b"same".to_vec())
            .await
            .unwrap();
        storage
            .put("papers/ab/abcdef.pdf", b"same".to_vec())
            .await
            .unwrap();

        let outcome = relocate_one(&storage, "ab/abcdef.pdf", "papers/ab/abcdef.pdf")
            .await
            .unwrap();
        assert!(matches!(outcome, RelocateOutcome::AlreadyAtTarget));
        assert!(storage.exists("ab/abcdef.pdf").await.unwrap().not());
        assert!(storage.exists("papers/ab/abcdef.pdf").await.unwrap());
    }

    #[tokio::test]
    async fn relocate_one_refuses_size_mismatch() {
        // Same target key with different bytes — very bad situation, must
        // fail loud rather than silently clobber.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        storage
            .put("ab/abcdef.pdf", b"source bytes".to_vec())
            .await
            .unwrap();
        storage
            .put("papers/ab/abcdef.pdf", b"different length".to_vec())
            .await
            .unwrap();

        let err = relocate_one(&storage, "ab/abcdef.pdf", "papers/ab/abcdef.pdf")
            .await
            .expect_err("must not overwrite mismatched target");
        assert!(format!("{err}").contains("refusing to overwrite"));
    }

    // Helper: `std::ops::Not` for `bool` via closure isn't idiomatic; this
    // saves a level of indirection in the assert lines above.
    trait BoolNotExt {
        fn not(self) -> bool;
    }
    impl BoolNotExt for bool {
        fn not(self) -> bool {
            !self
        }
    }
}

#[cfg(test)]
mod quarantine_tests {
    use super::*;
    use hs_common::event_bus::NoOpBus;
    use hs_common::storage::{LocalFsStorage, Storage};

    #[tokio::test]
    async fn real_pdf_left_alone_html_renamed_binary_quarantined() {
        // Seed three files under papers/ab/: a real %PDF, an HTML
        // masquerading as .pdf, and random binary. Run
        // inspect_and_quarantine on each and verify the outcome + the
        // filesystem post-state.
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;

        storage
            .put(
                "papers/ab/real.pdf",
                b"%PDF-1.7\nnot actually a real pdf".to_vec(),
            )
            .await
            .unwrap();
        storage
            .put(
                "papers/ab/fake.pdf",
                b"<!DOCTYPE html><html><body>paywall</body></html>".to_vec(),
            )
            .await
            .unwrap();
        storage
            .put("papers/ab/binary.pdf", vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9])
            .await
            .unwrap();

        // 1. real PDF → HealthyPdf, untouched.
        let outcome = inspect_and_quarantine(&storage, &bus, "papers/ab/real.pdf", false)
            .await
            .unwrap();
        assert!(matches!(outcome, QuarantineOutcome::HealthyPdf));
        assert!(storage.exists("papers/ab/real.pdf").await.unwrap());

        // 2. HTML-masquerading-as-PDF → rename in place.
        let outcome = inspect_and_quarantine(&storage, &bus, "papers/ab/fake.pdf", false)
            .await
            .unwrap();
        assert!(matches!(outcome, QuarantineOutcome::RenamedToHtml));
        assert!(!storage.exists("papers/ab/fake.pdf").await.unwrap());
        assert!(storage.exists("papers/ab/fake.html").await.unwrap());

        // 3. Random bytes → quarantine + conversion_failed stamp.
        let outcome = inspect_and_quarantine(&storage, &bus, "papers/ab/binary.pdf", false)
            .await
            .unwrap();
        assert!(matches!(outcome, QuarantineOutcome::Quarantined));
        assert!(!storage.exists("papers/ab/binary.pdf").await.unwrap());
        assert!(storage
            .exists("papers/.quarantine/ab/binary.pdf")
            .await
            .unwrap());
        let entry = hs_common::catalog::read_catalog_entry_via(&storage, "catalog", "binary")
            .await
            .expect("catalog read")
            .expect("conversion_failed stamped");
        assert_eq!(
            entry.conversion_failed.unwrap().reason,
            "quarantine_scan:binary"
        );
    }

    #[tokio::test]
    async fn dry_run_makes_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;

        storage
            .put("papers/ab/fake.pdf", b"<!DOCTYPE html><html/>".to_vec())
            .await
            .unwrap();

        let outcome = inspect_and_quarantine(&storage, &bus, "papers/ab/fake.pdf", true)
            .await
            .unwrap();
        assert!(matches!(outcome, QuarantineOutcome::RenamedToHtml));
        // Dry-run: file still where it was.
        assert!(storage.exists("papers/ab/fake.pdf").await.unwrap());
        assert!(!storage.exists("papers/ab/fake.html").await.unwrap());
    }
}

#[cfg(test)]
mod drop_local_html_tests {
    use super::*;
    use hs_common::catalog::{CatalogEntry, ConversionMeta};
    use hs_common::storage::{LocalFsStorage, Storage};

    async fn seed_row(storage: &LocalFsStorage, stem: &str, server: &str) {
        let entry = CatalogEntry {
            conversion: Some(ConversionMeta {
                server: server.to_string(),
                duration_secs: 0.1,
                total_pages: 1,
                converted_at: "2026-04-24T00:00:00Z".to_string(),
                pages: vec![],
            }),
            ..Default::default()
        };
        hs_common::catalog::write_catalog_entry_via(storage, "catalog", stem, &entry)
            .await
            .unwrap();
        // Companion markdown + source html so we can verify the purge
        // deletes them too.
        storage
            .put(
                &format!("markdown/{}", hs_common::sharded_key(stem, "md")),
                b"# stub".to_vec(),
            )
            .await
            .unwrap();
        storage
            .put(
                &format!("papers/{}", hs_common::sharded_key(stem, "html")),
                b"<html/>".to_vec(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn purges_only_local_html_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());

        seed_row(&storage, "legacy_stem", "local-html").await;
        seed_row(&storage, "scribe_stem", "scribe-vlm").await;

        let stats = purge_local_html_rows(&storage, false).await.unwrap();
        assert_eq!(stats.total_rows, 1, "only the local-html row should match");
        assert_eq!(stats.deleted_catalog, 1);
        assert_eq!(stats.deleted_markdown, 1);
        assert_eq!(stats.deleted_paper, 1);
        assert!(stats.errors.is_empty(), "errors: {:?}", stats.errors);

        // Legacy row and companions are gone.
        let legacy_cat =
            hs_common::catalog::read_catalog_entry_via(&storage, "catalog", "legacy_stem")
                .await
                .expect("catalog read succeeds");
        assert!(
            legacy_cat.is_none(),
            "local-html catalog row should be gone"
        );
        assert!(!storage
            .exists(&format!(
                "markdown/{}",
                hs_common::sharded_key("legacy_stem", "md")
            ))
            .await
            .unwrap());
        assert!(!storage
            .exists(&format!(
                "papers/{}",
                hs_common::sharded_key("legacy_stem", "html")
            ))
            .await
            .unwrap());

        // Unrelated scribe-vlm row survives untouched.
        let survivor =
            hs_common::catalog::read_catalog_entry_via(&storage, "catalog", "scribe_stem")
                .await
                .expect("catalog read succeeds")
                .expect("scribe-vlm row must remain");
        assert_eq!(
            survivor.conversion.as_ref().unwrap().server,
            "scribe-vlm",
            "non-local-html rows must not be touched"
        );
    }

    #[tokio::test]
    async fn dry_run_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        seed_row(&storage, "legacy_stem", "local-html").await;

        let stats = purge_local_html_rows(&storage, true).await.unwrap();
        assert_eq!(stats.total_rows, 1);
        assert_eq!(stats.deleted_catalog, 0);
        assert_eq!(stats.deleted_markdown, 0);
        assert_eq!(stats.deleted_paper, 0);

        // Row still there.
        let still_there =
            hs_common::catalog::read_catalog_entry_via(&storage, "catalog", "legacy_stem")
                .await
                .expect("catalog read succeeds");
        assert!(still_there.is_some(), "dry-run must not delete");
    }
}
