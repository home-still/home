//! `hs distill purge --store <store> <doc_id>`: remove one document from a
//! store, from the search index and from storage.
//!
//! The document's records go first, then every object storage holds for it
//! under the store's layout: its source (under whichever extension it has),
//! its converted markdown, its catalog entry and its word boxes. Removing
//! the records alone would not hold: the nightly `hs distill reconcile
//! --reembed` indexes every markdown object that has no records, so the
//! markdown has to go with them.

use anyhow::{Context, Result};
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;
use hs_common::stores::{DocumentLayout, StoreKind, StoreSpec, Stores, ABSTRACTS, ACADEMIC};
use hs_distill::client::DistillClient;

/// The records the search index holds for a document.
pub(crate) trait RecordIndex {
    /// How many records `store` (and what is stored with it) holds for `stem`.
    async fn count(&self, store: &str, stem: &str) -> Result<u64>;

    /// Delete them; returns how many were deleted.
    async fn delete(&self, store: &str, stem: &str) -> Result<u64>;
}

impl RecordIndex for DistillClient {
    async fn count(&self, store: &str, stem: &str) -> Result<u64> {
        let (_, chunks) = self.doc_chunks(store, stem).await?;
        if store != ACADEMIC {
            return Ok(chunks);
        }
        // A paper's abstract is a record of its own, in another store.
        let (_, abstracts) = self.doc_chunks(ABSTRACTS, stem).await?;
        Ok(chunks + abstracts)
    }

    async fn delete(&self, store: &str, stem: &str) -> Result<u64> {
        if store == ACADEMIC {
            return crate::distill_cmd::delete_paper_everywhere(self, stem).await;
        }
        self.delete_doc(store, stem).await
    }
}

/// What a purge is about to remove.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Records in the search index.
    pub records: u64,
    /// The keys of the objects storage holds, in the order they go.
    pub objects: Vec<String>,
}

/// What a purge removed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Purged {
    /// Records deleted from the search index.
    pub records: u64,
    /// The keys of the objects deleted from storage.
    pub objects: Vec<String>,
}

/// The stores purge accepts: those with documents of their own in storage.
fn accepted(stores: &Stores) -> String {
    stores
        .iter()
        .filter(|spec| matches!(spec.kind, StoreKind::Documents(_)))
        .map(|spec| spec.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The store named `name` and where its documents live, or an error naming
/// the stores purge accepts.
pub(crate) fn resolve<'a>(
    stores: &'a Stores,
    name: &str,
) -> Result<(&'a StoreSpec, &'a DocumentLayout)> {
    let accepted = accepted(stores);
    let spec = stores
        .get(name)
        .map_err(|_| anyhow::anyhow!("no store named {name:?}; purge accepts: {accepted}"))?;
    match &spec.kind {
        StoreKind::Documents(layout) => Ok((spec, layout)),
        StoreKind::Derived { of, .. } => anyhow::bail!(
            "the store {name:?} has no documents of its own: its records are made from the \
             documents of {of:?}, and go with them. Purge the document in {of:?}. \
             Purge accepts: {accepted}"
        ),
        StoreKind::Memory => anyhow::bail!(
            "the store {name:?} has no documents in storage: only the memory service holds its \
             records, so there is nothing for purge to delete. Purge accepts: {accepted}"
        ),
    }
}

/// Every key storage may hold for `stem` under `layout`: the source under
/// each extension the layout allows, the markdown, the catalog entry and
/// the word boxes.
pub(crate) fn purge_keys(layout: &DocumentLayout, stem: &str) -> Result<Vec<String>> {
    let mut keys: Vec<String> = layout
        .source_extensions
        .iter()
        .map(|extension| layout.source_key(stem, extension))
        .collect();
    keys.push(layout.markdown_key(stem));
    keys.push(hs_common::catalog::catalog_key(
        &layout.catalog_prefix,
        stem,
    )?);
    keys.push(hs_common::word_boxes::boxes_key(layout, stem));
    Ok(keys)
}

/// Purge `stem` from the store `spec` describes.
///
/// Looks first: a document with no records and no objects is an error, not
/// a purge that did nothing. `confirm` sees what will go and decides; when it
/// says no, nothing is deleted and the result is `None`. Any failed delete is
/// an error naming what was not removed.
pub(crate) async fn run(
    storage: &dyn Storage,
    records: &impl RecordIndex,
    (spec, layout): (&StoreSpec, &DocumentLayout),
    stem: &str,
    confirm: impl FnOnce(&Plan) -> Result<bool>,
    reporter: &dyn Reporter,
) -> Result<Option<Purged>> {
    let keys = purge_keys(layout, stem)?;
    let record_count = records
        .count(spec.name, stem)
        .await
        .with_context(|| format!("counting the records of {stem} in {}", spec.name))?;
    let mut objects = Vec::new();
    for key in keys {
        let held = storage
            .head(&key)
            .await
            .with_context(|| format!("looking for {key}"))?;
        if held.is_some() {
            objects.push(key);
        }
    }
    if record_count == 0 && objects.is_empty() {
        anyhow::bail!(
            "{stem} is not in the {} store: it has no records and no object in storage",
            spec.name
        );
    }

    let plan = Plan {
        records: record_count,
        objects,
    };
    if !confirm(&plan)? {
        return Ok(None);
    }

    reporter.status("Purging", &format!("{stem} from {}", spec.name));
    let deleted = records
        .delete(spec.name, stem)
        .await
        .with_context(|| format!("deleting the records of {stem} from {}", spec.name))?;
    reporter.status("Deleted", &format!("{deleted} record(s)"));
    let mut removed = Vec::with_capacity(plan.objects.len());
    for key in plan.objects {
        storage
            .delete(&key)
            .await
            .with_context(|| format!("deleting {key}: its records are already deleted"))?;
        reporter.status("Deleted", &key);
        removed.push(key);
    }
    Ok(Some(Purged {
        records: deleted,
        objects: removed,
    }))
}

/// `hs distill purge`: build the components from the user's configuration
/// and run, asking for confirmation unless `yes`.
pub(crate) async fn cmd(
    store: &str,
    doc_id: &str,
    server: Option<&str>,
    yes: bool,
    reporter: &dyn Reporter,
) -> Result<()> {
    hs_common::validate_stem(doc_id).map_err(|e| anyhow::anyhow!("doc_id {doc_id:?}: {e}"))?;
    let ctx = super::MaintainCtx::load()?;
    let stores = ctx.stores()?;
    let target = resolve(&stores, store)?;
    let storage = ctx.storage().await?;
    let client = ctx.distill(server).await?;

    let confirm = |plan: &Plan| -> Result<bool> {
        reporter.status(
            "Found",
            &format!(
                "{} record(s) and {} object(s) for {doc_id} in {store}",
                plan.records,
                plan.objects.len()
            ),
        );
        for key in &plan.objects {
            reporter.status("Object", key);
        }
        if yes {
            return Ok(true);
        }
        dialoguer::Confirm::new()
            .with_prompt(format!(
                "Delete {doc_id} from {store}: {} record(s) and {} object(s)? This cannot be undone",
                plan.records,
                plan.objects.len()
            ))
            .default(false)
            .interact()
            .context("asking for confirmation (pass --yes to skip the question)")
    };

    match run(storage.as_ref(), &client, target, doc_id, confirm, reporter).await? {
        Some(purged) => reporter.finish(&format!(
            "Purged {doc_id} from {store}: {} record(s), {} object(s)",
            purged.records,
            purged.objects.len()
        )),
        None => reporter.finish("Aborted — nothing was deleted."),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintain::test_support::stores;
    use hs_common::reporter::SilentReporter;
    use hs_common::storage::LocalFsStorage;
    use hs_common::stores::{MEMORY, PERSONAL, SYNTHESES, WEB};
    use parking_lot::Mutex;

    /// A stem whose shard directory is `ab`.
    const STEM: &str = "abcdef0123";

    /// A search index that holds `records` for every document and counts the
    /// deletes it is asked for.
    struct FakeIndex {
        records: u64,
        deletes: Mutex<Vec<(String, String)>>,
    }

    impl FakeIndex {
        fn holding(records: u64) -> Self {
            Self {
                records,
                deletes: Mutex::new(Vec::new()),
            }
        }
    }

    impl RecordIndex for FakeIndex {
        async fn count(&self, _store: &str, _stem: &str) -> Result<u64> {
            Ok(self.records)
        }

        async fn delete(&self, store: &str, stem: &str) -> Result<u64> {
            self.deletes.lock().push((store.into(), stem.into()));
            Ok(self.records)
        }
    }

    fn keys(name: &str) -> Vec<String> {
        let stores = stores();
        let (_, layout) = resolve(&stores, name).unwrap();
        purge_keys(layout, STEM).unwrap()
    }

    #[test]
    fn a_nested_store_purges_its_source_markdown_catalog_and_boxes() {
        assert_eq!(
            keys(WEB),
            [
                "web/ab/abcdef0123.html",
                "web/ab/abcdef0123.md",
                "web/ab/abcdef0123.txt",
                "web/markdown/ab/abcdef0123.md",
                "web/catalog/ab/abcdef0123.yaml",
                "web/boxes/ab/abcdef0123.words.json.gz",
            ]
        );
        assert_eq!(
            keys(SYNTHESES),
            [
                "syntheses/ab/abcdef0123.md",
                "syntheses/markdown/ab/abcdef0123.md",
                "syntheses/catalog/ab/abcdef0123.yaml",
                "syntheses/boxes/ab/abcdef0123.words.json.gz",
            ]
        );
    }

    #[test]
    fn the_academic_store_purges_every_source_extension_and_the_derived_objects() {
        assert_eq!(
            keys(ACADEMIC),
            [
                "papers/ab/abcdef0123.pdf",
                "papers/ab/abcdef0123.html",
                "papers/ab/abcdef0123.epub",
                "markdown/ab/abcdef0123.md",
                "catalog/ab/abcdef0123.yaml",
                "boxes/ab/abcdef0123.words.json.gz",
            ]
        );
    }

    #[test]
    fn a_store_without_documents_is_refused_naming_the_stores_purge_accepts() {
        let stores = stores();
        let want = format!("accepts: {ACADEMIC}, {PERSONAL}, {WEB}, {SYNTHESES}");
        for name in [MEMORY, hs_common::stores::REFERENCES, ABSTRACTS, "made_up"] {
            let err = resolve(&stores, name).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains(&want), "{name}: {msg}");
        }
        let memory = format!("{:#}", resolve(&stores, MEMORY).unwrap_err());
        assert!(memory.contains("only the memory service"), "{memory}");
    }

    async fn put(storage: &LocalFsStorage, key: &str) {
        storage.put(key, b"x".to_vec()).await.unwrap();
    }

    /// The records go, then every object the document has, and nothing
    /// else: a sibling document's objects and the markdown of a document
    /// whose source shares the stem's shard directory stay.
    #[tokio::test]
    async fn a_purge_deletes_the_records_and_only_the_documents_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        for key in [
            "web/ab/abcdef0123.md",
            "web/markdown/ab/abcdef0123.md",
            "web/catalog/ab/abcdef0123.yaml",
            "web/ab/abother.md",
            "web/markdown/ab/abother.md",
        ] {
            put(&storage, key).await;
        }
        let index = FakeIndex::holding(3);
        let stores = stores();

        let purged = run(
            &storage,
            &index,
            resolve(&stores, WEB).unwrap(),
            STEM,
            |plan| {
                assert_eq!(plan.records, 3);
                Ok(true)
            },
            &SilentReporter,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(purged.records, 3);
        assert_eq!(
            purged.objects,
            [
                "web/ab/abcdef0123.md",
                "web/markdown/ab/abcdef0123.md",
                "web/catalog/ab/abcdef0123.yaml",
            ]
        );
        assert_eq!(*index.deletes.lock(), [(WEB.to_string(), STEM.to_string())]);
        for gone in &purged.objects {
            assert!(storage.head(gone).await.unwrap().is_none(), "{gone}");
        }
        for kept in ["web/ab/abother.md", "web/markdown/ab/abother.md"] {
            assert!(storage.head(kept).await.unwrap().is_some(), "{kept}");
        }
    }

    #[tokio::test]
    async fn declining_the_confirmation_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        put(&storage, "web/markdown/ab/abcdef0123.md").await;
        let index = FakeIndex::holding(1);
        let stores = stores();

        let purged = run(
            &storage,
            &index,
            resolve(&stores, WEB).unwrap(),
            STEM,
            |_| Ok(false),
            &SilentReporter,
        )
        .await
        .unwrap();

        assert_eq!(purged, None);
        assert!(index.deletes.lock().is_empty());
        assert!(storage
            .head("web/markdown/ab/abcdef0123.md")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn a_document_with_no_records_and_no_objects_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let index = FakeIndex::holding(0);
        let stores = stores();

        let err = run(
            &storage,
            &index,
            resolve(&stores, ACADEMIC).unwrap(),
            STEM,
            |_| panic!("nothing to confirm"),
            &SilentReporter,
        )
        .await
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains(STEM) && msg.contains("academic"), "{msg}");
        assert!(index.deletes.lock().is_empty());
    }

    /// Markdown left in storage is indexed again by reconcile, so a purge of
    /// a document that has objects and no records still removes them.
    #[tokio::test]
    async fn objects_without_records_are_purged() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        put(&storage, "markdown/ab/abcdef0123.md").await;
        put(&storage, "papers/ab/abcdef0123.pdf").await;
        let index = FakeIndex::holding(0);
        let stores = stores();

        let purged = run(
            &storage,
            &index,
            resolve(&stores, ACADEMIC).unwrap(),
            STEM,
            |_| Ok(true),
            &SilentReporter,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            purged.objects,
            ["papers/ab/abcdef0123.pdf", "markdown/ab/abcdef0123.md"]
        );
    }
}
