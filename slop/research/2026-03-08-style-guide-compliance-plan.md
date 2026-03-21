# Plan: Full Home Still Style Guide Compliance for paper-fetch

## Context

paper-fetch needs full compliance with the [Home Still Style Guide](https://github.com/home-still/.github/blob/main/2026-03-08-home-still-standards-style-guide.md). The `hs-style` repo exists at `github.com/home-still/hs-style` (empty). We'll develop both locally side-by-side — hs-style cloned as a sibling, referenced via path dep — then add as a git submodule once stable.

## Development Setup

```
~/paper-fetch/          ← this repo
~/paper-fetch/hs-style/ ← git clone of home-still/hs-style, workspace member + path dep
```

We'll add `hs-style/` to `.gitignore` during development. When ready, convert to a proper git submodule.

---

## Phase 1: Directory Restructure (mechanical moves)

Restructure to match the style guide's required layout (section 1.2):

```
paper-fetch/
├── crates/
│   ├── paper-fetch-core/   ← moved from paper-fetch-core/
│   └── paper-fetch/        ← moved from api/, package renamed
├── hs-style/               ← cloned locally
├── config/
│   └── default.yaml        ← new
├── CHANGELOG.md             ← new
└── Cargo.toml               ← updated
```

**Changes:**
- `git mv paper-fetch-core/ crates/paper-fetch-core/`
- `git mv api/ crates/paper-fetch/`
- Rename package in `crates/paper-fetch/Cargo.toml`: `name = "paper-fetch"` (was `paper-fetch-cli`)
- Update path dep: `paper-fetch-core = { path = "../paper-fetch-core" }`
- Create `config/default.yaml` (copy from existing config structure)
- Create empty `CHANGELOG.md`

---

## Phase 2: Workspace Cargo.toml (section 2.1)

Update root `Cargo.toml` to virtual manifest with `workspace.dependencies`:

```toml
[workspace]
resolver = "2"
members = ["crates/*", "hs-style"]

[workspace.dependencies]
tokio        = { version = "1",   features = ["full"] }
clap         = { version = "4.5", features = ["derive", "env", "wrap_help"] }
serde        = { version = "1",   features = ["derive"] }
serde_json   = "1"
anyhow       = "1"
thiserror    = "2"
tracing      = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
figment      = { version = "0.10", features = ["yaml", "env"] }
reqwest      = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
hs-style     = { path = "hs-style" }

[profile.release]
opt-level     = 3
lto           = "fat"
codegen-units = 1
panic         = "abort"
strip         = "symbols"
```

Update both crate `Cargo.toml` files to use `{ workspace = true }` for all shared deps.

---

## Phase 3: Create `hs-style` Crate

Clone the empty repo into `hs-style/` and populate:

```
hs-style/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── mode.rs           # OutputMode + detect() — no external deps
    ├── reporter.rs       # Reporter + StageHandle traits, SilentReporter — no external deps
    ├── global_args.rs    # GlobalArgs + ColorChoice + OutputFormat — [cli]
    ├── styles.rs         # Styles struct — [cli]
    ├── tty_reporter.rs   # TtyReporter (indicatif + owo-colors) — [cli]
    ├── pipe_reporter.rs  # PipeReporter — [cli]
    └── exit_codes.rs     # Standard exit code constants — no external deps
```

### `Cargo.toml`
```toml
[package]
name = "hs-style"
version = "0.1.0"
edition = "2021"

[features]
default = []
cli = ["dep:indicatif", "dep:owo-colors", "dep:supports-color", "dep:clap"]
k8s = ["dep:serde_json", "dep:tracing", "dep:tracing-subscriber"]

[dependencies]
indicatif = { version = "0.17", optional = true }
owo-colors = { version = "4", optional = true }
supports-color = { version = "3", optional = true }
clap = { version = "4.5", features = ["derive"], optional = true }
serde_json = { version = "1", optional = true }
tracing = { version = "0.1", optional = true }
tracing-subscriber = { version = "0.3", features = ["json"], optional = true }
```

### `mode.rs` — Output mode detection (style guide section 4.2)
```rust
pub enum OutputMode { Rich, Plain, Pipe, Json, Headless }

pub fn detect(color: &ColorChoice, output_format: &OutputFormat) -> OutputMode
```
Full precedence: `--output json` → `KUBERNETES_SERVICE_HOST`/`CI` → `--color never` → `--color always` → stderr not TTY → `NO_COLOR` → `TERM=dumb` → Rich

### `reporter.rs` — Traits (style guide section 4.3)
```rust
pub trait Reporter: Send + Sync {
    fn status(&self, verb: &str, message: &str);
    fn warn(&self, message: &str);
    fn error(&self, message: &str);
    fn begin_stage(&self, name: &str, total: Option<u64>) -> Box<dyn StageHandle>;
    fn finish(&self, summary: &str);
}

pub trait StageHandle: Send + Sync {
    fn set_message(&self, msg: &str);
    fn set_length(&self, total: u64);  // spinner → bar transition
    fn inc(&self, delta: u64);
    fn finish_with_message(&self, msg: &str);
    fn finish_and_clear(&self);
}
```
Plus `SilentReporter` and `NoopStageHandle` (for `--quiet`).

### `global_args.rs` — Universal CLI flags (style guide section 3.3)
```rust
#[derive(clap::Args)]
pub struct GlobalArgs {
    #[arg(long, global = true, default_value = "auto")]
    pub color: ColorChoice,
    #[arg(long, short, global = true, default_value = "text")]
    pub output: OutputFormat,
    #[arg(long, short, global = true)]
    pub quiet: bool,
    #[arg(long, short, global = true)]
    pub verbose: bool,
    #[arg(long, global = true)]
    pub config_dir: Option<PathBuf>,
}

#[derive(clap::ValueEnum)]
pub enum ColorChoice { Auto, Always, Never }

#[derive(clap::ValueEnum)]
pub enum OutputFormat { Text, Json, Ndjson }
```

### `styles.rs` — Color palette (style guide section 4.5)

| Field | Color | Usage |
|-------|-------|-------|
| `verb` | green bold | "Fetching", "Done" |
| `title` | bold | Paper titles |
| `doi` | cyan underline | DOIs |
| `url` | cyan | URLs |
| `date` | dimmed | Timestamps |
| `label` | white | "Authors:", "Source:" |
| `warning` | yellow | Warning prefix |
| `error_style` | red bold | Error prefix |
| `success` | green | Completion messages |

`Styles::colored()` and `Styles::plain()` (all `Style::default()`).

### `tty_reporter.rs` — Indicatif-based (style guide section 4.4)
- Wraps `MultiProgress`
- `begin_stage(_, None)` → spinner, `enable_steady_tick(120ms)`
- `begin_stage(_, Some(n))` → bounded bar with `{bar:30}` template
- `status`/`warn` via `mp.println()` with owo-colors
- `error` via `eprintln!` with red bold
- `finish` → green bold "Done" prefix
- Spinner→bar transition in `IndicatifStageHandle::set_length()`
- Constructor takes `use_color: bool` (Plain mode = bars without color)

### `pipe_reporter.rs` — Plain stderr, no progress
- `eprintln!` for status/warn/error
- Noop stage handles

### `exit_codes.rs` — Standard codes (style guide section 5.2)
```rust
pub const SUCCESS: u8 = 0;
pub const GENERAL_ERROR: u8 = 1;
pub const USAGE_ERROR: u8 = 2;
pub const NETWORK_ERROR: u8 = 3;
pub const PARTIAL_SUCCESS: u8 = 4;
```

### `lib.rs` — Feature-gated re-exports
Unconditional: `mode`, `reporter`, `exit_codes`
Behind `cli`: `global_args`, `styles`, `tty_reporter`, `pipe_reporter`

---

## Phase 4: Update `paper-fetch-core`

**File: `crates/paper-fetch-core/Cargo.toml`**
- Switch all deps to `{ workspace = true }`
- Add `figment = { workspace = true }` and `tracing = { workspace = true }`
- Remove version-pinned deps

**File: `crates/paper-fetch-core/src/config.rs`**
- Replace custom `Config::load()` with figment-based loading (style guide section 6.1):
  ```
  compiled defaults → /etc/home-still/config.yaml → ~/.home-still/config.yaml
  → ~/.home-still/paper-fetch/config.yaml → HOME_STILL_* env vars → CLI flags
  ```
- Accept optional `config_dir: Option<PathBuf>` param (from `--config-dir`)
- Support `HOME_STILL_CONFIG_DIR` env var

**File: `crates/paper-fetch-core/src/services/download.rs`**
- Simplify callback: replace `ProgressEvent`/`ProgressCallback` with:
  ```rust
  pub enum DownloadEvent {
      Started { index: usize, total: usize, title: String },
      Completed { index: usize, total: usize, size_bytes: u64 },
      Failed { index: usize, total: usize, title: String, error: String },
  }
  pub type OnProgress = Arc<dyn Fn(DownloadEvent) + Send + Sync>;
  ```
- Core stays free of hs-style deps

**File: `crates/paper-fetch-core/src/error.rs`**
- Map errors to hs-style exit codes (keep `thiserror` as-is, add a method or `From` impl)

---

## Phase 5: Update `paper-fetch` Binary Crate

**File: `crates/paper-fetch/Cargo.toml`**
```toml
[package]
name = "paper-fetch"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "paper-fetch"
path = "src/main.rs"

[features]
default = ["cli"]
cli = ["hs-style/cli"]
k8s = ["hs-style/k8s"]
lib = []   # exposes Commands + run() for hs unified CLI

[dependencies]
paper-fetch-core = { path = "../paper-fetch-core" }
hs-style         = { workspace = true, features = ["cli"] }
clap             = { workspace = true }
tokio            = { workspace = true }
anyhow           = { workspace = true }
serde_json       = { workspace = true }
tracing          = { workspace = true }
tracing-subscriber = { workspace = true }
mimalloc         = "0.1"
```

**File: `crates/paper-fetch/src/main.rs`**
- Add mimalloc global allocator
- Detect `OutputMode` via `hs_style::mode::detect()`
- Initialize tracing subscriber based on mode (fmt for TTY, json for Headless)
- Construct reporter: `SilentReporter` | `TtyReporter` | `PipeReporter`
- Construct `Styles` based on mode
- Call `owo_colors::set_override(false)` for non-Rich modes
- Pass `Arc<dyn Reporter>` + `&Styles` into commands
- Error display via `reporter.error()`
- Exit codes via `hs_style::exit_codes`

**File: `crates/paper-fetch/src/cli.rs`**
- Remove local `GlobalOpts` — flatten `hs_style::global_args::GlobalArgs` into `Cli`
- Remove `--no-color`, `--json` (replaced by `--color` and `--output json` from GlobalArgs)
- Keep tool-specific subcommand structs

**File: `crates/paper-fetch/src/output.rs`**
- Import `hs_style::styles::Styles`
- Update all print functions to accept `&Styles` and apply `.style()` calls
- For `--output json`: use `print_json()` as-is (data on stdout)
- For `--output ndjson`: future — emit NDJSON events (stub for now, full impl later)

**File: `crates/paper-fetch/src/commands/paper.rs`**
- Accept `reporter: Arc<dyn Reporter>`, `styles: &Styles`
- Search: spinner → colored results
- Get: spinner → colored paper detail
- Download (single): spinner → finish summary
- Download (batch): spinner for search phase, bounded progress bar for download phase
- Bridge `DownloadEvent` callback → reporter calls
- All `eprintln!` → `reporter.status()` / `reporter.warn()` / `tracing::debug!()`

**File: `crates/paper-fetch/src/exit_codes.rs`**
- Use constants from `hs_style::exit_codes`
- Update error-to-exit-code mapping to style guide's 0-4 codes

---

## Phase 6: Expose `lib` Feature (style guide section 8)

In `crates/paper-fetch/src/lib.rs` (new file, behind `lib` feature):
```rust
pub use cli::PaperAction as Commands;
pub async fn run(action: Commands, reporter: Arc<dyn Reporter>, styles: &Styles) -> Result<()> { ... }
```

This enables the future `hs` unified CLI to `use paper_fetch::Commands`.

---

## Summary of All Files

| Location | Action | Notes |
|----------|--------|-------|
| `Cargo.toml` | Rewrite | workspace members, workspace.dependencies, release profile |
| `crates/paper-fetch-core/Cargo.toml` | Update | workspace deps, add figment + tracing |
| `crates/paper-fetch-core/src/config.rs` | Rewrite | figment-based config loading |
| `crates/paper-fetch-core/src/services/download.rs` | Update | simplify callback types |
| `crates/paper-fetch/Cargo.toml` | Rewrite | rename, features, add hs-style + mimalloc + tracing |
| `crates/paper-fetch/src/main.rs` | Rewrite | mimalloc, mode detection, tracing, reporter |
| `crates/paper-fetch/src/cli.rs` | Update | flatten GlobalArgs, remove --no-color/--json |
| `crates/paper-fetch/src/output.rs` | Update | Styles param, colored output |
| `crates/paper-fetch/src/commands/paper.rs` | Update | reporter + styles, spinners, progress bars |
| `crates/paper-fetch/src/exit_codes.rs` | Update | use hs-style constants |
| `crates/paper-fetch/src/lib.rs` | New | lib feature for hs integration |
| `hs-style/Cargo.toml` | New | feature-gated deps |
| `hs-style/src/lib.rs` | New | re-exports |
| `hs-style/src/mode.rs` | New | OutputMode + detect() |
| `hs-style/src/reporter.rs` | New | Reporter + StageHandle traits, SilentReporter |
| `hs-style/src/global_args.rs` | New | GlobalArgs, ColorChoice, OutputFormat |
| `hs-style/src/styles.rs` | New | Styles stylesheet |
| `hs-style/src/tty_reporter.rs` | New | TtyReporter (indicatif + owo-colors) |
| `hs-style/src/pipe_reporter.rs` | New | PipeReporter |
| `hs-style/src/exit_codes.rs` | New | standard exit code constants |
| `config/default.yaml` | New | documented defaults |
| `CHANGELOG.md` | New | empty |

---

## Verification

1. `cargo build` — full workspace compiles
2. `cargo test --all` — all tests pass
3. `paper-fetch paper search "transformers"` — colored output + search spinner
4. `paper-fetch paper search "transformers" --color never` — no ANSI codes
5. `paper-fetch paper search "transformers" | cat` — pipe mode, no spinner/color
6. `NO_COLOR=1 paper-fetch paper search "transformers"` — respects NO_COLOR
7. `paper-fetch paper download "attention" -n 3` — progress bar
8. `paper-fetch paper download "attention" -n 3 -q` — silent
9. `paper-fetch paper search "transformers" -o json` — JSON on stdout, no progress artifacts
10. `paper-fetch --help` — shows all global flags, exits 0
11. `paper-fetch --version` — shows semver, exits 0
