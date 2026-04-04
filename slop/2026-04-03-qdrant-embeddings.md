# Qdrant for billion-scale local academic search in Rust

**Qdrant can index billions of 768-dim vectors on a local workstation, but at billion scale it demands 128+ GB RAM and 4+ TB NVMe for acceptable query latency of 200ms–5s.** For a laptop with ≤64 GB RAM, the practical ceiling is closer to 100–200 million vectors before latency degrades severely. This report covers every engineering decision needed to build Home Still's Qdrant-backed search pipeline — from collection configuration and the Rust client API to hybrid search, ingestion architecture, and an honest comparison with LanceDB, which may be a better fit for truly laptop-scale billion-vector workloads.

The latest stable release is **Qdrant v1.17.0** (February 19, 2026), which replaced RocksDB with Gridstore and added io_uring batch reads. The Rust client crate is **`qdrant-client` v1.16.0** on crates.io, using gRPC exclusively via Tonic.

---

## 1. Architecture: how Qdrant handles billions of vectors on disk

### Storage tiers and memmap mechanics

Qdrant stores four independently configurable components: raw vectors, the HNSW graph, quantized vectors, and payloads. Each can live in RAM or on disk via memory-mapped files. With memmap, the OS page cache automatically promotes frequently accessed data into RAM while cold data stays on disk. This is not "loading into RAM" — it's virtual address space mapping, and performance depends entirely on available page cache and **disk IOPS**.

| Component | `on_disk: false` (default) | `on_disk: true` |
|---|---|---|
| Raw float32 vectors | RAM | Disk (memmap) |
| HNSW graph | RAM | Disk (memmap) |
| Quantized vectors | RAM (if `always_ram: true`) | Disk |
| Payload data | RAM | Disk (RocksDB → Gridstore in v1.17) |

**RAM per million vectors at 768 dims (float32):** Qdrant's documented formula is `num_vectors × dims × 4 bytes × 1.5`, where the 1.5× covers HNSW graph, metadata, and temporary optimization segments.

| Configuration | Per 1M vectors | Per 1B vectors |
|---|---|---|
| Full in-memory (float32) | ~4.6 GB | ~4.6 TB |
| Vectors on disk, SQ int8 in RAM | ~1.15 GB | ~1.15 TB |
| Vectors on disk, BQ 2-bit in RAM | ~290 MB | ~290 GB |
| Everything on disk (memmap) | ~1 GB page cache ideal | Disk I/O–bound |

Qdrant benchmarked 1M 100-dim vectors with both vectors and HNSW on disk at **135 MB RAM** — but queries took **3 seconds** with 63K IOPS storage, improving to ~20ms only with 183K+ IOPS NVMe and 600 MB page cache. The takeaway: on-disk mode works, but **NVMe IOPS is the dominant performance factor**.

### Quantization: scalar wins for nomic-embed-text-v1.5

**Scalar Quantization (SQ)** is the primary recommendation. It converts each float32 dimension to uint8, yielding **4× compression** with typically **<1% recall loss** at 768 dimensions. SIMD-optimized int8 distance calculations provide up to 2× faster comparisons. Configure with `quantile: 0.99` to clip outliers and `always_ram: true` to keep quantized vectors in fast memory while originals stay on disk.

**Product Quantization (PQ)** achieves up to 64× compression but drops recall to ~70% and is slower than SQ (lookup-table-based, not SIMD-friendly). Only use PQ when memory is the absolute constraint and accuracy is secondary.

**Binary Quantization (BQ)** offers 32× compression with 40× speed gains, but Qdrant's documentation explicitly warns it produces "poorer results for embeddings less than 1024 dimensions." However, nomic-embed-text-v1.5 was specifically trained for binary quantization, so **2-bit BQ** (16× compression, added in v1.15) is a viable aggressive option — use `oversampling: 3.0` and `rescore: true` to recover accuracy. Qdrant's newer **asymmetric quantization** stores vectors as binary but queries with scalar precision, offering a compelling middle ground.

```json
{
  "vectors": { "size": 768, "distance": "Cosine", "on_disk": true },
  "quantization_config": {
    "scalar": { "type": "int8", "quantile": 0.99, "always_ram": true }
  },
  "hnsw_config": { "m": 16, "ef_construct": 200, "on_disk": true },
  "on_disk_payload": true
}
```

### HNSW tuning for academic retrieval

Set **m=16** and **ef_construct=200** as the starting point. Each HNSW node stores 2×m edges; at m=16, the graph for 1B vectors occupies ~119 GB on disk. Increasing m to 32 doubles graph size to ~256 GB with diminishing recall gains. Search-time **ef=128–256** provides good precision for academic text — higher ef yields better recall at linear latency cost.

**Critical ingestion trick:** Set `m=0` during bulk loading to completely disable HNSW construction, then re-enable with `m=16` after all data is loaded. This avoids massive CPU spikes during ingestion and lets Qdrant build the graph once over the complete dataset.

### Segment architecture and optimization

All collection data is split into **segments**, each with independent vector storage, HNSW index, and payload indexes. Three optimizers run automatically: the **vacuum optimizer** (triggers when >20% of vectors are deleted), the **merge optimizer** (reduces segment count toward the target), and the **indexing optimizer** (builds HNSW when a segment exceeds `indexing_threshold`). Optimization is non-blocking — a copy-on-write proxy serves reads from the old segment while the new one builds.

Set `default_segment_number` to match CPU cores for parallel search across segments. For latency-optimized single queries, use 2–4 large segments. The `max_segment_size` parameter caps how large segments can grow before the merge optimizer leaves them alone.

### Payload indexing strategy

| Field | Type | Index | Rationale |
|---|---|---|---|
| `document_id` | string | keyword | Delete-by-filter, dedup lookups |
| `year` | integer | integer (lookup + range) | Range queries: "papers after 2020" |
| `source` | string | keyword | Filter by provider (arxiv, semantic_scholar) |
| `authors` | string[] | keyword | Array indexing — matches any element |
| `journal` | string | keyword | Exact match filter |
| `topics` | string[] | keyword | Array of topic tags |
| `title` | string | text (full-text) | Word-tokenized search within titles |

Keyword indexes match exact values (or any array element for array fields). The full-text index tokenizes into words with optional stemming. **Only index fields you filter on** — each index consumes RAM even with `on_disk_payload: true`, because indexed field values remain in memory regardless of payload storage mode.

### Named vectors for Matryoshka embeddings

Qdrant fully supports multiple named vectors per point with independent dimensions, HNSW configs, and quantization. Store both the full 768-dim and Matryoshka-truncated 256-dim embedding:

```json
{
  "vectors": {
    "full": { "size": 768, "distance": "Cosine", "on_disk": true },
    "matryoshka": { "size": 256, "distance": "Cosine", "on_disk": true }
  }
}
```

The powerful pattern is **multi-stage search via prefetch**: first search the 256-dim Matryoshka vector (cheaper, fewer bytes), then rescore the top candidates with the full 768-dim vector. Each named vector gets its own HNSW graph and quantization, so storage doubles — but at 256 dims, the Matryoshka SQ vectors need only 256 MB per billion points (vs 768 MB for full), making it feasible to keep the small vector's quantized index in RAM while the full vector stays on disk.

---

## 2. The Rust client: `qdrant-client` v1.16 patterns

### Connection and setup

The Rust client communicates **exclusively via gRPC** on port 6334. There is no HTTP/REST mode — the crate uses Tonic for protobuf serialization over HTTP/2 with multiplexing, which is faster than JSON/REST for batch operations. The client is `Clone + Send + Sync` and can be shared across Tokio tasks.

```rust
use qdrant_client::Qdrant;

let client = Qdrant::from_url("http://localhost:6334")
    .timeout(std::time::Duration::from_secs(30))
    .build()?;
```

The v1.10 API redesign replaced `QdrantClient` with `Qdrant` and introduced builder types (`CreateCollectionBuilder`, `UpsertPointsBuilder`, `QueryPointsBuilder`) generated via `derive_builder`. All methods are async on Tokio.

### Upsert: from fastembed-rs Vec\<f32\> to Qdrant points

fastembed-rs outputs `Vec<Vec<f32>>`. The mapping to `PointStruct` is direct:

```rust
use qdrant_client::qdrant::{PointStruct, UpsertPointsBuilder};
use serde_json::json;

let embeddings: Vec<Vec<f32>> = model.embed(documents, None)?;

let points: Vec<PointStruct> = embeddings
    .into_iter()
    .enumerate()
    .map(|(i, vector)| {
        PointStruct::new(
            chunk_uuids[i].clone(),   // String UUID or u64
            vector,                     // Vec<f32> accepted directly
            json!({
                "document_id": dois[i],
                "year": years[i],
                "title": titles[i],
                "topics": topics[i],    // Vec<String> for array field
            }).try_into().unwrap(),
        )
    })
    .collect();

// Automatic sub-batching at 1000 points per gRPC call
client.upsert_points_chunked(
    UpsertPointsBuilder::new("academic_papers", points),
    1000,
).await?;
```

For named vectors, use `NamedVectors`:

```rust
use qdrant_client::qdrant::{NamedVectors, Vector};

let point = PointStruct::new(
    uuid_string,
    NamedVectors::default()
        .add_vector("full", Vector::new_dense(full_768_vec))
        .add_vector("matryoshka", Vector::new_dense(truncated_256_vec)),
    payload,
);
```

**Optimal batch size:** 500–1,000 points per upsert call for 768-dim vectors. Use `wait(false)` for fire-and-forget throughput; periodically use `wait(true)` for backpressure. The built-in `upsert_points_chunked` handles sub-batching automatically.

### Search and filtering

The **Query API** (`QueryPointsBuilder`) is the recommended search interface, replacing the older `SearchPointsBuilder`:

```rust
use qdrant_client::qdrant::{
    QueryPointsBuilder, SearchParamsBuilder, Condition, Filter, Range,
};

let results = client.query(
    QueryPointsBuilder::new("academic_papers")
        .query(query_vector)          // Vec<f32>
        .limit(10)
        .with_payload(true)
        .filter(Filter::must([
            Condition::matches("topics", "machine-learning".to_string()),
            Condition::range("year", Range {
                gte: Some(2020.0), lte: Some(2025.0),
                gt: None, lt: None,
            }),
        ]))
        .params(SearchParamsBuilder::default().hnsw_ef(128)),
).await?;
```

Filters compose with `Filter::must` (AND), `Filter::should` (OR), and `Filter::must_not` (NOT). `Condition::matches` works for keyword exact match and array-contains. Qdrant applies filters **during** HNSW traversal (not pre/post), maintaining recall even with selective filters.

### Error handling and retries

The client returns `QdrantError` with variants including `ResponseError` (server-side gRPC status) and Tonic transport errors. There is **no built-in retry logic** — implement exponential backoff manually. All Qdrant operations are idempotent, so retrying upserts with the same point ID is safe:

```rust
async fn upsert_with_retry(
    client: &Qdrant, request: UpsertPointsBuilder, max_retries: u32,
) -> Result<(), QdrantError> {
    for attempt in 0..max_retries {
        match client.upsert_points(request.clone()).await {
            Ok(_) => return Ok(()),
            Err(e) if attempt < max_retries - 1 => {
                tokio::time::sleep(Duration::from_millis(100 * 2u64.pow(attempt))).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
```

### Cargo.toml

```toml
[dependencies]
qdrant-client = { version = "1.16", features = ["serde"] }
fastembed = "5"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
uuid = { version = "1", features = ["v5"] }
```

---

## 3. Hybrid search: dense + sparse in a single query

### Sparse vectors and native BM25

Qdrant has supported sparse vectors since v1.7 (November 2023), with 16× performance improvements in v1.8. Sparse vectors are stored as (index, value) pairs with no fixed dimensionality, indexed via an **inverted index** that provides exact (non-approximate) retrieval using dot product scoring.

Configure a collection with both dense and sparse vectors:

```rust
let mut sparse_config = SparseVectorsConfigBuilder::default();
sparse_config.add_named_vector_params(
    "sparse",
    SparseVectorParamsBuilder::default().modifier(Modifier::Idf),
);

client.create_collection(
    CreateCollectionBuilder::new("academic_papers")
        .vectors_config(VectorsConfigBuilder::new_named(
            "dense", VectorParamsBuilder::new(768, Distance::Cosine),
        ))
        .sparse_vectors_config(sparse_config),
).await?;
```

The `modifier: Idf` setting enables server-side Inverse Document Frequency calculation — critical for proper BM25 scoring. Since **Qdrant v1.15.2**, native BM25 inference is available: pass raw text and let the server tokenize and vectorize.

### The Query API with prefetch and fusion

Hybrid search uses the **prefetch mechanism** introduced in v1.10's universal Query API. Each prefetch runs a sub-query in parallel; the main query fuses results:

```rust
let results = client.query(
    QueryPointsBuilder::new("academic_papers")
        .add_prefetch(
            PrefetchQueryBuilder::default()
                .query(Query::new_nearest(
                    Vector::new_sparse(sparse_indices, sparse_values)
                ))
                .using("sparse")
                .limit(100u64)
        )
        .add_prefetch(
            PrefetchQueryBuilder::default()
                .query(Query::new_nearest(dense_vector))
                .using("dense")
                .limit(100u64)
        )
        .query(Query::new_fusion(Fusion::Rrf))  // Native RRF
        .with_payload(true)
        .limit(10u64)
).await?;
```

**Reciprocal Rank Fusion is fully native** — no application-code needed. Qdrant offers two fusion methods: **RRF** (default k=2, configurable) and **DBSF** (Distribution-Based Score Fusion, since v1.11). Recent versions add parameterized RRF with per-prefetch weights via `RrfBuilder::with_k(60)`.

### Generating sparse vectors in Rust

**fastembed-rs supports SPLADE natively** via `SparseTextEmbedding`:

```rust
use fastembed::{SparseTextEmbedding, SparseInitOptions, SparseModel};

let model = SparseTextEmbedding::try_new(
    SparseInitOptions::new(SparseModel::SPLADEPPV1)
)?;
let sparse_embeddings = model.embed(documents, None)?;
// Each embedding has .indices: Vec<u32> and .values: Vec<f32>
```

For BM25 specifically, the simplest path in Rust is Qdrant's **native BM25 inference** (v1.15.2+) — pass raw text and let the server handle tokenization. Alternatively, implement a lightweight BM25 tokenizer in Rust (tokenize → stem → hash to u32 indices → compute TF weights) and let Qdrant apply IDF server-side.

**Skip BM42** — Qdrant's own benchmarks showed it does not outperform BM25; it's labeled experimental.

### Is hybrid search worth it for academic text?

**Strongly yes.** Research on scientific document retrieval shows hybrid methods yield **+18.5% MRR improvement** and **+7.2% Recall@5** over dense-only search. Academic text benefits disproportionately because technical terms, chemical formulas, gene names, and acronyms need exact lexical matching (sparse excels) while conceptual paraphrases need semantic understanding (dense excels). The latency overhead is modest — Qdrant runs prefetch sub-queries in parallel, so hybrid adds roughly the time of the slower sub-query plus ~1ms for fusion, not the sum.

At billion scale, **BM25 sparse vectors are far more storage-efficient than SPLADE**: BM25 produces ~5–30 non-zero values per short chunk (~200–400 bytes/vector = ~186–372 GB at 1B) versus SPLADE's ~100–200 values (~1,200 bytes/vector = ~1.12 TB). Start with BM25; graduate to SPLADE only if recall is insufficient.

---

## 4. Performance reality check for local hardware

### Query latency at scale

| Scale | Configuration | Expected p50 | Expected p99 |
|---|---|---|---|
| **10M vectors** | SQ in RAM, vectors on disk, NVMe | 5–20ms | 30–80ms |
| **100M vectors** | SQ in RAM, HNSW on disk, NVMe | 20–80ms | 100–500ms |
| **1B vectors** | All on disk, SQ mmap'd, NVMe | 100–500ms | 1–5s |

These numbers assume NVMe with 500K+ IOPS. With SATA SSD (100K IOPS), multiply latencies by 3–5×. Qdrant's own benchmarks show IOPS is the single largest performance lever when data exceeds RAM — **183K IOPS gave 50 RPS** versus 5 RPS at 63K IOPS.

### RAM requirements at 1B vectors

The hard math makes billion-scale on a laptop extremely challenging:

- **Raw float32 vectors:** 1B × 768 × 4 = **2.86 TB** (stored on disk)
- **SQ int8 in RAM:** 1B × 768 = **715 GB** — far exceeds workstation RAM
- **HNSW graph (m=16):** 1B × 32 edges × 4 bytes ≈ **119 GB** on disk
- **Payload indexes:** ~1–5 GB for typical academic metadata

With **everything on disk** (vectors, HNSW, SQ all mmap'd), Qdrant will function with as little as 8–16 GB of usable page cache, but queries touch random graph nodes across a 119 GB structure, so cache hit rates drop rapidly. **64 GB RAM yields multi-second latencies at 1B scale. 128 GB is the practical minimum for sub-second queries**, and even then only with NVMe.

The LAION-400M benchmark (512-dim) ran 400M vectors on **64 GB RAM** using binary quantization with `always_ram: true` plus on-disk vectors and HNSW with m=6 — demonstrating that ~400M vectors is the realistic ceiling for a 64 GB machine.

### Disk I/O and concurrency

HNSW traversal generates **random 4KB reads** — the worst-case pattern for HDDs, acceptable for SSDs, fast on NVMe. On Linux, enable `storage.async_scorer: true` for io_uring-based async I/O, which reduces context-switch overhead significantly. **This is not available on macOS.**

For concurrent interactive + batch queries: Qdrant uses Tokio async + thread pools. Set `optimizer_cpu_budget: 1` to prevent the background optimizer from starving search threads during heavy ingestion. There is no formal QoS/priority system — you must throttle batch agent queries in application code (e.g., with a Tokio semaphore) to preserve interactive latency.

**Disk space budget for 1B vectors:** ~2.86 TB (raw) + ~715 GB (SQ copy) + ~119 GB (HNSW) + ~500 GB (payloads) ≈ **4+ TB total**. A 4 TB NVMe is the minimum recommendation.

---

## 5. Ingestion pipeline for billions of academic chunks

### Collection schema in Rust

```rust
// 1. Create collection
client.create_collection(
    CreateCollectionBuilder::new("academic_papers")
        .vectors_config(VectorParamsBuilder::new(768, Distance::Cosine).on_disk(true))
        .hnsw_config(HnswConfigDiffBuilder::default().m(0)) // Disabled for bulk load
        .on_disk_payload(true)
        .shard_number(2) // Parallel WAL writes
).await?;

// 2. Create payload indexes
client.create_field_index(
    CreateFieldIndexCollectionBuilder::new("academic_papers", "document_id", FieldType::Keyword)
).await?;
client.create_field_index(
    CreateFieldIndexCollectionBuilder::new("academic_papers", "year", FieldType::Integer)
        .field_index_params(IntegerIndexParamsBuilder::default().lookup(true).range(true))
).await?;
// ... repeat for source, authors, journal, topics (all Keyword)
```

### Point ID strategy

Use **deterministic UUIDv5** from DOI + chunk_index for idempotent upserts:

```rust
const NAMESPACE: Uuid = Uuid::from_bytes([/* fixed bytes */]);

fn point_id(doi: &str, chunk_index: u32) -> String {
    Uuid::new_v5(&NAMESPACE, format!("{}:{}", doi, chunk_index).as_bytes()).to_string()
}
```

Qdrant's upsert is idempotent — re-upserting the same point ID overwrites both vector and payload atomically. For re-chunked documents (different chunk count), **delete by filter first**, then re-insert:

```rust
client.delete_points(
    DeletePointsBuilder::new("academic_papers")
        .points(PointsSelector { points_selector_one_of: Some(
            PointsSelectorOneOf::Filter(
                Filter::must([Condition::matches("document_id", doi.to_string())])
            )
        )})
        .wait(true)
).await?;
```

### Throughput and bottleneck analysis

| Pipeline Stage | Throughput (CPU, no GPU) |
|---|---|
| PDF parsing + chunking | ~50–200 docs/sec |
| fastembed-rs embedding (nomic, 768d) | **~50–200 chunks/sec** (bottleneck) |
| Qdrant upsert (indexing disabled) | ~5,000–20,000 points/sec |

**Embedding is the bottleneck** by 10–100× over Qdrant insertion. Use a producer-consumer pipeline with `tokio::sync::mpsc` channels and 2–4 `spawn_blocking` embedding workers to parallelize ONNX inference. Expect **1–2M chunks/hour** on an 8-core workstation with parallel embedding workers, meaning **1B chunks takes 20–40 days** of continuous processing on a single machine.

Disable HNSW during bulk load (`m=0`), then re-enable afterward. HNSW construction at 1B scale takes additional hours to days. Increase `wal_capacity_mb` to 64–128 for bulk workloads and set `wal_segments_ahead: 1` for pre-allocation.

---

## 6. Integration architecture: Qdrant + DuckDB split

### What lives where

| Data | Qdrant | DuckDB |
|---|---|---|
| Embedding vectors | ✅ Primary | ❌ |
| Filter fields (year, source, authors, topics, journal) | ✅ Indexed payload | ✅ Full columns |
| title, chunk snippet (~200 chars) | ✅ Stored payload | ✅ |
| Full chunk markdown, abstract, citation graph | ❌ | ✅ |
| Ingestion metadata, audit trail | ❌ | ✅ Source of truth |

**Store enough metadata in Qdrant to render search results without a DuckDB round-trip** (title + snippet + filter fields), but keep Qdrant lean by excluding full text and abstracts. With `on_disk_payload: true`, payload data lives on NVMe — only indexed field values consume RAM. At 1B points with ~500–1,000 bytes of payload each, that's 500 GB–1 TB on disk, which is manageable.

**DuckDB is the system of record.** Generate the deterministic UUID once, write to DuckDB first (source of truth), then upsert to Qdrant. If Qdrant and DuckDB drift, reconcile by re-ingesting from DuckDB data.

### Search flow

```
Query text → fastembed-rs embed (1-10ms)
  → Qdrant ANN + payload filter (5-50ms at 10-100M scale)
  → Return top-K with title, snippet, score, document_id
  → [Optional] DuckDB fetch full text for expanded results (1-5ms)
```

Total interactive latency: **~10–70ms at 10–100M vectors** — well within interactive budgets. Use Qdrant's built-in filtering for 90% of queries; fall back to DuckDB for complex SQL joins, citation graph traversal, or full-text search via DuckDB's FTS extension.

---

## 7. Running Qdrant locally: operational guide

### Deployment choice

**On Linux:** Docker or native binary with systemd. Docker is simplest; native binary avoids container overhead.

**On macOS:** Use the **native binary** — Docker Desktop's VirtioFS layer adds significant disk I/O latency for memmap workloads. macOS also lacks io_uring (no `async_scorer: true`), transparent huge pages, and fine-grained page cache tuning. For serious billion-scale work, **Linux is strongly recommended**.

```yaml
# Qdrant config.yaml for Home Still
storage:
  storage_path: /mnt/nvme/qdrant/storage
  on_disk_payload: true
  performance:
    max_search_threads: 0       # Auto
    optimizer_cpu_budget: 1     # Reserve most CPU for search
  wal:
    wal_capacity_mb: 64
  hnsw_index:
    m: 16
    ef_construct: 200
    on_disk: true
service:
  host: 127.0.0.1
  http_port: 6333
  grpc_port: 6334
```

### Monitoring without Prometheus

Qdrant exposes `/healthz`, `/telemetry` (rich JSON with per-collection stats, optimizer status, SIMD capabilities), and `/collections/{name}` (vector count, indexed count, segment info). A simple `curl http://localhost:6333/telemetry?details_level=3` gives everything needed for local monitoring.

### Backup

Create snapshots via `POST /collections/academic_papers/snapshots` — these are consistent point-in-time captures that work while Qdrant is running and include HNSW indexes (so restoration doesn't require a rebuild). Full-storage snapshots via `POST /snapshots` back up all collections at once.

---

## 8. LanceDB may be a better fit for laptop-scale billions

### The honest comparison

The most important finding from this research is that **Qdrant's HNSW architecture is fundamentally memory-hungry at billion scale**, while **LanceDB's IVF-PQ architecture is designed for disk-first operation with minimal RAM**.

| Factor | Qdrant | LanceDB |
|---|---|---|
| RAM at 1B × 768d | 100–715 GB (depending on config) | **2–8 GB** (only centroids + codebooks) |
| Disk at 1B × 768d | ~4 TB (raw + SQ + HNSW + payload) | **~96 GB** (IVF_PQ compressed) |
| Deployment | Separate server process (gRPC) | **Embedded, in-process** (`cargo add lancedb`) |
| Language | Rust server, Rust gRPC client | **Native Rust library** |
| Query latency (1B, NVMe) | 100–500ms (needs 128+ GB RAM) | 10–50ms (with 4 GB RAM) |
| Quantization | SQ, BQ, PQ, asymmetric | PQ, SQ, **RabitQ** (~1 bit/dim) |
| Hybrid search | Native sparse vectors + RRF | FTS (Tantivy) + vector + SQL |
| Maturity | High, stable APIs | Medium — **Rust API explicitly "not stable yet"** |
| Filtering | Excellent (indexed HNSW filtering) | Good (BTree, Bitmap, SQL WHERE) |

LanceDB's IVF_PQ compresses 768-dim vectors from 3,072 bytes to ~96 bytes each — **1B vectors fit in ~96 GB on disk** and need only a few GB of RAM for centroids. With RabitQ (IVF_RQ), compression reaches ~1 bit/dim, fitting 1B vectors in ~20–50 GB on disk. This is a fundamentally different operating point that actually fits on a laptop.

The tradeoffs are real: LanceDB's Rust API is unstable (expect breaking changes between versions), its community is younger, and IVF-based search is slightly less accurate than HNSW at equivalent speed. But for a personal academic search tool, **10–30ms at 95%+ recall is perfectly acceptable**, and the ability to run entirely in-process with 4 GB RAM versus needing a separate server with 128+ GB RAM is decisive.

### Practical recommendation

- **At 10–200M vectors with 64 GB RAM:** Qdrant works well. Use SQ in RAM, vectors + HNSW on disk, NVMe. Expect 5–80ms queries.
- **At 200M–1B vectors on a laptop (≤64 GB RAM):** LanceDB is the better choice. Qdrant will degrade to multi-second queries.
- **At 1B vectors on a workstation (128+ GB RAM, 4 TB NVMe, Linux):** Qdrant is viable but operating at its limits. LanceDB remains more comfortable.

If you choose Qdrant, **plan for a phased approach**: start at 10–100M vectors where it excels, and only scale to 1B if hardware permits. The architecture described in this report (SQ quantization, on-disk everything, Matryoshka multi-stage search, hybrid dense+sparse) is the optimal Qdrant configuration for maximizing scale on constrained hardware.

---

## Conclusion

Building Home Still on Qdrant is fully viable up to ~200M vectors on a 64 GB laptop with NVMe, delivering 5–80ms query latency with scalar-quantized vectors in RAM and on-disk HNSW. The Rust client (v1.16, gRPC-only) offers clean async APIs with builder patterns, native hybrid search via prefetch+RRF fusion, and idempotent upserts that simplify deduplication. The embedding step at ~100–200 chunks/sec on CPU is the true ingestion bottleneck — not Qdrant.

At true billion scale, however, the HNSW graph structure demands RAM that consumer hardware cannot provide. **LanceDB deserves serious consideration as the primary vector store** for this use case: its IVF-PQ index operates natively from disk with minimal RAM, it embeds directly in the Rust process, and it has proven at billion-scale workloads. The practical path is to use Qdrant for the initial 10–200M vectors with the configuration patterns documented here, then evaluate migration to LanceDB (or a Qdrant workstation deployment with 128+ GB RAM) when scaling beyond that threshold. Either way, keep DuckDB as the metadata source of truth and fastembed-rs as the local embedding engine — that architectural layer is independent of vector store choice.