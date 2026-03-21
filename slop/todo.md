# PDF-Mash Technical TODO

This document tracks future improvements, technical debt, and known issues.

---

## High Priority

### ✅ Add Missing Class Names [COMPLETED]
**Status:** ✅ Resolved in Session 006
**Discovered:** Session 004, Stage 2 testing
**Resolved:** Session 006 (2025-10-18)

**Issue:** DocLayout-YOLO model detects classes 5, 6, 7 that weren't defined in our `class_names` array, showing as "unknown_5", "unknown_6", "unknown_7".

**Root Cause:** Original class mapping was completely wrong - all indices were incorrect, not just missing ones.

**Solution:** Extracted correct class names from PyTorch model metadata:
```rust
let class_names = vec![
    String::from("title"),           // 0
    String::from("plain text"),      // 1
    String::from("abandon"),         // 2
    String::from("figure"),          // 3
    String::from("figure_caption"),  // 4
    String::from("table"),           // 5
    String::from("table_caption"),   // 6
    String::from("table_footnote"),  // 7
    String::from("isolate_formula"), // 8
    String::from("formula_caption"), // 9
];
```

**Changes Made:**
1. Updated `pdf-mash/src/models/layout.rs:38-49` with correct 10-class array
2. Updated `pdf-mash/src/pipeline/markdown_generator.rs` to handle all 10 classes
3. Added "abandon" filtering (skips page numbers, headers, footers)
4. Fixed figure and formula output (were missing content)

**Validation:**
- All 10 classes now correctly identified
- Detection breakdown shows table-heavy pages (3-8) as expected
- Markdown output properly structured with captions, footnotes
- No compiler warnings

**References:**
- Session 006 journal: `slop/journal/session_006.md`
- Corrected output: `output/corrected_classes_output.md`

---

## Medium Priority

### Implement OCR Integration (Phase 5)
**Status:** Planned
**Description:** Add PaddleOCR for actual text extraction from detected regions.

**Requirements:**
- Download and convert PP-OCRv4 models (det, rec, cls)
- Create `OCREngine` struct in `src/models/ocr.rs`
- Integrate with layout detector pipeline
- Update markdown generator to use real text

**Estimated Effort:** 3-4 hours

---

### Table Extraction (Phase 6+)
**Status:** Future
**Description:** Add RapidTable/TableTransformer for structured table extraction.

---

### Formula Recognition (Phase 6+)
**Status:** Future
**Description:** Add Pix2Text-MFR or UniMERNet for LaTeX formula extraction.

---

## Low Priority / Nice to Have

### Make Magic Numbers Configurable
**Status:** Enhancement
**Discovered:** Session 004, Stage 3
**Issue:** Detection thresholds are hardcoded and should be tunable.

**Magic Numbers to Expose:**
1. **NMS IoU Threshold:** `0.45` in `layout.rs:180`
   - Controls how much overlap triggers suppression
   - Lower = more aggressive (removes more boxes)
   - Higher = less aggressive (keeps more boxes)

2. **Reading Order Row Threshold:** `50.0` in `layout.rs:194`
   - Pixel difference to consider boxes on same row
   - Affects multi-column text ordering
   - May need tuning per document type

3. **Confidence Threshold:** `0.25` in `layout.rs:50`
   - Minimum detection confidence to keep
   - Lower = more detections (more false positives)
   - Higher = fewer detections (may miss valid elements)

**Implementation Options:**
- CLI flags: `--iou-threshold 0.5 --row-threshold 40.0 --confidence 0.3`
- Config file: `config.toml` with sensible defaults
- Environment variables: `PDF_MASH_IOU_THRESHOLD=0.5`

**Recommended:** Config file with CLI override capability.

**Effort:** 2-3 hours

---

### REST API Implementation
**Status:** Planned
**Description:** Add Axum-based REST API for remote processing.

**Endpoints:**
- `POST /convert` - Upload PDF, get markdown
- `GET /health` - Health check
- `POST /batch` - Batch processing

---

## Technical Debt

### ✅ Warnings to Address [RESOLVED]
**Status:** ✅ Resolved in Session 006
**Location:** `src/pipeline/markdown_generator.rs`

**Previous Warnings:**
- `unused variable: bboxes` - Fixed by implementing real markdown generation
- `field include_images is never read` - Now used for title confidence display

**Current Status:** No compiler warnings. All code in use.

---

## Documentation

### Add INSTALL.md
**Status:** To Do
**Description:** Step-by-step setup guide covering:
- System dependencies (CUDA, cuDNN, Pdfium)
- ONNX Runtime installation
- Model download and conversion
- Troubleshooting common issues

---

### Add ARCHITECTURE.md
**Status:** To Do
**Description:** Document system architecture, data flow, and design decisions.

---

## Testing

### Create Test Suite
**Status:** Future
**Description:**
- Unit tests for IoU, NMS, coordinate transformations
- Integration tests for pipeline
- Benchmark suite for performance tracking

---

**Last Updated:** 2025-10-18 (Session 006)
**Maintainer:** Session journals in `slop/journal/`
