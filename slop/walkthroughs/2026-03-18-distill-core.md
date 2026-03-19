# Walkthrough 1: distill-core — Shared Types

## Goal

Create the foundation crate that every pipeline crate depends on. It defines the
data types that flow through channels, the config loader, and the error enum.

**Acceptance criteria:** `cargo check -p distill-core` passes.

---

## Workspace changes

No new workspace deps needed — `serde`, `serde_json`, `thiserror`, and `figment`
are already in root `Cargo.toml`.

---

## Files to create

```
crates/distill-core/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── document.rs
    ├── chunk.rs
    ├── config.rs
    └── error.rs
```

---

## Step 1: `crates/distill-core/Cargo.toml`

```toml
[package]
name = "distill-core"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
figment = { workspace = true }
```

---

## Step 2: `src/lib.rs`

Re-export everything so downstream crates do `use distill_core::AcademicDocument`.

```rust
mod document;
mod chunk;
mod config;
mod error;

pub use document::AcademicDocument;
pub use chunk::{Chunk, ChunkType, EmbeddedChunk, QdrantPoint};
pub use config::PipelineConfig;
pub use error::PipelineError;
```

---

## Step 3: `src/document.rs` — AcademicDocument

This is the parsed representation of one OpenAlex work. Every field that might be
missing in the source data is `Option<T>`.

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcademicDocument {
    /// OpenAlex ID (e.g., "W2741809807")
    pub id: String,
    pub doi: Option<String>,
    pub title: Option<String>,
    pub abstract_text: Option<String>,
    pub publication_year: Option<u32>,
    /// Top 10 author display names
    pub authors: Vec<String>,
    /// Primary source/journal name
    pub source: Option<String>,
    pub cited_by_count: u32,
    /// Top 3 topic display names
    pub topics: Vec<String>,
    /// e.g., "green", "gold", "closed"
    pub oa_status: Option<String>,
    /// ISO 639-1 code
    pub language: Option<String>,
    /// e.g., "article", "dissertation", "book-chapter"
    pub doc_type: Option<String>,
}
```

**TODO:** Implement this struct with all 12 fields.

---

## Step 4: `src/chunk.rs` — Chunk types

These types represent data as it flows through the pipeline:
- `Chunk`: after splitting text (carries metadata forward for Qdrant payload)
- `EmbeddedChunk`: after embedding (chunk + vector)
- `QdrantPoint`: ready for upsert (point_id + vector + payload)

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkType {
    Abstract,
    Body,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub doc_id: String,
    pub chunk_index: u32,
    pub total_chunks: u32,
    pub chunk_type: ChunkType,
    pub text: String,
    // Carried metadata for Qdrant payload
    pub doi: Option<String>,
    pub title: Option<String>,
    pub authors: Vec<String>,
    pub publication_year: Option<u32>,
    pub source: Option<String>,
    pub cited_by_count: u32,
    pub topics: Vec<String>,
    pub oa_status: Option<String>,
    pub language: Option<String>,
    pub doc_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EmbeddedChunk {
    pub chunk: Chunk,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct QdrantPoint {
    pub point_id: u64,
    pub vector: Vec<f32>,
    pub payload: Chunk,
}
```

**TODO:** Implement `ChunkType`, `Chunk`, `EmbeddedChunk`, and `QdrantPoint`.

**Key pattern:** `Chunk` carries all the document metadata forward because the
chunker and embedder are separate pipeline stages. By the time we build the
Qdrant payload, we need the title, authors, etc. — so we copy them into every chunk.

---

## Step 5: `src/config.rs` — PipelineConfig

Uses `figment` to load config from YAML file then overlay env vars.

```rust
use figment::{Figment, providers::{Env, Format, Yaml}};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    pub openalex_data_dir: PathBuf,
    pub qdrant_url: String,
    pub collection_name: String,
    pub embed_batch_size: usize,
    pub upsert_batch_size: usize,
    pub channel_capacity: usize,
    pub checkpoint_path: PathBuf,
}
```

**TODO:**
1. Implement `Default` with sensible values (data dir: `data/openalex/data/works`,
   qdrant: `http://localhost:6334`, batch sizes: 64/2000, capacity: 50000)
2. Implement `PipelineConfig::load()` that uses:
   - `Yaml::file(~/.home-still/distill/config.yaml)` for file config
   - `Env::prefixed("DISTILL_")` for env overrides

### Dragon

`figment::providers::Format` must be imported as a trait for `Yaml::file()` to
compile. The `use` statement is:

```rust
use figment::{Figment, providers::{Env, Format, Yaml}};
```

If you write `use figment::providers::Yaml;` without `Format`, you'll get a
confusing "no method named `file`" error.

---

## Step 6: `src/error.rs` — PipelineError

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PipelineError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Embedding error: {0}")]
    Embedding(String),

    #[error("Qdrant error: {0}")]
    Qdrant(String),

    #[error("Tokenizer error: {0}")]
    Tokenizer(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Channel closed")]
    ChannelClosed,
}
```

**TODO:** Implement with 7 variants. Use `#[from]` for auto-conversion from
`std::io::Error` and `serde_json::Error`. Use `String` for the rest (library
errors that don't implement `std::error::Error` uniformly).

**Key pattern:** `thiserror` generates `impl Display` and `impl Error` from the
`#[error("...")]` attributes, and `#[from]` generates `impl From<T>` so you can
use `?` to propagate errors.

---

## Verify

```bash
cargo check -p distill-core
```
