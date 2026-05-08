# Feature: HTML paywall guard at paper download

**Date:** 2026-05-06
**Status:** Draft

## Summary

Refuse to write a paper file to storage when the response is a small HTML paywall stub (DSpace SPA shell, PMC "Preparing to download" interstitial, generic Cloudflare/landing-page wrapper). Return `PaperError::NoDownloadUrl(doi)` instead so the caller treats it as a clean "no OA copy", same as a real 404. Eliminates the 74-cases-per-4-days of garbage HTML that scribe rejects downstream.

## Architecture Reference

Touches the `paper` crate's downloader path:
- `paper::providers::downloader::Downloader` — the single write site for all resolved download URLs (Unpaywall, OA provider chain, arXiv fast-path).
- Reuses existing `PaperError::NoDownloadUrl(String)` at `paper/src/error.rs:32-33`.
- Logging follows the `tracing::info!` pattern used elsewhere in `paper/src/providers/response.rs`.

Does not touch scribe, distill, MCP tools, or the catalog — it's a pre-write filter inside the downloader. Scribe's existing pre-conversion guard stays as the second line of defense.

## What It Does

When `hs paper download` (or the equivalent MCP tool) resolves a download URL and starts streaming the response body:

1. Inspect Content-Type and size before writing to disk.
2. If the body is HTML AND under 50 KB AND matches a known paywall/SPA pattern, abort the write, log the rejection with the matched pattern, and surface `PaperError::NoDownloadUrl(doi)` to the caller.
3. Otherwise (real PDF, real HTML paper ≥ 50 KB, anything else) write as today.

From the user's perspective:
- `hs paper download <doi>` for a paywalled DSpace link returns "Not found" instead of "Downloaded" + a 2 KB `<doi>.html` stub.
- The MCP tool result envelope is `isError: true, "Not found: …"` instead of `isError: false` with a useless path.
- `system_status` History pane stops showing fake "Download" entries for papers that are unreadable.
- The MCP-watch NDJSON `errors.html_paywall` rolling 24-hour count drops to ≤ 1/day.

## Scope

**In scope:**
- Pre-write Content-Type + size + body-pattern check inside `Downloader`.
- Rejection patterns for the three classes seen in production logs: DSpace SPA shell, PMC interstitial, Cloudflare challenge / generic SPA bootstrap.
- Unit tests with three captured-fixture HTML responses (one per class) plus one real-but-small HTML paper that must pass.
- One `tracing::info!` per rejection with `provider`, `doi`, `pattern`, `body_len`, `content_type`.

**Out of scope:**
- Removing existing legacy HTML files from storage. Use `hs pipeline purge-skipped` for that — already deployed and bounded by the `embedding_skip` stamp.
- Detecting paywalls *after* a redirect to a different domain that returns a real-looking PDF. Different problem; needs Content-Type checks the upstream doesn't always send.
- Any change to the convert / scribe path.
- Tuning the 50 KB threshold dynamically. Hardcoded constant.
- Storing rejected stubs anywhere "for later inspection". ONE PATH — reject means reject.

## Interface

**Behavioral interface (no API change):**
- `Downloader::download_to_storage(...)` returns `Result<DownloadOutcome, PaperError>` exactly as today; the new path is `Err(PaperError::NoDownloadUrl(doi))` for stubs.
- `paper::commands::paper::run_download` already maps `NoDownloadUrl` to its existing "no OA copy" branch — no caller changes needed.
- MCP tool result envelope: unchanged shape; stub responses move from `isError:false` to `isError:true, text: "Not found: …"`.

**Configuration:**
- One private constant in the downloader module: `STUB_HTML_MAX_BYTES: usize = 50_000`. Tuned from observed log data; not user-configurable (ONE PATH).

**New private helper:**

```rust
// paper/src/providers/downloader.rs
fn looks_like_paywall_stub(body: &[u8]) -> Option<&'static str> {
    // Returns Some(pattern_name) if the body is a known stub, else None.
    // Cheap byte-substring matches; no regex on the hot path.
    // Patterns:
    //   contains b"<ds-app>"                    -> "dspace_spa"
    //   contains b"Preparing to download"       -> "pmc_interstitial"
    //   contains b"<title>Just a moment...</title>" -> "cloudflare_challenge"
    //   contains b"<base href=\"/\">" AND zero <p> tags after script/style strip
    //                                           -> "angular_spa_shell"
}
```

## Behavior Details

**When to check:** after the response has streamed to a buffer (body is fully in memory because we already need its bytes for SHA-256). Check happens between the SHA-256 calculation and the storage write, so we don't waste a disk write.

**Order of checks (cheap → expensive, short-circuit on miss):**
1. If `body.len() >= STUB_HTML_MAX_BYTES` → not a stub, write.
2. If response `Content-Type` is present AND does NOT contain `text/html` or `application/xhtml+xml` → not a stub, write.
3. If `Content-Type` is missing AND body does NOT start with `<!DOCTYPE html`, `<html`, or `<?xml` (case-insensitive, after stripping leading whitespace) → not a stub, write.
4. Run `looks_like_paywall_stub(body)`. If `Some(pattern)` → reject with that pattern in the log. If `None` → write (small real HTML paper).

**False-positive risk:** the Angular SPA pattern (`<base href="/">` + zero `<p>` tags) is the loosest. A genuine paper hosted as a server-side-rendered SPA would pass — they typically render `<p>` tags in the static body. Empirically zero hits in the user's 4-day log against real papers.

**No retries.** A paywall stub is deterministic — same stub on retry. The downloader's existing retry-on-transient-network-error logic sees `NoDownloadUrl` as terminal and moves on to the next provider.

**Logging:** one `tracing::info!` per rejection. No metrics counter (the MCP watch script counts via the tool-call logs).

**No new env vars or config keys.** ONE PATH per the project rules.

## Acceptance Criteria

- [ ] `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test` all pass.
- [ ] New unit tests in `paper/src/providers/downloader.rs::tests`:
  - `paywall_dspace_spa_rejected` — fixture: 827 B `<ds-app>` body
  - `paywall_pmc_interstitial_rejected` — fixture: 2.7 KB body containing "Preparing to download"
  - `paywall_cloudflare_challenge_rejected` — fixture: "Just a moment…" body
  - `real_html_paper_under_50kb_passes` — fixture: a 30 KB EuropePMC abstract HTML or similar
  - `real_pdf_passes` — confirm existing flow unbroken
- [ ] Live test: `hs paper download --doi 10.1080/19317611.2021.1966564` (a known DSpace stub from the MCP log on 2026-05-06) returns "Not found" instead of writing a file. Same DOI today writes a 21 KB `.html`.
- [ ] After deploy, MCP-watch NDJSON shows `errors.html_paywall` rolling 24-hour count ≤ 1.
- [ ] No `*.html` files smaller than 50 KB written under `papers/` whose mtime is after the deploy timestamp:
  ```bash
  find /mnt/home-still/papers -name '*.html' -size -50k -newer <deploy_ts>
  # → expect zero output
  ```

## Technical Notes

- The body-buffer-before-write pattern is already what the downloader does (because of the SHA-256 requirement), so the new check piggybacks on existing memory state. No streaming-vs-buffered ambiguity.
- 50 KB threshold is a single empirical constant; revisit if the user finds a real paper rejected. The next-largest stub seen in logs is 58 KB (a Cloudflare landing page that we *should* still reject if we widen the threshold). Trade-off documented in a code comment.
- No new dependencies.
- Captured fixtures for the unit tests should be committed at `paper/tests/fixtures/paywall/` to keep them inspectable. Three small files (~3 KB each).

## Assumptions

- [ASSUMPTION] All current paywall-stub responses across providers fall into one of the four patterns above. If a new stub class shows up, we add a pattern and ship the next rc — observable via the MCP-watch script.
- [ASSUMPTION] The MCP-watch script (`~/.local/bin/hs-mcp-log-watch.sh`) is installed and running on `mac_air`, so post-deploy validation has signal.
- [ASSUMPTION] The downloader buffers the full body before writing (current behavior — required for the SHA-256 calc that catalog stamps).

---
*Spec conversation: 2026-05-06 — derived from 4-day MCP log scan revealing 74 stub-HTML downloads (17% of all `paper_download` calls).*
