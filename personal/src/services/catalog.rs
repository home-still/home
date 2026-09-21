//! Catalog operations on the personal store: list, read, delete, reindex.
//! Sidecars are flat YAML files at `{root}/{prefix}/{stem}.catalog.yaml`. We
//! walk the tree directly rather than maintain a separate index — the corpus
//! is small (personal records, not 200M papers), and a single source of truth
//! keeps the one-path discipline.

use crate::config::Config;
use crate::error::{PersonalError, Result};
use crate::services::distill::PersonalDistill;
use hs_common::catalog::CatalogEntry;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ListEntry {
    pub stem: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub original_format: Option<String>,
}

pub fn list_entries(cfg: &Config, category: Option<&str>, limit: usize) -> Result<Vec<ListEntry>> {
    let mut out = Vec::new();
    walk_sidecars(&cfg.root_dir(), &mut |path| {
        let stem = sidecar_stem(path);
        let entry = read_sidecar(path)?;
        if let Some(cat) = category {
            if entry.category.as_deref() != Some(cat) {
                return Ok(());
            }
        }
        out.push(ListEntry {
            stem,
            title: entry.title,
            category: entry.category,
            original_format: entry.original_format,
        });
        Ok(())
    })?;
    out.sort_by(|a, b| a.stem.cmp(&b.stem));
    out.truncate(limit);
    Ok(out)
}

pub fn read_markdown(cfg: &Config, stem: &str) -> Result<String> {
    let path = sharded(cfg.markdown_dir(), stem, "md");
    if !path.exists() {
        return Err(PersonalError::Other(anyhow::anyhow!(
            "no markdown found for stem '{stem}'"
        )));
    }
    Ok(std::fs::read_to_string(&path)?)
}

pub async fn delete(cfg: &Config, stem: &str) -> Result<u64> {
    let distill = PersonalDistill::new(cfg)?;
    let removed = distill.delete(stem).await?;

    let candidates = [
        sharded(cfg.root_dir(), stem, "pdf"),
        sharded(cfg.root_dir(), stem, "epub"),
        sharded(cfg.root_dir(), stem, "docx"),
        sharded(cfg.root_dir(), stem, "md"),
        sharded(cfg.root_dir(), stem, "txt"),
        sharded(cfg.root_dir(), stem, "catalog.yaml"),
        sharded(cfg.markdown_dir(), stem, "md"),
    ];
    for p in &candidates {
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }
    Ok(removed)
}

pub async fn reindex(cfg: &Config, stem: &str) -> Result<u32> {
    let entry_path = sharded(cfg.root_dir(), stem, "catalog.yaml");
    let entry = read_sidecar(&entry_path)?;
    let md = read_markdown(cfg, stem)?;
    let distill = PersonalDistill::new(cfg)?;
    distill.delete(stem).await?;
    let result = distill.index(&format!("{stem}.md"), &md, &entry).await?;
    Ok(result.chunks_indexed)
}

fn read_sidecar(path: &Path) -> Result<CatalogEntry> {
    let raw = std::fs::read_to_string(path)?;
    serde_yaml_ng::from_str(&raw).map_err(|e| {
        PersonalError::Other(anyhow::anyhow!(
            "sidecar {} is malformed: {e}",
            path.display()
        ))
    })
}

fn sidecar_stem(path: &Path) -> String {
    // path looks like `{root}/{prefix}/{stem}.catalog.yaml`. Strip both
    // ".yaml" and ".catalog" so the stem is what `build_stem` produced.
    let s = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    s.strip_suffix(".catalog.yaml").unwrap_or(s).to_string()
}

fn walk_sidecars(root: &Path, f: &mut dyn FnMut(&Path) -> Result<()>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Skip the markdown subtree — sidecars live next to originals,
            // not under markdown/.
            if path.file_name().and_then(|s| s.to_str()) == Some("markdown") {
                continue;
            }
            walk_sidecars(&path, f)?;
        } else if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with(".catalog.yaml"))
        {
            f(&path)?;
        }
    }
    Ok(())
}

fn sharded(dir: PathBuf, stem: &str, ext: &str) -> PathBuf {
    let prefix = &stem[..stem.len().min(2)];
    dir.join(prefix).join(format!("{stem}.{ext}"))
}
