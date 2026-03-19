# Walkthrough 5: qdrant-sink — Bulk Upsert to Qdrant

## Goal

Upsert embedded chunks into Qdrant with deterministic point IDs (idempotent
re-runs) and HNSW disabled during bulk load for speed.

**Acceptance criteria:** `cargo test -p qdrant-sink` — 3 tests pass (no Qdrant
server needed for deterministic ID tests).

---

## Workspace changes

Add to `[workspace.dependencies]` in root `Cargo.toml`:

```toml
qdrant-client = "1"
xxhash-rust = { version = "0.8", features = ["xxh3"] }
uuid = { version = "1", features = ["v5"] }
```

---

## Files to create

```
crates/qdrant-sink/
├── Cargo.toml
└── src/
    └── lib.rs
```

---

## Step 1: `crates/qdrant-sink/Cargo.toml`

```toml
[package]
name = "qdrant-sink"
version = "0.1.0"
edition = "2021"

[dependencies]
distill-core = { path = "../distill-core" }
qdrant-client = { workspace = true }
xxhash-rust = { workspace = true }
uuid = { workspace = true }
anyhow = { workspace = true }
tokio = { workspace = true }
serde_json = { workspace = true }
tracing = { workspace = true }
```

---

## Step 2: `src/lib.rs` — QdrantSink

### Namespace UUID constant

```rust
const NAMESPACE_UUID: Uuid = Uuid::from_bytes([
    0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1,
    0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
]);
```

### Struct

```rust
pub struct QdrantSink {
    client: Qdrant,
    collection_name: String,
    batch_size: usize,
}
```

### TODO: Implement these methods

1. **`async fn new(url, collection_name, batch_size) -> Result<Self>`**
   - `Qdrant::from_url(url).build()?`

2. **`async fn create_collection(&self, vector_size: u64) -> Result<()>`**
   - Check if collection exists first
   - Create with HNSW disabled (`m=0, ef_construct=4`) for fast bulk load
   - Enable scalar quantization via `ScalarQuantizationBuilder::default()`
   - Use `VectorParamsBuilder::new(vector_size, Distance::Cosine)`

3. **`async fn enable_hnsw(&self, m: u64, ef_construct: u64) -> Result<()>`**
   - Call after bulk load completes
   - `UpdateCollectionBuilder` with `HnswConfigDiffBuilder`
   - Typical values: `m=8, ef_construct=200`

4. **`async fn upsert(&self, chunks: Vec<EmbeddedChunk>) -> Result<usize>`**
   - Process in batches of `self.batch_size`
   - For each chunk: generate deterministic ID, build payload, create `PointStruct`
   - Use `UpsertPointsBuilder::new(...).wait(false)` for async upsert

5. **`fn deterministic_id(doc_id: &str, chunk_index: u32) -> String`** (free function)
   - Hash `"{doc_id}:{chunk_index}"` with `xxh3_64`
   - Feed hash bytes into `Uuid::new_v5(&NAMESPACE_UUID, &hash.to_le_bytes())`
   - Return UUID string

6. **`fn build_payload(ec: &EmbeddedChunk) -> HashMap<String, Value>`** (free function)
   - Map chunk fields to `qdrant_client::qdrant::Value`
   - Include: paper_id, chunk_index, chunk_type, doi, title, authors (top 3 joined),
     year, source, cited_by_count, oa_status, language, abstract_preview (first 200 chars)

### Key patterns

**Qdrant builder pattern:**
```rust
CreateCollectionBuilder::new(&self.collection_name)
    .vectors_config(VectorParamsBuilder::new(vector_size, Distance::Cosine))
    .hnsw_config(HnswConfigDiffBuilder::default().m(0).ef_construct(4))
    .quantization_config(ScalarQuantizationBuilder::default())
```

**Deterministic IDs:** Same doc_id + chunk_index always produces the same UUID,
so re-running the pipeline overwrites existing points instead of duplicating.

### Dragon

`qdrant_client::qdrant::Value` — use `Value::from()` for string/i64 conversion.
Don't try to use serde serialization.

### Dragon

`UpsertPointsBuilder::new(...).wait(false)` — don't block waiting for Qdrant to
index. The pipeline pushes data faster than Qdrant indexes, so waiting would be
a bottleneck.

---

## Tests to write (3 tests)

1. **`test_deterministic_id_stable`** — same inputs produce same UUID
2. **`test_deterministic_id_varies`** — different chunk_index or doc_id → different UUID
3. **`test_deterministic_id_is_uuid`** — output parses as valid UUID

These tests don't need a running Qdrant server.

---

## Verify

```bash
cargo test -p qdrant-sink
```

Expected: 3 tests pass.
