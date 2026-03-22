# Walkthrough: Unpaywall DOI Resolver

**Date:** 2026-03-22
**Status:** Planning
**Checkpoint:** d8669601330930b13e13f77c80902b5ab94d1a5a

## Goal

Add Unpaywall as a DOI-to-PDF resolver so `download_by_doi` works for non-arXiv papers.

## Acceptance Criteria

- [ ] `download_by_doi("10.1038/s41586-021-03819-2")` resolves via Unpaywall and downloads
- [ ] arXiv DOIs still use the fast direct-URL path (no Unpaywall call)
- [ ] DOIs with no OA PDF return an actionable error message
- [ ] `unpaywall_email` is configurable in `config.yaml`
- [ ] `cargo check -p paper` passes

## Technical Approach

### Architecture

The current `download_by_doi` in `providers/downloader.rs` only handles arXiv DOIs. We add a resolver chain:

1. arXiv prefix check (existing, fast, no network call)
2. Unpaywall API lookup (new, one HTTP GET)
3. Error with clear message

The resolver lives inside `PaperDownloader` — no new traits or services needed. The existing `download_single` in `services/download.rs` already prefers `paper.download_url` over `download_by_doi`, so papers from OpenAlex that already have PDF URLs skip this entirely.

### Key Decisions

- **Resolver chain inside `download_by_doi`**: Not a separate trait. The method already exists, we're just making it smarter.
- **`unpaywall_email` in `DownloadConfig`**: Unpaywall requires a `mailto` param (not a key). Goes in config, defaults to `None`. When `None`, skip Unpaywall and fall through.

### Dependencies

- No new crates. Uses existing `reqwest::Client` and `serde`.

### Files to Create/Modify

- `paper/src/providers/downloader.rs`: Add `resolve_unpaywall()`, refactor `download_by_doi` into resolver chain
- `paper/src/config.rs`: Add `unpaywall_email: Option<String>` to `DownloadConfig`
- `crates/hs/config/default.yaml`: Add commented `unpaywall_email` field

## Build Order

1. **Config**: Add `unpaywall_email` field — no behavior change, just plumbing
2. **Unpaywall response types**: Small structs for JSON deserialization
3. **Resolver method**: `resolve_unpaywall()` async method on `PaperDownloader`
4. **Refactor `download_by_doi`**: Chain arXiv → Unpaywall → error

## Steps

### Step 1: Add `unpaywall_email` to DownloadConfig

**File:** `paper/src/config.rs`

Add one field to `DownloadConfig`:
```rust
pub unpaywall_email: Option<String>,
```

Default to `None` in the `Default` impl.

### Step 2: Add Unpaywall response types

**File:** `paper/src/providers/downloader.rs`

Add deserialization structs at the top:
```rust
#[derive(Deserialize)]
struct UnpaywallResponse {
    is_oa: bool,
    best_oa_location: Option<UnpaywallLocation>,
}

#[derive(Deserialize)]
struct UnpaywallLocation {
    url_for_pdf: Option<String>,
}
```

### Step 3: Store email + add resolver method

**File:** `paper/src/providers/downloader.rs`

Add `unpaywall_email: Option<String>` to the `PaperDownloader` struct. Accept it in `new()`.

Add method:
```rust
async fn resolve_unpaywall(&self, doi: &str) -> Option<String>
```

GET `https://api.unpaywall.org/v2/{doi}?email={email}`, deserialize, return `url_for_pdf` if present. Return `None` on any error (this is a best-effort resolver, not a hard failure).

### Step 4: Refactor `download_by_doi` into resolver chain

**File:** `paper/src/providers/downloader.rs`

Replace the current single-path logic with:
```
1. If DOI starts with "10.48550/arXiv." → direct arXiv URL (existing fast path)
2. If unpaywall_email is set → try resolve_unpaywall()
3. If resolved → download_by_url(resolved_url)
4. Else → NotFound with message suggesting unpaywall_email config
```

### Step 5: Update default.yaml template

**File:** `crates/hs/config/default.yaml`

Add under `download:`:
```yaml
    # Email for Unpaywall API (enables non-arXiv DOI resolution)
    # unpaywall_email: you@example.com
```

### Step 6: Thread email through PaperDownloader construction

**File:** `paper/src/commands/paper.rs`

Where `PaperDownloader::new()` is called, pass `config.download.unpaywall_email.clone()`.

---
*Plan created: 2026-03-22*
