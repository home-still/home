//! `status`: how far the pipeline is, or how far one document is.
//!
//! Without a document it asks three things that answer in constant time:
//! the broker for its queues, the distill server for the index, each
//! scribe server for its readiness. It never lists the storage. With a
//! document it looks that document up at each stage. A source that cannot
//! answer is reported where its answer would be; the others still answer.

use hs_common::event_bus::specs::{PAPERS_INGESTED, SCRIBE_BOXES, SCRIBE_COMPLETED};
use hs_common::event_bus::ConsumerSpec;
use hs_common::storage::Storage;
use hs_common::stores::{StoreKind, StoreSpec};
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
            let depth = match self.events.queue_depth(queue).await {
                Ok(depth) => json!({
                    "waiting": depth.pending,
                    "in_progress": depth.ack_pending,
                }),
                Err(e) => unavailable(format!("{e:#}")),
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
            entry.embedding_skip.as_ref().map(|skipped| {
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

    /// The stores a document store's documents are indexed into: the store
    /// itself, then those derived from it.
    fn stores_fed_by<'a>(&'a self, holder: &'a StoreSpec) -> Vec<&'a StoreSpec> {
        let derived = self.stores.iter().filter(
            |spec| matches!(&spec.kind, StoreKind::Derived { of, .. } if *of == holder.name),
        );
        std::iter::once(holder).chain(derived).collect()
    }
}
