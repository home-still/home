# Walkthrough 6: distill — Pipeline Orchestrator

## Goal

Connect all 5 crates into a 4-stage pipeline using threads and bounded channels.
This is the binary crate — the thing you actually run.

**Acceptance criteria:** `cargo check -p distill` passes.

---

## Workspace changes

Add to `[workspace.dependencies]` in root `Cargo.toml`:

```toml
crossbeam-channel = "0.5"
indicatif = "0.18"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

---

## Files to create

```
crates/distill/
├── Cargo.toml
└── src/
    └── main.rs
```

---

## Step 1: `crates/distill/Cargo.toml`

```toml
[package]
name = "distill"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "distill"
path = "src/main.rs"

[dependencies]
distill-core = { path = "../distill-core" }
openalex-ingest = { path = "../openalex-ingest" }
chunker = { path = "../chunker" }
embedder = { path = "../embedder" }
qdrant-sink = { path = "../qdrant-sink" }
clap = { workspace = true }
tokio = { workspace = true }
anyhow = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
crossbeam-channel = { workspace = true }
indicatif = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
```

---

## Step 2: `src/main.rs` — CLI + Pipeline

### CLI with clap derive

```rust
#[derive(Parser)]
#[command(name = "distill", about = "Academic paper ingestion pipeline")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Ingest OpenAlex snapshot into Qdrant
    Openalex {
        #[arg(long, default_value = "data/openalex/data/works")]
        data_dir: PathBuf,
        #[arg(long, default_value = "http://localhost:6334")]
        qdrant_url: String,
        #[arg(long, default_value = "academic_papers")]
        collection: String,
        #[arg(long)]
        model_path: String,
        #[arg(long, default_value = "nomic-ai/nomic-embed-text-v1.5")]
        tokenizer: String,
        #[arg(long, default_value = "64")]
        embed_batch_size: usize,
        #[arg(long, default_value = "50000")]
        channel_capacity: usize,
    },
    /// Show pipeline status
    Status,
}
```

### TODO: Implement `main()` and `run_openalex_pipeline()`

**`main()`:**
1. Init `tracing_subscriber` with env filter defaulting to `distill=info`
2. Parse CLI with `Cli::parse()`
3. Match on command, call `run_openalex_pipeline` for `Openalex`

**`run_openalex_pipeline()`:**

This is a 4-thread pipeline connected by bounded channels:

```
[Parser] --docs--> [Chunker] --chunks--> [Embedder] --embedded--> [Sink]
```

1. **Create channels:**
   ```rust
   let (doc_tx, doc_rx) = bounded::<Vec<AcademicDocument>>(capacity / 1000);
   let (chunk_tx, chunk_rx) = bounded::<Vec<Chunk>>(capacity / 100);
   let (embed_tx, embed_rx) = bounded::<Vec<EmbeddedChunk>>(capacity / 100);
   ```

2. **Create progress bars** with `indicatif::MultiProgress` + 4 `ProgressBar`s

3. **Stage 1 — Parser thread:**
   - Calls `openalex_ingest::read_all_partitions` with a callback that sends
     docs through `doc_tx`
   - Drops `doc_tx` when done (signals downstream)

4. **Stage 2 — Chunker thread:**
   - Creates `Chunker::from_pretrained`
   - Loops `while let Ok(docs) = doc_rx.recv()`, chunks each doc, sends batch
     through `chunk_tx`
   - Drops `chunk_tx` when done

5. **Stage 3 — Embedder thread:**
   - Creates `Embedder::new` with model path
   - Loops `while let Ok(chunks) = chunk_rx.recv()`, embeds batch, sends through
     `embed_tx`
   - Drops `embed_tx` when done

6. **Stage 4 — Sink thread:**
   - Creates its own `tokio::runtime::Builder::new_current_thread()` runtime
   - Creates `QdrantSink::new`, calls `create_collection`
   - Loops `while let Ok(embedded) = embed_rx.recv()`, calls `sink.upsert`

7. **Join all threads**, check for panics

### Key patterns

**Bounded channels for backpressure:**
```rust
crossbeam_channel::bounded(capacity)
```
When a channel is full, the sender blocks. This prevents the fast parser from
OOMing the system by buffering millions of docs.

**Graceful shutdown via channel drops:**
When a sender is dropped, the receiver's `recv()` returns `Err`, naturally ending
the loop. No explicit shutdown signal needed.

**Mixing sync + async:**
The pipeline is sync (threads), but Qdrant client is async (gRPC). The sink thread
creates its own single-threaded tokio runtime:
```rust
let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()?;
rt.block_on(async { ... });
```

**Progress bars:**
```rust
let multi = MultiProgress::new();
let style = ProgressStyle::with_template(
    "{prefix:>20.bold} {bar:40.cyan/blue} {pos}/{len} [{eta}] {msg}",
)?;
let pb = multi.add(ProgressBar::new(estimated_total));
pb.set_prefix("Parsing");
pb.set_style(style);
```

### Dragon

Channel capacities need tuning:
- Too small → GPU starved, low utilization
- Too large → OOM from buffered chunks
- Start with `capacity / 1000` for docs, `capacity / 100` for chunks/embeddings

### Dragon

The embedder thread owns a `&mut Session` (ORT requirement). The `Session` cannot
be shared across threads — it must live entirely in the embedder thread.

### Dragon

The sink thread needs its own tokio runtime because the main pipeline is
synchronous (thread-based). Don't try to use `#[tokio::main]` on the outer main
function — that would make the whole pipeline async, which doesn't work well with
the thread + channel architecture.

---

## Verify

```bash
cargo check -p distill
```

To actually run the pipeline:
```bash
cargo run -p distill -- openalex --model-path path/to/model.onnx
```
