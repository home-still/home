# OCR Pipeline Refactoring

**Date:** 2025-11-29
**Walkthrough:** 038
**Component:** pdf-mash/src/ocr/

---

## Goal

Consolidate scattered OCR logic into a clean, model-agnostic pipeline architecture. The original problem: PaddleOCR produces garbage when input width exceeds ~480px.

---

## Before State

```
models/ocr.rs         → Detection + Recognition + Preprocessing (mixed)
text_correction/      → Spelling + Confusables (separate)
ocr/                  → Partial abstractions (incomplete)
```

**Issues identified:**
- DRY violations: same image scaling logic in multiple places
- Hardcoded magic numbers (`width < 10 || height < 5`)
- Mixed concerns: detection and recognition tightly coupled
- Inconsistent error handling (some `unwrap()`, some `?`)
- `println!` instead of `tracing`

---

## After State (Target)

```
models/ocr.rs         → Detection ONLY (slimmed down)
ocr/
  backends/paddle.rs  → Recognition (extracted)
  preprocessor.rs     → Chunking + Normalization
  postprocessor.rs    → Segmentation + Confusables + Spelling (merged)
  pipeline.rs         → Orchestrator
text_correction/      → DELETED (merged into postprocessor)
```

---

## Completed Steps

### Phase 1: Shared Utilities & Config

| Step | File | Status | Notes |
|------|------|--------|-------|
| 1.1 | `ocr/config.rs` | ✅ | Added `enable_confusable_correction`, `min_crop_width`, `min_crop_height` |
| 1.2 | `ocr/preprocessor.rs` | ✅ | Added `normalize_image()` function |

### Phase 2: Complete OCR Module

| Step | File | Status | Notes |
|------|------|--------|-------|
| 2.1 | `ocr/backends/mod.rs` | ✅ | Created module |
| 2.2 | `ocr/backends/paddle.rs` | ✅ | Extracted recognition from `models/ocr.rs` |
| 2.3 | `ocr/postprocessor.rs` | ✅ | Merged confusables, added `CorrectionEvent` tracking |
| 2.4 | `ocr/pipeline.rs` | 🔄 | In progress |
| 2.5 | `ocr/mod.rs` | ⏳ | Pending |

### Phase 3: Update Integration

| Step | File | Status |
|------|------|--------|
| 3.1 | `models/ocr.rs` | ⏳ | Remove recognition, keep detection only |
| 3.2 | `pipeline/processor.rs` | ⏳ | Use `OcrPipeline` instead of `OCREngine` |
| 3.3 | `text_correction/` | ⏳ | Delete module |

### Phase 4: Cleanup

| Step | Status |
|------|--------|
| 4.1 Replace println with tracing | ⏳ |
| 4.2 Remove unused code | ⏳ |
| 4.3 cargo clippy | ⏳ |
| 4.4 Tests | ⏳ |

---

## Key Technical Decisions

### 1. Trait-based Recognition Backend

```rust
pub trait TextRecognizer: Send {
    fn recognize(&mut self, image: &DynamicImage) -> Result<RecognitionResult>;
    fn name(&self) -> &str;
    fn expected_height(&self) -> u32;
    fn warmup(&mut self) -> Result<()> { Ok(()) }
}
```

**Why:** Enables swapping backends (PaddleOCR, Tesseract, TrOCR) without changing pipeline code.

### 2. Two-Pass Confusable Correction

```rust
fn correct_confusables(&mut self, text: &str) -> String {
    // First pass: collect tokens
    // Second pass: correct words
}
```

**Why:** Avoids borrow checker conflict - can't hold `&self.dictionary` while calling `&mut self.correct_word()`.

### 3. Scoped Tensor Extraction

```rust
let (shape, data) = {
    let outputs = self.session.run(...)?;
    let tensor = outputs[0].try_extract_tensor::<f32>()?;
    (tensor.0.to_vec(), tensor.1.to_vec())
};
// outputs dropped here, safe to call self.decode_ctc()
```

**Why:** ONNX Runtime's `SessionOutputs` holds mutable borrow of session. Scoping ensures it drops before next method call.

### 4. Normalization as Standalone Function

```rust
pub fn normalize_image(image: &DynamicImage) -> Array4<f32>
```

**Why:** Pure utility, no state needed. Can be called from any backend without duplication.

---

## Bugs Fixed During Implementation

| Location | Bug | Fix |
|----------|-----|-----|
| `paddle.rs:35` | Error message referenced wrong variable | Changed `model_path` to `dict_path` |
| `postprocessor.rs:95` | `process(&self, ...)` should be mutable | Changed to `&mut self` |
| `paddle.rs:98-102` | Used `.shape()` on tuple | Changed to `tensor.0.to_vec()` |
| `paddle.rs:101` | Used `.unwrap()` | Changed to `.context()?` |

---

## Files Created/Modified

**Created:**
- `pdf-mash/src/ocr/backends/mod.rs`
- `pdf-mash/src/ocr/backends/paddle.rs`

**Modified:**
- `pdf-mash/src/ocr/config.rs` - added 3 fields
- `pdf-mash/src/ocr/preprocessor.rs` - added `normalize_image()`
- `pdf-mash/src/ocr/postprocessor.rs` - merged confusables, added correction tracking
- `pdf-mash/src/ocr/mod.rs` - uncommented backends

---

## Current Compilation Status

```
cargo check: ✅ PASS (4 warnings - all expected dead code in text_correction/)
```

---

## Next Steps

1. Complete `ocr/pipeline.rs` orchestrator
2. Update `ocr/mod.rs` exports
3. Slim down `models/ocr.rs` to detection only
4. Update `pipeline/processor.rs` to use new `OcrPipeline`
5. Delete `text_correction/` module
6. Cleanup: tracing, clippy, tests
