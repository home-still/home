//! Move the personal store behind the corpus's `Storage`.
//!
//! The personal store writes through raw `std::fs` and lives only in the
//! host's local directory. That means three things, all measured on
//! `big` on 2026-10-21:
//!
//! * the docs service cannot serve a personal document, so an agent's
//!   `personal_search` hit comes back with **no link** — the user
//!   cannot open what the agent found;
//! * the store is not in the bucket, so it is on **one** host
//!   (`two`, `bmb` and `mac_air` have nothing) and not in the backup;
//! * the store is invisible to every other read path the corpus has.
//!
//! The fix is not a second copy of the corpus and not a second access
//! scheme: the objects move to the `personal/` prefix of the *same*
//! store the corpus uses, and the docs service serves them with the
//! *same* signed links.
//!
//! **Dry-run by default.** A migration writes to the store, so it is a
//! CLI command, it previews what it would do, and it only writes when
//! the operator asks for it.

use std::sync::Arc;

use anyhow::Result;
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;

/// The prefix the personal store's objects live under.
const PERSONAL_PREFIX: &str = "personal";

/// Move the personal store into the corpus's storage.
///
/// The objects keep their names: a personal document's markdown is
/// `<shard>/<stem>.md` under `personal/markdown/`, and its source is
/// `<shard>/<stem>.pdf` under `personal/`. The layout is the corpus's
/// layout, one prefix deeper — which is why the docs service can serve
/// it with the same code.
///
/// Nothing is deleted: the local files stay where they are until the
/// operator removes them, so a failed run leaves the store exactly as it
/// was.
pub async fn run_personal_to_storage(
    reporter: &Arc<dyn Reporter>,
    dry_run: bool,
    limit: Option<usize>,
) -> Result<()> {
    let personal_cfg = personal::config::Config::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage_cfg = hs_common::config_file::ConfigFile::load()?
        .section::<hs_common::storage::StorageConfig>("storage")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "this migration needs a `storage:` section in ~/.home-still/config.yaml \
                 (backend: local|s3) — the personal store is being moved into it"
            )
        })?;
    let storage: Arc<dyn Storage> = storage_cfg
        .build()
        .map_err(|e| anyhow::anyhow!("storage config invalid: {e:#}"))?;
    storage
        .ensure_ready()
        .await
        .map_err(|e| anyhow::anyhow!("storage ensure_ready failed: {e:#}"))?;

    // The files that have to move: the sources and the sidecars under
    // the store's root, and the converted text under its `markdown/`.
    let root = personal_cfg.root_dir();
    let markdown = personal_cfg.markdown_dir();
    let mut sources = Vec::new();
    collect_files(
        &root,
        &root,
        &mut sources,
        &["pdf", "html", "epub", "docx", "txt", "catalog.yaml"],
    )?;
    let mut texts = Vec::new();
    collect_files(&markdown, &markdown, &mut texts, &["md"])?;

    let mut plan: Vec<(String, String)> = Vec::new();
    for (base, files) in [
        (PERSONAL_PREFIX.to_string(), &sources),
        (format!("{PERSONAL_PREFIX}/markdown"), &texts),
    ] {
        for file in files {
            let key = format!("{base}/{}", file.relative);
            plan.push((key, file.path.display().to_string()));
        }
    }
    plan.sort();

    let total = plan.len();
    if total == 0 {
        reporter.finish("The personal store is empty: nothing to move");
        return Ok(());
    }
    if let Some(lim) = limit {
        plan.truncate(lim);
    }
    let planned = plan.len();

    reporter.status(
        "Plan",
        &format!(
            "{total} personal file(s) found; {planned} planned this run ({})",
            if dry_run { "dry-run" } else { "live" }
        ),
    );
    for (key, _) in plan.iter().take(5) {
        reporter.status("Sample", key);
    }

    if dry_run {
        reporter.finish(&format!(
            "Dry-run: {planned} file(s) would be written under {PERSONAL_PREFIX}/"
        ));
        return Ok(());
    }

    let stop = crate::shutdown::cooperative();
    let mut moved = 0u64;
    let mut skipped = 0u64;
    let mut failures: Vec<String> = Vec::new();

    for (key, path) in &plan {
        if stop.requested() {
            break;
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                failures.push(format!("{path}: {e}"));
                continue;
            }
        };
        // An object that is already there with the same size is left
        // alone: the run is idempotent, so a re-run after a partial
        // failure does not rewrite everything.
        if let Ok(Some(meta)) = storage.head(key).await {
            if meta.size == bytes.len() as u64 {
                skipped += 1;
                continue;
            }
        }
        match storage.put(key, bytes).await {
            Ok(()) => moved += 1,
            Err(e) => failures.push(format!("{key}: {e}")),
        }
    }

    reporter.status("Moved", &moved.to_string());
    reporter.status("Skipped", &skipped.to_string());
    if failures.is_empty() {
        reporter.finish(&format!(
            "The personal store is now under {PERSONAL_PREFIX}/ in the corpus's storage. \
             The local files were left in place."
        ));
        Ok(())
    } else {
        for f in failures.iter().take(20) {
            reporter.error(f);
        }
        anyhow::bail!(
            "{} file(s) could not be moved ({} shown)",
            failures.len(),
            failures.len().min(20)
        )
    }
}

/// One file to move: its path on disk and its path relative to the
/// directory it is copied from.
struct FileToMove {
    path: std::path::PathBuf,
    relative: String,
}

/// Collect the files under `dir` that carry one of `extensions`.
///
/// The `catalog.yaml` sidecar is named with a dot, so the extensions
/// are matched as a suffix rather than as an extension.
fn collect_files(
    dir: &std::path::Path,
    base: &std::path::Path,
    out: &mut Vec<FileToMove>,
    extensions: &[&str],
) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, base, out, extensions)?;
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !extensions.iter().any(|e| name.ends_with(e)) {
            continue;
        }
        let relative = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(FileToMove { path, relative });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_files_keep_their_names_under_the_new_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let shard = dir.path().join("ab");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join("paper.pdf"), b"pdf").unwrap();
        std::fs::write(shard.join("paper.catalog.yaml"), b"yaml").unwrap();
        std::fs::write(shard.join("notes.txt"), b"txt").unwrap();
        std::fs::write(shard.join("ignored.bin"), b"bin").unwrap();

        let mut out = Vec::new();
        collect_files(dir.path(), dir.path(), &mut out, &["pdf", "catalog.yaml"]).unwrap();
        let mut names: Vec<String> = out.iter().map(|f| f.relative.clone()).collect();
        names.sort();
        assert_eq!(names, ["ab/paper.catalog.yaml", "ab/paper.pdf"]);
    }
}
