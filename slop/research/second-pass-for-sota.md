# Updated Implementation Plan for pdf-mash

## Current Codebase Status

```
pdf-mash/src/
├── api/mod.rs                    # ⚠️ EMPTY - needs implementation
├── cli/
│   ├── args.rs                   # ✓ Complete
│   └── mod.rs                    # ✓ Complete
├── lib.rs                        # ✓ Complete
├── main.rs                       # ✓ Complete
├── models/
│   ├── layout.rs                 # ✓ Complete (DocLayout-YOLO)
│   └── mod.rs                    # ✓ Complete
├── ocr/
│   ├── backends/
│   │   ├── mod.rs                # ✓ Needs paddle_detector export
│   │   └── paddle.rs             # ✓ Complete (PaddleRecognizer)
│   ├── config.rs                 # ✓ Complete
│   ├── mod.rs                    # ✓ Needs detector export
│   ├── pipeline.rs               # ⚠️ MISSING multiline routing
│   ├── postprocessor.rs          # ✓ Complete
│   ├── preprocessor.rs           # ✓ Complete
│   └── traits.rs                 # ✓ Needs TextDetector trait
├── pipeline/
│   ├── markdown_generator.rs     # ✓ Complete
│   ├── mod.rs                    # ✓ Complete
│   ├── pdf_parser.rs             # ✓ Complete
│   ├── processor.rs              # ✓ Complete
│   └── reading_order.rs          # ✓ Complete
├── text_correction/              # ⚠️ REDUNDANT with ocr/postprocessor
│   ├── confusables.rs
│   └── mod.rs
└── utils/
    ├── deduplication.rs          # ✓ Complete
    ├── gpu_monitor.rs            # ✓ Complete
    ├── mod.rs                    # ✓ Complete
    ├── preprocessing.rs          # ✓ Complete
    └── visualization.rs          # ✓ Complete
```

---

## Phase 1: Fix Multi-line OCR (CRITICAL)
**Time: 3-4 hours | Priority: BLOCKING**

This is the root cause of your OCR failures. Without this, multi-line text blocks return empty strings.

### 1.1 Add TextDetector Trait

**File:** `src/ocr/traits.rs` (modify)

```rust
// Add after RecognitionResult

/// Detected text line with bounding box
#[derive(Debug, Clone)]
pub struct DetectedLine {
    pub bbox: (f32, f32, f32, f32), // x1, y1, x2, y2
    pub confidence: f32,
}

/// Trait for text line detection backends
pub trait TextDetector: Send {
    fn detect_lines(&mut self, image: &DynamicImage) -> Result<Vec<DetectedLine>>;
    fn name(&self) -> &str;
    fn warmup(&mut self) -> Result<()> { Ok(()) }
}
```

### 1.2 Create PaddleLineDetector

**File:** `src/ocr/backends/paddle_detector.rs` (NEW)

```rust
//! DBNet-based text line detection using PP-OCRv5
//! 
//! Takes a paragraph/block image, returns individual line bounding boxes.

use crate::ocr::traits::{DetectedLine, TextDetector};
use anyhow::{Context, Result};
use image::{DynamicImage, GenericImageView, GrayImage};
use ndarray::Array4;
use ort::execution_providers::CUDAExecutionProvider;
use ort::session::{builder::GraphOptimizationLevel, Session};
use tracing::{debug, info};

pub struct PaddleLineDetector {
    session: Session,
    config: DetectorConfig,
}

#[derive(Debug, Clone)]
pub struct DetectorConfig {
    pub target_size: u32,      // Resize longest side to this (960)
    pub thresh: f32,           // Binarization threshold (0.3)
    pub box_thresh: f32,       // Min confidence to keep box (0.5)
    pub unclip_ratio: f32,     // Polygon expansion factor (1.5)
    pub min_size: u32,         // Min box dimension (3)
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            target_size: 960,
            thresh: 0.3,
            box_thresh: 0.5,
            unclip_ratio: 1.5,
            min_size: 3,
        }
    }
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

        info!("PaddleOCR line detector initialized (CUDA: {})", use_cuda);

        Ok(Self {
            session,
            config: DetectorConfig::default(),
        })
    }

    /// Preprocess image for DBNet
    /// - Resize preserving aspect ratio
    /// - Pad to multiple of 32
    /// - Normalize with ImageNet mean/std
    fn preprocess(&self, image: &DynamicImage) -> Result<(Array4<f32>, f32, u32, u32)> {
        let (orig_w, orig_h) = image.dimensions();
        
        // Scale to target size
        let scale = self.config.target_size as f32 / orig_w.max(orig_h) as f32;
        let new_w = ((orig_w as f32 * scale) as u32 / 32) * 32;  // Round to 32
        let new_h = ((orig_h as f32 * scale) as u32 / 32) * 32;
        let new_w = new_w.max(32);
        let new_h = new_h.max(32);

        let resized = image.resize_exact(
            new_w, new_h,
            image::imageops::FilterType::CatmullRom  // Faster than Lanczos3
        );

        let rgb = resized.to_rgb8();
        let mut array = Array4::<f32>::zeros((1, 3, new_h as usize, new_w as usize));

        // ImageNet normalization
        let mean = [0.485, 0.456, 0.406];
        let std = [0.229, 0.224, 0.225];

        for y in 0..new_h {
            for x in 0..new_w {
                let pixel = rgb.get_pixel(x, y);
                for c in 0..3 {
                    let val = pixel[c] as f32 / 255.0;
                    array[[0, c, y as usize, x as usize]] = (val - mean[c]) / std[c];
                }
            }
        }

        Ok((array, scale, new_w, new_h))
    }

    /// Convert probability map to bounding boxes
    fn postprocess(
        &self,
        prob_map: &[f32],
        map_h: usize,
        map_w: usize,
        scale: f32,
        orig_w: u32,
        orig_h: u32,
    ) -> Vec<DetectedLine> {
        // Binarize probability map
        let mut binary: Vec<u8> = prob_map
            .iter()
            .map(|&p| if p > self.config.thresh { 255 } else { 0 })
            .collect();

        // Find contours using connected components
        let contours = self.find_contours(&binary, map_w, map_h);

        let mut lines = Vec::new();

        for contour in contours {
            if contour.len() < 4 {
                continue;
            }

            // Get bounding rect
            let (min_x, min_y, max_x, max_y) = self.bounding_rect(&contour);
            
            // Check minimum size
            let width = max_x - min_x;
            let height = max_y - min_y;
            if width < self.config.min_size as f32 || height < self.config.min_size as f32 {
                continue;
            }

            // Calculate confidence as mean probability in region
            let confidence = self.region_confidence(prob_map, map_w, min_x, min_y, max_x, max_y);
            if confidence < self.config.box_thresh {
                continue;
            }

            // Unclip (expand) the bounding box
            let area = width * height;
            let perimeter = 2.0 * (width + height);
            let distance = area * self.config.unclip_ratio / perimeter;

            let x1 = ((min_x - distance) / scale).max(0.0);
            let y1 = ((min_y - distance) / scale).max(0.0);
            let x2 = ((max_x + distance) / scale).min(orig_w as f32);
            let y2 = ((max_y + distance) / scale).min(orig_h as f32);

            lines.push(DetectedLine {
                bbox: (x1, y1, x2, y2),
                confidence,
            });
        }

        // Sort by Y coordinate (top to bottom)
        lines.sort_by(|a, b| a.bbox.1.partial_cmp(&b.bbox.1).unwrap());

        lines
    }

    /// Simple connected component labeling for contour finding
    fn find_contours(&self, binary: &[u8], width: usize, height: usize) -> Vec<Vec<(f32, f32)>> {
        let mut visited = vec![false; width * height];
        let mut contours = Vec::new();

        for y in 0..height {
            for x in 0..width {
                let idx = y * width + x;
                if binary[idx] == 255 && !visited[idx] {
                    let contour = self.flood_fill(binary, &mut visited, width, height, x, y);
                    if !contour.is_empty() {
                        contours.push(contour);
                    }
                }
            }
        }

        contours
    }

    fn flood_fill(
        &self,
        binary: &[u8],
        visited: &mut [bool],
        width: usize,
        height: usize,
        start_x: usize,
        start_y: usize,
    ) -> Vec<(f32, f32)> {
        let mut stack = vec![(start_x, start_y)];
        let mut points = Vec::new();

        while let Some((x, y)) = stack.pop() {
            let idx = y * width + x;
            if visited[idx] || binary[idx] != 255 {
                continue;
            }

            visited[idx] = true;
            points.push((x as f32, y as f32));

            // 4-connected neighbors
            if x > 0 { stack.push((x - 1, y)); }
            if x < width - 1 { stack.push((x + 1, y)); }
            if y > 0 { stack.push((x, y - 1)); }
            if y < height - 1 { stack.push((x, y + 1)); }
        }

        points
    }

    fn bounding_rect(&self, points: &[(f32, f32)]) -> (f32, f32, f32, f32) {
        let min_x = points.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
        let min_y = points.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
        let max_x = points.iter().map(|p| p.0).fold(f32::NEG_INFINITY, f32::max);
        let max_y = points.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max);
        (min_x, min_y, max_x, max_y)
    }

    fn region_confidence(
        &self,
        prob_map: &[f32],
        width: usize,
        x1: f32, y1: f32, x2: f32, y2: f32,
    ) -> f32 {
        let mut sum = 0.0;
        let mut count = 0;

        for y in (y1 as usize)..(y2 as usize).min(prob_map.len() / width) {
            for x in (x1 as usize)..(x2 as usize).min(width) {
                sum += prob_map[y * width + x];
                count += 1;
            }
        }

        if count > 0 { sum / count as f32 } else { 0.0 }
    }
}

impl TextDetector for PaddleLineDetector {
    fn detect_lines(&mut self, image: &DynamicImage) -> Result<Vec<DetectedLine>> {
        let (orig_w, orig_h) = image.dimensions();
        
        // Preprocess
        let (input_tensor, scale, new_w, new_h) = self.preprocess(image)?;
        
        // Run inference
        let value = ort::value::Value::from_array(input_tensor)?;
        let outputs = self.session.run(ort::inputs!["x" => value])?;
        
        // Extract probability map
        let output = outputs[0].try_extract_tensor::<f32>()?;
        let shape = output.0.clone();
        let data = output.1.to_vec();
        
        // Output shape is [1, 1, H, W]
        let map_h = shape[2] as usize;
        let map_w = shape[3] as usize;
        
        // Postprocess to get bounding boxes
        let lines = self.postprocess(&data, map_h, map_w, scale, orig_w, orig_h);
        
        debug!("Detected {} text lines", lines.len());
        
        Ok(lines)
    }

    fn name(&self) -> &str {
        "PaddleOCR-DBNet"
    }

    fn warmup(&mut self) -> Result<()> {
        let dummy = DynamicImage::new_rgb8(640, 480);
        let _ = self.detect_lines(&dummy)?;
        Ok(())
    }
}
```

### 1.3 Update Module Exports

**File:** `src/ocr/backends/mod.rs` (modify)

```rust
pub mod paddle;
pub mod paddle_detector;

pub use paddle::PaddleRecognizer;
pub use paddle_detector::PaddleLineDetector;
```

**File:** `src/ocr/mod.rs` (modify)

```rust
pub mod backends;
pub mod config;
pub mod pipeline;
pub mod postprocessor;
pub mod preprocessor;
pub mod traits;

// Re-exports
pub use backends::{PaddleRecognizer, PaddleLineDetector};
pub use config::OcrConfig;
pub use pipeline::OcrPipeline;
pub use postprocessor::{CorrectionEvent, Postprocessor};
pub use preprocessor::Preprocessor;
pub use traits::{RecognitionResult, TextRecognizer, DetectedLine, TextDetector};
```

### 1.4 Update OcrPipeline for Multi-line

**File:** `src/ocr/pipeline.rs` (modify)

```rust
use anyhow::Result;
use image::{DynamicImage, GenericImageView};
use tracing::{debug, warn};

use crate::ocr::{
    backends::PaddleLineDetector,
    config::OcrConfig,
    postprocessor::{CorrectionEvent, Postprocessor},
    preprocessor::Preprocessor,
    traits::{RecognitionResult, TextRecognizer, TextDetector},
};

pub struct OcrPipeline<R: TextRecognizer> {
    config: OcrConfig,
    preprocessor: Preprocessor,
    recognizer: R,
    postprocessor: Postprocessor,
    line_detector: Option<PaddleLineDetector>,  // NEW
}

impl<R: TextRecognizer> OcrPipeline<R> {
    pub fn new(
        config: OcrConfig, 
        recognizer: R, 
        dictionary_path: Option<&str>,
    ) -> Result<Self> {
        let preprocessor = Preprocessor::new(config.clone());
        let postprocessor = Postprocessor::new(config.clone(), dictionary_path)?;

        Ok(Self {
            config,
            preprocessor,
            recognizer,
            postprocessor,
            line_detector: None,  // Lazy initialization
        })
    }

    /// Initialize line detector (call once at startup)
    pub fn with_line_detector(mut self, model_path: &str, use_cuda: bool) -> Result<Self> {
        self.line_detector = Some(PaddleLineDetector::new(model_path, use_cuda)?);
        Ok(self)
    }

    pub fn extract_text(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
        let (width, height) = image.dimensions();
        
        if width < self.config.min_crop_width || height < self.config.min_crop_height {
            debug!("Image too small ({}x{}), skipping", width, height);
            return Ok(RecognitionResult::empty());
        }

        // KEY DECISION: Route based on height
        let result = if height > 80 && self.line_detector.is_some() {
            // Multi-line: detect lines first, then recognize each
            self.extract_multiline(image)?
        } else if width > self.config.max_recognition_width as u32 {
            // Wide single line: chunk horizontally
            self.extract_chunked(image)?
        } else {
            // Simple single line
            self.extract_single_line(image)?
        };

        // Log raw result
        eprintln!(
            "RAW OCR: '{}' (conf: {:.2}) [{}x{}]",
            result.text, result.confidence, width, height
        );

        if result.confidence < self.config.min_confidence {
            warn!(
                "Low confidence {:.2}, discarding: '{}'",
                result.confidence,
                truncate(&result.text, 30)
            );
            return Ok(RecognitionResult::empty());
        }

        // Post-process (spelling, confusables)
        let corrected_text = self.postprocessor.process(&result.text);
        eprintln!("CORRECTED: '{}'", corrected_text);
        
        Ok(RecognitionResult {
            text: corrected_text,
            confidence: result.confidence,
            char_confidences: result.char_confidences,
        })
    }

    /// NEW: Handle multi-line text blocks
    fn extract_multiline(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
        let detector = self.line_detector.as_mut()
            .expect("Line detector not initialized");

        // Detect individual lines
        let lines = detector.detect_lines(image)?;
        
        if lines.is_empty() {
            debug!("No lines detected in multi-line block");
            return Ok(RecognitionResult::empty());
        }

        eprintln!("MULTILINE: Detected {} lines", lines.len());

        let mut all_text = Vec::new();
        let mut total_confidence = 0.0;
        let mut line_count = 0;

        for (idx, line) in lines.iter().enumerate() {
            let (x1, y1, x2, y2) = line.bbox;
            
            // Crop line from image
            let x = (x1 as u32).min(image.width().saturating_sub(1));
            let y = (y1 as u32).min(image.height().saturating_sub(1));
            let w = ((x2 - x1) as u32).min(image.width() - x).max(1);
            let h = ((y2 - y1) as u32).min(image.height() - y).max(1);
            
            let line_img = image.crop_imm(x, y, w, h);
            
            // Recognize single line
            let result = self.extract_single_line(&line_img)?;
            
            if !result.text.is_empty() {
                eprintln!("  Line {}: '{}' (conf: {:.2})", idx + 1, result.text, result.confidence);
                all_text.push(result.text);
                total_confidence += result.confidence;
                line_count += 1;
            }
        }

        let merged_text = all_text.join(" ");
        let avg_confidence = if line_count > 0 {
            total_confidence / line_count as f32
        } else {
            0.0
        };

        Ok(RecognitionResult {
            text: merged_text,
            confidence: avg_confidence,
            char_confidences: None,
        })
    }

    /// Handle wide single-line images by chunking
    fn extract_chunked(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
        let chunks = self.preprocessor.prepare(image)?;
        let num_chunks = chunks.len();

        debug!("Split wide image into {} chunks", num_chunks);

        let mut chunk_results = Vec::with_capacity(num_chunks);
        for chunk in &chunks {
            let result = self.recognizer.recognize(&chunk.image)?;
            chunk_results.push(result);
        }

        Ok(self.merge_chunks(&chunk_results))
    }

    /// Simple single-line recognition
    fn extract_single_line(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
        // Scale to target height
        let chunks = self.preprocessor.prepare(image)?;
        
        if chunks.is_empty() {
            return Ok(RecognitionResult::empty());
        }

        // Should be single chunk for single line
        self.recognizer.recognize(&chunks[0].image)
    }

    fn merge_chunks(&self, results: &[RecognitionResult]) -> RecognitionResult {
        if results.is_empty() {
            return RecognitionResult::empty();
        }

        if results.len() == 1 {
            return results[0].clone();
        }

        let overlap_chars = (self.config.chunk_overlap / 10) as usize;

        let mut merged_text = String::new();
        let mut total_confidence = 0.0;

        for (i, result) in results.iter().enumerate() {
            if i == 0 {
                merged_text.push_str(&result.text);
            } else if result.text.len() > overlap_chars {
                merged_text.push_str(&result.text[overlap_chars..]);
            }
            total_confidence += result.confidence;
        }

        RecognitionResult {
            text: merged_text,
            confidence: total_confidence / results.len() as f32,
            char_confidences: None,
        }
    }

    pub fn get_corrections(&self) -> &[CorrectionEvent] {
        self.postprocessor.get_corrections()
    }

    pub fn clear_corrections(&mut self) {
        self.postprocessor.clear_corrections();
    }

    pub fn export_corrections(&self, path: &str) -> std::io::Result<()> {
        self.postprocessor.export_corrections(path)
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        String::from(s)
    } else {
        format!("{}...", &s[..max_len])
    }
}
```

### 1.5 Update Processor to Initialize Line Detector

**File:** `src/pipeline/processor.rs` (modify constructor)

```rust
impl Processor {
    pub fn new(layout_model_path: &str, use_cuda: bool) -> Result<Self> {
        let ocr_config = OcrConfig::default();
        let recognizer = PaddleRecognizer::new(
            "models/pp-ocrv5_rec_en.onnx",
            "models/en_dict.txt",
            use_cuda,
        )?;
        
        // Initialize pipeline WITH line detector
        let ocr_pipeline = OcrPipeline::new(
            ocr_config,
            recognizer,
            Some("data/frequency_dictionary_en_82_765.txt"),
        )?
        .with_line_detector("models/pp-ocrv5_det_en.onnx", use_cuda)?;  // NEW

        Ok(Self {
            pdf_parser: PdfParser::new()?,
            layout_detector: LayoutDetector::new(layout_model_path, use_cuda)?,
            markdown_generator: MarkdownGenerator::new(),
            ocr_pipeline,
        })
    }
    // ... rest unchanged
}
```

### 1.6 Download Detection Model

```bash
# From Hugging Face
wget -O models/pp-ocrv5_det_en.onnx \
  "https://huggingface.co/monkt/paddleocr-onnx/resolve/main/ppocrv5/det/en/ppocrv5_det_en.onnx"

# Verify
ls -la models/pp-ocrv5_det_en.onnx
# Should be ~4MB
```

### 1.7 Test the Fix

```bash
cargo run -- test.pdf -o test.md --verbose
```

**Expected change:**
- Before: Multi-line blocks → `''` (conf: 0.00)
- After: Multi-line blocks → `'Detected text from line 1 line 2 line 3...'` (conf: 0.85+)

---

## Phase 2: Table Extraction
**Time: 4-5 hours | Priority: HIGH**

### 2.1 Create Table Extractor

**File:** `src/models/table.rs` (NEW)

```rust
//! Table extraction using Microsoft Table Transformer
//! 
//! Two-stage: detection (find tables) + structure (parse cells)

use anyhow::{Context, Result};
use image::{DynamicImage, GenericImageView};
use ndarray::Array4;
use ort::execution_providers::CUDAExecutionProvider;
use ort::session::{builder::GraphOptimizationLevel, Session};

#[derive(Debug, Clone)]
pub struct TableCell {
    pub row: usize,
    pub col: usize,
    pub bbox: (f32, f32, f32, f32),
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub bbox: (f32, f32, f32, f32),
    pub cells: Vec<TableCell>,
    pub num_rows: usize,
    pub num_cols: usize,
}

impl Table {
    pub fn to_markdown(&self) -> String {
        if self.cells.is_empty() || self.num_cols == 0 {
            return String::from("| Empty Table |\n|---|\n");
        }

        // Build grid
        let mut grid = vec![vec![String::new(); self.num_cols]; self.num_rows];
        for cell in &self.cells {
            if cell.row < self.num_rows && cell.col < self.num_cols {
                grid[cell.row][cell.col] = cell.text.clone().unwrap_or_default();
            }
        }

        let mut md = String::new();

        // Header row
        md.push_str("| ");
        md.push_str(&grid[0].join(" | "));
        md.push_str(" |\n");

        // Separator
        md.push('|');
        for _ in 0..self.num_cols {
            md.push_str(" --- |");
        }
        md.push('\n');

        // Data rows
        for row in grid.iter().skip(1) {
            md.push_str("| ");
            md.push_str(&row.join(" | "));
            md.push_str(" |\n");
        }

        md
    }
}

pub struct TableExtractor {
    detection_session: Session,
    structure_session: Session,
}

impl TableExtractor {
    pub fn new(
        detection_model: &str,
        structure_model: &str,
        use_cuda: bool,
    ) -> Result<Self> {
        let provider = if use_cuda {
            vec![CUDAExecutionProvider::default().build().error_on_failure()]
        } else {
            vec![]
        };

        let detection_session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_execution_providers(provider.clone())?
            .commit_from_file(detection_model)
            .context("Failed to load table detection model")?;

        let structure_session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_execution_providers(provider)?
            .commit_from_file(structure_model)
            .context("Failed to load table structure model")?;

        Ok(Self {
            detection_session,
            structure_session,
        })
    }

    pub fn detect_tables(&mut self, image: &DynamicImage) -> Result<Vec<(f32, f32, f32, f32)>> {
        // Preprocess for DETR (1000x1000, ImageNet normalization)
        let input = self.preprocess(image, 1000)?;
        
        let value = ort::value::Value::from_array(input)?;
        let outputs = self.detection_session.run(ort::inputs!["pixel_values" => value])?;
        
        // Parse DETR output format
        // ... (implementation details)
        
        Ok(vec![]) // Placeholder
    }

    pub fn extract_structure(
        &mut self, 
        table_image: &DynamicImage,
    ) -> Result<Table> {
        let input = self.preprocess(table_image, 1000)?;
        
        let value = ort::value::Value::from_array(input)?;
        let outputs = self.structure_session.run(ort::inputs!["pixel_values" => value])?;
        
        // Parse structure output
        // ... (implementation details)
        
        Ok(Table {
            bbox: (0.0, 0.0, 0.0, 0.0),
            cells: vec![],
            num_rows: 0,
            num_cols: 0,
        })
    }

    fn preprocess(&self, image: &DynamicImage, size: u32) -> Result<Array4<f32>> {
        let resized = image.resize_exact(
            size, size,
            image::imageops::FilterType::CatmullRom
        );

        let rgb = resized.to_rgb8();
        let mut array = Array4::<f32>::zeros((1, 3, size as usize, size as usize));

        let mean = [0.485, 0.456, 0.406];
        let std = [0.229, 0.224, 0.225];

        for y in 0..size {
            for x in 0..size {
                let pixel = rgb.get_pixel(x, y);
                for c in 0..3 {
                    let val = pixel[c] as f32 / 255.0;
                    array[[0, c, y as usize, x as usize]] = (val - mean[c]) / std[c];
                }
            }
        }

        Ok(array)
    }

    pub fn warmup(&mut self) -> Result<()> {
        let dummy = DynamicImage::new_rgb8(1000, 1000);
        let _ = self.detect_tables(&dummy)?;
        Ok(())
    }
}
```

### 2.2 Update models/mod.rs

```rust
pub mod layout;
pub mod table;  // ADD

// ... existing code
```

### 2.3 Download Models

```bash
# Table detection (convert from HuggingFace or use pre-converted)
# Check https://huggingface.co/microsoft/table-transformer-detection
mkdir -p models/table
# ... conversion steps from walkthrough 06
```

---

## Phase 3: Formula Recognition
**Time: 3-4 hours | Priority: MEDIUM**

### 3.1 Create Formula Recognizer

**File:** `src/models/formula.rs` (NEW)

Similar structure to table.rs - encoder/decoder for LaTeX output.

### 3.2 Download Models

```bash
mkdir -p models/formula
wget -O models/formula/encoder_model.onnx \
  "https://huggingface.co/breezedeus/pix2text-mfr/resolve/main/encoder_model.onnx"
wget -O models/formula/decoder_model.onnx \
  "https://huggingface.co/breezedeus/pix2text-mfr/resolve/main/decoder_model.onnx"
wget -O models/formula/tokenizer.json \
  "https://huggingface.co/breezedeus/pix2text-mfr/resolve/main/tokenizer.json"
```

---

## Phase 4: Integrate Table/Formula into Pipeline
**Time: 2-3 hours | Priority: MEDIUM**

### 4.1 Update Processor

**File:** `src/pipeline/processor.rs`

Add optional TableExtractor and FormulaRecognizer, route "table" and "isolate_formula" regions through them.

### 4.2 Update Markdown Generator

Handle actual table markdown and LaTeX formulas instead of placeholders.

---

## Phase 5: REST API
**Time: 3-4 hours | Priority: LOW**

### 5.1 Implement API Routes

**File:** `src/api/mod.rs`

```rust
pub mod handlers;
pub mod routes;

pub use routes::create_router;
```

**File:** `src/api/routes.rs`

```rust
use axum::{Router, routing::{get, post}};
use crate::api::handlers;

pub fn create_router() -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/convert", post(handlers::convert))
}
```

**File:** `src/api/handlers.rs`

```rust
use axum::{Json, extract::Multipart};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct HealthResponse {
    status: &'static str,
}

pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[derive(Serialize)]
pub struct ConvertResponse {
    markdown: String,
    pages: usize,
}

pub async fn convert(mut multipart: Multipart) -> Json<ConvertResponse> {
    // Extract PDF from multipart
    // Process with Processor
    // Return markdown
    Json(ConvertResponse {
        markdown: String::new(),
        pages: 0,
    })
}
```

### 5.2 Add Server Command to CLI

Update `main.rs` to handle `server` subcommand.

---

## Phase 6: Cleanup & Testing
**Time: 2-3 hours | Priority: MEDIUM**

### 6.1 Remove Redundant Code

- Delete `src/text_correction/` (duplicates `ocr/postprocessor.rs`)
- Consolidate any other duplications

### 6.2 Add Tests

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_line_detection() { ... }
    
    #[test]  
    fn test_multiline_ocr() { ... }
    
    #[test]
    fn test_table_extraction() { ... }
}
```

### 6.3 Performance Optimization

- Profile with `cargo flamegraph`
- Consider batching multiple lines for recognition
- Use `CatmullRom` instead of `Lanczos3` for resize

---

## Summary: Priority Order

| Phase | Task | Time | Blocks |
|-------|------|------|--------|
| **1** | **Line Detection (CRITICAL)** | 3-4h | Everything |
| 2 | Table Extraction | 4-5h | Table markdown |
| 3 | Formula Recognition | 3-4h | LaTeX output |
| 4 | Pipeline Integration | 2-3h | Full features |
| 5 | REST API | 3-4h | Web deployment |
| 6 | Cleanup & Testing | 2-3h | Production ready |

**Total: 18-23 hours**

---

## Files to Create/Modify Checklist

### NEW Files
- [ ] `src/ocr/backends/paddle_detector.rs` (Phase 1)
- [ ] `src/models/table.rs` (Phase 2)
- [ ] `src/models/formula.rs` (Phase 3)
- [ ] `src/api/routes.rs` (Phase 5)
- [ ] `src/api/handlers.rs` (Phase 5)

### MODIFY Files
- [ ] `src/ocr/traits.rs` - Add TextDetector trait (Phase 1)
- [ ] `src/ocr/backends/mod.rs` - Export detector (Phase 1)
- [ ] `src/ocr/mod.rs` - Export detector (Phase 1)
- [ ] `src/ocr/pipeline.rs` - Add multiline routing (Phase 1)
- [ ] `src/pipeline/processor.rs` - Init detector (Phase 1)
- [ ] `src/models/mod.rs` - Export table, formula (Phase 2-3)
- [ ] `src/pipeline/markdown_generator.rs` - Real table/formula (Phase 4)
- [ ] `src/api/mod.rs` - Implement routes (Phase 5)
- [ ] `src/main.rs` - Server subcommand (Phase 5)

### DELETE Files
- [ ] `src/text_correction/` (Phase 6 - redundant)

### DOWNLOAD Models
- [ ] `models/pp-ocrv5_det_en.onnx` (~4MB) (Phase 1)
- [ ] `models/table/detection.onnx` (~76MB) (Phase 2)
- [ ] `models/table/structure.onnx` (~110MB) (Phase 2)
- [ ] `models/formula/encoder_model.onnx` (~88MB) (Phase 3)
- [ ] `models/formula/decoder_model.onnx` (~30MB) (Phase 3)
- [ ] `models/formula/tokenizer.json` (~2MB) (Phase 3)

---

Ready to start Phase 1?