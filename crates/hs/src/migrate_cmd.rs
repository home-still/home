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
        let entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let path = e.path();
                // Only migrate files directly in the root (not already in shard subdirs)
                !path.is_dir()
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
                continue; // skip unusually short filenames
            }

            let target = hs_common::sharded_path(dir, stem, ext);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&path, &target)?;
            moved += 1;
        }

        reporter.status("OK", &format!("{name}: migrated {moved} files"));
        total_moved += moved;
    }

    if total_moved > 0 {
        reporter.finish(&format!("Migrated {total_moved} files to sharded layout"));
    } else {
        reporter.finish("All directories already using sharded layout");
    }

    Ok(())
}
