# Walkthrough 2: openalex-ingest — Parse Real Data

## Goal

Parse OpenAlex JSONL snapshots into `AcademicDocument`s. This is where you first
touch the 3.4TB dataset. The core insight is reconstructing abstracts from
OpenAlex's inverted index format.

**Acceptance criteria:** `cargo test -p openalex-ingest` — 8 tests pass.

---

## Workspace changes

Add to `[workspace.dependencies]` in root `Cargo.toml`:

```toml
rayon = "1.10"
tracing = "0.1"
```

---

## Files to create

```
crates/openalex-ingest/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── model.rs
    ├── parser.rs
    └── reader.rs
```

---

## Step 1: `crates/openalex-ingest/Cargo.toml`

```toml
[package]
name = "openalex-ingest"
version = "0.1.0"
edition = "2021"

[dependencies]
distill-core = { path = "../distill-core" }
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
rayon = { workspace = true }
tracing = { workspace = true }
flate2 = "1"

[dev-dependencies]
tempfile = "3"
```

---

## Step 2: `src/lib.rs`

```rust
mod model;
mod parser;
mod reader;

pub use model::OpenAlexWork;
pub use parser::reconstruct_abstract;
pub use reader::{read_partition, read_all_partitions, PartitionStats};
```

---

## Step 3: `src/model.rs` — Serde structs for OpenAlex JSON

OpenAlex works have deeply nested JSON. We only deserialize the fields we need.

```rust
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAlexWork {
    pub id: String,
    pub doi: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    pub publication_year: Option<u32>,
    pub language: Option<String>,
    #[serde(rename = "type")]
    pub work_type: Option<String>,
    pub cited_by_count: Option<u32>,
    pub is_retracted: Option<bool>,
    pub abstract_inverted_index: Option<HashMap<String, Vec<u32>>>,
    #[serde(default)]
    pub authorships: Vec<Authorship>,
    pub primary_location: Option<Location>,
    #[serde(default)]
    pub topics: Vec<Topic>,
    pub open_access: Option<OpenAccess>,
}
```

**TODO:** Implement `OpenAlexWork` plus 5 nested structs: `Authorship`,
`AuthorInfo`, `Location`, `SourceInfo`, `Topic`, `OpenAccess`.

**Key patterns:**
- `#[serde(rename = "type")]` — `type` is a Rust reserved word, so rename the
  field to `work_type`
- `#[serde(default)]` on `Vec` fields — OpenAlex may omit empty arrays entirely;
  `default` gives you `vec![]` instead of a parse error
- Only deserialize fields you need — serde ignores unknown fields by default

### Dragon

`display_name` exists on OpenAlex works but is different from `title`. Use
`title` for the document title.

---

## Step 4: `src/parser.rs` — Abstract reconstruction + conversion

This is the most interesting module. OpenAlex stores abstracts as an
**inverted index**: `{"word": [position1, position2], ...}`. You need to invert
it back into text.

### The algorithm

1. Find `max_position` across all position lists
2. Create a `Vec<Option<&str>>` of size `max_position + 1`
3. For each `(word, positions)`, place `word` at each position
4. Join all `Some` values with spaces

```rust
pub fn reconstruct_abstract(inverted_index: Option<&HashMap<String, Vec<u32>>>) -> Option<String> {
    let index = inverted_index?;
    if index.is_empty() {
        return None;
    }

    let max_position = index
        .values()
        .flat_map(|positions| positions.iter())
        .max()
        .copied()? as usize;

    let mut words: Vec<Option<&str>> = vec![None; max_position + 1];

    for (word, positions) in index {
        for &pos in positions {
            if let Some(slot) = words.get_mut(pos as usize) {
                *slot = Some(word.as_str());
            }
        }
    }

    let text: String = words.into_iter().flatten().collect::<Vec<&str>>().join(" ");

    if text.trim().is_empty() { None } else { Some(text) }
}
```

**TODO:**
1. Implement `reconstruct_abstract`
2. Implement `to_document(work: OpenAlexWork) -> Option<AcademicDocument>` that:
   - Skips retracted papers (`is_retracted == true`)
   - Skips papers with no title AND no abstract
   - Strips `https://openalex.org/` prefix from IDs
   - Strips `https://doi.org/` prefix from DOIs
   - Takes top 10 authors, top 3 topics
   - Chains `primary_location.source.display_name` for source

**Key pattern — Option chaining:**
```rust
let source = work
    .primary_location
    .and_then(|loc| loc.source)
    .and_then(|s| s.display_name);
```

### Tests to write (7 unit tests)

1. `test_reconstruct_simple` — basic 4-word abstract
2. `test_reconstruct_repeated_words` — "the" appears at positions 0 and 4
3. `test_reconstruct_with_gaps` — positions with gaps (words at 0 and 3)
4. `test_reconstruct_empty` — `None` and empty `HashMap` both return `None`
5. `test_to_document_skips_retracted` — retracted work returns `None`
6. `test_to_document_skips_no_title_no_abstract` — neither title nor abstract
7. `test_to_document_strips_prefixes` — verify ID/DOI prefix stripping

### Dragon

OpenAlex IDs look like `https://openalex.org/W2741809807`. Strip the prefix so
your internal ID is just `W2741809807`. Same for DOIs: `https://doi.org/10.1234/x`
becomes `10.1234/x`.

---

## Step 5: `src/reader.rs` — File I/O + parallel partitions

Reads JSONL files from disk. OpenAlex snapshots are organized as:
```
data/openalex/data/works/
├── updated_date=2024-01-01/
│   ├── part_0000.jsonl
│   └── part_0001.jsonl
├── updated_date=2024-01-02/
│   └── ...
```

**TODO:**
1. Implement `PartitionStats` with `AtomicU64` counters (total_records, parsed_ok,
   filtered_out, parse_errors)
2. Implement `read_jsonl_file(path, stats)` — reads one `.jsonl` or `.jsonl.gz`
   file line by line
3. Implement `read_partition(partition_dir)` — reads all JSONL files in a dir
4. Implement `read_all_partitions(works_dir, on_partition)` — uses `rayon::par_iter()`
   to process partitions in parallel

**Key patterns:**
- `BufReader::with_capacity(256 * 1024, reader)` — 256KB buffer for large files
- `flate2::read::GzDecoder` for gzip support
- `AtomicU64` with `Ordering::Relaxed` for lock-free concurrent counters
- `rayon::par_iter().for_each()` for parallel partition processing
- Callback pattern: `on_partition: F` where `F: Fn(Vec<AcademicDocument>, &str)`

### Integration test (1 test)

`test_read_partition` — creates a temp dir with a sample JSONL file containing one
good record and one retracted record. Asserts 1 doc returned, 2 total records,
1 filtered out. Verifies all fields on the parsed doc.

---

## Verify

```bash
cargo test -p openalex-ingest
```

Expected: 8 tests pass (7 parser + 1 reader).

---

## Reference

If you get stuck on the serde struct shapes, look at:
`/home/ladvien/research_hub_mcp/src/client/providers/openalex.rs`
- Lines 30-77: serde struct definitions
- Lines 233-267: abstract reconstruction algorithm
