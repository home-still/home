use qdrant_client::qdrant::{
    Condition, CountPointsBuilder, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder,
    Distance, FacetCountsBuilder, FieldType, Filter, HnswConfigDiffBuilder, PointStruct,
    QueryPointsBuilder, SearchParamsBuilder, UpsertPointsBuilder, VectorParamsBuilder,
};
use qdrant_client::Qdrant;
use uuid::Uuid;

use crate::error::DistillError;
use crate::types::EmbeddedChunk;

const NAMESPACE_UUID: Uuid = Uuid::from_bytes([
    0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
]);

/// Generate a deterministic point ID from doc_id and chunk_index.
pub fn deterministic_id(doc_id: &str, chunk_index: u32) -> String {
    let hash = xxhash_rust::xxh3::xxh3_64(format!("{}:{}", doc_id, chunk_index).as_bytes());
    Uuid::new_v5(&NAMESPACE_UUID, &hash.to_le_bytes()).to_string()
}

/// Ensure the collection exists with the correct schema.
pub async fn ensure_collection(
    client: &Qdrant,
    collection_name: &str,
    dimension: usize,
) -> Result<(), DistillError> {
    // Check if collection exists
    let collections = client
        .list_collections()
        .await
        .map_err(|e| DistillError::Qdrant(format!("Failed to list collections: {e}")))?;

    let exists = collections
        .collections
        .iter()
        .any(|c| c.name == collection_name);

    if exists {
        tracing::info!("Collection '{}' already exists", collection_name);
        return Ok(());
    }

    tracing::info!(
        "Creating collection '{}' with {}d vectors",
        collection_name,
        dimension
    );

    // Create with HNSW disabled for bulk load (m=0)
    client
        .create_collection(
            CreateCollectionBuilder::new(collection_name)
                .vectors_config(
                    VectorParamsBuilder::new(dimension as u64, Distance::Cosine).on_disk(true),
                )
                .hnsw_config(HnswConfigDiffBuilder::default().m(0))
                .on_disk_payload(true),
        )
        .await
        .map_err(|e| DistillError::Qdrant(format!("Failed to create collection: {e}")))?;

    // Create payload indexes
    create_indexes(client, collection_name).await?;

    Ok(())
}

async fn create_indexes(client: &Qdrant, collection_name: &str) -> Result<(), DistillError> {
    let keyword_fields = ["doc_id", "authors", "topics", "keywords", "pdf_path"];
    let integer_fields = ["year", "line_start", "page"];

    for field in keyword_fields {
        client
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                collection_name,
                field,
                FieldType::Keyword,
            ))
            .await
            .map_err(|e| DistillError::Qdrant(format!("Failed to create index '{field}': {e}")))?;
    }

    for field in integer_fields {
        client
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                collection_name,
                field,
                FieldType::Integer,
            ))
            .await
            .map_err(|e| DistillError::Qdrant(format!("Failed to create index '{field}': {e}")))?;
    }

    // Full-text index on title
    client
        .create_field_index(CreateFieldIndexCollectionBuilder::new(
            collection_name,
            "title",
            FieldType::Text,
        ))
        .await
        .map_err(|e| DistillError::Qdrant(format!("Failed to create title index: {e}")))?;

    Ok(())
}

/// Upsert embedded chunks to Qdrant.
pub async fn upsert_chunks(
    client: &Qdrant,
    collection_name: &str,
    chunks: &[EmbeddedChunk],
) -> Result<(), DistillError> {
    let points: Vec<PointStruct> = chunks
        .iter()
        .map(|ec| {
            let point_id = deterministic_id(&ec.chunk.doc_id, ec.chunk.chunk_index);
            let meta = &ec.chunk.meta;

            let payload = serde_json::json!({
                "doc_id": ec.chunk.doc_id,
                "chunk_index": ec.chunk.chunk_index,
                "chunk_text": ec.chunk.raw_text,
                "title": meta.title,
                "authors": meta.authors,
                "doi": meta.doi,
                "year": meta.publication_date.as_ref()
                    .and_then(|d| d.get(..4))
                    .and_then(|y| y.parse::<i64>().ok()),
                "topics": meta.topics,
                "keywords": meta.keywords,
                "pdf_path": meta.pdf_path,
                "markdown_path": meta.markdown_path,
                "line_start": ec.chunk.span.line_start as i64,
                "line_end": ec.chunk.span.line_end as i64,
                "page": ec.chunk.page.map(|p| p as i64),
                "cited_by_count": meta.cited_by_count,
            });

            let qdrant_payload: qdrant_client::Payload = payload.try_into().unwrap();
            PointStruct::new(point_id, ec.embedding.dense.clone(), qdrant_payload)
        })
        .collect();

    client
        .upsert_points(UpsertPointsBuilder::new(collection_name, points))
        .await
        .map_err(|e| DistillError::Qdrant(format!("Failed to upsert: {e}")))?;

    Ok(())
}

/// Search the collection with a query vector.
pub async fn search(
    client: &Qdrant,
    collection_name: &str,
    query_vector: Vec<f32>,
    limit: u64,
) -> Result<Vec<qdrant_client::qdrant::ScoredPoint>, DistillError> {
    let results = client
        .query(
            QueryPointsBuilder::new(collection_name)
                .query(qdrant_client::qdrant::Query::from(query_vector))
                .limit(limit)
                .with_payload(true)
                .params(SearchParamsBuilder::default().hnsw_ef(128)),
        )
        .await
        .map_err(|e| DistillError::Qdrant(format!("Search failed: {e}")))?;

    Ok(results.result)
}

/// Get the number of points in a collection.
pub async fn collection_info(client: &Qdrant, collection_name: &str) -> Result<u64, DistillError> {
    let info = client
        .collection_info(collection_name)
        .await
        .map_err(|e| DistillError::Qdrant(format!("Failed to get collection info: {e}")))?;

    Ok(info
        .result
        .map(|r| r.points_count.unwrap_or(0))
        .unwrap_or(0))
}

/// Check if a document has any chunks in the collection.
pub async fn doc_exists(
    client: &Qdrant,
    collection_name: &str,
    doc_id: &str,
) -> Result<(bool, u64), DistillError> {
    let filter = Filter::must([Condition::matches("doc_id", doc_id.to_string())]);
    let response = client
        .count(
            CountPointsBuilder::new(collection_name)
                .filter(filter)
                .exact(true),
        )
        .await
        .map_err(|e| DistillError::Qdrant(format!("Failed to check doc existence: {e}")))?;

    let count = response.result.map(|r| r.count).unwrap_or(0);
    Ok((count > 0, count))
}

/// Count distinct documents in a collection via facet on doc_id.
pub async fn distinct_doc_count(
    client: &Qdrant,
    collection_name: &str,
) -> Result<u64, DistillError> {
    let response = client
        .facet(
            FacetCountsBuilder::new(collection_name, "doc_id")
                .limit(100_000)
                .exact(true),
        )
        .await
        .map_err(|e| DistillError::Qdrant(format!("Failed to count documents: {e}")))?;

    Ok(response.hits.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
