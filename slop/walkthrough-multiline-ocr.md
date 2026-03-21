# Multiline OCR Implementation Walkthrough

## Mental Model

Right now, your OCR pipeline assumes every crop is a single line of text. Multi-line paragraphs get fed directly to the recognizer, which expects 48-pixel-tall single lines. Result: empty strings or garbage.

The fix: add a **detection** step before recognition. When a crop is "tall" (>80px), we run the DBNet detector to find individual text lines, then recognize each line separately.

```
BEFORE:  tall_crop → recognizer → '' (conf: 0.00)
AFTER:   tall_crop → detector → [line1, line2, line3] → recognizer × 3 → joined text
```

## Build Order

1. **DetectedLine struct + TextDetector trait** — Define what line detection returns
2. **PaddleLineDetector** — Wrap the DBNet ONNX model
3. **Module exports** — Wire up the new detector
4. **OcrPipeline updates** — Route tall images through detection
5. **Processor init** — Load the detector model at startup
6. **Test & verify** — Run on a multi-line PDF

## Known Dragons

- **Wrong input name**: PP-OCRv5 det model expects `"x"` not `"pixel_values"`
- **Output shape**: Detection outputs `[1, 1, H, W]` probability map, not boxes
- **Scale factor**: Must track original → resized ratio to unproject boxes
- **Connected components**: Flood fill can be slow on large images (we'll use a simple impl first)

---

## Step 1: Add DetectedLine and TextDetector trait

**Pattern Recognition:**
```rust
// Similar to RecognitionResult, but for detection output
pub struct DetectedLine {
    pub bbox: (f32, f32, f32, f32), // x1, y1, x2, y2
    pub confidence: f32,
}

pub trait TextDetector: Send {
    fn detect_lines(&mut self, image: &DynamicImage) -> Result<Vec<DetectedLine>>;
    // ...
}
```

**Let's code:**

File: `pdf-mash/src/ocr/traits.rs`

```rust
// After RecognitionResult struct (around line 23):

// TODO: Add DetectedLine struct with bbox (x1, y1, x2, y2) and confidence fields

// TODO: Add TextDetector trait with:
//   - detect_lines(&mut self, image: &DynamicImage) -> Result<Vec<DetectedLine>>
//   - name(&self) -> &str
//   - warmup(&mut self) -> Result<()> with default Ok(()) implementation
```

**Verify:**
`cargo check` should pass (we haven't broken anything yet)

---

## Step 2: Create PaddleLineDetector

**Pattern Recognition:**
This is structurally similar to `PaddleRecognizer`, but:
- Input: full RGB image (not height-48 strip)
- Output: probability map → bounding boxes (not text)

**Let's code:**

File: `pdf-mash/src/ocr/backends/paddle_detector.rs` (NEW FILE)

```rust
//! DBNet-based text line detection using PP-OCRv5
//!
//! Takes a paragraph/block image, returns individual line bounding boxes.

use crate::ocr::traits::{DetectedLine, TextDetector};
use anyhow::{Context, Result};
use image::{DynamicImage, GenericImageView};
use ndarray::Array4;
use ort::execution_providers::CUDAExecutionProvider;
use ort::session::{builder::GraphOptimizationLevel, Session};
use tracing::{debug, info};

// TODO: Define DetectorConfig struct with fields:
//   - target_size: u32 (960)
//   - thresh: f32 (0.3)
//   - box_thresh: f32 (0.5)
//   - unclip_ratio: f32 (1.5)
//   - min_size: u32 (3)

// TODO: Implement Default for DetectorConfig

// TODO: Define PaddleLineDetector struct with:
//   - session: Session
//   - config: DetectorConfig

// TODO: Implement PaddleLineDetector::new(model_path, use_cuda)
//   - Build Session with CUDA if requested
//   - Load model from model_path
//   - Return Self with default config

// TODO: Implement preprocess(&self, image) -> (Array4<f32>, scale, new_w, new_h)
//   - Resize longest side to target_size, round to multiple of 32
//   - ImageNet normalize: mean=[0.485, 0.456, 0.406], std=[0.229, 0.224, 0.225]

// TODO: Implement postprocess(&self, prob_map, scale, orig_dims) -> Vec<DetectedLine>
//   - Binarize at thresh
//   - Find connected components
//   - Get bounding rect for each
//   - Filter by box_thresh and min_size
//   - Unclip (expand) boxes
//   - Sort by Y coordinate

// TODO: Implement TextDetector trait for PaddleLineDetector
//   - detect_lines: preprocess → session.run → postprocess
//   - name: "PaddleOCR-DBNet"
//   - warmup: run on dummy 640x480 image
```

This is the biggest piece. I'll provide the full implementation after you've structured it.

---

## Step 3: Update module exports

**Let's code:**

File: `pdf-mash/src/ocr/backends/mod.rs`

```rust
pub mod paddle;
// TODO: Add paddle_detector module

pub use paddle::PaddleRecognizer;
// TODO: Re-export PaddleLineDetector
```

File: `pdf-mash/src/ocr/mod.rs`

```rust
// At the bottom, add to re-exports:
// TODO: Add PaddleLineDetector to backends re-export
// TODO: Add DetectedLine and TextDetector to traits re-export
```

**Verify:**
`cargo check` — should fail because paddle_detector.rs is empty/missing

---

## Step 4: Add multiline routing to OcrPipeline

**Pattern Recognition:**
```rust
pub fn extract_text(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
    // Route based on height
    if height > 80 && self.line_detector.is_some() {
        self.extract_multiline(image)
    } else if width > max_width {
        self.extract_chunked(image)
    } else {
        self.extract_single_line(image)
    }
}
```

**Let's code:**

File: `pdf-mash/src/ocr/pipeline.rs`

```rust
// TODO: Add line_detector field to OcrPipeline struct:
//   line_detector: Option<PaddleLineDetector>

// TODO: Update new() to initialize line_detector as None

// TODO: Add builder method:
//   pub fn with_line_detector(mut self, model_path: &str, use_cuda: bool) -> Result<Self>

// TODO: Update extract_text() to route based on height:
//   - if height > 80 && line_detector.is_some() → extract_multiline()
//   - else if width > max_width → existing chunk logic
//   - else → single line

// TODO: Add extract_multiline() method:
//   - Call line_detector.detect_lines()
//   - For each line: crop, call extract_single_line()
//   - Join results with space, average confidence

// TODO: Refactor existing chunk logic into extract_chunked()

// TODO: Add extract_single_line() wrapping current single-chunk recognition
```

---

## Step 5: Update Processor to initialize line detector

**Let's code:**

File: `pdf-mash/src/pipeline/processor.rs`

```rust
// In Processor::new(), after creating ocr_pipeline:

// TODO: Chain .with_line_detector() to add detection capability:
//   .with_line_detector("models/pp-ocrv5_det_en.onnx", use_cuda)?
```

---

## Step 6: Test

```bash
cd pdf-mash
cargo run -- ../test.pdf -o ../test.md --verbose
```

**Expected change:**
- BEFORE: `RAW OCR: '' (conf: 0.00)` for paragraphs
- AFTER: `MULTILINE: Detected 3 lines` followed by actual text

---

## Files Checklist

| Action | File | Status |
|--------|------|--------|
| MODIFY | `src/ocr/traits.rs` | ⬜ |
| CREATE | `src/ocr/backends/paddle_detector.rs` | ⬜ |
| MODIFY | `src/ocr/backends/mod.rs` | ⬜ |
| MODIFY | `src/ocr/mod.rs` | ⬜ |
| MODIFY | `src/ocr/pipeline.rs` | ⬜ |
| MODIFY | `src/pipeline/processor.rs` | ⬜ |

---

Ready to start with Step 1?
