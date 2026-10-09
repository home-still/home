//! Qdrant implementation of [`VectorStore`], plus collection setup.

use std::collections::BTreeMap;

use async_trait::async_trait;
use qdrant_client::qdrant::{
    self, vectors_config, Condition, CountPointsBuilder, CreateCollectionBuilder,
    CreateFieldIndexCollectionBuilder, DeletePointsBuilder, Distance, FacetCountsBuilder,
    FieldType, Filter, HnswConfigDiffBuilder, PayloadSchemaType, PointId, PointStruct,
    QueryPointsBuilder, Range, ScrollPointsBuilder, SearchParamsBuilder, UpdateCollectionBuilder,
    UpsertPointsBuilder, VectorParamsBuilder,
};
use qdrant_client::Qdrant;
use uuid::Uuid;

use crate::client::SearchHit;
use crate::collection::{plan_collection, CollectionSpec, FieldKind, Observed, SchemaPlan};
use crate::config::HnswConfig;
use crate::error::DistillError;
use crate::store::{hnsw_matches, DocIds, HnswEnable, SearchFilter, VectorStore, YearFilter};
use crate::types::{EmbeddedChunk, ScrubReport, ScrubbedChunk};

const NAMESPACE_UUID: Uuid = Uuid::from_bytes([
    0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
]);

/// Generate a deterministic point ID from doc_id and chunk_index.
///
/// Collision bound: the ID is xxh3-64 of `"{doc_id}:{chunk_index}"` fed into
/// UUIDv5, so the effective ID space is 64 bits, not 122. Birthday odds of any
/// collision are about N²/2⁶⁵: ~3e-6 at 10M chunks, ~3e-4 at 100M, ~1e-3 at
/// 200M. A collision overwrites one chunk with another. Changing the scheme
/// changes every point ID and requires a full re-embed, so it is left as is.
pub fn deterministic_id(doc_id: &str, chunk_index: u32) -> String {
    let hash = xxhash_rust::xxh3::xxh3_64(format!("{}:{}", doc_id, chunk_index).as_bytes());
    Uuid::new_v5(&NAMESPACE_UUID, &hash.to_le_bytes()).to_string()
}

fn qerr(what: &str, e: impl std::fmt::Display) -> DistillError {
    DistillError::Qdrant(format!("{what}: {e}"))
}

// ── Collection setup ───────────────────────────────────────────

/// What [`ensure_collection`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureOutcome {
    pub created: bool,
    pub indexes_created: Vec<&'static str>,
    pub hnsw_disabled: bool,
}

/// Make `collection_name` exist with the schema in `spec`, or fail.
///
/// One path for new and existing collections: create the collection if it
/// is absent, then read back what Qdrant reports, check vector width and
/// distance, and create whichever required payload indexes are missing. So
/// a process that died between `create_collection` and the last index is
/// repaired by the next start, and a collection of the right name but the
/// wrong shape is rejected instead of accepted.
///
/// A collection whose HNSW is off (`m == 0`, how every collection was
/// created before RA-41) is reported at ERROR level but left alone:
/// enabling HNSW makes Qdrant build the graph over the whole corpus, which
/// must not happen at an uncontrolled moment such as a service restart.
pub async fn ensure_collection(
    client: &Qdrant,
    collection_name: &str,
    spec: &CollectionSpec,
) -> Result<EnsureOutcome, DistillError> {
    let exists = client
        .collection_exists(collection_name)
        .await
        .map_err(|e| qerr("Failed to check whether the collection exists", e))?;

    if !exists {
        tracing::info!(
            "Creating collection '{}' with {}d cosine vectors, HNSW m={} ef_construct={}",
            collection_name,
            spec.dimension,
            spec.hnsw_m,
            spec.hnsw_ef_construct
        );
        client
            .create_collection(
                CreateCollectionBuilder::new(collection_name)
                    .vectors_config(
                        VectorParamsBuilder::new(spec.dimension as u64, Distance::Cosine)
                            .on_disk(true),
                    )
                    .hnsw_config(
                        HnswConfigDiffBuilder::default()
                            .m(spec.hnsw_m)
                            .ef_construct(spec.hnsw_ef_construct),
                    )
                    .on_disk_payload(true),
            )
            .await
            .map_err(|e| qerr("Failed to create collection", e))?;
    }

    let info = client
        .collection_info(collection_name)
        .await
        .map_err(|e| qerr("Failed to read collection info", e))?
        .result
        .ok_or_else(|| {
            DistillError::Qdrant(format!(
                "Qdrant returned no info for collection '{collection_name}'"
            ))
        })?;
    let plan: SchemaPlan = plan_collection(collection_name, &observe(&info), spec)?;

    for (field, kind) in &plan.create_indexes {
        client
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                collection_name,
                *field,
                field_type(*kind),
            ))
            .await
            .map_err(|e| qerr(&format!("Failed to create payload index '{field}'"), e))?;
    }

    if plan.hnsw_disabled {
        tracing::error!(
            collection = collection_name,
            "HNSW is DISABLED on this collection (hnsw m = 0): every search is a brute-force \
             scan of all vectors and hnsw_ef never applies. It was created with m=0 and nothing \
             ever enabled the index. Enabling it makes Qdrant build the graph over the whole \
             corpus (heavy CPU/IO), so it is not done automatically: run \
             `hs distill hnsw enable --collection {collection_name}` in a maintenance window, \
             or rebuild the collection"
        );
    }

    Ok(EnsureOutcome {
        created: !exists,
        indexes_created: plan.create_indexes.iter().map(|(f, _)| *f).collect(),
        hnsw_disabled: plan.hnsw_disabled,
    })
}

fn field_type(kind: FieldKind) -> FieldType {
    match kind {
        FieldKind::Keyword => FieldType::Keyword,
        FieldKind::Integer => FieldType::Integer,
        FieldKind::Text => FieldType::Text,
    }
}

fn field_kind(data_type: i32) -> Option<FieldKind> {
    match PayloadSchemaType::try_from(data_type).ok()? {
        PayloadSchemaType::Keyword => Some(FieldKind::Keyword),
        PayloadSchemaType::Integer => Some(FieldKind::Integer),
        PayloadSchemaType::Text => Some(FieldKind::Text),
        _ => None,
    }
}

/// Effective (m, ef_construct) of a collection: vector-level override first,
/// else the collection-level config.
fn observe_hnsw(info: &qdrant::CollectionInfo) -> (Option<u64>, Option<u64>) {
    let config = info.config.as_ref();
    let vector = config
        .and_then(|c| c.params.as_ref())
        .and_then(|p| p.vectors_config.as_ref())
        .and_then(|v| match v.config.as_ref()? {
            vectors_config::Config::Params(p) => p.hnsw_config.as_ref(),
            vectors_config::Config::ParamsMap(_) => None,
        });
    let coll = config.and_then(|c| c.hnsw_config.as_ref());
    (
        vector.and_then(|h| h.m).or(coll.and_then(|h| h.m)),
        vector
            .and_then(|h| h.ef_construct)
            .or(coll.and_then(|h| h.ef_construct)),
    )
}

/// Reduce a live `CollectionInfo` to the facts [`plan_collection`] checks.
fn observe(info: &qdrant::CollectionInfo) -> Observed {
    let config = info.config.as_ref();
    let vector = config
        .and_then(|c| c.params.as_ref())
        .and_then(|p| p.vectors_config.as_ref())
        .and_then(|v| match v.config.as_ref()? {
            vectors_config::Config::Params(p) => Some(p),
            vectors_config::Config::ParamsMap(_) => None,
        });
    Observed {
        vector_size: vector.map(|p| p.size),
        cosine: vector.map(|p| p.distance == Distance::Cosine as i32),
        hnsw_m: vector
            .and_then(|p| p.hnsw_config.as_ref())
            .and_then(|h| h.m)
            .or_else(|| {
                config
                    .and_then(|c| c.hnsw_config.as_ref())
                    .and_then(|h| h.m)
            }),
        indexes: info
            .payload_schema
            .iter()
            .map(|(field, schema)| (field.clone(), field_kind(schema.data_type)))
            .collect::<BTreeMap<_, _>>(),
    }
}

// ── Store ──────────────────────────────────────────────────────

/// [`VectorStore`] over a live Qdrant.
pub struct QdrantStore {
    client: Qdrant,
    /// `ef` requested for every query.
    search_ef: u64,
}

impl QdrantStore {
    pub fn new(client: Qdrant, search_ef: u64) -> Self {
        Self { client, search_ef }
    }
}

fn doc_filter(doc_id: &str, from_chunk: u32) -> Filter {
    let mut must = vec![Condition::matches("doc_id", doc_id.to_string())];
    if from_chunk > 0 {
        must.push(Condition::range(
            "chunk_index",
            Range {
                gte: Some(f64::from(from_chunk)),
                ..Default::default()
            },
        ));
    }
    Filter::must(must)
}

fn year_condition(year: YearFilter) -> Condition {
    let range = |build: fn(f64) -> Range, y: i64| Condition::range("year", build(y as f64));
    match year {
        YearFilter::Eq(y) => Condition::matches("year", y),
        YearFilter::Gt(y) => range(
            |v| Range {
                gt: Some(v),
                ..Default::default()
            },
            y,
        ),
        YearFilter::Gte(y) => range(
            |v| Range {
                gte: Some(v),
                ..Default::default()
            },
            y,
        ),
        YearFilter::Lt(y) => range(
            |v| Range {
                lt: Some(v),
                ..Default::default()
            },
            y,
        ),
        YearFilter::Lte(y) => range(
            |v| Range {
                lte: Some(v),
                ..Default::default()
            },
            y,
        ),
    }
}

fn to_qdrant_filter(filter: &SearchFilter) -> Option<Filter> {
    let mut conditions = Vec::new();
    if let Some(year) = filter.year {
        conditions.push(year_condition(year));
    }
    if let Some(topic) = &filter.topic {
        conditions.push(Condition::matches("topics", topic.clone()));
    }
    if let Some(category) = &filter.category {
        conditions.push(Condition::matches("category", category.clone()));
    }
    (!conditions.is_empty()).then(|| Filter::must(conditions))
}

fn chunk_payload(ec: &EmbeddedChunk) -> Result<qdrant_client::Payload, DistillError> {
    let meta = &ec.chunk.meta;
    let payload = serde_json::json!({
        "doc_id": ec.chunk.doc_id,
        "chunk_index": ec.chunk.chunk_index,
        "chunk_text": ec.chunk.raw_text,
        "title": meta.title,
        "authors": meta.authors,
        "doi": meta.doi,
        "year": sanitize_year(meta.publication_date.as_deref()),
        "topics": meta.topics,
        "keywords": meta.keywords,
        "pdf_path": meta.pdf_path,
        "markdown_path": meta.markdown_path,
        "line_start": ec.chunk.span.line_start as i64,
        "line_end": ec.chunk.span.line_end as i64,
        "page": ec.chunk.page.map(|p| p as i64),
        "cited_by_count": meta.cited_by_count,
        "category": meta.category,
        "original_format": meta.original_format,
        "ingested_at": meta.ingested_at,
    });
    qdrant_client::Payload::try_from(payload).map_err(|e| qerr("Invalid point payload", e))
}

fn hit_from_point(point: qdrant::ScoredPoint) -> Option<SearchHit> {
    let payload = point.payload;
    Some(SearchHit {
        doc_id: payload
            .get("doc_id")?
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        title: payload
            .get("title")
            .and_then(|v| v.as_str().map(|s| s.to_string())),
        authors: payload
            .get("authors")
            .and_then(|v| v.as_list())
            .map(|list| {
                list.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        year: payload
            .get("year")
            .and_then(|v| v.as_integer())
            .map(|v| v as u64),
        doi: payload
            .get("doi")
            .and_then(|v| v.as_str().map(|s| s.to_string())),
        chunk_text: payload
            .get("chunk_text")?
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        score: point.score,
        pdf_path: payload
            .get("pdf_path")
            .and_then(|v| v.as_str().map(|s| s.to_string())),
        line_start: payload
            .get("line_start")
            .and_then(|v| v.as_integer())
            .unwrap_or(0) as usize,
        line_end: payload
            .get("line_end")
            .and_then(|v| v.as_integer())
            .unwrap_or(0) as usize,
        page: payload
            .get("page")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize),
        category: payload
            .get("category")
            .and_then(|v| v.as_str().map(|s| s.to_string())),
    })
}

#[async_trait]
impl VectorStore for QdrantStore {
    async fn health(&self) -> Result<String, DistillError> {
        self.client
            .health_check()
            .await
            .map(|r| r.version)
            .map_err(|e| qerr("qdrant unreachable", e))
    }

    async fn upsert(&self, collection: &str, chunks: &[EmbeddedChunk]) -> Result<(), DistillError> {
        let points = chunks
            .iter()
            .map(|ec| {
                Ok(PointStruct::new(
                    deterministic_id(&ec.chunk.doc_id, ec.chunk.chunk_index),
                    ec.embedding.dense.clone(),
                    chunk_payload(ec)?,
                ))
            })
            .collect::<Result<Vec<_>, DistillError>>()?;

        self.client
            .upsert_points(UpsertPointsBuilder::new(collection, points))
            .await
            .map_err(|e| qerr("Failed to upsert", e))?;
        Ok(())
    }

    async fn delete_chunks_from(
        &self,
        collection: &str,
        doc_id: &str,
        from_chunk: u32,
    ) -> Result<(), DistillError> {
        self.client
            .delete_points(
                DeletePointsBuilder::new(collection).points(doc_filter(doc_id, from_chunk)),
            )
            .await
            .map_err(|e| qerr("Failed to delete points", e))?;
        Ok(())
    }

    async fn doc_chunks(&self, collection: &str, doc_id: &str) -> Result<u64, DistillError> {
        let response = self
            .client
            .count(
                CountPointsBuilder::new(collection)
                    .filter(doc_filter(doc_id, 0))
                    .exact(true),
            )
            .await
            .map_err(|e| qerr("Failed to count a document's chunks", e))?;
        response
            .result
            .map(|r| r.count)
            .ok_or_else(|| DistillError::Qdrant("Qdrant returned no count".into()))
    }

    async fn search(
        &self,
        collection: &str,
        vector: Vec<f32>,
        limit: u64,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>, DistillError> {
        let mut builder = QueryPointsBuilder::new(collection)
            .query(qdrant::Query::from(vector))
            .limit(limit)
            .with_payload(true)
            .params(SearchParamsBuilder::default().hnsw_ef(self.search_ef));
        if let Some(f) = to_qdrant_filter(filter) {
            builder = builder.filter(f);
        }
        let results = self
            .client
            .query(builder)
            .await
            .map_err(|e| qerr("Search failed", e))?;
        Ok(results
            .result
            .into_iter()
            .filter_map(hit_from_point)
            .collect())
    }

    async fn points_count(&self, collection: &str) -> Result<u64, DistillError> {
        points_count(&self.client, collection).await
    }

    async fn doc_ids(&self, collection: &str, limit: u64) -> Result<DocIds, DistillError> {
        // Ask for one more than `limit` so "exactly limit" and "more than
        // limit" are told apart.
        let response = self
            .client
            .facet(
                FacetCountsBuilder::new(collection, "doc_id")
                    .limit(limit.saturating_add(1))
                    .exact(true),
            )
            .await
            .map_err(|e| qerr("Failed to list doc_ids", e))?;
        use qdrant::facet_value::Variant;
        let mut ids: Vec<String> = response
            .hits
            .into_iter()
            .filter_map(|h| {
                h.value.and_then(|v| match v.variant? {
                    Variant::StringValue(s) => Some(s),
                    _ => None,
                })
            })
            .collect();
        let truncated = ids.len() as u64 > limit;
        ids.truncate(limit as usize);
        Ok(DocIds { ids, truncated })
    }

    async fn reset(&self, collection: &str, spec: &CollectionSpec) -> Result<u64, DistillError> {
        reset_collection(&self.client, collection, spec).await
    }

    async fn enable_hnsw(
        &self,
        collection: &str,
        hnsw: &HnswConfig,
    ) -> Result<HnswEnable, DistillError> {
        let info = self
            .client
            .collection_info(collection)
            .await
            .map_err(|e| qerr("Failed to read collection info", e))?
            .result
            .ok_or_else(|| {
                DistillError::Qdrant(format!("Qdrant returned no info for '{collection}'"))
            })?;
        let (m, ef) = observe_hnsw(&info);
        if hnsw_matches(m, ef, hnsw) {
            return Ok(HnswEnable {
                collection: collection.into(),
                submitted: false,
                m: hnsw.m,
                ef_construct: hnsw.ef_construct,
                max_indexing_threads: 0,
                message: "HNSW already enabled with these parameters; nothing changed".into(),
            });
        }
        self.client
            .update_collection(
                UpdateCollectionBuilder::new(collection).hnsw_config(
                    HnswConfigDiffBuilder::default()
                        .m(hnsw.m)
                        .ef_construct(hnsw.ef_construct)
                        .max_indexing_threads(hnsw.max_indexing_threads),
                ),
            )
            .await
            .map_err(|e| qerr("Failed to update collection HNSW config", e))?;
        tracing::warn!(
            collection,
            m = hnsw.m,
            ef_construct = hnsw.ef_construct,
            max_indexing_threads = hnsw.max_indexing_threads,
            "HNSW update submitted; Qdrant builds the graph in the background"
        );
        Ok(HnswEnable {
            collection: collection.into(),
            submitted: true,
            m: hnsw.m,
            ef_construct: hnsw.ef_construct,
            max_indexing_threads: hnsw.max_indexing_threads,
            message: "HNSW update submitted; Qdrant builds the graph in the background (search stays available, brute force until each segment is indexed)".into(),
        })
    }

    async fn scrub_interstitials(
        &self,
        collection: &str,
        dry_run: bool,
    ) -> Result<ScrubReport, DistillError> {
        scrub_interstitial_chunks(&self.client, collection, dry_run).await
    }
}

async fn points_count(client: &Qdrant, collection_name: &str) -> Result<u64, DistillError> {
    let info = client
        .collection_info(collection_name)
        .await
        .map_err(|e| qerr("Failed to get collection info", e))?
        .result
        .ok_or_else(|| {
            DistillError::Qdrant(format!("Qdrant returned no info for '{collection_name}'"))
        })?;
    info.points_count.ok_or_else(|| {
        DistillError::Qdrant(format!(
            "Qdrant reported no points_count for '{collection_name}'"
        ))
    })
}

/// Drop the collection (if it exists) and recreate it with the configured
/// schema. Returns the pre-drop point count.
///
/// Qdrant has no atomic truncate, so the failure modes are made loud
/// instead: the point count is read BEFORE anything is dropped (an
/// unreadable collection is never dropped), and if recreation fails the
/// error says the collection is gone and how to repair it. The repair is
/// the same call (idempotent) or a server restart, because
/// [`ensure_collection`] runs at every start.
pub async fn reset_collection(
    client: &Qdrant,
    collection_name: &str,
    spec: &CollectionSpec,
) -> Result<u64, DistillError> {
    let exists = client
        .collection_exists(collection_name)
        .await
        .map_err(|e| qerr("Failed to check whether the collection exists", e))?;

    let prior_points = if exists {
        points_count(client, collection_name).await?
    } else {
        0
    };

    if exists {
        client
            .delete_collection(collection_name)
            .await
            .map_err(|e| qerr("Failed to drop collection", e))?;
        tracing::warn!(
            "Dropped collection '{}' ({} points)",
            collection_name,
            prior_points
        );
    }

    ensure_collection(client, collection_name, spec)
        .await
        .map_err(|e| {
            DistillError::Qdrant(format!(
                "collection '{collection_name}' was dropped ({prior_points} points) but could not \
                 be recreated: {e}. It does not exist until recreation succeeds: repeat the reset \
                 (idempotent) or restart the server"
            ))
        })?;
    Ok(prior_points)
}

/// Walk every point in the collection, identify chunks whose `chunk_text`
/// payload matches a known anti-bot / cookie-banner interstitial signature,
/// and (unless `dry_run`) delete just those points by ID — leaving the rest
/// of each document intact. Used to scrub contamination from real papers
/// where the conversion swept up a trailing cookie banner alongside the
/// real article body. For full-stub markdowns the `purge-poisoned` CLI
/// command is the right tool; this is the per-chunk parallel.
pub async fn scrub_interstitial_chunks(
    client: &Qdrant,
    collection_name: &str,
    dry_run: bool,
) -> Result<ScrubReport, DistillError> {
    const SCROLL_BATCH: u32 = 1024;
    const SAMPLE_CAP: usize = 10;
    const DELETE_BATCH: usize = 256;

    let mut offset: Option<PointId> = None;
    let mut total_scanned: u64 = 0;
    let mut matched_ids: Vec<PointId> = Vec::new();
    let mut samples: Vec<ScrubbedChunk> = Vec::new();

    loop {
        let mut builder = ScrollPointsBuilder::new(collection_name)
            .limit(SCROLL_BATCH)
            .with_payload(true)
            .with_vectors(false);
        if let Some(ofs) = offset.clone() {
            builder = builder.offset(ofs);
        }
        let response = client
            .scroll(builder)
            .await
            .map_err(|e| qerr("scroll failed", e))?;

        for point in &response.result {
            total_scanned += 1;
            let chunk_text = match point.payload.get("chunk_text").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            if hs_common::html::is_known_interstitial(chunk_text) {
                if let Some(id) = point.id.clone() {
                    if samples.len() < SAMPLE_CAP {
                        let doc_id = point
                            .payload
                            .get("doc_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_default();
                        let excerpt: String = chunk_text.chars().take(120).collect();
                        samples.push(ScrubbedChunk { doc_id, excerpt });
                    }
                    matched_ids.push(id);
                }
            }
        }

        offset = response.next_page_offset;
        if offset.is_none() {
            break;
        }
    }

    let mut deleted: u64 = 0;
    if !dry_run && !matched_ids.is_empty() {
        for batch in matched_ids.chunks(DELETE_BATCH) {
            client
                .delete_points(DeletePointsBuilder::new(collection_name).points(batch.to_vec()))
                .await
                .map_err(|e| qerr("delete failed", e))?;
            deleted += batch.len() as u64;
        }
    }

    Ok(ScrubReport {
        total_scanned,
        matched: matched_ids.len() as u64,
        deleted,
        samples,
    })
}

/// Coerce a `publication_date` string into a sane integer year for the Qdrant
/// payload. The first 4 chars must parse as i64 within `[1900, current_year+1]`;
/// anything outside that band (e.g. `"2041-..."` from a citation regex hit, or
/// a corrupt catalog row) becomes `None` so the field is null in Qdrant rather
/// than misleadingly precise. `+1` tolerates next-year preprints.
pub(crate) fn sanitize_year(publication_date: Option<&str>) -> Option<i64> {
    use chrono::Datelike;
    let year = publication_date?.get(..4)?.parse::<i64>().ok()?;
    let now_year = chrono::Utc::now().year() as i64;
    (1900..=now_year + 1).contains(&year).then_some(year)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use qdrant::{
        CollectionConfig, CollectionInfo, CollectionParams, HnswConfigDiff, PayloadSchemaInfo,
        VectorParams, VectorParamsMap, VectorsConfig,
    };

    #[test]
    fn deterministic_id_is_stable() {
        let id1 = deterministic_id("doc-123", 0);
        let id2 = deterministic_id("doc-123", 0);
        assert_eq!(id1, id2);
    }

    #[test]
    fn different_inputs_different_ids() {
        let id1 = deterministic_id("doc-123", 0);
        let id2 = deterministic_id("doc-123", 1);
        assert_ne!(id1, id2);
    }

    #[test]
    fn sanitize_year_keeps_plausible_publication_year() {
        assert_eq!(sanitize_year(Some("2018-05-15")), Some(2018));
        assert_eq!(sanitize_year(Some("1995")), Some(1995));
    }

    #[test]
    fn sanitize_year_keeps_next_year_preprint() {
        let next = chrono::Utc::now().year() as i64 + 1;
        let s = format!("{next}-01-01");
        assert_eq!(sanitize_year(Some(&s)), Some(next));
    }

    #[test]
    fn sanitize_year_rejects_far_future_year() {
        // The 2041 case from the self-test: regex-extracted citation year
        // bleeding into the payload.
        assert_eq!(sanitize_year(Some("2041-01-01")), None);
        assert_eq!(sanitize_year(Some("9999")), None);
    }

    #[test]
    fn sanitize_year_rejects_pre_1900() {
        assert_eq!(sanitize_year(Some("1899-12-31")), None);
        assert_eq!(sanitize_year(Some("0001")), None);
    }

    #[test]
    fn sanitize_year_handles_missing_or_unparseable() {
        assert_eq!(sanitize_year(None), None);
        assert_eq!(sanitize_year(Some("")), None);
        assert_eq!(sanitize_year(Some("XX")), None); // first 4 chars need to parse, "XX" is too short
        assert_eq!(sanitize_year(Some("nope")), None);
    }

    // ── observe(): live CollectionInfo -> Observed ─────────────────────

    fn info(
        vector: Option<VectorParams>,
        collection_hnsw_m: Option<u64>,
        schema: &[(&str, PayloadSchemaType)],
    ) -> CollectionInfo {
        CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams {
                    vectors_config: vector.map(|p| VectorsConfig {
                        config: Some(vectors_config::Config::Params(p)),
                    }),
                    ..Default::default()
                }),
                hnsw_config: Some(HnswConfigDiff {
                    m: collection_hnsw_m,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            payload_schema: schema
                .iter()
                .map(|(f, t)| {
                    (
                        f.to_string(),
                        PayloadSchemaInfo {
                            data_type: *t as i32,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn vector(size: u64, distance: Distance) -> VectorParams {
        VectorParams {
            size,
            distance: distance as i32,
            ..Default::default()
        }
    }

    #[test]
    fn observe_reads_width_distance_hnsw_and_indexes() {
        let o = observe(&info(
            Some(vector(1024, Distance::Cosine)),
            Some(0),
            &[
                ("doc_id", PayloadSchemaType::Keyword),
                ("year", PayloadSchemaType::Integer),
                ("title", PayloadSchemaType::Text),
                ("odd", PayloadSchemaType::Float),
            ],
        ));
        assert_eq!(o.vector_size, Some(1024));
        assert_eq!(o.cosine, Some(true));
        assert_eq!(o.hnsw_m, Some(0));
        assert_eq!(o.indexes["doc_id"], Some(FieldKind::Keyword));
        assert_eq!(o.indexes["year"], Some(FieldKind::Integer));
        assert_eq!(o.indexes["title"], Some(FieldKind::Text));
        assert_eq!(o.indexes["odd"], None);
    }

    #[test]
    fn observe_prefers_the_vector_level_hnsw_override() {
        let mut v = vector(1024, Distance::Cosine);
        v.hnsw_config = Some(HnswConfigDiff {
            m: Some(32),
            ..Default::default()
        });
        let o = observe(&info(Some(v), Some(0), &[]));
        assert_eq!(o.hnsw_m, Some(32));
    }

    #[test]
    fn observe_flags_other_distances_and_named_vectors() {
        let o = observe(&info(Some(vector(1024, Distance::Dot)), Some(16), &[]));
        assert_eq!(o.cosine, Some(false));

        let mut named = info(None, Some(16), &[]);
        named
            .config
            .as_mut()
            .unwrap()
            .params
            .as_mut()
            .unwrap()
            .vectors_config = Some(VectorsConfig {
            config: Some(vectors_config::Config::ParamsMap(VectorParamsMap::default())),
        });
        let o = observe(&named);
        assert_eq!(o.vector_size, None);
        assert_eq!(o.cosine, None);
    }

    #[test]
    fn year_filter_maps_to_the_matching_range_bound() {
        use qdrant::condition::ConditionOneOf;
        let bounds = |y: YearFilter| match year_condition(y).condition_one_of {
            Some(ConditionOneOf::Field(f)) => f.range.map(|r| (r.gt, r.gte, r.lt, r.lte)),
            other => panic!("unexpected condition {other:?}"),
        };
        assert_eq!(
            bounds(YearFilter::Gt(2020)),
            Some((Some(2020.0), None, None, None))
        );
        assert_eq!(
            bounds(YearFilter::Gte(2020)),
            Some((None, Some(2020.0), None, None))
        );
        assert_eq!(
            bounds(YearFilter::Lt(2020)),
            Some((None, None, Some(2020.0), None))
        );
        assert_eq!(
            bounds(YearFilter::Lte(2020)),
            Some((None, None, None, Some(2020.0)))
        );
        assert_eq!(
            bounds(YearFilter::Eq(2020)),
            None,
            "exact year is a match, not a range"
        );
    }

    #[test]
    fn stale_tail_filter_is_scoped_to_the_doc_and_the_tail() {
        let whole = doc_filter("d", 0);
        assert_eq!(whole.must.len(), 1, "from 0 deletes the whole document");
        let tail = doc_filter("d", 3);
        assert_eq!(tail.must.len(), 2);
        use qdrant::condition::ConditionOneOf;
        let range = tail
            .must
            .iter()
            .find_map(|c| match &c.condition_one_of {
                Some(ConditionOneOf::Field(f)) if f.key == "chunk_index" => f.range,
                _ => None,
            })
            .expect("chunk_index range");
        assert_eq!(range.gte, Some(3.0));
    }

    #[test]
    fn empty_search_filter_builds_no_qdrant_filter() {
        assert!(to_qdrant_filter(&SearchFilter::default()).is_none());
        let f = to_qdrant_filter(&SearchFilter {
            year: Some(YearFilter::Eq(2020)),
            topic: Some("t".into()),
            category: Some("medical".into()),
        })
        .unwrap();
        assert_eq!(f.must.len(), 3);
    }
}
