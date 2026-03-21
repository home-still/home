# Step 001: Project Initialization and Pipeline Skeleton

**Date:** 2025-10-15
**Session Goal:** Build the foundational architecture for pdf-mash, a Rust-based PDF to Markdown converter using ONNX Runtime for ML inference.

---

## What We Built

### 1. Core Data Structures

**File: `src/models/mod.rs`**
- Created `ModelInference` trait as common interface for all ML models
- Added `use anyhow::Result` to simplify error handling
- Fixed compilation errors around Result type signatures

**File: `src/models/layout.rs`**
- Created `BBox` struct for bounding box detections with:
  - Coordinates: `x1, y1, x2, y2`
  - Metadata: `confidence`, `class_id`, `class_name`
- Built `LayoutDetector` struct to wrap ONNX Runtime session
- Implemented `LayoutDetector::new()` with GPU acceleration support via CUDAExecutionProvider
- Added stub `detect()` method returning empty Vec (to be filled later)

**File: `src/pipeline/pdf_parser.rs`**
- Created `PageData` struct to hold parsed PDF page information:
  - Page index, image, dimensions, optional text
- Implemented `PdfParser` struct wrapping Pdfium library
- Built `parse_to_pages()` method that:
  - Loads PDF documents
  - Renders pages to images at specified DPI (150 default)
  - Extracts dimensions and metadata

**File: `src/pipeline/markdown_generator.rs`**
- Created `MarkdownGenerator` struct with configuration options
- Implemented `generate()` method stub that accepts bboxes and text
- Returns formatted markdown (basic implementation for now)

**File: `src/pipeline/processor.rs`**
- Built `Processor` struct that orchestrates the complete pipeline:
  - Owns PdfParser, LayoutDetector, and MarkdownGenerator
  - `new()` initializes all components with model path
  - `process_pdf()` runs full pipeline: parse → detect → generate
- This is the main entry point for PDF processing

### 2. CLI Interface

**File: `src/main.rs`**
- Created command-line interface accepting:
  - PDF file path (required)
  - Model path (optional, defaults to `models/layout.onnx`)
- Uses `std::env::args()` for argument parsing
- Instantiates Processor and runs pipeline
- Prints generated markdown to stdout

### 3. Project Configuration

**File: `pdf-mash/Cargo.toml`**
Already configured with dependencies:
- `pdfium-render = "0.8.35"` - PDF parsing and rendering
- `image = "0.25"` - Image processing
- `ort = { version = "2.0.0-rc.10", features = ["cuda"]}` - ONNX Runtime with GPU
- `ndarray = "0.16"` - Array operations for tensors
- `anyhow = "1.0"` / `thiserror = "1.0"` - Error handling
- `serde = "1.0"` / `serde_json = "1.0"` - Serialization
- `tracing = "0.1"` - Logging
- `tokio = "1.42"` / `axum = "0.7"` - Async runtime and REST API

### 4. Model Setup

**Directory: `/mnt/datadrive_m2/pdf_masher/models/`**
- Created models directory (added to .gitignore)
- Downloaded DocLayout-YOLO-DocStructBench repository
- Used Git LFS to pull actual model file (39MB .pt format)
- Created Python venv `model_tools` for ONNX conversion

---

## Compilation Status

✅ **All code compiles successfully**

Current warnings (expected during development):
- Unused variables: `image`, `bboxes` (will be used when implementing real inference)
- Unused fields: `session`, `input_size`, `conf_threshold`, `class_names` (used in future detect() implementation)
- Unused field: `include_images` (for future markdown formatting options)

---

## Architecture Decisions

### 1. **Modular Pipeline Design**
Each processing stage is isolated:
- **Parser** → extracts pages as images
- **Detector** → finds layout elements (text, tables, figures)
- **Generator** → converts detections to markdown

**Why:** Allows independent testing and swapping components (e.g., different OCR engines)

### 2. **ONNX Runtime Over PyTorch**
Using ONNX models instead of native PyTorch:
- **Performance:** 2-4x faster inference
- **No Python dependency:** Pure Rust deployment
- **Smaller binary:** No Python runtime needed
- **GPU acceleration:** Via CUDA execution provider

### 3. **Pdfium for PDF Processing**
Chose Pdfium over alternatives (Poppler, MuPDF):
- **Speed:** Faster rendering (Chrome's engine)
- **Reliability:** Battle-tested in production browsers
- **API quality:** Clean Rust bindings available

### 4. **DPI Selection (150)**
Rendering at 150 DPI balances:
- **Quality:** Sufficient for OCR and layout detection
- **Performance:** ~4MB per page vs ~15MB at 300 DPI
- **Memory:** Fits multiple pages in VRAM simultaneously

---

## Key Learning Moments

### 1. **Result Type Confusion**
**Problem:** Rust's `Result<T, E>` requires two type parameters.
**Solution:** Import `anyhow::Result` which is an alias for `Result<T, anyhow::Error>`.

### 2. **Module Visibility**
**Pattern learned:**
```rust
// In mod.rs
pub mod layout;  // Declares module

// In other files
use crate::models::layout::BBox;  // Access with full path
```

### 3. **ONNX Runtime Provider Setup**
**Pattern:**
```rust
let mut builder = Session::builder()?
    .with_optimization_level(GraphOptimizationLevel::Level3)?;

if use_cuda {
    builder = builder.with_execution_providers([
        CUDAExecutionProvider::default().build()
    ])?;
}
```
**Why:** Builder pattern allows conditional GPU acceleration.

### 4. **Generic Type Syntax**
**Common mistake:** `Result<<Self>` (double bracket)
**Correct:** `Result<Self>` (single bracket)
Nested generics only appear in cases like `Vec<Vec<T>>`.

### 5. **Git LFS for Large Files**
Model files (100MB-1GB) require Git LFS:
```bash
git lfs install
git lfs pull
```
Without LFS, you get pointer files (~100 bytes) instead of actual models.

---

## Current State

### ✅ Completed
1. Full pipeline architecture implemented
2. All modules compile without errors
3. CLI interface ready
4. Model file downloaded (PyTorch format)
5. Python environment set up for conversion

### 🔄 In Progress
- Installing doclayout-yolo Python library for ONNX conversion

### ⏳ Next Steps
1. Convert `.pt` model to `.onnx` format
2. Verify ONNX model loads correctly
3. Find/create test PDF
4. Build and run first end-to-end test
5. Implement real inference logic in `detect()` method
6. Add OCR integration (PaddleOCR)
7. Implement table extraction
8. Build REST API with Axum

---

## Code Statistics

**Lines written:** ~300 lines of Rust
**Files created:** 6 Rust source files
**Compilation time:** 0.16-0.19 seconds (fast!)
**Binary size:** TBD (not built yet)

---

## System Configuration

**Environment:**
- OS: Manjaro Linux
- Rust: 1.75+
- CUDA: 12.3
- GPU: NVIDIA (VRAM 6GB+)
- Python: 3.x (in venv)

**Dependencies Installed:**
- ONNX Runtime with GPU support (1.22.0)
- Pdfium library
- CUDA Toolkit + cuDNN
- Git LFS

---

## Design Patterns Used

1. **Builder Pattern:** ONNX Session configuration
2. **Facade Pattern:** Processor hides pipeline complexity
3. **Strategy Pattern:** Swappable detection/OCR engines
4. **Result/Option Types:** Rust-idiomatic error handling
5. **Ownership:** Zero-copy image passing with references

---

## Lessons Learned

**What worked well:**
- Building skeleton first validates architecture before complex implementation
- Using anyhow::Result reduces boilerplate
- Module structure mirrors logical pipeline stages
- Starting with compilation ensures no syntax debt

**What to watch for:**
- ONNX model format compatibility (PT vs ONNX)
- GPU memory management with multiple models
- Image resolution vs performance tradeoffs
- Error handling across async boundaries (future API work)

**Rust-specific insights:**
- Import statements must precede usage
- Trait methods need full Result specification unless aliased
- The `?` operator elegantly propagates errors up the call stack
- For loops evaluate to `()`, need explicit return

---

## Next Session Preview

**Goal:** Convert model to ONNX and run first real inference test.

**Tasks:**
1. Complete `pip install` for conversion tools
2. Export ONNX with: `model.export(format="onnx", simplify=True)`
3. Verify model with `onnx.checker`
4. Test loading in Rust: `Session::builder()?.commit_from_file()`
5. Run pipeline on sample PDF
6. Debug any runtime issues

**Expected challenges:**
- ONNX export compatibility issues
- GPU memory allocation errors
- Image preprocessing mismatches
- Coordinate system transformations

---

## References Used

- Implementation Guide: `slop/walkthroughs/implementation_guide.md`
- System Setup: `slop/walkthroughs/00.md`
- ONNX Runtime Rust docs: `docs.rs/ort`
- Pdfium Render docs: `docs.rs/pdfium-render`
- DocLayout-YOLO paper: ArXiv (2024)

---

**End of Step 001**

**Session outcome:** ✅ Successful - Full pipeline skeleton complete and compiling.
