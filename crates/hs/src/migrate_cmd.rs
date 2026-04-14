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
