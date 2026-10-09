//! Catalog operations on the personal store: list, read, delete, reindex.
//! Sidecars are flat YAML files at `{root}/{prefix}/{stem}.catalog.yaml`. We
//! walk the tree directly rather than maintain a separate index — the corpus
//! is small (personal records, not 200M papers), and a single source of truth
//! keeps the one-path discipline.

use crate::config::Config;
use crate::error::{PersonalError, Result};
use crate::services::distill::PersonalDistill;
use hs_common::catalog::CatalogEntry;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ListEntry {
    pub stem: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub original_format: Option<String>,
}

/// Documents in the store, most recently ingested first (the order the MCP
/// `personal_list` tool promises), at most `limit` of them. `category` is
/// checked against the store's taxonomy (see [`Config::resolve_category`]).
pub fn list_entries(cfg: &Config, category: Option<&str>, limit: usize) -> Result<Vec<ListEntry>> {
    let category = category.map(|c| cfg.resolve_category(c)).transpose()?;
    let mut found: Vec<(Option<String>, ListEntry)> = Vec::new();
    walk_sidecars(&cfg.root_dir(), &mut |path| {
        let stem = sidecar_stem(path);
        let entry = read_sidecar(path)?;
        if let Some(cat) = category {
            let matches = entry
                .category
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(cat.as_str()));
            if !matches {
                return Ok(());
            }
        }
        found.push((
            entry.downloaded_at,
            ListEntry {
                stem,
                title: entry.title,
                category: entry.category,
                original_format: entry.original_format,
            },
        ));
        Ok(())
    })?;
    // `downloaded_at` is the ingest time as RFC 3339 UTC, which orders as
    // text; a sidecar without one sorts last. Ties break by stem.
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.stem.cmp(&b.1.stem)));
    Ok(found
        .into_iter()
        .take(limit)
        .map(|(_, entry)| entry)
        .collect())
}

/// A stem names one document's files under the store: reject anything that
/// could name a path instead (`../..`, separators, NUL) before it reaches
/// `sharded_path`. The MCP `personal_read` / `personal_reindex` tools pass a
/// caller-supplied stem straight here.
fn check_stem(stem: &str) -> Result<()> {
    hs_common::validate_stem(stem)
        .map_err(|e| PersonalError::Other(anyhow::anyhow!("invalid stem {stem:?}: {e}")))
}

pub fn read_markdown(cfg: &Config, stem: &str) -> Result<String> {
    check_stem(stem)?;
    let path = hs_common::sharded_path(&cfg.markdown_dir(), stem, "md");
    if !path.exists() {
        return Err(PersonalError::Other(anyhow::anyhow!(
            "no markdown found for stem '{stem}'"
        )));
    }
    Ok(std::fs::read_to_string(&path)?)
}

pub async fn delete(cfg: &Config, stem: &str) -> Result<u64> {
    check_stem(stem)?;
    let distill = PersonalDistill::new(cfg)?;
    let removed = distill.delete(stem).await?;

    let candidates = [
        hs_common::sharded_path(&cfg.root_dir(), stem, "pdf"),
        hs_common::sharded_path(&cfg.root_dir(), stem, "epub"),
        hs_common::sharded_path(&cfg.root_dir(), stem, "docx"),
        hs_common::sharded_path(&cfg.root_dir(), stem, "md"),
        hs_common::sharded_path(&cfg.root_dir(), stem, "txt"),
        hs_common::sharded_path(&cfg.root_dir(), stem, "catalog.yaml"),
        hs_common::sharded_path(&cfg.markdown_dir(), stem, "md"),
    ];
    let mut removed_files = 0usize;
    for p in &candidates {
        if p.exists() {
            std::fs::remove_file(p)?;
            removed_files += 1;
        }
    }
    // A stem that names nothing is a typo to report, not a deletion to confirm.
    if removed == 0 && removed_files == 0 {
        return Err(PersonalError::Other(anyhow::anyhow!(
            "no personal document with stem '{stem}' (no vectors, no files)"
        )));
    }
    Ok(removed)
}

pub async fn reindex(cfg: &Config, stem: &str) -> Result<u32> {
    check_stem(stem)?;
    let entry_path = hs_common::sharded_path(&cfg.root_dir(), stem, "catalog.yaml");
    let entry = read_sidecar(&entry_path)?;
    let md = read_markdown(cfg, stem)?;
    let distill = PersonalDistill::new(cfg)?;
    // Indexing replaces a document in place (new chunks upserted, stale tail
    // removed after success), so nothing is deleted first: a failed index
    // call leaves the previous vectors untouched.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_document(server_url: &str) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            project_dir: dir.path().to_path_buf(),
            distill_url: server_url.to_string(),
            ..Config::default()
        };
        let sidecar = hs_common::sharded_path(&cfg.root_dir(), "record", "catalog.yaml");
        let md = hs_common::sharded_path(&cfg.markdown_dir(), "record", "md");
        for (path, body) in [
            (
                sidecar,
                serde_yaml_ng::to_string(&CatalogEntry::default()).unwrap(),
            ),
            (md, "# Record\n\nBody.".to_string()),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        (dir, cfg)
    }

    /// F6: reindex used to delete the document's vectors before indexing, so
    /// an embedder failure left a personal record with no vectors.
    #[tokio::test]
    async fn a_failing_index_call_never_costs_the_document_its_vectors() {
        let mut distill = mockito::Server::new_async().await;
        let delete = distill
            .mock("DELETE", mockito::Matcher::Any)
            .expect(0)
            .create_async()
            .await;
        let index = distill
            .mock("POST", "/distill")
            .with_status(500)
            .with_body("embedder down")
            .expect(1)
            .create_async()
            .await;
        let (_dir, cfg) = store_with_document(&distill.url());

        let err = reindex(&cfg, "record").await.unwrap_err();

        assert!(err.to_string().contains("embedder down"), "{err}");
        index.assert_async().await;
        delete.assert_async().await;
    }

    fn write_sidecar(cfg: &Config, stem: &str, category: &str, downloaded_at: Option<&str>) {
        let entry = CatalogEntry {
            title: Some(format!("Title of {stem}")),
            category: Some(category.to_string()),
            downloaded_at: downloaded_at.map(str::to_string),
            ..CatalogEntry::default()
        };
        let path = hs_common::sharded_path(&cfg.root_dir(), stem, "catalog.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_yaml_ng::to_string(&entry).unwrap()).unwrap();
    }

    #[test]
    fn list_is_most_recent_first_and_its_category_filter_ignores_case() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            project_dir: dir.path().to_path_buf(),
            ..Config::default()
        };
        write_sidecar(&cfg, "jan", "tax", Some("2026-01-05T10:00:00+00:00"));
        write_sidecar(&cfg, "mar", "medical", Some("2026-03-05T10:00:00+00:00"));
        write_sidecar(&cfg, "feb", "tax", Some("2026-02-05T10:00:00+00:00"));
        write_sidecar(&cfg, "undated", "tax", None);
        let stems = |entries: Vec<ListEntry>| -> Vec<String> {
            entries.into_iter().map(|e| e.stem).collect()
        };

        assert_eq!(
            stems(list_entries(&cfg, None, 10).unwrap()),
            ["mar", "feb", "jan", "undated"]
        );
        // `limit` keeps the most recent ones, not the alphabetically first.
        assert_eq!(stems(list_entries(&cfg, None, 1).unwrap()), ["mar"]);
        // The stored category is lowercase; `--category Tax` must still match.
        assert_eq!(
            stems(list_entries(&cfg, Some("Tax"), 10).unwrap()),
            ["feb", "jan", "undated"]
        );
        // A category outside the taxonomy matches nothing by construction.
        let err = list_entries(&cfg, Some("payroll"), 10).unwrap_err();
        assert!(matches!(err, PersonalError::UnknownCategory(_)), "{err:?}");
    }

    #[tokio::test]
    async fn deleting_a_stem_that_names_nothing_is_an_error() {
        let mut distill = mockito::Server::new_async().await;
        distill
            .mock("DELETE", mockito::Matcher::Any)
            .with_status(200)
            .with_body(r#"{"deleted":0}"#)
            .create_async()
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            project_dir: dir.path().to_path_buf(),
            distill_url: distill.url(),
            ..Config::default()
        };

        let err = delete(&cfg, "no-such-doc").await.unwrap_err();

        assert!(err.to_string().contains("no personal document"), "{err}");
    }
}

#[cfg(test)]
mod stem_boundary_tests {
    use super::*;

    /// `personal_read` takes a caller-supplied stem; `../../x` used to be
    /// spliced into `sharded_path` and read any `.md` file on the host.
    #[tokio::test]
    async fn a_stem_that_names_a_path_is_rejected_before_touching_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            project_dir: dir.path().to_path_buf(),
            ..Config::default()
        };
        let outside = dir.path().join("secret.md");
        std::fs::write(&outside, "secret").unwrap();

        for stem in ["../../secret", "..", "a/b", "", "a\\b"] {
            assert!(read_markdown(&cfg, stem).is_err(), "read {stem:?}");
            assert!(delete(&cfg, stem).await.is_err(), "delete {stem:?}");
            assert!(reindex(&cfg, stem).await.is_err(), "reindex {stem:?}");
        }
        assert!(outside.exists());
    }
}
