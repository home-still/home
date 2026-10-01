//! Behavior of the tools against fake storage and a loopback distill server.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;

use crate::testkit::{seed_catalog, seed_markdown, server, FakeDistill, FaultyStorage};
use crate::{DistillReconcileParams, DistillReindexParams};

mod reindex {
    use super::*;

    fn reindex(stem: &str) -> Parameters<DistillReindexParams> {
        Parameters(DistillReindexParams {
            stem: crate::stem::Stem::parse(stem).unwrap(),
        })
    }

    /// RA-9: the old tool deleted the document's vectors before it had read
    /// the catalog or verified the markdown, so a storage blip left the
    /// document with no vectors and nothing re-indexed.
    #[tokio::test]
    async fn a_storage_outage_is_an_error_and_no_vector_is_deleted() {
        let storage = FaultyStorage::new();
        seed_markdown(
            &*storage,
            "paper-a",
            "# A\n\nBody text long enough to index.",
        )
        .await;
        seed_catalog(&*storage, "paper-a").await;
        let distill = FakeDistill::start(vec![]).await;
        let mcp = server(storage.clone(), Some(&distill));

        FaultyStorage::set(&storage.fail_head, true);
        let err = mcp.distill_reindex(reindex("paper-a")).await.unwrap_err();

        assert!(
            err.contains("simulated storage outage"),
            "the cause must reach the caller: {err}"
        );
        assert_eq!(
            distill.requests(),
            vec![],
            "no request (certainly no DELETE) may reach the vector store"
        );
        assert!(storage.recorded_deletes().is_empty());
    }

    #[tokio::test]
    async fn missing_markdown_is_an_error_and_the_vectors_stay() {
        let storage = FaultyStorage::new();
        let distill = FakeDistill::start(vec![]).await;
        let mcp = server(storage.clone(), Some(&distill));

        let err = mcp
            .distill_reindex(reindex("never-converted"))
            .await
            .unwrap_err();

        assert!(err.contains("Markdown not found"), "{err}");
        assert_eq!(distill.count("DELETE"), 0);
        assert_eq!(distill.count("POST"), 0);
    }

    #[tokio::test]
    async fn reindex_replaces_in_place_with_one_index_call_and_no_delete() {
        let storage = FaultyStorage::new();
        seed_markdown(
            &*storage,
            "paper-a",
            "# A\n\nBody text long enough to index.",
        )
        .await;
        seed_catalog(&*storage, "paper-a").await;
        let distill = FakeDistill::start(vec![]).await;
        let mcp = server(storage.clone(), Some(&distill));

        let out = mcp.distill_reindex(reindex("paper-a")).await.unwrap();

        let out: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["chunks_indexed"], 3);
        assert_eq!(out["has_catalog"], true);
        assert_eq!(distill.count("POST"), 1);
        assert_eq!(distill.count("DELETE"), 0);
        let entry = hs_common::catalog::read_catalog_entry_via(&*storage, "catalog", "paper-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.embedding.map(|e| e.chunks_indexed),
            Some(3),
            "the catalog must carry the new embedding stamp"
        );
    }
}

mod reconcile {
    use super::*;

    fn reconcile() -> Parameters<DistillReconcileParams> {
        Parameters(DistillReconcileParams {
            dry_run: true,
            limit: None,
        })
    }

    /// RA-32: `exists().unwrap_or(false)` reported every document as an
    /// orphan during a storage outage — a purge list built from a blip.
    #[tokio::test]
    async fn a_storage_outage_is_an_error_not_a_list_of_orphans() {
        let storage = FaultyStorage::new();
        seed_markdown(
            &*storage,
            "present",
            "# P\n\nBody text long enough to index.",
        )
        .await;
        let distill = FakeDistill::start(vec!["present".into(), "gone".into()]).await;
        let mcp = server(storage.clone(), Some(&distill));

        // Catalog reads keep working; only the markdown existence probe
        // fails, as when one S3 prefix times out.
        *storage.only_keys_containing.lock().unwrap() = Some("markdown/".into());
        FaultyStorage::set(&storage.fail_head, true);
        let err = mcp.distill_reconcile(reconcile()).await.unwrap_err();

        assert!(err.contains("simulated storage outage"), "{err}");
    }

    #[tokio::test]
    async fn only_ids_without_markdown_are_orphans() {
        let storage = FaultyStorage::new();
        seed_markdown(
            &*storage,
            "present",
            "# P\n\nBody text long enough to index.",
        )
        .await;
        let distill = FakeDistill::start(vec!["present".into(), "gone".into()]).await;
        let mcp = server(storage, Some(&distill));

        let out = mcp.distill_reconcile(reconcile()).await.unwrap();

        let out: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["orphans"], serde_json::json!(["gone"]));
        assert_eq!(out["scanned_doc_ids"], 2);
    }

    /// The tool asks the server for the whole list; the old 100 000 default
    /// made a larger collection fail outright.
    #[tokio::test]
    async fn the_whole_collection_is_requested() {
        let storage: Arc<FaultyStorage> = FaultyStorage::new();
        let distill = FakeDistill::start(vec![]).await;
        let mcp = server(storage, Some(&distill));

        mcp.distill_reconcile(reconcile()).await.unwrap();

        let docs_requests: Vec<_> = distill
            .requests()
            .into_iter()
            .filter(|s| s.path == "/docs")
            .collect();
        assert_eq!(docs_requests.len(), 1);
        assert_eq!(
            docs_requests[0].query,
            format!("limit={}", hs_distill::client::MAX_DOC_LIST_LIMIT)
        );
    }
}

mod convert {
    use super::*;
    use crate::stem::Stem;
    use crate::testkit::{server_with_bus, RecordingBus};
    use hs_common::storage::Storage;

    const ARTICLE: &str = "<html><body><article><h1>A study of things</h1>\
        <p>This paragraph is long enough to clear the indexable floor of the pipeline.</p>\
        <p>A second paragraph keeps the converter from producing a stub document.</p>\
        </article></body></html>";

    fn stem(s: &str) -> Stem {
        Stem::parse(s).unwrap()
    }

    async fn put_source(storage: &FaultyStorage, stem: &str, ext: &str, bytes: &[u8]) -> String {
        use hs_common::storage::Storage;
        let key = format!("papers/{}", hs_common::sharded_key(stem, ext));
        storage.put(&key, bytes.to_vec()).await.unwrap();
        key
    }

    /// RA-31: `if let Ok(pdf) = storage.get(..)` read an S3 outage as "no
    /// PDF" and converted the HTML twin instead, stamping the catalog with a
    /// worse document than the PDF the corpus holds.
    #[tokio::test]
    async fn a_storage_outage_never_sends_the_stem_to_another_converter() {
        let storage = FaultyStorage::new();
        put_source(&storage, "paper", "pdf", b"%PDF-1.4 stand-in").await;
        put_source(&storage, "paper", "html", ARTICLE.as_bytes()).await;
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        *storage.only_keys_containing.lock().unwrap() = Some(".pdf".into());
        FaultyStorage::set(&storage.fail_head, true);
        let err = mcp.convert_source(&stem("paper")).await.unwrap_err();

        assert!(err.contains("simulated storage outage"), "{err}");
        assert!(
            !storage
                .exists(&hs_common::markdown::markdown_storage_key("paper"))
                .await
                .unwrap(),
            "nothing may be converted"
        );
        assert!(bus.published.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_source_names_every_key_that_was_tried() {
        let storage = FaultyStorage::new();
        let mcp = server_with_bus(storage, Arc::new(RecordingBus::default()));

        let err = mcp.convert_source(&stem("ghost")).await.unwrap_err();

        for ext in ["pdf", "html", "epub"] {
            assert!(err.contains(&format!("ghost.{ext}")), "{err}");
        }
    }

    /// HTML goes through the watcher's function: markdown stored, catalog
    /// stamped by the html parser, `scribe.completed` announced.
    #[tokio::test]
    async fn an_html_source_is_converted_by_the_shared_path() {
        let storage = FaultyStorage::new();
        let source_key = put_source(&storage, "paper", "html", ARTICLE.as_bytes()).await;
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        let out = mcp.convert_source(&stem("paper")).await.unwrap();

        let out: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["server"], "html-parser");
        assert_eq!(out["already_converted"], false);
        let md = hs_common::markdown::read_markdown_via(&*storage, "markdown", "paper")
            .await
            .unwrap()
            .expect("markdown stored");
        assert!(md.contains("A study of things"), "{md}");
        let published = bus.published.lock().unwrap().clone();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].0, "scribe.completed");
        assert_eq!(published[0].1["source_key"], source_key);
    }

    /// A paywall page is refused, not converted into markdown, and the
    /// refusal is the same one the watcher records.
    #[tokio::test]
    async fn a_paywall_page_is_refused_and_nothing_is_stored() {
        let storage = FaultyStorage::new();
        put_source(
            &storage,
            "wall",
            "html",
            b"<html><body><p>Access denied. Sign in to read this article.</p></body></html>",
        )
        .await;
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        let err = mcp.convert_source(&stem("wall")).await.unwrap_err();

        assert!(err.to_lowercase().contains("paywall"), "{err}");
        assert!(!storage
            .exists(&hs_common::markdown::markdown_storage_key("wall"))
            .await
            .unwrap());
        assert!(bus.published.lock().unwrap().is_empty());
        let entry = hs_common::catalog::read_catalog_entry_via(&*storage, "catalog", "wall")
            .await
            .unwrap();
        assert!(
            entry.is_none_or(|e| e.conversion.is_none()),
            "a refused document must not carry a conversion stamp"
        );
    }

    #[tokio::test]
    async fn existing_markdown_is_announced_not_converted_again() {
        let storage = FaultyStorage::new();
        put_source(&storage, "paper", "html", ARTICLE.as_bytes()).await;
        seed_markdown(
            &*storage,
            "paper",
            "# Already here\n\nBody text long enough to index.",
        )
        .await;
        let bus = Arc::new(RecordingBus::default());
        let mcp = server_with_bus(storage.clone(), bus.clone());

        let out = mcp.convert_source(&stem("paper")).await.unwrap();

        let out: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["already_converted"], true);
        let md = hs_common::markdown::read_markdown_via(&*storage, "markdown", "paper")
            .await
            .unwrap()
            .unwrap();
        assert!(
            md.contains("Already here"),
            "the stored markdown must be untouched"
        );
        assert_eq!(bus.published.lock().unwrap().len(), 1);
    }
}
