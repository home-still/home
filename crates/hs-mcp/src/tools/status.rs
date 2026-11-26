//! `status`: how far the pipeline is, or how far one document is.
//!
//! Without a document it asks three things that answer in constant time:
//! the broker for its queues, the distill server for the index, each
//! scribe server for its readiness. It never lists the storage. With a
//! document it looks that document up at each stage. A source that cannot
//! answer is reported where its answer would be; the others still answer.

use hs_common::catalog::{CatalogEntry, EmbeddingSkip};
use hs_common::event_bus::specs::{PAPERS_INGESTED, SCRIBE_BOXES, SCRIBE_COMPLETED};
use hs_common::event_bus::ConsumerSpec;
use hs_common::storage::Storage;
use hs_common::stores::{DerivedText, StoreKind, StoreSpec};
use rmcp::schemars;
use serde_json::{json, Value};

use super::{catalog_entry, object_exists, source_extension};
use crate::stem::Stem;
use crate::HomeStillMcp;

/// The only source format that has word boxes.
const BOXED_SOURCE_EXTENSION: &str = "pdf";

/// The queues the pipeline's stages take their work from, and what each
/// stage does with it.
const QUEUES: [(&ConsumerSpec, &str); 3] = [
    (&PAPERS_INGESTED, "sources waiting to be converted to text"),
    (
        &SCRIBE_COMPLETED,
        "converted documents waiting to be indexed",
    ),
    (&SCRIBE_BOXES, "PDFs waiting for their word boxes"),
];

/// How long the broker gets to answer for one queue. A broker that cannot
/// be reached must not keep the index and the converters from answering.
const QUEUE_DEPTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The arguments of `status`.
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct StatusParams {
    #[schemars(
        description = "The store to report on. Default: `academic`. With `doc_id`, the store the document is in."
    )]
    pub store: Option<String>,
    #[schemars(
        description = "Report on this one document instead of the pipeline: whether its source is stored, whether it is converted, how many records each index holds for it, whether its word boxes are built, and the failure recorded for it if any."
    )]
    pub doc_id: Option<Stem>,
}

/// Where an answer would be, when its source could not give one.
fn unavailable(error: impl std::fmt::Display) -> Value {
    json!({ "error": error.to_string() })
}

/// The indexing failure that stands for a document. A skip also records a
/// document found to hold nothing to index, which is no failure; and the
/// catalog keeps a skip when the document is indexed later, so one from
/// before the last indexing is history.
fn standing_indexing_failure(entry: &CatalogEntry) -> Option<&EmbeddingSkip> {
    let skip = entry.embedding_skip.as_ref()?;
    if !hs_distill::reconcile::is_embed_failure(Some(skip.reason.as_str())) {
        return None;
    }
    let time = |at: &str| chrono::DateTime::parse_from_rfc3339(at).ok();
    let indexed_since = entry
        .embedding
        .as_ref()
        .and_then(|stamp| time(&stamp.embedded_at))
        .zip(time(&skip.at))
        .is_some_and(|(indexed, failed)| indexed >= failed);
    (!indexed_since).then_some(skip)
}

impl HomeStillMcp {
    /// Report on the pipeline, or on one document.
    pub(crate) async fn report_status(&self, p: StatusParams) -> Result<String, String> {
        let asked = p.store.as_deref().unwrap_or(hs_common::stores::ACADEMIC);
        let report = match &p.doc_id {
            Some(doc_id) => self.document_status(asked, doc_id).await?,
            None => self.pipeline_status(asked).await?,
        };
        crate::to_json(&report)
    }

    /// The queues, the index and the converters.
    async fn pipeline_status(&self, store: &str) -> Result<Value, String> {
        let spec = self.stores.get(store).map_err(|e| e.to_string())?;

        let mut queues = Vec::with_capacity(QUEUES.len());
        for (queue, holds) in QUEUES {
            let asked = tokio::time::timeout(QUEUE_DEPTH_TIMEOUT, self.events.queue_depth(queue));
            let depth = match asked.await {
                Ok(Ok(depth)) => json!({
                    "waiting": depth.pending,
                    "in_progress": depth.ack_pending,
                }),
                Ok(Err(e)) => unavailable(format!("{e:#}")),
                Err(_) => unavailable("timed out asking the broker for the queue's depth"),
            };
            queues.push(json!({ "queue": queue.subject, "holds": holds, "depth": depth }));
        }

        let index = match self.distill_client() {
            Ok(client) => match client.status(spec.name).await {
                Ok(status) => json!({
                    "store": status.store,
                    "documents": status.documents_count,
                    "documents_is_a_floor": status.documents_count_truncated,
                    "records": status.records_count,
                    "embed_model": status.embed_model,
                }),
                Err(e) => unavailable(format!("{e:#}")),
            },
            Err(e) => unavailable(e),
        };

        let mut converters = Vec::with_capacity(self.scribe_servers.len());
        for url in &self.scribe_servers {
            let readiness = match hs_scribe::client::ScribeClient::new(url) {
                Ok(client) => client.readiness().await,
                Err(e) => Err(e),
            };
            converters.push(match readiness {
                Ok(r) => json!({
                    "server": url,
                    "ready": r.ready,
                    "slots_total": r.vlm_slots_total,
                    "slots_available": r.vlm_slots_available,
                    "converting": r.in_flight_conversions,
                    "backend_status": r.backend_status,
                }),
                Err(e) => json!({ "server": url, "error": format!("{e:#}") }),
            });
        }

        Ok(json!({ "queues": queues, "index": index, "converters": converters }))
    }

    /// One document, stage by stage.
    async fn document_status(&self, asked: &str, doc_id: &Stem) -> Result<Value, String> {
        // A derived store's documents are its parent's.
        let (holder, layout) = self.stores.documents(asked).map_err(|e| e.to_string())?;
        let stem: &str = doc_id;
        let storage: &dyn Storage = &*self.storage;

        let extension = source_extension(storage, layout, stem).await?;
        let converted = object_exists(storage, &layout.markdown_key(stem)).await?;
        let entry = catalog_entry(storage, holder, layout, stem).await?;
        let boxes = match extension {
            Some(BOXED_SOURCE_EXTENSION) => {
                let key = hs_common::word_boxes::boxes_key(layout, stem);
                if object_exists(storage, &key).await? {
                    "built"
                } else {
                    "pending"
                }
            }
            _ => "not_applicable",
        };

        let mut records = Vec::new();
        for store in self.stores_fed_by(holder) {
            let count = match self.distill_client() {
                Ok(client) => match client.doc_chunks(store.name, stem).await {
                    Ok((_, chunks)) => json!(chunks),
                    Err(e) => unavailable(format!("{e:#}")),
                },
                Err(e) => unavailable(e),
            };
            records.push(json!({ "store": store.name, "records": count }));
        }

        let failure = entry.as_ref().and_then(|entry| {
            if let Some(failed) = &entry.conversion_failed {
                return Some(json!({
                    "stage": "conversion",
                    "reason": failed.reason,
                    "at": failed.at,
                    "attempts": failed.attempts,
                }));
            }
            standing_indexing_failure(entry).map(|skipped| {
                json!({ "stage": "indexing", "reason": skipped.reason, "at": skipped.at })
            })
        });

        Ok(json!({
            "store": asked,
            "doc_id": stem,
            "source": { "stored": extension.is_some(), "extension": extension },
            "catalog_entry": entry.is_some(),
            "converted": converted,
            "records": records,
            "boxes": boxes,
            "failure": failure,
        }))
    }

    /// The stores whose record counts say where a document of `holder` is
    /// indexed: the store itself, then those derived from it. The store of
    /// its reference lists is not asked: the index counts a document's
    /// chunks whole, so those are in the store's own figure.
    fn stores_fed_by<'a>(&'a self, holder: &'a StoreSpec) -> Vec<&'a StoreSpec> {
        let derived = self.stores.derived_from(holder.name).filter(|spec| {
            !matches!(
                &spec.kind,
                StoreKind::Derived {
                    text: DerivedText::ReferenceChunks,
                    ..
                }
            )
        });
        std::iter::once(holder).chain(derived).collect()
    }
}
