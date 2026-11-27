# home-still

Academic research engine: 211M+ vector search with OpenAlex + PMC OA + Qdrant.

## Non-negotiables

- **ONE PATH per feature. No fallbacks. No legacy shims. No stub placeholders. No "backup" modes. No rollover behavior.** Every operation has exactly one execution path. When the primary path can't produce a usable result, **fail loudly** — don't write a degraded substitute to storage, don't emit silent defaults, don't stamp rows `conversion_failed: true` as a substitute for "we shouldn't have ingested this in the first place." Validate at system boundaries; reject bad input at the door. Two paths = magic results that take hours to trace. If you find a legacy/backup branch, delete it and make the primary right. Concrete anti-patterns this project has suffered from: `local-html` event-watch converter, `is_stub_pdf` silent-skip gate, `html_fallbacks` download path. Do not reintroduce any equivalent.
- **Distill embedding MUST run 100 % on the GPU.** Embedding is ollama `bge-m3-8k:latest` (`bge-m3` with `num_batch 8192`; the model as pulled reads only 2,048 tokens of an input) behind the myelin store. `hs-distill-server` asks ollama where the model is resident (`/api/ps`: `size_vram` against `size`) and refuses to start, to report healthy or ready, or to index unless it is fully on the GPU. Do not "fall back to CPU" for distill / embedding — not as a quick fix, not temporarily, not as a workaround. If the GPU path is broken, fix ollama/CUDA (driver, free VRAM, ollama's CUDA libraries) — never move the model to CPU. CPU embedding is too slow to be useful at this corpus size and will silently degrade throughput/latency for every downstream consumer.

## Working Style

- **Guided walkthrough mode.** When the user asks for a "walkthrough", "guide me", or "mentor me", do NOT write the implementation. Propose ONE small chunk with explanation, wait for the user to write it, review their actual code, then propose the next chunk.
- **Execute remote commands directly.** Run SSH / build / deploy commands via Bash yourself. Do not hand shell work back to the user. When deploying, name the target host explicitly (e.g., "deploying to `big_mac`") — never ask "run where?".
- **Verify before declaring done.** After a deploy, either (a) run the self-test / targeted smoke check and paste the green output, or (b) explicitly identify the blocker and log it as a P0 in `BACKLOG.md`. Do not close on code changes alone.
- **Parallel-audit count is exact.** When the user asks for N parallel agents, dispatch exactly N in a single message — never silently launch fewer.

## Debugging Philosophy

- Before proposing any fix: state the observed symptom, list 2–3 competing hypotheses with evidence, and name a cheap test that discriminates between them. No code until the diagnosis is confirmed.
- No band-aid guards or legacy-compatibility fallbacks in this greenfield project — fix root causes. See the "ONE PATH per feature" non-negotiable above.
- Before declaring a fix shipped, grep the repo for the original failure pattern (panic message, error string, the crashing code path) and confirm zero remaining sites. rc.304's JPEG 0-dim guard covered one of several call sites — the rest crashed in production and forced rc.305.

## Release Process

- Before tagging any `rc.*`: run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test`. Only tag if all three pass.
- All deployments go through tagged CI/CD release artifacts (GitHub Actions → `gh release` → `hs upgrade` on each host). Never hand-`scp` a locally-built binary to a deploy target. If `hs upgrade` doesn't cover a path (e.g., raw scribe/distill server binaries), fix `hs upgrade` — don't sidestep with manual copy.
- Verify the version string is correctly baked into each artifact BEFORE pushing the tag: run `./target/<arch>/release/hs --version` and confirm it matches the tag. `git describe` silently falls back to the prior tag when `GITHUB_REF_NAME` isn't set on local builds — rc.245 shipped with rc.244's binary because of this.
- After building per-arch artifacts, verify each binary's architecture with `file` before deploying — `x86_64` to `big`/`one`, `aarch64` to the Pis, `arm64` Mach-O to Apple Silicon hosts. Wrong-arch ships have wasted entire RCs.

## Documentation / Privacy

- Never hardcode real LAN IPs, hostnames, or private network details in docs/READMEs committed to the repo. Use `<host>` or `example.local` placeholders.
- Same rule for credentials of any kind: invite codes, S3 keys, Cloudflare tunnel tokens, OAuth client secrets. Use `<token>` / `<s3-key>` placeholders.

## Project Conventions

- **Bad-PDF folder is `corrupted/`.** Do not invent alternatives like `quarantine/`, `rejected/`, `bad/` — there's exactly one and it's already wired up.
- **Watcher liveness is heartbeat-based**, not PID-file-based. When diagnosing "is the watcher alive?" check `last_tick_seconds_ago` from status, not `/var/run/*.pid`.
- **Status counters can have multiple writers.** Multiple daemons may stamp the same status field — if a counter looks like it's "bouncing", check for multi-writer races before chasing a logic bug in any single writer.
- **OpenAlex works ingest is streaming pre-dedupe**, not merge-on-insert and not load-then-dedupe. The loader walks `updated_date=*` partitions newest-first and gates each row through an in-RAM `HashSet<u64>` of seen integer work-IDs (the `SeenSet` in `crates/openalex-ingest/src/seen_set.rs`); first-sightings flow into PK'd live tables via plain bulk INSERT, duplicates are skipped silently. The set checkpoints to `~/home-still/data/openalex/seen_set.bin` every 10 partitions for crash recovery. Run `hs openalex load-works` then `hs openalex build-indexes` / `build-fts`. Don't reintroduce `ON CONFLICT` and don't drop the PRIMARY KEYs — both are symptoms of fighting the snapshot's natural ordering. The streaming pre-dedupe is the ONE PATH.

## Build & Test

```bash
cargo check                        # Check all crates
cargo clippy --workspace --all-targets -- -D warnings # Fix issues
cargo test                         # Run all tests
cargo test -p paper                # Test paper crate
cargo build --release -p paper     # Build paper CLI
cargo check -p pdf-mash            # Check pdf-mash
```

## MCP tools — citation graph

- `graph(doi, direction, source, limit?, year_from?, sort?)` — step along the
  citation graph from a DOI. `direction` is `references` or `citations`;
  `source` is `semantic_scholar` or `openalex` (the local corpus). Default
  limit 100, max 1000. Sort by `year` (default) or `citations`.

With `source: "semantic_scholar"` the tool calls the Semantic Scholar Graph API
and reuses the same HTTP client, auth, and 429-retry path as `discover` (see
`paper/src/providers/semantic_scholar.rs`). It is the forward-chaining
primitive for the `home-still-bridge` snowballing skill. The live integration
test (`citation_graph_live_attention_is_all_you_need`) is `#[ignore]` — run
with `cargo test -p paper -- --ignored citation_graph_live`.

The whole tool surface (ten tools) is in `docs/reference/mcp-tools.md`.

