//! The single ingest path. Format → markdown → name/category → on-disk store
//! → distill index. There are no fallbacks: every step either succeeds or
//! aborts the operation. A failure mid-flight does NOT leave a half-written
//! sidecar, half-indexed Qdrant points, or a "conversion_failed: true" stamp
//! on disk — the caller sees a typed error and the personal store stays in
//! the state it was in before the call.

use crate::config::Config;
use crate::converters;
use crate::error::{PersonalError, Result};
use crate::models::{Category, SourceFormat};
use crate::services::{catalog, distill::PersonalDistill, naming};
use chrono::Utc;
use hs_common::catalog::{CatalogEntry, ConversionMeta};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct IngestOptions {
    pub category_override: Option<String>,
    pub title_override: Option<String>,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct IngestOutcome {
    pub stem: String,
    pub title: String,
    pub category: String,
    pub chunk_count: u32,
}

pub async fn ingest(cfg: &Config, file: &Path, opts: IngestOptions) -> Result<IngestOutcome> {
    let format = converters::detect_format(file)?;
    // Overrides are checked before the (possibly minutes-long) conversion: a
    // typo in `--category` must not cost a PDF conversion and an LLM call.
    // `personal.categories` is the taxonomy the user allows; a pinned
    // category must be inside it.
    let category_override = opts
        .category_override
        .as_deref()
        .map(|c| cfg.resolve_category(c))
        .transpose()?;
    let title_override = opts.title_override.as_deref().map(str::trim);
    if title_override == Some("") {
        return Err(PersonalError::Naming("the title override is empty".into()));
    }
    let bytes = std::fs::read(file)?;
    let sha = sha256_hex(&bytes);
    let size = bytes.len() as u64;

    // The same content under a new LLM-picked title gets a new stem, so the
    // stem check below cannot see it: compare content hashes with the stored
    // sidecars instead, before the (possibly minutes-long) conversion.
    let same_content = catalog::stems_with_sha256(cfg, &sha)?;
    if let (false, Some(existing)) = (opts.force, same_content.first()) {
        return Err(PersonalError::DuplicateContent(existing.clone()));
    }

    // Convert first so the LLM has the actual extracted text, not raw bytes.
    let stem_hint = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("personal");
    let markdown = converters::convert(cfg, format, bytes.clone(), stem_hint).await?;

    // Resolve title + category. Overrides bypass Ollama for that field
    // only — partial overrides are fine and explicitly supported so a user
    // who knows the category but not the title can still leverage the LLM.
    // The model's pick is validated against the taxonomy by `naming`.
    let (title, category) =
        resolve_name_and_category(cfg, &markdown, title_override, category_override).await?;

    let stem = build_stem(&title, &sha);
    let (dest_orig, dest_md, dest_cat) = store_paths(cfg, &stem, format);

    let already_stored = dest_orig.exists() || dest_md.exists() || dest_cat.exists();
    if !opts.force && already_stored {
        return Err(PersonalError::DuplicateStem(stem));
    }

    let job = Commit {
        cfg: cfg.clone(),
        stem: stem.clone(),
        format,
        title: title.clone(),
        category,
        size,
        sha,
        bytes,
        markdown,
        already_stored,
    };
    // The unit runs as its own task. A caller that gives up (an MCP session
    // closing, a client timeout) drops this future; a unit dropped between
    // "files written" and "index finished" would leave the half-stored
    // document its cleanup exists to prevent. As a task it always finishes:
    // stored and indexed, or cleaned up.
    let chunk_count = tokio::spawn(commit(job))
        .await
        .map_err(|e| PersonalError::Other(anyhow::anyhow!("ingest task failed: {e}")))??;

    // `--force`: the new document is stored and indexed first (a failed
    // ingest leaves the old one untouched), then every older document with
    // the same content is removed — files, markdown, sidecar and vectors.
    // The same stem was overwritten in place above and stays.
    for old in same_content.iter().filter(|old| **old != stem) {
        catalog::delete(cfg, old).await.map_err(|e| {
            PersonalError::Other(anyhow::anyhow!(
                "ingested as '{stem}' but could not remove the replaced document '{old}': {e}"
            ))
        })?;
    }

    Ok(IngestOutcome {
        stem,
        title,
        category: category.as_str().to_string(),
        chunk_count,
    })
}

/// `(original, markdown, sidecar)` paths of one stem in the store.
fn store_paths(cfg: &Config, stem: &str, format: SourceFormat) -> (PathBuf, PathBuf, PathBuf) {
    (
        hs_common::sharded_path(&cfg.root_dir(), stem, format.as_str()),
        hs_common::sharded_path(&cfg.markdown_dir(), stem, "md"),
        hs_common::sharded_path(&cfg.root_dir(), stem, "catalog.yaml"),
    )
}

/// Everything the store-and-index unit needs, owned so it can be a task.
struct Commit {
    cfg: Config,
    stem: String,
    format: SourceFormat,
    title: String,
    category: Category,
    size: u64,
    sha: String,
    bytes: Vec<u8>,
    markdown: String,
    /// The stem already had files before this call (`--force` over it).
    already_stored: bool,
}

/// Write the files and index the document, as one unit: if any step fails the
/// documented contract is "the store stays as it was", so the files this
/// call created are removed (a retry would otherwise hit `DuplicateStem`
/// on a document that was never indexed), and so are any vectors a
/// failed or timed-out index call left behind (indexing upserts batch by
/// batch). With `--force` over an existing document the files are
/// overwritten in place and kept: the previous vectors are still there
/// and `hs personal reindex` repairs the rest. Returns the chunk count.
async fn commit(job: Commit) -> Result<u32> {
    let Commit {
        cfg,
        stem,
        format,
        title,
        category,
        size,
        sha,
        bytes,
        markdown,
        already_stored,
    } = job;
    let (dest_orig, dest_md, dest_cat) = store_paths(&cfg, &stem, format);

    let mut index_attempted = false;
    let stored = async {
        write_atomic(&dest_orig, &bytes)?;
        write_atomic(&dest_md, markdown.as_bytes())?;

        let entry = build_catalog(&CatalogInput {
            stem: &stem,
            format,
            title: &title,
            category,
            size,
            sha: &sha,
            orig_path: &dest_orig,
            md_path: &dest_md,
        });
        let yaml = serde_yaml_ng::to_string(&entry)
            .map_err(|e| PersonalError::Other(anyhow::anyhow!("catalog yaml: {e}")))?;
        write_atomic(&dest_cat, yaml.as_bytes())?;

        let distill = PersonalDistill::new(&cfg)?;
        index_attempted = true;
        distill
            .index(&format!("{stem}.md"), &markdown, &entry)
            .await
    }
    .await;
    match stored {
        Ok(result) => Ok(result.chunks_indexed),
        Err(e) => {
            if !already_stored {
                if index_attempted {
                    let purged = match PersonalDistill::new(&cfg) {
                        Ok(distill) => distill.delete(&stem).await.map(|_| ()),
                        Err(rm) => Err(rm),
                    };
                    if let Err(rm) = purged {
                        tracing::warn!(
                            %stem,
                            error = %rm,
                            "could not remove vectors after failed ingest"
                        );
                    }
                }
                for path in [&dest_cat, &dest_md, &dest_orig] {
                    match std::fs::remove_file(path) {
                        Ok(()) => {}
                        Err(rm) if rm.kind() == std::io::ErrorKind::NotFound => {}
                        Err(rm) => tracing::warn!(
                            path = %path.display(),
                            error = %rm,
                            "could not remove file after failed ingest"
                        ),
                    }
                }
            }
            Err(e)
        }
    }
}

async fn resolve_name_and_category(
    cfg: &Config,
    markdown: &str,
    title: Option<&str>,
    category: Option<Category>,
) -> Result<(String, Category)> {
    match (title, category) {
        (Some(t), Some(c)) => Ok((t.to_string(), c)),
        (Some(t), None) => {
            let suggestion = naming::suggest(cfg, markdown).await?;
            Ok((t.to_string(), suggestion.category(cfg)?))
        }
        // The model's category is not looked at: a pinned one must not be
        // failed by a bad pick for the field it replaces.
        (None, Some(c)) => {
            let suggestion = naming::suggest(cfg, markdown).await?;
            Ok((suggestion.title()?, c))
        }
        (None, None) => {
            let r = naming::title_and_category(cfg, markdown).await?;
            Ok((r.title, r.category))
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Build a filesystem-safe stem from the LLM title plus a short hash suffix.
/// The hash suffix prevents collisions when two documents share a sanitized
/// title (e.g. two "lab results 2024" PDFs from different visits).
pub(crate) fn build_stem(title: &str, sha: &str) -> String {
    let mut buf = String::with_capacity(title.len());
    let mut last_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            buf.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !buf.is_empty() {
            buf.push('-');
            last_dash = true;
        }
    }
    while buf.ends_with('-') {
        buf.pop();
    }
    if buf.is_empty() {
        buf.push_str("doc");
    }
    if buf.len() > 80 {
        buf.truncate(80);
        while buf.ends_with('-') {
            buf.pop();
        }
    }
    let suffix: String = sha.chars().take(8).collect();
    format!("{buf}-{suffix}")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("part")
    ));
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        // A half-written or unrenamed temp file would sit in the store.
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(rm) if rm.kind() == std::io::ErrorKind::NotFound => {}
            Err(rm) => tracing::warn!(
                path = %tmp.display(),
                error = %rm,
                "could not remove temp file after failed write"
            ),
        }
        return Err(e.into());
    }
    Ok(())
}

struct CatalogInput<'a> {
    stem: &'a str,
    format: SourceFormat,
    title: &'a str,
    category: Category,
    size: u64,
    sha: &'a str,
    orig_path: &'a Path,
    md_path: &'a Path,
}

fn build_catalog(i: &CatalogInput<'_>) -> CatalogEntry {
    let now = Utc::now().to_rfc3339();
    CatalogEntry {
        title: Some(i.title.to_string()),
        pdf_path: Some(i.orig_path.display().to_string()),
        markdown_path: Some(i.md_path.display().to_string()),
        downloaded_at: Some(now.clone()),
        file_size_bytes: Some(i.size),
        sha256: Some(i.sha.to_string()),
        conversion: Some(ConversionMeta {
            server: format!("personal:{}", i.format.as_str()),
            duration_secs: 0.0,
            total_pages: 0,
            converted_at: now,
            pages: Vec::new(),
            converted_by: None,
            attempts_log: Vec::new(),
        }),
        category: Some(i.category.to_string()),
        original_format: Some(i.format.to_string()),
        source: Some(format!("personal:{}", i.stem)),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_stem_sanitizes_punctuation_and_appends_hash() {
        let s = build_stem("Lab Results: 2024 Q1", "abcdef0123456789");
        assert_eq!(s, "lab-results-2024-q1-abcdef01");
    }

    #[test]
    fn build_stem_collapses_whitespace_runs() {
        let s = build_stem("  too   many  spaces  ", "0011223344556677");
        assert_eq!(s, "too-many-spaces-00112233");
    }

    #[test]
    fn build_stem_truncates_overlong_titles() {
        let long = "a".repeat(200);
        let s = build_stem(&long, "deadbeef00000000");
        // 80 chars of title body + "-" + 8 hash chars = 89.
        assert_eq!(s.len(), 89);
        assert!(s.ends_with("-deadbeef"));
    }

    #[test]
    fn build_stem_falls_back_to_doc_when_title_is_punctuation_only() {
        let s = build_stem("!!!---???", "ffffeeeeddddcccc");
        assert_eq!(s, "doc-ffffeeee");
    }

    fn config_naming_at(ollama_url: &str) -> Config {
        Config {
            naming: crate::config::NamingConfig {
                ollama_url: ollama_url.to_string(),
                model: "test-model".to_string(),
                max_input_tokens: 64,
            },
            ..Config::default()
        }
    }

    async fn model_says(server: &mut mockito::Server, title: &str, category: &str) {
        let inner = serde_json::json!({ "title": title, "category": category }).to_string();
        server
            .mock("POST", "/api/generate")
            .with_status(200)
            .with_body(serde_json::json!({ "response": inner }).to_string())
            .create_async()
            .await;
    }

    #[tokio::test]
    async fn a_pinned_category_is_not_failed_by_the_models_bad_pick() {
        // `--category tax` exists so a user need not depend on the model's
        // category; the model answering "sports" used to fail the ingest anyway.
        let mut server = mockito::Server::new_async().await;
        model_says(&mut server, "Annual Return Summary", "sports").await;
        let cfg = config_naming_at(&server.url());

        let (title, category) = resolve_name_and_category(&cfg, "text", None, Some(Category::Tax))
            .await
            .unwrap();

        assert_eq!(title, "Annual Return Summary");
        assert_eq!(category, Category::Tax);
    }

    #[tokio::test]
    async fn a_pinned_title_is_not_failed_by_the_models_empty_title() {
        let mut server = mockito::Server::new_async().await;
        model_says(&mut server, "", "medical").await;
        let cfg = config_naming_at(&server.url());

        let (title, category) = resolve_name_and_category(&cfg, "text", Some("My Title"), None)
            .await
            .unwrap();

        assert_eq!((title.as_str(), category), ("My Title", Category::Medical));
    }

    #[tokio::test]
    async fn bad_overrides_are_refused_before_the_file_is_read_or_converted() {
        // The file does not exist: reaching the read would be an `Io` error.
        let cfg = Config::default();
        let missing = Path::new("/nonexistent/overrides.md");

        let bad_category = IngestOptions {
            category_override: Some("payroll".into()),
            ..IngestOptions::default()
        };
        let err = ingest(&cfg, missing, bad_category).await.unwrap_err();
        assert!(matches!(err, PersonalError::UnknownCategory(_)), "{err:?}");

        let empty_title = IngestOptions {
            title_override: Some("   ".into()),
            ..IngestOptions::default()
        };
        let err = ingest(&cfg, missing, empty_title).await.unwrap_err();
        assert!(matches!(err, PersonalError::Naming(_)), "{err:?}");
    }

    /// A stand-in distill server: every request is answered `500`, but only
    /// after `delay`, so a caller can walk away while the index call is out.
    async fn failing_distill(delay: std::time::Duration) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = vec![0u8; 64 * 1024];
                    let _ = socket.read(&mut request).await;
                    tokio::time::sleep(delay).await;
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\nboom",
                        )
                        .await;
                });
            }
        });
        format!("http://{addr}")
    }

    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    #[tokio::test]
    async fn a_caller_that_gives_up_mid_index_does_not_strand_a_half_stored_document() {
        // An MCP session closing drops the `personal_add` future. When the
        // write-and-index unit lived in that future, the files stayed on disk
        // (and any vectors in Qdrant) for a document that was never indexed.
        let distill_url = failing_distill(std::time::Duration::from_millis(400)).await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            project_dir: dir.path().to_path_buf(),
            distill_url,
            ..Config::default()
        };
        let note = dir.path().join("note.md");
        std::fs::write(&note, "# A note\n\nBody text.").unwrap();
        let stem = build_stem("My Note", &sha256_hex(&std::fs::read(&note).unwrap()));
        let (orig, md, sidecar) = store_paths(&cfg, &stem, SourceFormat::Markdown);
        let opts = IngestOptions {
            title_override: Some("My Note".into()),
            category_override: Some("other".into()),
            force: false,
        };

        let caller_cfg = cfg.clone();
        let caller = tokio::spawn(async move { ingest(&caller_cfg, &note, opts).await });
        // The unit has written its files and is waiting on the index call.
        wait_until("the files to be written", || sidecar.exists()).await;
        caller.abort();

        wait_until("the abandoned ingest to clean up after itself", || {
            !orig.exists() && !md.exists() && !sidecar.exists()
        })
        .await;
    }

    /// A store whose distill server accepts every index call.
    async fn store_with_distill(
        server: &mut mockito::Server,
    ) -> (tempfile::TempDir, Config, mockito::Mock) {
        let index = server
            .mock("POST", "/distill")
            .with_status(200)
            .with_body(r#"{"doc_id":"d","chunks_indexed":2,"embedding_device":"Cuda"}"#)
            .expect_at_least(1)
            .create_async()
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            project_dir: dir.path().to_path_buf(),
            distill_url: server.url(),
            ..Config::default()
        };
        (dir, cfg, index)
    }

    fn titled(title: &str, force: bool) -> IngestOptions {
        IngestOptions {
            title_override: Some(title.into()),
            category_override: Some("other".into()),
            force,
        }
    }

    #[tokio::test]
    async fn the_same_content_under_a_new_title_is_a_duplicate() {
        let mut server = mockito::Server::new_async().await;
        let (dir, cfg, _index) = store_with_distill(&mut server).await;
        let note = dir.path().join("note.md");
        std::fs::write(&note, "# A note\n\nBody text.").unwrap();

        let first = ingest(&cfg, &note, titled("First Title", false))
            .await
            .unwrap();
        let err = ingest(&cfg, &note, titled("Second Title", false))
            .await
            .unwrap_err();

        match err {
            PersonalError::DuplicateContent(existing) => assert_eq!(existing, first.stem),
            other => panic!("expected DuplicateContent, got {other:?}"),
        }
        let listed = catalog::list_entries(&cfg, None, 10).unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[tokio::test]
    async fn force_replaces_the_old_stem_with_the_new_ingest() {
        let mut server = mockito::Server::new_async().await;
        let (dir, cfg, _index) = store_with_distill(&mut server).await;
        let note = dir.path().join("note.md");
        std::fs::write(&note, "# A note\n\nBody text.").unwrap();

        let first = ingest(&cfg, &note, titled("First Title", false))
            .await
            .unwrap();
        let delete = server
            .mock(
                "DELETE",
                mockito::Matcher::Regex(format!("^/doc/{}", first.stem)),
            )
            .with_status(200)
            .with_body(r#"{"deleted":2}"#)
            .expect(1)
            .create_async()
            .await;
        let second = ingest(&cfg, &note, titled("Second Title", true))
            .await
            .unwrap();

        assert_ne!(first.stem, second.stem);
        delete.assert_async().await;
        let listed = catalog::list_entries(&cfg, None, 10).unwrap();
        assert_eq!(
            listed.iter().map(|e| e.stem.as_str()).collect::<Vec<_>>(),
            [second.stem.as_str()]
        );
        let (orig, md, sidecar) = store_paths(&cfg, &first.stem, SourceFormat::Markdown);
        assert!(!orig.exists() && !md.exists() && !sidecar.exists());
        let (orig, md, sidecar) = store_paths(&cfg, &second.stem, SourceFormat::Markdown);
        assert!(orig.exists() && md.exists() && sidecar.exists());
    }
}
