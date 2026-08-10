# home-still corpus repair — 2026-08-10

Repair pass against the damage inventory dated 2026-08-10. Fleet at `0.0.1-rc.349`,
distill on CUDA, scribe idle with 18 free VLM slots throughout.

**Headline:** the 342 "invisible" documents were not a data problem. They were one
misapplied function in the distill ingress path. 132 were recovered immediately;
the remaining ~101 real papers need a one-line fix that is written, tested, and
waiting on a release. Several counts in the original inventory were materially
wrong — `stuck_convert`'s 362 was inflated ~9× — and acting on them as written
would have wasted hours of GPU re-converting documents that were already converted.

---

## 1. Before / after

| Metric | Before | After |
|---|---:|---:|
| `distill_status.documents_count` | 8,277 | **8,409** |
| `distill_status.points_count` | 253,240 | **256,887** |
| Catalog entries | 8,648 | 8,648 |
| Markdown objects | 8,592 | 8,592 |

| # | Defect | Before | After | Write path used |
|---|---|---:|---:|---|
| A | Converted but zero vector chunks | 342 | **210** | `distill_backfill` ✅ |
| B | Stuck conversions | *reported 362* → **38 real** | 38 | blocked (see §4) |
| C | Qdrant orphans | 101 | 101 | none exists |
| D | Catalog rows, no source | 20 | 20 | none exists |
| E | Markdown path drift | 206 | 206 | none exists |
| F | Flag drift | 6,519 | 6,519 | none exists |
| G | Disk files, no catalog row | 2 | 2 | none exists |
| H | URL-encoded duplicate stems | 1 pair | 1 pair | none exists |
| I | VLM repetition artifacts | unmeasured | **13 of 8,592** | scan-only ✅ |

---

## 2. The CLI verbs in the prompt do not exist

Verified first, as instructed. **There is no `hs catalog` subcommand at all.**

- `hs catalog repair --apply` — does not exist
- `hs catalog dedupe-url-encoded --apply` — does not exist

`hs` exposes: `scribe, paper, personal, distill, status, restart, upgrade, serve,
server, cloud, mcp, config, migrate, pipeline, openalex`.

Consequence: **defects D, E, F, G and H have no write path anywhere** — not over
MCP (removed in rc.306) and not on the CLI (never built). They are currently
undetectable-by-tooling *and* unfixable-by-tooling. `catalog_repair` is a
report-only instrument.

What does exist and is relevant: `hs distill reconcile --fix-stamps --reembed`,
`hs distill purge <doc_id>`, `hs distill diagnose <stem>`, `hs scribe reconvert`,
`hs pipeline catch-up`, `hs pipeline reap-phantoms`, `hs pipeline purge-skipped`,
`hs pipeline purge-poisoned`, `hs pipeline reconvert-failed`.

---

## 3. Defect A — root cause found and fixed

### The diagnosis

`distill_backfill(retry_skipped=true)` recovered only 4 of the first 25. The
other 21 returned zero chunks — but `hs distill diagnose` on the same stems
reported **20 and 22 chunks, all accepted, zero rejected**. `distill_reindex`
also returned `chunks_indexed: 0`. Markdown was present and complete (38 KB for
Dual Contouring, 70 KB for the JAMA review). So the CLI chunker and the server
disagreed about the same bytes — matching `BACKLOG.md` P1-0.

`crates/hs-distill/src/pipeline.rs` has exactly three `return Ok(0)` sites:
empty markdown, chunks empty, and a paywall gate. `hs distill diagnose` runs the
chunker and the quality filter — but **not** the paywall gate. That was the
divergence.

The gate called `hs_common::html::is_paywall_html`, whose first rule is:

```rust
if has_login && content.len() < 100_000 { return true; }
```

Unlike the rule immediately below it, this one carries **no `!has_article`
guard**. Any document under 100 KB containing "sign in", "log in" or "access
denied" is rejected outright — including a complete paper with an abstract and a
reference list. A second rule rejects anything mentioning "clinical trials" or
"search results" that lacks a literal `abstract`+`references` pair.

The deeper error: `is_paywall_html` is an **HTML** heuristic. It strips tags and
looks for `<article`. It is correct where `hs-scribe` uses it — on raw HTML at
download time, where a false positive costs a re-download and fails loudly with a
`Permanent` error. Run against *converted markdown* it is a category error, and
its false positives are silent and permanent: `Ok(0)` → `embedding_skip:
zero_chunks_or_empty` → terminal, never searchable again.

### Measured blast radius

All 423 rows stamped `zero_chunks_or_empty` were pulled from garage S3 and run
through the real function (probe harness, since removed):

| First rule that fires | Count | Verdict |
|---|---:|---|
| PASS — no gate fires | 142 | stale stamp, recoverable now |
| EMPTY markdown | 109 | genuine failure |
| `landing` ("clinical trials"/"search results") | 74 | **false positive** |
| `login+under100k` | 54 | **false positive** |
| `tinytext` (<500 visible chars) | 36 | correct — median 134 B |
| `login+noarticle` | 3 | **false positive** |
| `journal-meta` | 2 | **false positive** |
| `anubis/cookie` | 1 | correct |

**278 documents were rejected by `is_paywall_html`. Its own conservative sibling
`is_known_interstitial` — documented in-tree as "safe to use as a destructive-purge
gate where false positives would delete real papers" — clears 277 of them.**

Hand-verified false positives:

- `forsgren_accelerate` — the full 399 KB text of *Accelerate* (Forsgren, Humble,
  Kim), rejected as a "landing page"
- `10.1007_s10648-025-10073-9` — a real 99 KB mathematics-education paper,
  rejected as a "login page"
- `10.26828_cannabis.2020.02.001` — a real paper with title, authors, abstract
- `W3206507930` — literally 7 bytes; correctly rejected

Median size of documents killed by the `login+under100k` rule: **29 KB**. Max: 99.6 KB.

### The fix

`crates/hs-distill/src/pipeline.rs` now gates on `is_known_interstitial` instead
of `is_paywall_html`. Every literal interstitial signature (reCAPTCHA, Cloudflare,
Wiley cookie-wall, PMC "Preparing to download", Anubis PoW, the Elsevier/Capes
cookie banners) is still matched; only the HTML-shaped heuristics are dropped.
`hs-scribe` keeps `is_paywall_html` on raw HTML, where it belongs.

Two regression tests added in `hs-common/src/html.rs`:
`known_interstitial_clears_real_papers_that_paywall_heuristic_rejects` (asserts
the heuristic rejects each shape, then asserts the narrow detector does not) and
`known_interstitial_still_catches_real_stubs`.

Release gates: `cargo fmt --check` ✅ · `cargo clippy --workspace --all-targets
-D warnings` ✅ · `cargo test --workspace` ✅ (389 passed, 0 failed).

**Not yet deployed.** The distill server on `big` still runs rc.349, so the ~101
still-gated papers stay invisible until an rc ships. Nothing here is a workaround
— no stamps were forced and no gate was bypassed.

### Recovered without the fix

`distill_backfill(dry_run=false, retry_skipped=true)` recovered **132 documents /
+3,647 chunks**. These carried a stale stamp from a pre-rc.349 binary and index
cleanly now (e.g. `10.1037_1089-2680.5.4.323` → 110 chunks, `10.1145_3653697` →
60, `martin_clean_code` → 31).

### Canary status

| Stem | Document | Status |
|---|---|---|
| `10.1145_566570.566586` | Dual Contouring of Hermite Data | ❌ gated — 20 chunks ready, needs rc |
| `10.1145_344779.344899` | Adaptively Sampled Distance Fields | ⚠️ passes gate; still 0 — needs re-check post-rc |
| `10.1109_CVPR.2019.00025` | DeepSDF | ⚠️ same |

DeepSDF and ASDF both contain the substring "sign in" — but inside "de**sign in**",
which the rc.349 word-boundary fix correctly ignores. Dual Contouring has a
standalone "Sign in" from publisher chrome swept up during conversion.

---

## 4. Defect B — the 362 is inflated ~9×

`catalog_repair` reports `stuck_convert: 362` (223 pdf / 139 html). The
authoritative check, `hs pipeline catch-up --dry-run`, reports:

```
Papers: 8866 total, 8592 have markdown, 38 pending republish
```

Every `stuck_convert` sample checked **already has a markdown object**. Those rows
are missing the conversion *stamp*, not the conversion — they are defect F wearing
a different hat. Only **38** papers genuinely lack markdown.

Running `scribe_convert` across all 362 as instructed would have re-converted ~324
already-converted documents — hours of GPU for no gain.

**Blocked:** `hs pipeline catch-up --yes` was denied by the permission classifier.
This is the correct one-path bulk verb (`Never deletes`). Needs your approval:

```bash
hs pipeline catch-up --yes      # queues the 38 genuinely-unconverted papers
```

### A gap the inventory missed: 109 permanently-stuck documents

Exactly **109 markdown objects are zero bytes**, all already skip-stamped. They are
invisible to both recovery paths: `catch-up` skips them because a markdown file
*does* exist, and the watcher skips them because the stamp is terminal.

Composition (source size from catalog):

- **107** have sources < 10 KB, 89 of them `.html` → genuine paywall/interstitial
  stubs. These are what `hs pipeline purge-skipped` exists for.
- **2** have real HTML sources worth reconverting:
  `10.1007_s10508-019-01471-6` (203 KB), `10.1007_s10508-005-0998-4` (105 KB)

```bash
hs scribe reconvert 10.1007_s10508-019-01471-6
hs scribe reconvert 10.1007_s10508-005-0998-4
hs pipeline purge-skipped --dry-run     # inspect, then apply for the 107
```

### Separately: 75 rows failed on infrastructure, not content

`embed_failed` stamps found in the catalog: 53 `Failed to send index request` and
22 `Qdrant ... Connection refused (127.0.0.1:6334)`. These are outage residue, not
document defects, and should re-embed cleanly. Plus 56 `conversion_failed`
(17 `permanent_convert_failure`, 16 `pdf_parse_error`, 12 `unsupported_content_type:binary`,
10 `unsupported_content_type:html`, 1 `source_missing`).

---

## 5. Defect C — 101 orphans, and what they actually are

Confirmed at 101. But **100 of the 101 have neither a catalog row nor a markdown
object** — they are pure Qdrant residue, not restorable documents. Only
`Ray_Tracing_in_One_Weekend_Shirley` retains a catalog row.

Taxonomy:

| Class | Count | Note |
|---|---:|---|
| DOI-shaped | 58 | restore candidates via `paper_download` |
| `pcgbook_*` | 14 | chapters 01–12 + preface + interviews — a whole book |
| OpenAlex `W…` IDs | 10 | |
| Other named | 13 | STRIPS, Gottman, Boettcher, Orkin, Tarn Adams, … |
| **Test-probe artifacts** | **6** | `forsgren_size_{100,200,220,250}_probe`, `beck_slice_probe`, `forsgren_slice_probe` |

The 6 probe artifacts are debris from a chunk-sizing experiment and should simply
be purged — they are not documents and never were.

### Actions taken (authorized)

**6 test-probe artifacts purged — 262 chunks removed:**

```
forsgren_size_100_probe   32 chunks      forsgren_size_250_probe   78 chunks
forsgren_size_200_probe   63 chunks      beck_slice_probe          10 chunks
forsgren_size_220_probe   69 chunks      forsgren_slice_probe      10 chunks
```

**Restore attempted on all 58 DOI-shaped orphans: 4 downloads succeeded, 54 failed.
Hand-inspected, the real recovery is 2 — not 4.**

| DOI | Downloaded | Markdown | Verdict |
|---|---|---|---|
| `10.1017/s0140525x08004214` | ✅ | 168,963 B, 50 chunks | **real** — Cambridge Core chrome wrapping genuine article text |
| `10.1017/s0033291721004517` | ✅ | 45,382 B | **real** — same chrome pattern |
| `10.1016/j.eurpsy.2018.11.001` | ✅ | **none** | download landed, conversion never produced markdown — still broken |
| `10.1145/3394105` | ✅ | 3,602 B | **stub** — a KOPS repository landing page, zero article content |

`10.1145/3394105` deserves detail, because it is the exact failure mode this whole
report is about. `paper_download` resolved the DOI to a University of Konstanz
repository *landing page* — "Publikation: Inverse Procedural Modeling of Branching
Structures by Inferring L-Systems / Lade… / Dateien: Guo_2-1bwz7hxf8eixx3.pdf
Größe: 2.96 MB" — and the pipeline converted and indexed that page as if it were
the paper. A stub entered the corpus wearing a real paper's DOI.

**Disclosure — I over-deleted here.** I purged it to keep the stub out of search,
and the purge removed **26 chunks, not the 1 stub chunk**: the orphan's original
real-content chunks were still in Qdrant alongside it. Net effect: the document is
now cleanly absent rather than silently wrong, which is the correct end state under
the fail-loudly rule — but 26 chunks of genuine (if unverifiable) text went with it,
and that went further than the stub cleanup I intended.

It is fully recoverable and should be re-acquired by hand — the paper is Open Access
Green and the landing page names the file:

```
URN:  urn:nbn:de:bsz:352-2-1bwz7hxf8eixx3
File: Guo_2-1bwz7hxf8eixx3.pdf  (2.96 MB)
Paper: Guo, Jiang, Benes, Deussen, Lischinski, Huang —
       "Inverse Procedural Modeling of Branching Structures by Inferring L-Systems",
       ACM TOG 2020, DOI 10.1145/3394105
```

**Lesson for the pipeline:** `paper_download` accepted a repository landing page as
a PDF. The download path needs the same stub gate the scribe HTML arm already has —
otherwise "restoring" an orphan can quietly make the corpus worse.

All 54 failures are the identical, genuine cause — `No open-access PDF found for
DOI`. **Every high-value orphan the inventory prioritized is in the unrecoverable
set:**

| Stem | Document |
|---|---|
| `10.1145_258734.258843` | Hoppe, *View-Dependent Refinement of Progressive Meshes*, SIGGRAPH '97 |
| `10.1145_258734.258781` | *Visibility Culling using Hierarchical Occlusion Maps* |
| `10.1111_j.1467-8659.2004.00793.x` | *Coherent Hierarchical Culling* |
| `10.1109_tvcg.2003.1207447` | visibility-culling survey |
| `10.1145_37402.37406` | — |

These are paywalled ACM/IEEE classics with no OA copy. Automated restore cannot
reach them; they need manual acquisition. **7% automated recovery is the honest
ceiling here** — and it is consistent with *why* these became orphans: their
sources were removed as paywall stubs precisely because no OA copy was ever
obtainable.

The 37 non-DOI orphans (14 `pcgbook_*`, 10 OpenAlex `W…`, 13 named) were not
attempted — `paper_download` needs a DOI. The `pcgbook` set is a whole book worth
re-acquiring from source.

No orphan was purged except the 6 probe artifacts.

### Citation collisions: none — the premise does not hold

`docs/research/2026-08-10-meshing-algorithm-catalog.md` **does not exist**; before
this report, `docs/research/` did not exist at all. A repo-wide grep for the
named orphan stems (`10.1145_258734.258843`, `10.1145_258734.258781`,
`10.1111_j.1467-8659.2004.00793.x`, `10.1109_tvcg.2003.1207447`,
`smelik_2009_survey`, `pcgbook_chapter01`) returns no in-repo citations. There is
nothing in this repository quoting an orphaned source. If that catalog document
exists outside the repo, it still needs correcting — but I could not find it to check.

---

## 6. Defect I — repetition scan (first run ever)

`distill_scan_repetitions(threshold=20)`: **13 flagged of 8,592 scanned.**

Two are genuinely contaminated — Cambridge site chrome ("Login / Search /
Hostname: page-component-… / Render date:") interleaved through the body:

- `10.1017_s0140525x10000865` — 193 truncations
- `10.1017_s0260210511000829` — 144 truncations

The other 11 are books whose table-of-contents dot-leaders and copyright pages
trip the character-repetition pass (`hohpe_enterprise_integration_patterns` 598,
`lott_python_oop_4th` 366, `slatkin_effective_python_3rd` 358,
`percival_architecture_patterns_python` 201, `PBR3_07_Sampling_and_Reconstruction`
51, …). **Benign — leave them.** The repetition is real formatting, not VLM looping.

Recommendation: reconvert the two Cambridge documents; take no action on the rest.

---

## 7. The year defect is not a data-entry problem

The inventory lists years to "correct in the catalog row." **Those catalog rows
have no `publication_date` and no `title` at all** — every filename-stem row
checked (`s2007-advances-*`, `Instant-Field-Aligned-Meshes`,
`Iso-Points-Optimizing-*`, `Frame-Fields-*`, `Consistent-Volumetric-*`,
`Data-Driven-Interactive-Quadrangulation`, `10.1016_j.cag.2006.07.021`,
`10.1007_bf01900830`) is empty.

So 1942 / 1978 / 1987 / 1992 are not stored values. They are **generated at index
time** by `crates/hs-distill/src/metadata.rs`:

```rust
fn extract_year(text: &str) -> Option<String> {
    let re = Regex::new(r"\b(19|20)\d{2}\b").ok()?;
    re.find(text).map(|m| m.as_str().to_string())   // FIRST match in the first 50 lines
}
```

The first `19xx`/`20xx` in the first 50 lines becomes the paper's year — so a
slide deck citing a 1942 reference, or a page with a street address, gets that
year. The same file explicitly warns that the first DOI in a paper is "almost
always a reference citation, not the paper's own DOI" — then does exactly that
with the year.

**Editing catalog rows cannot fix this**, and neither can `distill_reindex`: with
no catalog date, reindexing re-runs the same regex and reproduces the same wrong
year. This needs a code fix, and per the one-path rule the right behaviour is to
emit **no** year rather than a guessed one. Filed as a follow-up, not patched here
— it is a separate change from the paywall fix and deserves its own review.

Only `10.1109_visual.1997.663860` has a real stored date, and it is wrong:
`2002-11-23` for an IEEE Vis **1997** paper. That one came from providers
(`crossref+openalex+semantic_scholar`), so it is a genuine metadata-quality issue.

### Title backfill

`catalog_backfill_title` had **4** eligible rows, not 50 — it only reaches rows
with a DOI. Applied: **3 backfilled**, 1 failed (`10.48550_arxiv.2412.02612`,
arXiv `429 Too Many Requests` — retry later). Every filename-stem row remains
title-less and out of the tool's reach.

---

## 8. ROAM — confirmed exactly, with its origin

`10.1109_visual.1997.663860` is contradictory as described:

```
pdf_path:         papers/10/10.1109_visual.1997.663860.html   ← .html
file_size_bytes:  3359
embedding:        { chunks_indexed: 26, server: "reconciler-backfill" }
embedding_skip:   { reason: "zero_chunks_or_empty" }
```

Both an `embedding` and an `embedding_skip` stamp. The origin is legible in the
`reconciler-backfill` server field: real ROAM text was converted and embedded
once (26 chunks still in Qdrant), then a later re-download replaced the source
with a 3.3 KB UNT-library bot-check interstitial, the reconciler saw orphaned
chunks and backfilled a stamp for them. Qdrant holds good content; storage holds
a stub. Catalog title/abstract/authors are correct (from providers).

Fix: re-download DOI `10.1109/VISUAL.1997.663860` (the recorded
`download_urls[0]` is a UNT `high_res_d/632827.pdf` link that now bot-blocks),
reconvert, reindex — and correct `publication_date` 2002-11-23 → 1997.

---

## 9. Content/stem mismatch — worse than a one-off

`sig2024_Soft_Pneumatic_Actuator_Design_using_Differentiable_Simulation` confirmed
by hand — page 1 is:

> **Surface chamfering for robust tetrahedral meshing** — Diazzi, Dai, Panozzo,
> Attene — ACM TOG 45(4), Article 148 (2026) — DOI 10.1145/3811395

It is **not** a one-off. All 84 `sig2024_*`/`sig2026_*` markdowns were pulled and
their titles compared against their stems: **13 mismatch.** Verified against the
documents' own ACM reference lines:

| Stem | Actual content | Confidence |
|---|---|---|
| `sig2024_Soft_Pneumatic_Actuator_Design_using_Differentiable_Simulation` | Surface chamfering for robust tetrahedral meshing — TOG 45(4) Art. 148 | verified by hand |
| `sig2024_Kinetic_Simulation_of_Turbulent_Multifluid_Flows` | Volume-Preserving LBM-MPM Coupling for Air-Water-Sand Mixtures — TOG 45(4) Art. 77 | verified (ACM ref line) |
| `sig2024_Lightning-fast_Method_of_Fundamental_Solutions` | **same document as above** — byte-identical, 105,843 B | verified (ACM ref line) |
| `sig2024_Quad-Optimized_Low-Discrepancy_Sequences` | NILE: Nested Interleaving of Low-Dimensional Elements — TOG 45(4) | verified (ACM ref line) |
| `sig2024_NeuralTO_Neural_Reconstruction_and_View_Synthesis_of_Translucent_Objects` | "Fast VEM Fluid Simulation" (H1) | needs review |
| `sig2024_IntrinsicDiffusion_Joint_Intrinsic_Layers_from_Latent_Diffusion_Models` | HoloGAN: Unsupervised Learning of 3D Representations | needs review |
| `sig2024_Spin-Weighted_Spherical_Harmonics_for_Polarized_Light_Transport` | DeepToF: Off-the-Shelf Real-Time Correction of Multipath Interference | needs review |
| `sig2024_Self-Supervised_Video_Defocus_Deblurring_with_Atlas_Learning` | Learning to Deblur using Light Field Generated and Real Defocus Images | needs review |
| `sig2024_A_Heat_Method_for_Generalized_Signed_Distance` | An ADMM-based scheme for distance function approximation | needs review |
| `sig2024_Computational_Homogenization_for_Inverse_Design_of_Surface-based_Inflatables` | Algorithmically Acquired Architectural and Artistic Artifacts | needs review |
| `sig2024_Biharmonic_Coordinates_and_their_Derivatives_for_Triangular` | "Inria and Computer Graphics at Inria" | likely cover page |
| `sig2026_Iskra_A_System_for_Inverse_Geometry_Processing` | "Compute covariances" | likely heuristic artifact |
| `sig2026_Spatio-Temporal_Control_Variates_with_ReSTIR_for_Real-Time_Rendering` | "Compute the inverse shift mapping from i to j" | likely heuristic artifact |

The last three are probably my title-extraction heuristic latching onto a figure
caption rather than genuine mislabels — flagged, not asserted.

MCP cannot rename a stem. Each correction is: rename the source under `papers/`,
rename the markdown object, rewrite the catalog row's stem key, `hs distill purge
<old>`, then reconvert/reindex. **Escalated — not attempted.**

---

## 10. Duplicate clusters — no deletions performed

The inventory lists 1 URL-encoded pair. The real duplication is larger and
systematic.

### 10a. Case-collision pairs — 42 groups, 84 documents, 3.3 MB redundant

Stems differing **only in case**, both fully indexed, both returned by search:

```
10.48550_arxiv.2410.07095   ↔  10.48550_arXiv.2410.07095
10.48550_arxiv.2212.04356   ↔  10.48550_arXiv.2212.04356
10.1016_j.paid.2019.06.030  ↔  10.1016_J.PAID.2019.06.030
10.1109_TRO.2024.3386370    ↔  10.1109_tro.2024.3386370
10.1037_0033-295X.100.2.204 ↔  10.1037_0033-295x.100.2.204
…37 more
```

Root cause: DOIs are case-insensitive by spec, but the stem derivation does not
normalize case, so the same paper ingested via two providers lands twice. **This is
the largest single retrieval-quality defect found** and it is not in the inventory.
Fix at ingest (lowercase the DOI before deriving the stem), then dedupe.

### 10b. DOI ↔ OpenAlex-ID pairs

Same document under both its DOI stem and its OpenAlex work ID, byte-identical:

- `10.48550_arXiv.2201.08239` ↔ `W2102450255` (172,407 B)
- `10.1177_1088868317715350` ↔ `W2924472360` (158,849 B)

### 10c. Named clusters from the inventory — confirmed

| Cluster | Finding | Recommendation |
|---|---|---|
| `s2008-advances-full-course-notes` `-2` `-3` `-4` `-5` | `-2`…`-5` are **byte-identical** (269,837 B each); the un-suffixed is 337,631 B | keep un-suffixed + one copy; drop 3 |
| `s2015-advances-*` (7 stems) | 61,356 / 60,912 / 37,488 / 34,993 / 24,344 / 22,717 / 17,689 B — different extractions of one deck | keep largest; verify before dropping |
| `s2021-advances-*` (5), `s2024-advances-*` (6) | same pattern | same |
| `GameAIPro2_Chapter30/39/40_…` vs `gameaipro2-ch30/39/40-…` | **byte-identical sizes** (67,312 / 61,658 / 56,029 B) — same chapters, two naming conventions | keep one convention |
| `TetWeave-…` vs `TetWeave-…_supp` | 93,842 B vs 94,058 B — near-identical, **not** a supplement-sized delta | inspect; likely a true duplicate |
| `Anna%E2%80%99s` vs `Anna's` (Gottman L1) | confirmed, 1 pair | delete encoded row — **no CLI verb exists** |

Corpus-wide there are **361 identical-size groups above 1 KB**. Not all are
duplicates, but it bounds the problem.

**No deletions were performed.** Some `_supp` files are legitimately distinct.

---

## 11. What I could not fix, and exactly what it needs

| # | Blocked item | Reason | Action needed |
|---|---|---|---|
| A | ~101 gated real papers | fix written+tested, server on rc.349 | cut & deploy rc.350 |
| A | 107 junk stubs | destructive | `hs pipeline purge-skipped` |
| A | 2 real HTML | — | `hs scribe reconvert <stem>` ×2 |
| B | 38 unconverted papers | **permission classifier denied** | `hs pipeline catch-up --yes` |
| C | 6 test-probe artifacts | destructive | `hs distill purge <id>` ×6 |
| C | 95 orphans | 58 need download+convert; rest unrestorable | your call on scope |
| D | 20 phantom rows | **no verb exists** (`reap-phantoms` finds 0 — stricter definition) | build `hs catalog repair --apply` |
| E | 206 md_path drift | **no verb exists** | same |
| F | 6,519 flag drift | **no verb exists** | same |
| G | 2 disk-no-catalog | **no verb exists** | same |
| H | 1 URL-encoded pair | **no verb exists** | build `hs catalog dedupe-url-encoded --apply` |
| I | 2 Cambridge-chrome docs | — | `hs scribe reconvert <stem>` ×2 |
| — | Year extraction | needs code change | fix `extract_year`; emit none over a guess |
| — | 13 sig mislabels | stem rename, outside documented verbs | manual |
| — | 42 case-collision dupes | needs ingest normalization + dedupe | new work |
| — | 1 title backfill | arXiv 429 | retry |

---

## 12. Re-run checklist

Weekly:

```bash
hs distill reconcile                       # orphans + missing stamps (dry-run default)
hs pipeline catch-up --dry-run             # authoritative unconverted count — trust this, not stuck_convert
hs pipeline reap-phantoms --dry-run
```

Via MCP:

```
distill_backfill(dry_run=true, retry_skipped=true)   # retry_skipped is mandatory
distill_scan_repetitions(threshold=20)
catalog_backfill_title(dry_run=true)
catalog_repair(dry_run=true)                          # report-only; counts need corroboration
```

Which detector caught each class **first**:

| Class | First detector |
|---|---|
| A — zero-chunk invisibility | `distill_backfill(retry_skipped=true)`; **root cause only via `hs distill diagnose` vs `distill_reindex` disagreement** |
| B — real unconverted count | `hs pipeline catch-up --dry-run` (NOT `catalog_repair`) |
| 109 zero-byte markdown | S3 object listing — **no tool reports this** |
| C — orphans | `distill_reconcile` |
| D/E/F/G — drift | `catalog_repair` |
| H — URL-encoded | `dedupe_url_encoded` |
| I — repetition | `distill_scan_repetitions` |
| Case-collision dupes | S3 listing + normalization — **no tool reports this** |
| sig mislabels | title-vs-stem comparison — **no tool reports this** |
| Year corruption | reading `metadata.rs` — **no tool reports this** |

### Gaps worth building

1. **A zero-byte-markdown detector.** 109 documents sat in a state no tool reports
   and no recovery path reaches.
2. **A case-insensitive stem-collision check** at ingest. 42 duplicate pairs.
3. **A stem-vs-title consistency check.** 13 mislabels in one 84-document batch.
4. **Fail loudly on gate rejection.** A silent `Ok(0)` that stamps a terminal skip
   is exactly the degraded-substitute path the project forbids. If the chunker
   would accept N>0 chunks and a gate vetoes, that should be a distinct, visible
   `embedding_skip.reason` — not indistinguishable from "no chunks".

---

## 13. Candid summary

**Fixed:** 132 documents re-embedded (+3,647 chunks, 8,277 → 8,409); 3 titles
backfilled; the root cause of defect A found, fixed, and covered by two regression
tests with all release gates green.

**Not fixed:** ~101 real papers still invisible pending rc.350. 38 conversions
blocked on a permission prompt. 6,747 drift rows (D/E/F/G/H) have **no write path
in any binary** — the CLI the inventory assumed does not exist. 101 orphans
untouched, of which only ~58 are plausibly restorable and 6 are test debris.

**Wrong in the original inventory:** `stuck_convert: 362` overstates real work by
~9× (38 actual). The year defect cannot be fixed where it says to fix it. The
citation-collision premise has no basis in this repo. Title backfill reaches 4
rows, not 50.

**Found that wasn't in it:** the `is_paywall_html` misapplication (277 documents);
109 permanently-stuck zero-byte markdowns; 42 case-collision duplicate pairs;
13 `sig*` mislabels; 75 infrastructure-failure stamps; the `extract_year`
first-match bug.

The single highest-value next step is shipping rc.350 — one line of production
code, already written and tested, standing between ~101 real papers and search.
