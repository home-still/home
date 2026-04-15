IS## Home-Still CLI Standard

### Tools in scope
`paper-fetch`, `xycut-plus-plus`, `home-still` (embedding search), and the pipeline binaries (`hs-embed`, `hs-index`, `hs-gateway`, `hs-pdf-extract`).

---

### 1. Shared crate: `hs-style`

Lives in `crates/hs-style`. All tools depend on it. Gate heavy deps behind features:

```toml
[features]
default = []
cli   = ["dep:indicatif", "dep:owo-colors", "dep:supports-color"]
k8s   = ["dep:serde_json", "dep:tracing-subscriber"]
```

`paper-fetch` → `cli`. Pipeline binaries → `cli` + `k8s`.

---

### 2. Output mode detection (runtime, at startup)

```rust
pub enum OutputMode { Rich, Plain, Pipe, Json, Headless }

pub fn detect() -> OutputMode {
    if flag("--output json") || flag("--output ndjson") { return Json; }
    if env("KUBERNETES_SERVICE_HOST").is_ok() || env("CI").is_ok() { return Headless; }
    if !stderr().is_terminal() { return Pipe; }
    if env("NO_COLOR").is_ok() || env("TERM") == "dumb"  { return Plain; }
    Rich
}
```

**Headless** emits structured tracing JSON (for K8s/Loki). **Pipe** emits plain line-per-item text on stdout, no color, no progress. **Rich** gets indicatif + owo-colors.

---

### 3. `Reporter` trait — the one abstraction all code uses

```rust
pub trait Reporter: Send + Sync {
    fn status(&self, verb: &str, msg: &str);   // "Fetching" "arxiv..."
    fn warn(&self, msg: &str);
    fn error(&self, msg: &str);
    fn begin_stage(&self, name: &str, total: Option<u64>) -> Box<dyn StageHandle>;
    fn finish(&self, summary: &str);           // "Found 1,204 papers in 3.2s"
}
```

Business logic calls `reporter.begin_stage(...)` — never touches indicatif or tracing directly. The correct `Reporter` impl (`TtyReporter`, `PipeReporter`, `JsonReporter`, `SilentReporter`) is constructed once in `main()` and passed down.

---

### 4. Universal flags (every binary, every subcommand)

| Flag | Values | Default |
|------|--------|---------|
| `--color` | `auto\|always\|never` | `auto` |
| `--output` / `-o` | `text\|json\|ndjson` | `text` |
| `--quiet` / `-q` | boolean | false |
| `--verbose` / `-v` | boolean | false |
| `--config-dir` | path | `~/.home-still/` |

These live in a shared `GlobalArgs` struct in `hs-common` that every binary flattens into its clap `Cli`:

```rust
#[derive(clap::Args)]
pub struct GlobalArgs {
    #[arg(long, global = true, default_value = "auto")]
    pub color: ColorChoice,
    #[arg(long, short, global = true)]
    pub output: Option<OutputFormat>,
    #[arg(long, short, global = true)]
    pub quiet: bool,
    #[arg(long, short, global = true)]
    pub verbose: bool,
}
```

---

### 5. Config precedence (enforced via `figment`)

```
CLI flags > HOME_STILL_* env vars > ~/.home-still/<tool>/config.yaml
          > ~/.home-still/config.yaml > compiled defaults
```

This is already designed in your architecture doc. The only addition: `HOME_STILL_COLOR`, `HOME_STILL_OUTPUT`, `HOME_STILL_QUIET` must map to the global flags above.

---

### 6. stdout/stderr split

- **stdout**: results only — one item per line in text mode, NDJSON in json mode
- **stderr**: everything else — progress bars, status lines, warnings, errors, timing summaries

This makes every tool pipeable: `paper-fetch search "crispr" | jq .doi`

---

### 7. Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error (bad input, not found) |
| 2 | Config/usage error |
| 3 | Network/provider error (retriable) |
| 4 | Partial success (some providers failed) |

---

### 8. Subcommand naming convention

Pick the verb-noun pattern and stick to it across all tools:

```
paper-fetch search <query>
paper-fetch download <doi>
paper-fetch providers list
hs-embed run --input <file>
home-still index <path>
home-still search <query>
home-still serve
```

No `get`, `fetch`, `query` mixed with `search`. No `list-providers` vs `providers list` — pick one style per tool and document it.

---

### 9. Progress UX rules (from your research doc)

- Spinner first, transition to bounded bar when `total` is known (`pb.set_length()`)
- Always `finish_and_clear()` on success, then print a one-line summary to stderr
- Throttle: delay first update 500ms, then max 100ms between redraws
- Call `pb.reset_eta()` after any pause (rate limit, retry)
- In `Pipe`/`Headless` mode: emit `stage_start`/`stage_progress`/`stage_complete` NDJSON events instead

---

### 10. What goes in `hs-common` vs `hs-style`

| | `hs-common` | `hs-style` |
|--|-------------|------------|
| Contains | `GlobalArgs`, config loading, error types, `OutputMode` detection | `Reporter` trait + impls, `SourceStyles`, `StageHandle`, progress helpers |
| Dependencies | `clap`, `figment`, `thiserror` | `indicatif`, `owo-colors`, `serde_json` (feature-gated) |
| Used by | everything | everything with output |

---

The most important rule: **no binary touches indicatif, owo-colors, or serde_json directly. All output flows through a `Reporter`.** That single constraint is what makes the K8s headless mode work without code changes.
