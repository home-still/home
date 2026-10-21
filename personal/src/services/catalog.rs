//! Catalog operations on the personal store: list, read, delete, reindex.
//!
//! The catalog is read and written through the corpus's `Storage` — the
//! same one the academic corpus uses, from the same `storage:` section
//! of the config file. One implementation, so a personal document that
//! is in the corpus is in the corpus for every host, not only for the
//! one whose disk happens to hold a copy.
//!
//! The sidecars are YAML objects at `{prefix}/{shard}/{stem}.catalog.yaml`
//! and the converted text at `{prefix}/markdown/{shard}/{stem}.md`. The
//! catalog is walked rather than indexed separately — the store is small
//! (personal records, not 200M papers), and a single source of truth
//! keeps the one-path discipline.

use crate::config::Config;
use crate::error::{PersonalError, Result};
use crate::services::distill::PersonalDistill;
use hs_common::catalog::CatalogEntry;
use hs_common::storage::Storage;

#[derive(Debug, Clone)]
pub struct ListEntry {
    pub stem: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub original_format: Option<String>,
}

/// A storage rooted at `dir`.
///
/// The store's objects and the code that reads them must agree on where
/// they live: a store whose objects are written under one root and
/// read from another is a store that cannot find its own documents.
#[cfg(test)]
pub(crate) fn storage_at(dir: &std::path::Path) -> hs_common::storage::StorageConfig {
    let mut storage = hs_common::storage::StorageConfig::default();
    storage.local.root = dir.to_path_buf();
    storage
}

/// The key of a document's catalog sidecar.
fn sidecar_key(prefix: &str, stem: &str) -> String {
    format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        hs_common::sharded_key(stem, "catalog.yaml")
    )
}

/// The prefix the store's converted text lives under.
///
/// The corpus keeps the academic markdown in `markdown/` and the
/// personal markdown in `personal/markdown/` — the same sharded layout,
/// one prefix deeper, which is what the migration produced.
fn markdown_prefix(prefix: &str) -> String {
    format!("{}/markdown", prefix.trim_end_matches('/'))
}

/// Documents in the store, most recently ingested first (the order the MCP
/// `personal_list` tool promises), at most `limit` of them. `category` is
/// checked against the store's taxonomy (see [`Config::resolve_category`]).
pub async fn list_entries(
    cfg: &Config,
    category: Option<&str>,
    limit: usize,
) -> Result<Vec<ListEntry>> {
    let category = category.map(|c| cfg.resolve_category(c)).transpose()?;
    let storage = cfg.build_storage().map_err(storage_error)?;
    let mut found: Vec<(Option<String>, ListEntry)> = Vec::new();
    for key in sidecar_keys(&*storage, &cfg.storage_dir).await? {
        let stem = sidecar_stem(&key);
        let entry = read_sidecar(&*storage, &key).await?;
        if let Some(cat) = category {
            let matches = entry
                .category
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(cat.as_str()));
            if !matches {
                continue;
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
    }
    // `downloaded_at` is the ingest time as RFC 3339 UTC, which orders as
    // text; a sidecar without one sorts last. Ties break by stem.
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.stem.cmp(&b.1.stem)));
    Ok(found
        .into_iter()
        .take(limit)
        .map(|(_, entry)| entry)
        .collect())
}

/// Every catalog sidecar's key, under `prefix`.
async fn sidecar_keys(storage: &dyn Storage, prefix: &str) -> Result<Vec<String>> {
    let root = prefix.trim_end_matches('/');
    let mut keys: Vec<String> = storage
        .list(&format!("{root}/"))
        .await
        .map_err(storage_error)?
        .into_iter()
        .map(|o| o.key)
        .filter(|k| k.ends_with(".catalog.yaml"))
        .collect();
    keys.sort();
    Ok(keys)
}

/// Stems (sorted) of every stored document whose source file has this
/// SHA-256 (lowercase hex), read from the sidecars. Empty when none does.
pub async fn stems_with_sha256(cfg: &Config, sha256: &str) -> Result<Vec<String>> {
    let storage = cfg.build_storage().map_err(storage_error)?;
    let mut stems = Vec::new();
    for key in sidecar_keys(&*storage, &cfg.storage_dir).await? {
        if read_sidecar(&*storage, &key).await?.sha256.as_deref() == Some(sha256) {
            stems.push(sidecar_stem(&key));
        }
    }
    stems.sort();
    Ok(stems)
}

/// A stem names one document's files under the store: reject anything that
/// could name a path instead (`../..`, separators, NUL) before it reaches
/// `sharded_path`. The MCP `personal_read` / `personal_reindex` tools pass a
/// caller-supplied stem straight here.
fn check_stem(stem: &str) -> Result<()> {
    hs_common::validate_stem(stem)
        .map_err(|e| PersonalError::Other(anyhow::anyhow!("invalid stem {stem:?}: {e}")))
}

/// A document's converted text, read from the store.
pub async fn read_markdown(cfg: &Config, stem: &str) -> Result<String> {
    check_stem(stem)?;
    let storage = cfg.build_storage().map_err(storage_error)?;
    let text =
        hs_common::markdown::read_markdown_via(&*storage, &markdown_prefix(&cfg.storage_dir), stem)
            .await
            .map_err(storage_error)?;
    text.ok_or_else(|| PersonalError::Other(anyhow::anyhow!("no markdown found for stem '{stem}'")))
}

pub async fn delete(cfg: &Config, stem: &str) -> Result<u64> {
    check_stem(stem)?;
    let distill = PersonalDistill::new(cfg)?;
    let removed = distill.delete(stem).await?;

    let storage = cfg.build_storage().map_err(storage_error)?;
    let prefix = cfg.storage_dir.trim_end_matches('/');
    let candidates = [
        format!("{prefix}/{}", hs_common::sharded_key(stem, "pdf")),
        format!("{prefix}/{}", hs_common::sharded_key(stem, "epub")),
        format!("{prefix}/{}", hs_common::sharded_key(stem, "docx")),
        format!("{prefix}/{}", hs_common::sharded_key(stem, "md")),
        format!("{prefix}/{}", hs_common::sharded_key(stem, "txt")),
        sidecar_key(prefix, stem),
        format!(
            "{}/{}",
            markdown_prefix(prefix),
            hs_common::sharded_key(stem, "md")
        ),
    ];
    let mut removed_files = 0usize;
    for key in candidates {
        if storage.head(&key).await.map_err(storage_error)?.is_some() {
            storage.delete(&key).await.map_err(storage_error)?;
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
    let storage = cfg.build_storage().map_err(storage_error)?;
    let entry = read_sidecar(&*storage, &sidecar_key(&cfg.storage_dir, stem)).await?;
    let md = read_markdown(cfg, stem).await?;
    let distill = PersonalDistill::new(cfg)?;
    // Indexing replaces a document in place (new chunks upserted, stale tail
    // removed after success), so nothing is deleted first: a failed index
    // call leaves the previous vectors untouched.
    let result = distill.index(&format!("{stem}.md"), &md, &entry).await?;
    Ok(result.chunks_indexed)
}

/// A document's catalog sidecar, read from the store.
async fn read_sidecar(storage: &dyn Storage, key: &str) -> Result<CatalogEntry> {
    let raw = storage.get(key).await.map_err(storage_error)?;
    serde_yaml_ng::from_str(&String::from_utf8_lossy(&raw))
        .map_err(|e| PersonalError::Other(anyhow::anyhow!("sidecar {key} is malformed: {e}")))
}

/// A storage failure, named.
fn storage_error(e: anyhow::Error) -> PersonalError {
    PersonalError::Other(anyhow::anyhow!(
        "the personal store could not be read: {e:#}"
    ))
}

/// The stem a sidecar's key names.
///
/// The key looks like `{prefix}/{shard}/{stem}.catalog.yaml`. Strip both
/// ".yaml" and ".catalog" so the stem is what `build_stem` produced.
fn sidecar_stem(key: &str) -> String {
    let name = key.rsplit('/').next().unwrap_or(key);
    name.strip_suffix(".catalog.yaml")
        .unwrap_or(name)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store_with_document(server_url: &str) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            project_dir: dir.path().to_path_buf(),
            distill_url: server_url.to_string(),
            storage: storage_at(dir.path()),
            ..Config::default()
        };
        // The store's objects live in the storage, at the keys the
        // migration produced: `personal/<shard>/<stem>.catalog.yaml` and
        // `personal/markdown/<shard>/<stem>.md`.
        let storage = cfg.build_storage().unwrap();
        storage
            .put(
                &sidecar_key(&cfg.storage_dir, "record"),
                serde_yaml_ng::to_string(&CatalogEntry::default())
                    .unwrap()
                    .into_bytes(),
            )
            .await
            .unwrap();
        storage
            .put(
                &format!(
                    "{}/{}",
                    markdown_prefix(&cfg.storage_dir),
                    hs_common::sharded_key("record", "md")
                ),
                "# Record\n\nBody.".to_string().into_bytes(),
            )
            .await
            .unwrap();
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
        let (_dir, cfg) = store_with_document(&distill.url()).await;

        let err = reindex(&cfg, "record").await.unwrap_err();

        assert!(err.to_string().contains("embedder down"), "{err}");
        index.assert_async().await;
        delete.assert_async().await;
    }

    /// Write a document's sidecar into the store, where the readers
    /// look for it.
    async fn write_sidecar(cfg: &Config, stem: &str, category: &str, downloaded_at: Option<&str>) {
        let entry = CatalogEntry {
            title: Some(format!("Title of {stem}")),
            category: Some(category.to_string()),
            downloaded_at: downloaded_at.map(str::to_string),
            ..CatalogEntry::default()
        };
        let storage = cfg.build_storage().unwrap();
        storage
            .put(
                &sidecar_key(&cfg.storage_dir, stem),
                serde_yaml_ng::to_string(&entry).unwrap().into_bytes(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn list_is_most_recent_first_and_its_category_filter_ignores_case() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            project_dir: dir.path().to_path_buf(),
            storage: storage_at(dir.path()),
            ..Config::default()
        };
        write_sidecar(&cfg, "jan", "tax", Some("2026-01-05T10:00:00+00:00")).await;
        write_sidecar(&cfg, "mar", "medical", Some("2026-03-05T10:00:00+00:00")).await;
        write_sidecar(&cfg, "feb", "tax", Some("2026-02-05T10:00:00+00:00")).await;
        write_sidecar(&cfg, "undated", "tax", None).await;
        let stems = |entries: Vec<ListEntry>| -> Vec<String> {
            entries.into_iter().map(|e| e.stem).collect()
        };

        assert_eq!(
            stems(list_entries(&cfg, None, 10).await.unwrap()),
            ["mar", "feb", "jan", "undated"]
        );
        // `limit` keeps the most recent ones, not the alphabetically first.
        assert_eq!(stems(list_entries(&cfg, None, 1).await.unwrap()), ["mar"]);
        // The stored category is lowercase; `--category Tax` must still match.
        assert_eq!(
            stems(list_entries(&cfg, Some("Tax"), 10).await.unwrap()),
            ["feb", "jan", "undated"]
        );
        // A category outside the taxonomy matches nothing by construction.
        let err = list_entries(&cfg, Some("payroll"), 10).await.unwrap_err();
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
            storage: storage_at(dir.path()),
            ..Config::default()
        };
        let outside = dir.path().join("secret.md");
        std::fs::write(&outside, "secret").unwrap();

        for stem in ["../../secret", "..", "a/b", "", "a\\b"] {
            assert!(read_markdown(&cfg, stem).await.is_err(), "read {stem:?}");
            assert!(delete(&cfg, stem).await.is_err(), "delete {stem:?}");
            assert!(reindex(&cfg, stem).await.is_err(), "reindex {stem:?}");
        }
        assert!(outside.exists());
    }
}
