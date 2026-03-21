## Implementation Plan: Text Line Detection for Multi-line Blocks

### Problem Summary

`LayoutDetector` returns paragraph-level blocks (height 300-500px). `PaddleRecognizer` expects single-line crops (~48px height). When multi-line blocks get scaled to 48px, all lines collapse into unreadable garbage → 0.00 confidence.

### Architecture Change

```
CURRENT (broken for paragraphs):
LayoutDetector → crop block → scale to 48px → PaddleRecognizer
                              ↓
                    490px → 48px = all lines smashed together

PROPOSED:
LayoutDetector → crop block → [height > 80px?]
                                    ↓ YES
                              PaddleLineDetector → line bboxes
                                    ↓
                              for each line: crop → scale → recognize
                                    ↓
                              join lines with spaces
                                    ↓ NO
                              scale to 48px → PaddleRecognizer (existing path)
```

---

### Files to Create/Modify

#### 1. NEW: `src/ocr/backends/paddle_detector.rs`

```rust
use anyhow::{Context, Result};
use image::{DynamicImage, GenericImageView};
use ndarray::Array4;
use ort::execution_providers::CUDAExecutionProvider;
use ort::session::{builder::GraphOptimizationLevel, Session};

use crate::models::layout::BBox;

/// Detects individual text lines within a cropped region.
/// Uses PaddleOCR's detection model (DB-based text detector).
pub struct PaddleLineDetector {
    session: Session,
    /// Minimum confidence for line detection
    min_confidence: f32,
    /// Target size for detection model input
    target_size: u32,
}

impl PaddleLineDetector {
    pub fn new(model_path: &str, use_cuda: bool) -> Result<Self> {
        let mut builder = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?;

        if use_cuda {
            builder = builder.with_execution_providers([
                CUDAExecutionProvider::default().build().error_on_failure()
            ])?;
        }

        let session = builder
            .commit_from_file(model_path)
            .with_context(|| format!("Failed to load detection model: {}", model_path))?;

        Ok(Self {
            session,
            min_confidence: 0.5,
            target_size: 960, // PaddleOCR det uses 960
        })
    }

    /// Detect text lines in image, return bboxes sorted top-to-bottom
    pub fn detect_lines(&mut self, image: &DynamicImage) -> Result<Vec<BBox>> {
        let (orig_w, orig_h) = image.dimensions();
        
        // Preprocess: resize to model input size
        let input_tensor = self.preprocess(image)?;
        
        // Run inference
        let value = ort::value::Value::from_array(input_tensor)?;
        let outputs = self.session.run(ort::inputs!["x" => value])?;
        
        // Parse output (DB text detector outputs a probability map)
        let prob_map = outputs[0].try_extract_tensor::<f32>()?;
        
        // Convert probability map to bboxes
        let mut bboxes = self.prob_map_to_bboxes(
            &prob_map.1,
            prob_map.0.as_slice(),
            orig_w,
            orig_h,
        )?;
        
        // Sort by Y coordinate (top to bottom)
        bboxes.sort_by(|a, b| {
            a.y1.partial_cmp(&b.y1).unwrap_or(std::cmp::Ordering::Equal)
        });
        
        Ok(bboxes)
    }

    fn preprocess(&self, image: &DynamicImage) -> Result<Array4<f32>> {
        let (w, h) = image.dimensions();
        
        // Calculate resize to fit target_size while maintaining aspect ratio
        let scale = (self.target_size as f32) / (w.max(h) as f32);
        let new_w = ((w as f32 * scale) as u32).max(32);
        let new_h = ((h as f32 * scale) as u32).max(32);
        
        // Round to multiple of 32 (required by DB detector)
        let new_w = ((new_w + 31) / 32) * 32;
        let new_h = ((new_h + 31) / 32) * 32;
        
        let resized = image.resize_exact(
            new_w, new_h,
            image::imageops::FilterType::CatmullRom
        );
        
        let rgb = resized.to_rgb8();
        let mut array = Array4::<f32>::zeros((1, 3, new_h as usize, new_w as usize));
        
        // Normalize: (pixel - mean) / std
        // PaddleOCR uses mean=[0.485, 0.456, 0.406], std=[0.229, 0.224, 0.225]
        let mean = [0.485f32, 0.456, 0.406];
        let std = [0.229f32, 0.224, 0.225];
        
        for y in 0..new_h {
            for x in 0..new_w {
                let pixel = rgb.get_pixel(x, y);
                for c in 0..3 {
                    let val = pixel[c] as f32 / 255.0;
                    array[[0, c, y as usize, x as usize]] = (val - mean[c]) / std[c];
                }
            }
        }
        
        Ok(array)
    }

    fn prob_map_to_bboxes(
        &self,
        data: &[f32],
        shape: &[i64],
        orig_w: u32,
        orig_h: u32,
    ) -> Result<Vec<BBox>> {
        // DB detector output is [1, 1, H, W] probability map
        // We need to threshold and find contours
        // This is a simplified version - proper implementation uses OpenCV or imageproc
        
        let map_h = shape[2] as usize;
        let map_w = shape[3] as usize;
        
        let scale_x = orig_w as f32 / map_w as f32;
        let scale_y = orig_h as f32 / map_h as f32;
        
        let mut bboxes = Vec::new();
        let threshold = 0.3f32;
        
        // Simple run-length encoding to find connected components
        // (Production code would use proper contour detection)
        let mut visited = vec![false; map_h * map_w];
        
        for y in 0..map_h {
            for x in 0..map_w {
                let idx = y * map_w + x;
                if visited[idx] || data[idx] < threshold {
                    continue;
                }
                
                // Flood fill to find component bounds
                let (min_x, min_y, max_x, max_y) = self.flood_fill(
                    data, &mut visited, map_w, map_h, x, y, threshold
                );
                
                // Convert to original coordinates
                let bbox = BBox {
                    x1: min_x as f32 * scale_x,
                    y1: min_y as f32 * scale_y,
                    x2: max_x as f32 * scale_x,
                    y2: max_y as f32 * scale_y,
                    confidence: 0.9, // Could compute from prob values
                    class_id: 0,
                    class_name: String::from("text_line"),
                    unique_id: bboxes.len(),
                };
                
                // Filter tiny boxes
                if bbox.width() > 10.0 && bbox.height() > 5.0 {
                    bboxes.push(bbox);
                }
            }
        }
        
        Ok(bboxes)
    }

    fn flood_fill(
        &self,
        data: &[f32],
        visited: &mut [bool],
        w: usize,
        h: usize,
        start_x: usize,
        start_y: usize,
        threshold: f32,
    ) -> (usize, usize, usize, usize) {
        let mut stack = vec![(start_x, start_y)];
        let mut min_x = start_x;
        let mut min_y = start_y;
        let mut max_x = start_x;
        let mut max_y = start_y;
        
        while let Some((x, y)) = stack.pop() {
            let idx = y * w + x;
            if visited[idx] || data[idx] < threshold {
                continue;
            }
            
            visited[idx] = true;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            
            // Check 4-connected neighbors
            if x > 0 { stack.push((x - 1, y)); }
            if x + 1 < w { stack.push((x + 1, y)); }
            if y > 0 { stack.push((x, y - 1)); }
            if y + 1 < h { stack.push((x, y + 1)); }
        }
        
        (min_x, min_y, max_x + 1, max_y + 1)
    }

    pub fn warmup(&mut self) -> Result<()> {
        let dummy = DynamicImage::new_rgb8(320, 320);
        let _ = self.detect_lines(&dummy)?;
        Ok(())
    }
}
```

#### 2. MODIFY: `src/ocr/backends/mod.rs`

```rust
pub mod paddle;
pub mod paddle_detector;  // ADD

pub use paddle::PaddleRecognizer;
pub use paddle_detector::PaddleLineDetector;  // ADD
```

#### 3. MODIFY: `src/ocr/pipeline.rs`

```rust
use anyhow::Result;
use image::{DynamicImage, GenericImageView};
use tracing::{debug, warn};

use crate::ocr::{
    backends::PaddleLineDetector,  // ADD
    config::OcrConfig,
    postprocessor::{CorrectionEvent, Postprocessor},
    preprocessor::Preprocessor,
    traits::{RecognitionResult, TextRecognizer},
};

pub struct OcrPipeline<R: TextRecognizer> {
    config: OcrConfig,
    preprocessor: Preprocessor,
    line_detector: Option<PaddleLineDetector>,  // ADD
    recognizer: R,
    postprocessor: Postprocessor,
}

impl<R: TextRecognizer> OcrPipeline<R> {
    pub fn new(
        config: OcrConfig,
        recognizer: R,
        line_detector: Option<PaddleLineDetector>,  // ADD PARAMETER
        dictionary_path: Option<&str>,
    ) -> Result<Self> {
        let preprocessor = Preprocessor::new(config.clone());
        let postprocessor = Postprocessor::new(config.clone(), dictionary_path)?;

        Ok(Self {
            config,
            preprocessor,
            line_detector,  // ADD
            recognizer,
            postprocessor,
        })
    }

    /// Height threshold: blocks taller than this need line detection
    const MULTILINE_HEIGHT_THRESHOLD: u32 = 80;

    pub fn extract_text(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
        let (width, height) = image.dimensions();
        
        if width < self.config.min_crop_width || height < self.config.min_crop_height {
            debug!("Image too small ({}x{}), skipping", width, height);
            return Ok(RecognitionResult::empty());
        }

        // DECISION POINT: single-line vs multi-line
        let is_multiline = height > Self::MULTILINE_HEIGHT_THRESHOLD;
        
        if is_multiline {
            self.extract_multiline(image)
        } else {
            self.extract_single_line(image)
        }
    }

    /// Multi-line path: detect lines first, then recognize each
    fn extract_multiline(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
        let Some(detector) = &mut self.line_detector else {
            // No detector available, fall back to single-line (will likely fail)
            warn!("Multi-line block but no line detector available, falling back");
            return self.extract_single_line(image);
        };

        let (width, height) = image.dimensions();
        debug!("Multi-line block {}x{}, detecting lines", width, height);

        // Detect text lines
        let line_bboxes = detector.detect_lines(image)?;
        
        if line_bboxes.is_empty() {
            debug!("No lines detected in block");
            return Ok(RecognitionResult::empty());
        }

        debug!("Detected {} text lines", line_bboxes.len());

        // Recognize each line
        let mut line_texts = Vec::with_capacity(line_bboxes.len());
        let mut total_confidence = 0.0;
        let mut valid_lines = 0;

        for (i, bbox) in line_bboxes.iter().enumerate() {
            // Crop line from image
            let x = (bbox.x1 as u32).min(width.saturating_sub(1));
            let y = (bbox.y1 as u32).min(height.saturating_sub(1));
            let w = ((bbox.x2 - bbox.x1) as u32).min(width - x);
            let h = ((bbox.y2 - bbox.y1) as u32).min(height - y);

            if w < 10 || h < 5 {
                continue;
            }

            let line_crop = image.crop_imm(x, y, w, h);
            
            // Use single-line recognition on each line crop
            let result = self.extract_single_line(&line_crop)?;
            
            if !result.is_empty() {
                debug!("Line {}: '{}' (conf: {:.2})", i + 1, result.text, result.confidence);
                line_texts.push(result.text);
                total_confidence += result.confidence;
                valid_lines += 1;
            }
        }

        if line_texts.is_empty() {
            return Ok(RecognitionResult::empty());
        }

        // Join lines with spaces
        let merged_text = line_texts.join(" ");
        let avg_confidence = total_confidence / valid_lines as f32;

        // Post-process the merged text
        let corrected = self.postprocessor.process(&merged_text);
        
        eprintln!(
            "MULTILINE OCR: {} lines → '{}' (conf: {:.2})",
            valid_lines,
            truncate(&corrected, 50),
            avg_confidence
        );

        Ok(RecognitionResult {
            text: corrected,
            confidence: avg_confidence,
            char_confidences: None,
        })
    }

    /// Single-line path: existing chunking logic
    fn extract_single_line(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
        let (width, height) = image.dimensions();
        
        let chunks = self.preprocessor.prepare(image)?;
        let num_chunks = chunks.len();

        if num_chunks > 1 {
            debug!("Split wide image into {} chunks", num_chunks);
        }

        let mut chunk_results = Vec::with_capacity(num_chunks);
        for chunk in &chunks {
            let result = self.recognizer.recognize(&chunk.image)?;
            chunk_results.push(result);
        }

        let merged = self.merge_chunks(&chunk_results);

        eprintln!(
            "RAW OCR: '{}' (conf: {:.2}) [{}x{}, {} chunks]",
            merged.text, merged.confidence, width, height, num_chunks
        );

        if merged.confidence < self.config.min_confidence {
            warn!(
                "Low confidence {:.2}, discarding: '{}'",
                merged.confidence,
                truncate(&merged.text, 30)
            );
            return Ok(RecognitionResult::empty());
        }

        let corrected_text = self.postprocessor.process(&merged.text);
        eprintln!("CORRECTED: '{}'", corrected_text);
        
        Ok(RecognitionResult {
            text: corrected_text,
            confidence: merged.confidence,
            char_confidences: merged.char_confidences,
        })
    }

    fn merge_chunks(&self, results: &[RecognitionResult]) -> RecognitionResult {
        // ... existing implementation unchanged ...
    }

    // ... rest of methods unchanged ...
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        String::from(s)
    } else {
        format!("{}...", &s[..max_len])
    }
}
```

#### 4. MODIFY: `src/ocr/mod.rs`

```rust
pub mod backends;
pub mod config;
pub mod pipeline;
pub mod postprocessor;
pub mod preprocessor;
pub mod traits;

// Re-exports
pub use backends::PaddleRecognizer;
pub use backends::PaddleLineDetector;  // ADD
pub use config::OcrConfig;
pub use pipeline::OcrPipeline;
// ... rest unchanged
```

#### 5. MODIFY: `src/pipeline/processor.rs`

```rust
use crate::models::layout::{BBox, LayoutDetector};
use crate::ocr::{
    backends::{PaddleRecognizer, PaddleLineDetector},  // ADD
    OcrConfig, OcrPipeline
};
// ... other imports unchanged ...

pub struct Processor {
    pdf_parser: PdfParser,
    layout_detector: LayoutDetector,
    markdown_generator: MarkdownGenerator,
    ocr_pipeline: OcrPipeline<PaddleRecognizer>,
}

impl Processor {
    pub fn new(layout_model_path: &str, use_cuda: bool) -> Result<Self> {
        let ocr_config = OcrConfig::default();
        
        let recognizer = PaddleRecognizer::new(
            "models/pp-ocrv5_rec_en.onnx",
            "models/en_dict.txt",
            use_cuda,
        )?;
        
        // ADD: Create line detector
        let line_detector = PaddleLineDetector::new(
            "models/pp-ocrv5_det_en.onnx",  // or paddle_ocr_det.onnx
            use_cuda,
        )?;
        
        let ocr_pipeline = OcrPipeline::new(
            ocr_config,
            recognizer,
            Some(line_detector),  // ADD
            Some("data/frequency_dictionary_en_82_765.txt"),
        )?;

        Ok(Self {
            pdf_parser: PdfParser::new()?,
            layout_detector: LayoutDetector::new(layout_model_path, use_cuda)?,
            markdown_generator: MarkdownGenerator::new(),
            ocr_pipeline,
        })
    }
    
    // ... rest unchanged ...
}
```

---

### Build Order

1. Create `src/ocr/backends/paddle_detector.rs`
2. Update `src/ocr/backends/mod.rs` 
3. Update `src/ocr/mod.rs`
4. Update `src/ocr/pipeline.rs` (change signature and add multiline logic)
5. Update `src/pipeline/processor.rs` (pass line detector)
6. `cargo build` and test

---

### Model File to Use

Based on your available models:
```
models/pp-ocrv5_det_en.onnx  (88 MB) ← Use this (newer, matches rec model)
models/paddle_ocr_det.onnx   (4.7 MB) ← Fallback (older, smaller)
```

The 88MB model is PP-OCRv5 detection which should pair well with your PP-OCRv5 recognition model.

---

### Key Design Decisions

1. **Threshold of 80px**: Recognition model expects 48px. Anything >1.5x that (72px, rounded to 80) is likely multi-line.

2. **Line detector is Optional**: `Option<PaddleLineDetector>` so pipeline works without it (graceful degradation).

3. **Postprocessing happens AFTER line joining**: Segmentation/spelling correction works on complete text, not per-line.

4. **Simple contour detection**: The `prob_map_to_bboxes` uses flood fill. Production would use proper contour extraction (OpenCV or imageproc crate). This is a known simplification.

5. **Lines joined with single space**: Could be improved with newline detection for paragraphs.

---

### Testing

After implementation:

```bash
# Should see:
# - "Multi-line block 1269x359, detecting lines"
# - "Detected 8 text lines"
# - "Line 1: 'First line of text' (conf: 0.95)"
# - "MULTILINE OCR: 8 lines → 'First line of text Second line...' (conf: 0.94)"

cargo run -- test.pdf -o test.md
```

Compare before/after on a document with paragraphs.