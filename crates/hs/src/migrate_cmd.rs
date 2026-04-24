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
        match hs_common::catalog::read_catalog_entry_via(storage, "catalog", &stem).await {
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
                .unwrap();
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
