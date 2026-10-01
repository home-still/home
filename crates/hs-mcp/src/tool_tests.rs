//! Behavior of the tools against fake storage and a loopback distill server.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;

use crate::testkit::{seed_catalog, seed_markdown, server, FakeDistill, FaultyStorage};
use crate::{DistillReconcileParams, DistillReindexParams};

mod reindex {
    use super::*;

    fn reindex(stem: &str) -> Parameters<DistillReindexParams> {
        Parameters(DistillReindexParams {
            stem: stem.to_string(),
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
