# 2026-05-02 drift cleanup — what was removed and why

After 24h of `pipeline_drift` rising from 6 → 16 because the same handful of stems kept being re-dispatched and failing, ran a manual cleanup.

## Step 1: bulk-deleted 42 noise keys from `papers/`

All `._*` AppleDouble files (macOS resource-fork metadata that rclone'd into S3) plus 2 zombie `.download/` dir contents. Drift went 50 → 12 candidates after this step.

## Step 2: deleted 6 garbage stems (paper + catalog row)

Catalog rows had `conversion_failed: unsupported_content_type:binary` with **70-94 convert attempts each** — every catch-up tick re-dispatched these and burned a slot for nothing.

| Stem | Size | Real type | DOI for re-download |
|---|---|---|---|
| 10.1002_aur.1831 | 0 B | empty | 10.1002/aur.1831 |
| 10.1007_s10803-019-04204-9 | 0 B | empty | 10.1007/s10803-019-04204-9 |
| 10.1177_13623613221150375 | 0 B | empty | 10.1177/13623613221150375 |
| 10.21037_tau.2016.05.08 | 0 B | empty (already in .quarantine) | 10.21037/tau.2016.05.08 |
| 10.5210_fm.v12i9.2003 | 4.5 KB | gzipped HTML — First Monday landing page | 10.5210/fm.v12i9.2003 |
| 10.1016_j.neubiorev.2021.07.036 | 282 KB | **JPEG** of the graphical abstract (downloader picked the figure URL over the DOI) | 10.1016/j.neubiorev.2021.07.036 |

If any of these need re-downloading later, the DOI lookup and `paper_download` flow now goes through the LAN-bypass gateway successfully (verified by deep_research agent runs).

## Step 3: moved 6 arxiv stems from `papers/` to `corrupted/` (canonical bad-PDF folder per CLAUDE.md)

Real PDFs (verified `%PDF-` magic, 1.9-8.2 MB each) but consistently fail VLM rejection on the rc.310 P0-12 QC gate (`conversion_failed: permanent_convert_failure`, 70 attempts each). Source preserved for future QC re-tune; catalog `pdf_path` field updated to point at the `corrupted/` location so they don't show up as `catalog_no_source` orphans.

| Stem | Size | Title |
|---|---|---|
| 10.48550_arXiv.2107.03374 | 1.9 MB | Evaluating Large Language Models Trained on Code (Codex / HumanEval) |
| 10.48550_arXiv.2310.06770 | 4.5 MB | (paper title TBD — not enriched in catalog) |
| 10.48550_arXiv.2403.07974 | 5.0 MB | (paper title TBD) |
| 10.48550_arXiv.2405.15793 | 5.0 MB | (paper title TBD) |
| 10.48550_arxiv.2406.15877 | 3.5 MB | (paper title TBD) |
| 10.48550_arXiv.2509.23045 | 8.2 MB | (paper title TBD) |

The QC threshold review is BACKLOG P1-19 — once `QC_BAD_PAGE_RATIO_PCT` is tuned (likely 10 → 25-30 to allow papers with minor cleanup activity), these can be re-attempted. Leaving them in `corrupted/` until then.

## Net effect on pipeline metrics

- Drift: 16 → 0 expected after this cleanup (the 12 stems above account for all of it; the 4 currently in_flight will drain on their own).
- GPU savings: each of the 6 arxiv stems was getting a 35-118 page VLM run roughly hourly via the catch-up timer — that's 30-60 minutes of GPU per attempt × 6 stems × 1 run/hour = the GPU was effectively pinned on dead-end work for half of every hour. With the timer disabled and these moved out of `papers/`, that loop is closed.
- 27 phantom catalog rows (`catalog_no_source`) and 3367 missing `downloaded_at` stamps remain — they're not driving drift but BACKLOG P1-15 (build `hs catalog purge`) and P2-15 (stamp `downloaded_at` at row synthesis) still need to land.
