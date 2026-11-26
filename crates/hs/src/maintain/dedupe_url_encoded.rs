//! `hs migrate dedupe-url-encoded`: documents stored under a URL-encoded stem
//! and again under the decoded one.
//!
//! The command reports the pairs; it changes nothing. Of a pair, the decoded
//! stem is the one that is indexed.

use std::collections::HashSet;

use anyhow::{Context, Result};
use hs_common::storage::Storage;
use hs_common::stores::DocumentLayout;
use serde::Serialize;

/// Pairs shown as examples in the report.
const SAMPLE_LIMIT: usize = 10;

/// One document stored under both spellings of its stem.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct Pair {
    pub encoded: String,
    pub decoded: String,
}

/// What a run found.
#[derive(Debug, Serialize)]
pub(crate) struct DedupeReport {
    pub pairs_found: usize,
    pub would_delete_encoded_rows: usize,
    pub samples: Vec<Pair>,
}

/// The stem of a markdown object, or `None` for an object that is not a
/// markdown document (a resource-fork file, another extension).
fn markdown_stem(key: &str) -> Option<String> {
    if !key.ends_with(".md") {
        return None;
    }
    let filename = key.rsplit('/').next()?;
    if filename.starts_with("._") {
        return None;
    }
    Some(filename.strip_suffix(".md").unwrap_or(filename).to_string())
}

/// Find the markdown stems of the store `layout` describes that contain a
/// percent-encoded byte and whose decoded form is also a stem.
pub(crate) async fn run(storage: &dyn Storage, layout: &DocumentLayout) -> Result<DedupeReport> {
    let markdown = storage
        .list(&layout.markdown_prefix)
        .await
        .with_context(|| format!("listing {}", layout.markdown_prefix))?;
    let stems: HashSet<String> = markdown
        .iter()
        .filter_map(|object| markdown_stem(&object.key))
        .collect();

    let mut pairs: Vec<Pair> = stems
        .iter()
        .filter(|stem| stem.contains('%'))
        .filter_map(|stem| {
            let decoded = percent_encoding::percent_decode_str(stem)
                .decode_utf8_lossy()
                .into_owned();
            (decoded != *stem && stems.contains(&decoded)).then(|| Pair {
                encoded: stem.clone(),
                decoded,
            })
        })
        .collect();
    pairs.sort_by(|a, b| a.encoded.cmp(&b.encoded));

    Ok(DedupeReport {
        pairs_found: pairs.len(),
        would_delete_encoded_rows: pairs.len(),
        samples: pairs.into_iter().take(SAMPLE_LIMIT).collect(),
    })
}

/// `hs migrate dedupe-url-encoded`.
pub(crate) async fn cmd() -> Result<()> {
    let ctx = super::MaintainCtx::load()?;
    let storage = ctx.storage().await?;
    let stores = ctx.stores()?;
    let (_, layout) = stores
        .documents(hs_common::stores::ACADEMIC)
        .context("finding the academic store's documents")?;

    let report = run(storage.as_ref(), layout).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("writing the report")?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintain::test_support::stores;
    use hs_common::storage::LocalFsStorage;
    use hs_common::stores::ACADEMIC;

    async fn put_markdown(storage: &LocalFsStorage, layout: &DocumentLayout, stem: &str) {
        storage
            .put(&layout.markdown_key(stem), b"# Body".to_vec())
            .await
            .unwrap();
    }

    /// An encoded stem is reported only when its decoded twin is also
    /// stored: a lone encoded stem, a decoded stem with no encoded twin and
    /// a resource-fork file are not pairs.
    #[tokio::test]
    async fn only_an_encoded_stem_with_a_decoded_twin_is_a_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        put_markdown(&storage, layout, "caf%C3%A9").await;
        put_markdown(&storage, layout, "café").await;
        put_markdown(&storage, layout, "lone%20stem").await;
        put_markdown(&storage, layout, "plain").await;
        put_markdown(&storage, layout, "._ghost%20twin").await;
        put_markdown(&storage, layout, "ghost twin").await;

        let report = run(&storage, layout).await.unwrap();

        assert_eq!(report.pairs_found, 1);
        assert_eq!(report.would_delete_encoded_rows, 1);
        assert_eq!(
            report.samples,
            [Pair {
                encoded: "caf%C3%A9".into(),
                decoded: "café".into()
            }]
        );
    }

    /// The samples are a prefix of the sorted pairs; the count covers all.
    #[tokio::test]
    async fn the_samples_are_capped_and_the_count_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let stores = stores();
        let (_, layout) = stores.documents(ACADEMIC).unwrap();
        let total = SAMPLE_LIMIT + 3;
        for n in 0..total {
            put_markdown(&storage, layout, &format!("doc{n:02}%20x")).await;
            put_markdown(&storage, layout, &format!("doc{n:02} x")).await;
        }

        let report = run(&storage, layout).await.unwrap();

        assert_eq!(report.pairs_found, total);
        assert_eq!(report.samples.len(), SAMPLE_LIMIT);
        assert_eq!(report.samples[0].encoded, "doc00%20x");
    }
}
