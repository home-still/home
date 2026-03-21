# Designing a unified CLI style library in Rust

**A shared `hs-style` crate built on `indicatif` + `owo-colors` with a `Reporter` trait that dispatches between rich TTY, plain text, and structured JSON output is the production-proven architecture used by uv, cargo, and bat.** The key insight from these projects is that output mode detection happens at runtime via `std::io::IsTerminal` (Rust 1.70+), but heavy dependencies like `indicatif` are gated at compile time behind Cargo feature flags. This report covers the full stack: terminal detection, progress bars, color, machine-readable output, shared crate architecture, and concrete patterns for both a multi-source API aggregator (paper-fetch) and a multi-stage data pipeline (pdf-mash).

---

## Output mode detection determines everything downstream

The foundation of any CLI style system is detecting the output context. As of Rust 1.70, **`std::io::IsTerminal`** is in the standard library, making the `atty` crate (deprecated) and `is-terminal` crate (its successor) unnecessary for most projects. The detection hierarchy follows a strict precedence order:

```rust
use std::io::IsTerminal;

#[derive(Debug, Clone, Copy)]
pub enum OutputMode {
    Rich,   // Interactive TTY: colors, progress bars, spinners
    Plain,  // TTY but NO_COLOR or TERM=dumb: progress bars without color
    Pipe,   // Piped or redirected: line-by-line text, no formatting
    Json,   // Machine-readable: NDJSON events on stdout
}

pub fn detect_output_mode(json_flag: bool, color_flag: Option<&str>) -> OutputMode {
    if json_flag { return OutputMode::Json; }
    match color_flag {
        Some("never") => return OutputMode::Plain,
        Some("always") => return OutputMode::Rich,
        _ => {}
    }
    if !std::io::stderr().is_terminal() { return OutputMode::Pipe; }
    if std::env::var("NO_COLOR").map_or(false, |v| !v.is_empty()) {
        return OutputMode::Plain;
    }
    if std::env::var("TERM").map_or(false, |v| v == "dumb") {
        return OutputMode::Plain;
    }
    OutputMode::Rich
}
```

The **`NO_COLOR` environment variable** (no-color.org, proposed 2017) has become the de facto standard — respected by LLVM/Clang, ripgrep, cargo, CMake, and hundreds of other tools. Its companion, `FORCE_COLOR`, overrides detection to enable color even in pipes. The `supports-color` crate (v3.0, port of the npm package by Sindre Sorhus) goes further by detecting the **level** of color support: None, Basic (16 colors), 256 colors, or TrueColor (16M colors). It checks `NO_COLOR`, `FORCE_COLOR`, `TERM`, `COLORTERM`, and CI environment variables automatically.

**How the major Rust tools handle this:** ripgrep uses `--color auto|always|never` and `termcolor` for cross-platform output, automatically disabling formatting when `--json` or `--vimgrep` flags are present. uv checks `std::io::IsTerminal` and routes through a `Printer` abstraction where `--quiet` mode returns `ProgressDrawTarget::hidden()`. Cargo wraps everything in a `Shell` struct containing `ShellOut` (either a `StandardStream` for terminal or a `Box<dyn Write>` for tests/buffers), with a `Verbosity` enum of `Verbose | Normal | Quiet`.

---

## Indicatif is the progress bar standard, but throttling matters

**`indicatif` (v0.17+, now at 0.18.x, ~90M+ total downloads)** dominates the Rust progress bar ecosystem. The v0.17 rewrite brought a **~95x performance improvement** over v0.16 by replacing mutex-based position tracking with atomic integer APIs and consolidating rate limiting into `ProgressDrawTarget` at a default **20 Hz (50ms interval)**.

The `MultiProgress` type manages multiple concurrent bars safely across threads. It is `Clone + Send`, and in v0.17+ no longer requires a `join()` call. Each `ProgressBar` added via `mp.add()` renders in its own row, and `mp.println()` prints messages above all bars without visual corruption. Performance note: standalone `ProgressBar` benchmarks at ~104,000 `inc(1)` calls/sec, while `MultiProgress` drops to ~14,400 calls/sec — important for high-throughput pipelines.

**uv issue #10384** (opened January 2025, labeled `performance`) revealed a subtler bottleneck. Profiling uv's resolver showed significant CPU time in `indicatif::style::ProgressStyle::format_state` — the style formatting computation happens on every `inc()` call *before* the draw target's rate limiter even decides whether to render. The proposed solution: throttle at the application level to **at most 1/60s (~16.67ms)**, preventing wasted formatting work. This is critical for any tool making thousands of state updates per second.

The **spinner-to-bar transition** pattern is essential for APIs that return a total count only in the first response:

```rust
// Phase 1: Spinner while discovering total
let pb = ProgressBar::new_spinner();
pb.enable_steady_tick(Duration::from_millis(120));
pb.set_style(ProgressStyle::with_template("{spinner:.blue} {msg}").unwrap());
pb.set_message("Querying OpenAlex...");

// Phase 2: First response arrives with total_count = 24,700
pb.disable_steady_tick();
pb.set_length(24_700);
pb.set_style(ProgressStyle::with_template(
    "{prefix:<18!} [{bar:30.green}] {pos}/{len} ({per_sec}) ETA {eta}"
).unwrap().progress_chars("█▓░"));
pb.reset_eta(); // Critical: prevents stale ETA from spinner phase
```

The `enable_steady_tick(Duration)` method spawns a background thread that keeps spinners animated regardless of work speed — essential for network-bound operations where updates are irregular. Manual `tick()` calls have no effect while steady tick is active. **Use steady tick for spinners and slow operations; use manual updates for fast, item-driven progress** where each unit of work naturally triggers an increment.

---

## Machine-readable output: the stderr/stdout split and NDJSON

The fundamental Unix pattern — **human-readable progress on stderr, machine-readable results on stdout** — enables simultaneous human and machine consumption. When a tool is piped (`mytool search "crispr" | jq .`), stderr still shows progress on the terminal while stdout flows structured data through the pipe.

**NDJSON (Newline-Delimited JSON)** is the standard for streaming progress events. Each line is independently parseable, `grep`-able, and works with `jq` directly. ripgrep's `--json` flag outputs typed NDJSON with a discriminator field:

```json
{"type":"begin","data":{"path":{"text":"src/lib.rs"}}}
{"type":"match","data":{"path":{"text":"src/lib.rs"},"line_number":37,"lines":{"text":"    Output::default()\n"}}}
{"type":"summary","data":{"stats":{"matched_lines":1,"searches":5}}}
```

For agent-friendly CLIs, research from Google's Justin Poehnelt identifies key principles: `--output json` is table stakes; `--fields` lets agents limit response size to protect their context window; `--dry-run` enables validation before mutation. **LLM agents waste ~1/3 of their context tokens parsing verbose human-readable output**, making structured output essential.

A complete `--output-format` implementation with `clap`:

```rust
#[derive(Debug, Clone, clap::ValueEnum)]
pub enum OutputFormat { Text, Json, Ndjson }

#[derive(Serialize)]
#[serde(tag = "type")]
enum ProgressEvent {
    #[serde(rename = "search_start")]
    SearchStart { source: String, query: String },
    #[serde(rename = "search_progress")]
    SearchProgress { source: String, found: usize, total: Option<usize> },
    #[serde(rename = "result")]
    Result { source: String, title: String, doi: Option<String> },
    #[serde(rename = "search_complete")]
    SearchComplete { source: String, total_results: usize, elapsed_ms: u64 },
    #[serde(rename = "error")]
    Error { source: String, message: String },
}

fn emit_ndjson(event: &ProgressEvent) {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, event).unwrap();
    writeln!(stdout).unwrap();
    stdout.flush().unwrap(); // Critical for streaming consumers
}
```

The `tracing-ndjson` crate provides a tracing subscriber that outputs structured NDJSON logs, and `tracing-subscriber`'s built-in `.json().flatten_event(true)` mode produces Kubernetes-compatible structured logs — one JSON object per line with flat fields for easy parsing by Loki, ELK, or Datadog.

---

## owo-colors wins the color crate comparison

**`owo-colors` (v4.x) is the recommended color crate: zero dependencies, zero allocations, `no_std` compatible, with `#![forbid(unsafe_code)]`.** It provides the `OwoColorize` extension trait that works on *any* type implementing `Display`, not just strings — meaning `42.green().bold()` compiles and renders without allocating. Rain's authoritative Rust CLI Recommendations (sunshowers.io) explicitly recommends owo-colors as the only library meeting all criteria: actively maintained, simple API, minimal global state, zero allocations.

The **stylesheet pattern** is the production approach for library crates. Rather than sprinkling color calls throughout business logic, define a styles struct that is either activated or left as no-ops:

```rust
use owo_colors::{OwoColorize, Style};

#[derive(Default)]
pub struct SourceStyles {
    pub openalex: Style,
    pub crossref: Style,
    pub semantic_scholar: Style,
    pub europe_pmc: Style,
    pub core_ac: Style,
    pub unpaywall: Style,
    pub doi: Style,
}

impl SourceStyles {
    pub fn colorize(&mut self) {
        self.openalex = Style::new().blue().bold();
        self.crossref = Style::new().green().bold();
        self.semantic_scholar = Style::new().yellow().bold();
        self.europe_pmc = Style::new().magenta().bold();
        self.core_ac = Style::new().cyan().bold();
        self.unpaywall = Style::new().red().bold();
        self.doi = Style::new().cyan().underline();
    }
}

// Usage: println!("[{}] {}", "OpenAlex".style(styles.openalex), title);
// When styles aren't colorized, Style::default() is a no-op — zero overhead.
```

The optional `supports-colors` feature enables `if_supports_color()`, which checks `NO_COLOR`, `FORCE_COLOR`, TTY status, and CI detection. For global override from a `--color` flag: `owo_colors::set_override(true)` forces color, `set_override(false)` disables it, and no call leaves auto-detection active.

| Crate | Dependencies | Allocations | `no_std` | API style | NO_COLOR support |
|---|---|---|---|---|---|
| **owo-colors 4.x** | **0** | **Zero** | **Yes** | `.green().bold()` trait | Via `supports-colors` feature |
| colored | ~2 | Allocates `ColoredString` | No | `.green().bold()` trait | CLICOLOR/CLICOLOR_FORCE |
| yansi | **0** | **Zero** | Yes | `Paint::green()` or trait | `Paint::disable()` |
| nu-ansi-term | 0 | Allocates `ANSIString` (Cow) | No | `Color::Red.paint("text")` | Manual |
| console 0.16 | ~3 | Moderate | No | `style("text").green()` | Via `colors_enabled()` |
| termcolor | 1 (winapi-util) | Buffer-based | No | `WriteColor` trait | Via `ColorChoice` |

**Do not use `termcolor`** for new projects — it targets deprecated Windows Console APIs. Instead, use `owo-colors` with the `enable-ansi-support` crate called early in `main()` to enable ANSI sequences on Windows.

---

## The shared crate architecture: lessons from uv, cargo, and bat

A workspace-level shared style crate should follow this structure:

```
workspace/
├── Cargo.toml              # [workspace] with resolver = "2"
├── crates/
│   ├── hs-style/           # Shared output/progress/color crate
│   │   ├── Cargo.toml      # Feature flags: cli, k8s, agent
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── reporter.rs # Reporter trait + Tty/Plain/Json/Silent impls
│   │       ├── progress.rs # StageHandle, throttling
│   │       └── colors.rs   # SourceStyles stylesheet
│   ├── paper-fetch/        # Binary: depends on hs-style with features = ["cli"]
│   └── pdf-mash/           # Binary: depends on hs-style with features = ["cli", "k8s"]
```

The core abstraction is a **`Reporter` trait** with implementations for each output mode. Production projects use three patterns:

**Cargo's `Option<State>` pattern** provides near-zero-cost disabled progress. When progress is disabled (quiet mode, CI, dumb terminal), the `Progress` struct holds `state: None`, and every method like `tick()` immediately returns without touching the terminal. This is a runtime check but effectively free — a single branch prediction that is almost always the same path. Cargo also applies a **Throttle** that delays the first update by 500ms (preventing flicker for fast operations) and rate-limits subsequent updates to every 100ms.

**bat's trait-based dispatch** uses `Box<dyn Printer>` with `InteractivePrinter` (rich syntax-highlighted output) and `SimplePrinter` (plain cat-like pass-through). The controller checks `config.loop_through` (set to `true` when output is piped) and constructs the appropriate implementation.

**uv's reporter + draw target pattern** wraps `indicatif::MultiProgress` behind a `Printer` struct whose `target()` method returns either `ProgressDrawTarget::stderr()` for interactive mode or `ProgressDrawTarget::hidden()` for quiet/non-interactive mode. Domain-specific reporters (like `BinaryDownloadReporter`) receive the `Printer` and create their own progress bars.

The recommended `Reporter` trait:

```rust
pub trait Reporter: Send + Sync {
    fn status(&self, action: &str, message: &str);
    fn warn(&self, message: &str);
    fn error(&self, message: &str);
    fn begin_stage(&self, name: &str, total: Option<u64>) -> Box<dyn StageHandle>;
    fn finish(&self, message: &str);
}

pub trait StageHandle: Send {
    fn set_message(&self, msg: &str);
    fn set_length(&self, total: u64); // For spinner→bar transition
    fn inc(&self, delta: u64);
    fn finish_with_message(&self, msg: &str);
}
```

Feature flags in `hs-style/Cargo.toml` gate dependencies:

```toml
[features]
default = []
cli = ["dep:indicatif", "dep:owo-colors"]
k8s = ["dep:serde_json", "dep:tracing"]
```

With **`resolver = "2"`** in the workspace, features are resolved per the actual dependency graph — `paper-fetch` enabling `hs-style/cli` will not force `pdf-mash` to compile with `cli` unless it also requests that feature.

---

## paper-fetch: concurrent fan-out with per-source progress

The multi-source aggregator pattern requires `MultiProgress` with one bar per API source, running concurrent `tokio` tasks. Each source starts as a spinner, transitions to a bounded bar when the API's first response returns `total_count`, and handles rate limiting gracefully.

```rust
const SOURCES: &[(&str, &str)] = &[
    ("OpenAlex",          "green"),
    ("Crossref",          "cyan"),
    ("Semantic Scholar",  "yellow"),
    ("Europe PMC",        "magenta"),
    ("CORE",              "blue"),
    ("Unpaywall",         "red"),
];

async fn run_fetch(mp: &MultiProgress, query: &str) {
    let handles: Vec<_> = SOURCES.iter().map(|(name, color)| {
        let pb = mp.add(ProgressBar::new_spinner());
        pb.set_style(ProgressStyle::with_template(
            &format!("{{spinner:.{color}}} {{prefix:<18!}} {{msg}}")
        ).unwrap().tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓"));
        pb.set_prefix(*name);
        pb.set_message("connecting…");
        pb.enable_steady_tick(Duration::from_millis(100));
        let color = *color;
        tokio::spawn(async move { fetch_source(pb, color).await })
    }).collect();
    futures::future::join_all(handles).await;
}
```

**Rate limit backpressure** is displayed by switching the bar's message to a countdown:

```rust
async fn handle_rate_limit(pb: &ProgressBar, retry_after: u64) {
    let deadline = Instant::now() + Duration::from_secs(retry_after);
    while Instant::now() < deadline {
        let remaining = (deadline - Instant::now()).as_secs();
        pb.set_message(format!("⏳ rate limited — waiting {remaining}s"));
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    pb.set_message("resumed");
    pb.reset_eta(); // Reset ETA after pause to avoid skewed estimates
}
```

**Multi-phase transitions** (Fetch → Deduplicate → OA Enrichment) are handled by finishing and removing the fetch bars via `pb.finish_and_clear()` + `mp.remove(&pb)`, then adding new bars for subsequent phases. Each phase gets its own colored style and clear completion message. The `mp.println()` method prints phase headers above all active bars.

---

## pdf-mash: nested progress with GPU throughput

The data pipeline pattern uses **nested progress bars** — an outer stage bar tracking pipeline phases, with an inner batch bar for the current stage's work items. The `mp.insert_after(&stage_bar, batch_bar)` method positions the inner bar visually beneath the outer one.

For GPU inference throughput, indicatif's **`{per_sec}` template key** automatically computes items/sec from position increments over elapsed time. Custom batch-level throughput can be shown in the message field:

```rust
let style = ProgressStyle::with_template(
    "{prefix:>12.bold.green} [{bar:30}] {pos}/{len} ({per_sec}) ETA {eta} {msg}"
).unwrap().progress_chars("█▓░");

// After each ONNX batch:
let batch_throughput = batch_size as f64 / batch_elapsed.as_secs_f64();
pb.set_message(format!("{batch_throughput:.0} items/sec"));
pb.inc(batch_size);
```

**Kubernetes headless mode** is detected via the `KUBERNETES_SERVICE_HOST` environment variable (injected by kubelet into every pod). When detected, the `Reporter` switches to a `Headless` variant that emits structured JSON tracing events instead of rendering progress bars:

```rust
fn detect_output_mode() -> OutputMode {
    let in_k8s = std::env::var("KUBERNETES_SERVICE_HOST").is_ok();
    let in_ci = std::env::var("CI").is_ok();
    let is_tty = std::io::stderr().is_terminal();
    if in_k8s || in_ci || !is_tty { OutputMode::Headless } else { OutputMode::Tty }
}
```

In headless mode, `tracing_subscriber::fmt().json().flatten_event(true).init()` produces flat JSON log lines that Loki, ELK, and Datadog parse directly. Each stage emits `stage_start`, `stage_progress` (with throughput), and `stage_complete` events — the structured equivalent of interactive progress bars.

---

## Terminal capabilities: Unicode, Windows, and OSC 9;4

**Unicode detection** should check `LANG` for "utf" and fall back to ASCII bars (`#-` instead of `█░`) when unavailable. On Windows, check for code page 65001 (UTF-8). The `enable-ansi-support` crate should be called early in `main()` to enable `ENABLE_VIRTUAL_TERMINAL_PROCESSING` on Windows — this is required for cmd.exe and PowerShell, while Windows Terminal works by default.

The **OSC 9;4 protocol** reports progress in the terminal's title bar and taskbar. Originally from ConEmu, now supported by Windows Terminal, WezTerm, and Ghostty. The `osc94` Rust crate (v0.1.1, MIT, zero dependencies) provides a clean API:

```rust
let mut progress = osc94::Progress::default();
progress.start();
progress.increment(1).flush()?; // Reports 0-100% to title bar
// Cleaned up automatically on Drop
```

This is a **low-cost supplementary indicator** that degrades gracefully — unsupported terminals simply ignore the escape sequence. It's worth adding alongside indicatif bars for a polished experience. Ghostty 1.2 is the first terminal to support both OSC 9;4 and iTerm2 notifications.

For advanced features like Kitty Graphics Protocol or OSC 8 hyperlinks (clickable URLs — used by ripgrep), detect support explicitly or require opt-in. **Default to 16-color palette for themed terminals** — TrueColor and 256-color should only be used if explicitly configured, since these don't adapt to terminal color schemes.

---

## The crate ecosystem as of early 2026

| Category | Recommended | Downloads | Alternatives |
|---|---|---|---|
| **Progress** | `indicatif` 0.17+ | ~90M total | `kdam` (tqdm port, ~8K), `linya` (minimal), `pbr` (unmaintained) |
| **Color** | `owo-colors` 4.x | Very high | `colored` (allocates), `yansi` (~116M, zero-alloc), `console` (~145M, mitsuhiko) |
| **TTY detection** | `std::io::IsTerminal` | stdlib | `is-terminal` (for MSRV <1.70) |
| **Color level** | `supports-color` 3.0 | Moderate-high | Built-in to owo-colors via feature flag |
| **Structured output** | `serde_json` | Universal | `ndjson-stream` (streaming), `tracing-ndjson` (tracing layer) |
| **Title bar progress** | `osc94` 0.1 | Small | Manual escape sequences |
| **Windows ANSI** | `enable-ansi-support` 0.2 | Moderate | Manual `SetConsoleMode` |

The recommended dependency set for a modern Rust CLI style crate:

```toml
[dependencies]
owo-colors = { version = "4", optional = true }
supports-color = { version = "3", optional = true }
indicatif = { version = "0.17", optional = true }
serde = { version = "1", features = ["derive"] }
serde_json = { version = "1", optional = true }

[features]
default = []
cli = ["dep:indicatif", "dep:owo-colors", "dep:supports-color"]
k8s = ["dep:serde_json"]
```

---

## What makes uv feel fast: UX principles for CLI progress

uv's progress feels noticeably better than cargo's for several concrete reasons. **uv shows feedback for any operation exceeding ~1 second** — GitHub issues #4825 and #5758 explicitly added spinners when operations like downloading a Python toolchain previously showed no output. uv uses `finish_and_clear()` to remove progress bars on completion, then prints concise timing summaries like "Installed 5 packages in 12ms." This **clear-and-summarize pattern** keeps the terminal clean while providing confirmation.

Cargo's approach — persisting "Compiling foo v1.0" lines — provides an audit trail but feels dated compared to uv's ephemeral progress. The clig.dev design guide captures the principle: "Hiding logs behind progress bars when things go well makes it much easier for the user, but if there is an error, make sure you print out the logs."

Cargo's own `Throttle` pattern is worth adopting: **delay the first update by 500ms** (preventing flicker for fast operations) then rate-limit subsequent updates to every 100ms. This makes fast operations feel instant while slow operations get smooth, non-overwhelming progress. For perceived performance, steady tick intervals of **100-120ms** produce smooth spinner animation without excessive CPU usage.

ETA displays work best for uniform work. For non-uniform work (API calls with variable latency), showing elapsed time plus throughput is more useful than a jittering ETA. Always call `pb.reset_eta()` after pauses (rate limits, network retries) to avoid misleading estimates. indicatif provides `{bytes_per_sec}`, `{binary_bytes_per_sec}` (MiB/s), and `{per_sec}` (items/s) as built-in template formatters, along with `HumanBytes`, `HumanCount`, and `HumanDuration` utilities for standalone formatting.

---

## Conclusion

The production-proven stack is clear: **`std::io::IsTerminal` for detection, `owo-colors` for color, `indicatif` for progress, and a `Reporter` trait for abstraction**. The key architectural decisions are compile-time feature gating of heavy dependencies, runtime dispatch via `Option<State>` for zero-cost disabled progress (cargo's pattern), and the stylesheet approach for colors (owo-colors' `Style::default()` is a no-op when not colorized).

For paper-fetch, the critical patterns are `MultiProgress` with per-source spinners transitioning to bounded bars via `set_length()`, rate limit countdown display via `set_message()`, and multi-phase transitions via `finish_and_clear()` + `mp.remove()`. For pdf-mash, nested bars via `mp.insert_after()`, `{per_sec}` throughput templates, and a headless `Reporter` variant emitting structured tracing JSON replace interactive progress in Kubernetes. Both tools share the same `hs-style` crate, with `paper-fetch` enabling the `cli` feature and `pdf-mash` enabling both `cli` and `k8s`. The `NO_COLOR` standard, stderr/stdout split for dual human/machine output, and NDJSON event streaming complete the picture for a CLI architecture that serves humans, LLM agents, and infrastructure equally well.
