# Feature: Distinguishable `not_found` reasons in `paper_download`

**Date:** 2026-05-06
**Status:** Draft

## Summary

Today `paper_download` returns `Not found: No open-access PDF found` whether the DOI is genuinely closed-access OR every OA provider is transiently failing. Split that into two error classes — a deterministic OA miss vs an aggregate provider-side problem — so dashboards and operators can tell "user is searching closed journals" (expected, ~45% of calls) from "Unpaywall is down" (incident).

## Architecture Reference

Touches `paper::providers::downloader::Downloader` — specifically the routine that fans out to Unpaywall + each `PaperProvider`'s `get_by_doi` to resolve a download URL.

Adds one variant to `PaperError` at `paper/src/error.rs`. Reuses the existing `error.category()` mapping pattern.

The MCP-watch script (`~/.local/bin/hs-mcp-log-watch.sh` on `mac_air`) is updated in lockstep — that's where the new bucket surfaces to the operator.

## What It Does

`paper_download` for a DOI today returns one of two outcomes when no PDF can be fetched:

**Today** (single error, both cases collapsed):
```
Not found: No open-access PDF found for DOI: <doi>
```

**After this feature** (two distinguishable cases):

- All providers responded "no record" / 404 — a clean OA miss:
  ```
  Not found: No open-access PDF available (DOI valid, no OA copy known)
  ```
- At least one provider returned a transient error (5xx, timeout, rate limit, connection error) AND none returned a usable URL:
  ```
  Providers unavailable: 3/3 providers failed transiently. Try again in a few minutes.
  Details: openalex=Timeout, unpaywall=503, europe_pmc=no record
  ```

A consumer (Claude session, MCP-watch script) can now distinguish:
- `not_found_oa` → expected, move on to the next DOI
- `not_found_transient` → infra problem, surface to operator, retry later

## Scope

**In scope:**
- New `PaperError::AllProvidersTransient { doi, provider_errors, summary }` variant.
- Refactor the provider fan-out in `Downloader` to track per-provider outcome (`Found(url) | NotFound | Transient(error)`) and aggregate into the new error class.
- Existing `NoDownloadUrl(doi)` keeps the "no OA copy" semantics; no behavior change for callers that already handle it.
- Update MCP-watch script: split `not_found` into `not_found_oa` (case A) and `not_found_transient` (case B).
- Unit tests covering the aggregation rules.

**Out of scope:**
- Retrying transient errors at the downloader level. The new variant signals "all providers transiently failed"; retry is the caller's call. The existing `send_with_429_retry` keeps doing its job per-call.
- Surfacing provider-by-provider health to the MCP user in real time (would need a separate `hs provider health` tool — different feature).
- Distinguishing "DOI doesn't exist" from "DOI exists but no OA". Both fall under `NoDownloadUrl` today and continue to.
- Auto-retry-with-backoff at any layer above the downloader.

## Interface

**New error variant:**

```rust
// paper/src/error.rs
use std::collections::BTreeMap;

#[derive(Error, Debug)]
pub enum PaperError {
    // ...existing variants...

    #[error("Providers unavailable for {doi}: {summary}. Try again in a few minutes. Details: {details}")]
    AllProvidersTransient {
        doi: String,
        provider_errors: BTreeMap<String, String>, // provider name -> short error msg
        summary: String,                            // e.g. "3/3 providers failed transiently"
        details: String,                            // pre-rendered "openalex=Timeout, unpaywall=503"
    },
}
```

**`error.category()` mapping** at `paper/src/error.rs:44-67`:
- `AllProvidersTransient` → `ErrorCategory::Transient` (consistent with `Http(timeout)` and `ProviderUnavailable`).

**MCP tool envelope:**
- `paper_download` result on `AllProvidersTransient`: `isError: true`, `text: "Providers unavailable: 3/3 providers failed transiently. Try again in a few minutes. Details: openalex=Timeout, unpaywall=503"`.
- Existing `NoDownloadUrl(doi)` text unchanged.

**MCP-watch script update** (separate file, on `mac_air`):
- New classifier branch: if response `isError:true` AND text starts with `Providers unavailable:` → bucket `not_found_transient`.
- Existing `Not found:` → bucket `not_found_oa` (renamed from `not_found`).
- Summary line: `not_found_oa=N  not_found_transient=N` (replaces single `not_found=N`).
- NDJSON record: `errors.not_found_oa` and `errors.not_found_transient` (replaces `errors.not_found`).

## Behavior Details

**Per-provider outcome classification** inside the Downloader fan-out:

```rust
enum ProviderOutcome {
    Found(String),                      // url
    NotFound,                           // 404 or "no record" — DOI not in this provider's DB
    Transient(String),                  // timeout, 5xx, rate-limit, connection refused, parse error
}
```

Mapping from `PaperError` to `ProviderOutcome`:
- `Ok(Some(_))` → `Found(url)`
- `Ok(None)` or `Err(NotFound(_))` → `NotFound`
- `Err(Http(e)) if e.is_timeout()` → `Transient("timeout")`
- `Err(Http(e)) if e.status() in 500..=599` → `Transient(format!("{}", e.status().unwrap()))`
- `Err(RateLimited { provider, .. })` → `Transient("rate_limited")`
- `Err(ProviderUnavailable(s))` → `Transient(s.split(' ').next().unwrap_or("unavailable"))`
- `Err(ParseError(_))` → `Transient("parse_error")` (don't blame the user for our brittleness)
- `Err(CircuitBreakerOpen(_))` → `Transient("circuit_open")`
- Any other error → `Transient(error.to_string().chars().take(40).collect())`

**Aggregate decision** after all providers respond:
- Any `Found` → return that URL (existing behavior; first-found-wins order unchanged).
- All `NotFound` (or zero providers attempted) → `Err(NoDownloadUrl(doi))`.
- Any `Transient` AND no `Found` → `Err(AllProvidersTransient { doi, provider_errors, summary, details })`.

**`provider_errors` map content:** keep messages short — per-provider error message truncated to 40 chars. Example: `{"openalex": "request timeout after 30s", "unpaywall": "503 Service Unavailable", "europe_pmc": "no record"}`.

**`summary` field:** computed as `format!("{}/{} providers failed transiently", transient_count, total_providers)`.

**`details` field:** computed as `provider_errors.iter().map(|(k, v)| format!("{k}={v}")).join(", ")` — pre-rendered for the MCP envelope.

**No new retries here.** The existing `send_with_429_retry` already handles 429 in 3 attempts per provider; if all 3 fail, that provider's outcome is `Transient` and the aggregate logic handles it.

**Order of provider attempts is unchanged.** This feature is observability-and-classification, not behavior change.

## Acceptance Criteria

- [ ] `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test` all pass.
- [ ] New unit tests in `paper/src/providers/downloader.rs::tests`:
  - `aggregate_all_not_found_returns_no_download_url`
  - `aggregate_one_transient_zero_found_returns_all_providers_transient`
  - `aggregate_one_found_one_transient_returns_found` (Found wins)
  - `error_message_includes_provider_breakdown` (asserts the `details` field)
  - `provider_outcome_classification` (table-driven: each `PaperError` maps to the right `ProviderOutcome`)
- [ ] No regression in any existing `paper_download` test.
- [ ] MCP-watch script on `mac_air` updated: running it on a log containing both classes produces:
  ```
  Errors:    timeout=N  not_found_oa=N  not_found_transient=N  html_paywall=N  other_error=N
  ```
- [ ] After deploy, NDJSON shows the two classes split. The user can compute a 24-hour rolling rate `not_found_transient / paper_download_calls` to decide "is something broken upstream".
- [ ] Documentation updated: a one-line entry in CLAUDE.md under a "Diagnostic semantics" section describing the two error classes.

## Technical Notes

- `BTreeMap<String, String>` (not `HashMap`) so the error message is deterministic / diff-able in test assertions and the `details` field renders alphabetically.
- 40-char truncation per-provider prevents the aggregate message from blowing past MCP envelope limits when 6 providers all timeout with long error messages. (6 × 40 = 240 chars max for the details field — comfortable for any MCP transport.)
- The MCP-watch script change is a 5-line patch to the classifier branch (path: `~/.local/bin/hs-mcp-log-watch.sh` on `mac_air`).
- Rolling out the script change before the rc deploy is fine — old-style `Not found:` strings just bucket as `not_found_oa` (the desired terminal state anyway).

## Assumptions

- [ASSUMPTION] The transient/permanent classification of provider responses in the existing `PaperError::category()` is correct enough to be the source of truth. If a permanent 403 (closed-access journal) is misclassified as `Transient`, this feature would surface those as `not_found_transient` — a false positive but a noisy one we'd notice in the watch script and fix by tightening the classification.
- [ASSUMPTION] The user wants `AllProvidersTransient` to be a single aggregate surface, not one error per failing provider. Per ONE PATH rule.
- [ASSUMPTION] The MCP-watch script update is in scope for this feature (not a separate spec), since it's the consumer of the new error class and is the only way the new signal becomes operator-visible.

---
*Spec conversation: 2026-05-06 — derived from 4-day MCP log scan: 204 `not_found` events with no way to distinguish OA-miss from infra-flake.*
