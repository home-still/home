# The definitive guide to CLI tool design

**A well-designed CLI is predictable, composable, self-documenting, and equally usable by humans and machines.** The best CLI tools follow a remarkably consistent set of principles drawn from POSIX standards, the GNU coding conventions, the Command Line Interface Guidelines (clig.dev), and hard-won lessons from tools like Git, Docker, and kubectl. This guide distills concrete, actionable recommendations across design principles, naming, documentation, output formatting, LLM-agent compatibility, and Rust/clap implementation — covering every layer from philosophy to code.

---

## Core design philosophy: humans first, machines close behind

The CLI Guidelines (clig.dev) — authored by the co-creators of Docker Compose — articulate nine principles that recur across every authoritative source. **Human-first design** means optimizing for discoverability, forgiveness, and progressive disclosure. **Composability** means using stdin/stdout/stderr correctly so programs become parts of larger pipelines. **Consistency** means following existing conventions so that behavior is "hardwired into users' fingers."

The practical rules that flow from these principles are well-established. Return **exit code 0 on success, non-zero on failure**. Send primary output to stdout and all messages, errors, warnings, and progress to stderr. Support `-h`/`--help` and `--version` on every command. Use a CLI argument parsing library — never hand-roll option parsing. Prefer flags to positional arguments because flags are self-documenting and order-independent. Make the default behavior the right thing for most users, and confirm before destructive actions.

The 12 Factor CLI Apps guide (from the Heroku CLI team) adds operational principles: aim for **sub-500ms response times** (print something within 100ms), show progress for long operations, respect `NO_COLOR` and TTY detection, and follow the XDG Base Directory spec for config (`~/.config/<app>`), data (`~/.local/share/<app>`), and cache (`~/.cache/<app>`). Configuration precedence should be: CLI flags > environment variables > project-level config > user-level config > system-level config.

Robustness deserves special emphasis. Validate all user input early. Make operations idempotent where possible. Make things time out with configurable defaults. Design for crash-only operation — avoid needing cleanup, defer to the next run. Expect misuse: bad connections, multiple concurrent instances, case-insensitive filesystems.

---

## Naming: commands, subcommands, flags, and arguments

**Command names** should be lowercase, short (2–5 characters ideal), easy to type, and avoid generic words like `tool` or `util`. POSIX recommends 2–9 lowercase alphanumeric characters. Check that the name isn't taken by popular tools before committing.

**Subcommands** follow one of two consistent patterns — pick one and stick to it. The **noun-verb pattern** (`docker container create`, `gh pr list`) is particularly agent-friendly because it groups related actions under resource nouns. The **verb-noun pattern** (`git add`, `npm install`) also works well. Use single lowercase words; if multi-word is unavoidable, use kebab-case (`list-sku-definitions`). Standard CRUD verbs should be consistent: `create`, `delete`, `list`, `show`/`describe`, `update`. Never have ambiguous siblings like "update" and "upgrade."

**Long flags** always use `--kebab-case` — this is the overwhelming universal convention across POSIX coreutils, GNU, and virtually every major CLI. Every flag must have a long-form version (`--verbose`); short forms (`-v`) are optional conveniences reserved for commonly-used flags only. Boolean flags don't require a value — `--force`, not `--force=true`. Negation uses the `--no-` prefix: `--no-color`, `--no-cache`. Value flags support both space and equals syntax: `--output file.txt` and `--output=file.txt`.

Standard flag names to reuse rather than reinvent:

- `-h`/`--help`, `--version`, `-v`/`--verbose`, `-q`/`--quiet`
- `-f`/`--force`, `-n`/`--dry-run`, `-o`/`--output`, `--json`
- `-p`/`--port`, `-u`/`--user`, `--no-color`, `-y`/`--yes`

**Positional arguments** should be limited to 1–2 at most. If you need more than two arguments of different types, you're probably doing something wrong — use flags instead. The exception is multiple values of the same type (`rm file1 file2 file3`). In usage strings, show required args in angle brackets (`<file>`), optional args in square brackets (`[file]`), and repeatable args with ellipsis (`<file>...`).

**Use subcommands** when the operation fundamentally changes what the tool does and requires different sets of arguments. **Use flags** when you're modifying or fine-tuning a single operation. The anti-pattern `--mode=backup` vs `--mode=restore` should be subcommands `tool backup` and `tool restore`.

---

## Documentation: help text, man pages, and error messages

The `--help` output is your tool's most important documentation — it's the first thing both humans and LLM agents read. The canonical structure follows this order:

```
<tool-name> - <one-line description>

Usage:
  tool <command> [flags]

Examples:
  $ tool create --name my-project
  $ tool list --format json

Commands:
  create      Create a new resource
  list        List all resources

Flags:
  -h, --help           Show this help message
  -v, --verbose        Enable verbose output
      --json           Output as JSON

Use "tool <command> --help" for more info.
```

**Lead with examples** — users reach for examples first, and LLM agents learn from examples faster than descriptions. Show the most common commands and flags first (not alphabetical). Keep help to one level of depth: `tool -h` shows top-level commands, `tool create -h` shows create-specific help. Support all access patterns: `-h`, `--help`, `help <subcommand>`, and `<subcommand> --help`. Always print help to stdout and ignore other flags when `-h` is present.

For commands with many subcommands, **group related commands** under descriptive headings as Git does ("start a working area," "work on the current change," "examine the history"). This dramatically improves scannability.

**Man pages** follow a required section order: NAME, SYNOPSIS, DESCRIPTION, OPTIONS, EXAMPLES, EXIT STATUS, ENVIRONMENT, FILES, SEE ALSO. The EXAMPLES section is the most underutilized and most valuable — include at least two tested examples (basic and advanced). Use bold for things the user types literally and italic for user-supplied values. Tools like `ronn` or `pandoc` can generate man pages from Markdown.

**Error messages** should follow a three-part structure: what went wrong, why, and how to fix it. Never pass through raw system errors — rewrite them for humans. "Can't write to file.txt. You might need to run `chmod +w file.txt`" is vastly better than "ENOENT: no such file." Suggest corrections on typos using edit distance (most parsing libraries do this automatically). Keep debug information hidden behind `--verbose`. Send errors to stderr, return non-zero exit codes, and group repeated errors under a single header rather than printing 500 identical lines.

---

## Making CLIs agent-friendly: the emerging frontier

This is the fastest-evolving area of CLI design, with authoritative guides emerging in 2025–2026 from Google DevRel and practitioners who've deployed agent-driven CLIs in production. The key insight: **human DX optimizes for discoverability; agent DX optimizes for predictability.** Both can coexist in the same binary.

**Structured output is table stakes.** Support `--json` on every command that produces output. JSON goes to stdout; everything else (progress, warnings, spinners) goes to stderr. Prefer flat structures over deep nesting — `{"pod_name": "web-1", "pod_status": "Running"}` is easier for agents than nested objects. Use consistent types across commands (if `age` is seconds in one command, don't return `"3 days"` as a string elsewhere). For streaming output, use **JSON Lines (NDJSON)** — one JSON object per line. Support field selection to limit response size (`gh pr list --json number,title`), because agents pay per token and lose reasoning capacity with irrelevant fields.

**Predictable behavior** means idempotent commands, meaningful exit codes, and deterministic output ordering. The kubectl `apply` model is the gold standard — prefer declarative verbs (`ensure`, `apply`, `sync`) over imperative ones (`create`) where possible. Use differentiated exit codes: **0** for success, **1** for general error, **2** for usage error, with additional codes for specific failure modes (resource not found, permission denied, conflict). An agent that gets exit code 5 can decide to skip creation; one that gets exit code 1 for everything must parse stderr to guess what happened.

**Self-describing interfaces** let agents learn tools at runtime. Rich `--help` output with examples, type information for flags, and clear required/optional marking is the foundation. Beyond that, consider a `usage` command that returns concise, comprehensive usage for all commands in a single output — more token-efficient than agents running `--help` on every subcommand. The kubectl `explain` command is best-in-class: `kubectl explain pods.spec.containers --recursive` describes schemas at runtime. Ship **CONTEXT.md or SKILL.md** files alongside your CLI to encode invariants agents can't intuit from help text ("always use `--dry-run` for mutating operations," "always confirm with user before write/delete commands").

**Input hardening against agent hallucinations** is a novel concern. Agents routinely hallucinate path traversals (`../../.ssh`), embed query parameters in resource IDs, and pre-encode strings causing double-encoding. Canonicalize and sandbox file paths. Reject control characters below ASCII 0x20, reject `?` and `#` in resource identifiers, and reject `%` in resource names. Treat the agent as an untrusted input source.

**The `--dry-run` flag** is evolving from nice-to-have to essential for agent workflows — it lets agents "think out loud" before mutating state. Auth should work via environment variables (`MYAPP_TOKEN`), not browser redirects. Always support `--yes`/`-y` to skip confirmation prompts, and `--no-input` to disable all interactive prompts.

---

## Output formatting: JSON, exit codes, color, and streams

**Exit codes** are part of your API contract. The universally understood convention is **0 for success, 1 for general error, 2 for usage error**. BSD sysexits.h defines codes 64–78 for specific conditions (66 = input not found, 69 = service unavailable, 77 = permission denied, 78 = configuration error). Avoid codes 126–128 (shell-reserved) and 128+N (signal-terminated). Use 3–125 for custom codes. Never exit 0 on failure, and document your exit codes — changing them is a breaking change.

**The stdout/stderr contract** is simple and crucial: stdout is for data (the actual result of the command), stderr is for everything else (errors, warnings, progress, spinners, status messages, debug output). This enables `mycli export > data.json` to produce a clean file while warnings appear on screen, and `mycli list | grep "active"` to filter output while progress indicators remain visible.

**Color** should be disabled when stdout is not a TTY, when `NO_COLOR` is set (the no-color.org standard adopted by 900+ programs), when `TERM=dumb`, or when `--no-color` is passed. Enable color when `FORCE_COLOR` is set (for color-supporting CI environments). The priority order: CLI flags > app-specific env vars > `FORCE_COLOR` > `NO_COLOR` > `TERM` check > TTY detection. Don't overuse color — if everything is highlighted, nothing is. Reserve red for actual errors, yellow for warnings, green for success.

**Tables** should use minimal decoration: simple column-aligned output without borders (like `kubectl get pods`). Include clear headers, left-align text, right-align numbers, and truncate to terminal width. Offer `--no-headers`, `--no-truncate`, and `--sort` flags for scripting. Each row should represent one entry to enable piping to `wc -l`, `grep`, and `awk`.

**Progress indicators** go to stderr, use animation only in TTY mode, and fall back to periodic line-based updates in non-TTY mode. Show progress for any operation taking more than 2 seconds — silence makes users think the program is broken. Use spinners when duration is unknown, progress bars when total work is measurable, and X-of-Y counters for discrete steps. Always offer `-q`/`--quiet` to suppress progress.

---

## Rust and clap: from derive macros to testing

Clap's derive API provides the canonical structure for Rust CLIs. The **top-level struct** uses `#[derive(Parser)]`, with a flattened `GlobalOpts` struct and a `#[command(subcommand)]` enum. Always make the top level a struct, not an enum — even if you think you'll never need global options.

```rust
#[derive(Debug, Parser)]
#[command(name = "my-app", version, about, long_about = None)]
pub struct Cli {
    #[command(flatten)]
    global_opts: GlobalOpts,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Args)]
struct GlobalOpts {
    #[arg(long, value_enum, global = true, default_value_t = Color::Auto)]
    color: Color,
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
}
```

Doc comments (`///`) become help text automatically — the first paragraph becomes `about`, subsequent paragraphs become `long_about`. Use `#[derive(ValueEnum)]` for fixed-choice arguments (clap auto-generates help with possible values and rejects invalid input). Use `#[derive(Args)]` for logical groupings and flatten with `#[command(flatten)]` for reuse. Mark global options with `#[arg(global = true)]` so they work before or after the subcommand.

Enable the **`env` feature** for environment variable fallback: `#[arg(long, env = "MYAPP_API_KEY", hide_env_values = true)]`. Priority is CLI arg > env var > default. Enable `wrap_help` for terminal-width-aware help text. Use `#[command(arg_required_else_help = true)]` to show help when no arguments are provided.

For **error handling**, use `anyhow` (or `color-eyre` for colored backtraces) since CLI errors ultimately get printed to the user. Use `.with_context(|| format!("Failed to read {}", path.display()))?` for actionable error messages. When you need custom exit codes, don't use `fn main() -> Result<()>` — instead, match on the error in main and call `std::process::exit()` with the appropriate code. Use `thiserror` for well-defined error types in your library layer that need to be matched on.

**Project structure** follows a consistent pattern: `main.rs` (minimal — parse args, set up logging, call `run()`), `cli.rs` (clap structs), `commands/` module (one file per subcommand with an `execute()` function), and `lib.rs` for core logic. This separation makes the core logic independently testable.

**Testing** operates at five layers. Unit tests validate core logic using `impl Write` instead of writing directly to stdout. Clap parsing tests use `Cli::try_parse_from(["myapp", "--verbose", "build"])` and `Cli::command().debug_assert()`. Integration tests use `assert_cmd` with the `predicates` crate for assertions on exit codes, stdout, and stderr. Snapshot tests use `trycmd` (by the clap maintainer) for treating test cases as data files — `.trycmd` files that capture expected output and can be bulk-updated with `TRYCMD=overwrite cargo test`. The `insta` crate handles snapshot testing for internal data structures and complex output with `cargo insta review` for interactive approval.

The recommended dependency set:

```toml
[dependencies]
clap = { version = "4.5", features = ["derive", "env", "wrap_help"] }
anyhow = "1.0"
serde = { version = "1", features = ["derive"] }

[dev-dependencies]
assert_cmd = "2.1"
predicates = "3"
trycmd = "0.15"
insta = { version = "1", features = ["yaml"] }
```

---

## Conclusion

The best CLIs share a surprisingly consistent DNA. They separate data (stdout) from messaging (stderr), use exit codes as a contract, provide `--json` for machines and formatted output for humans, lead their documentation with examples, and treat flags as the primary interface for options. The newest dimension — **agent-friendliness** — doesn't require a separate tool or protocol. It requires the same disciplines taken further: structured output, idempotent commands, differentiated exit codes, self-describing interfaces, input validation against hallucinations, and `--dry-run` as a first-class citizen. In Rust, clap's derive API encodes most of these conventions by default (kebab-case flags, auto-generated help, value enums, env var fallback), making the right thing the easy thing. The standards have converged. The remaining challenge is the discipline to apply them consistently.
