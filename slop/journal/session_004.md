# Session 004: Phase 4 Design - Detection Quality & Visualization

**Date:** 2025-10-16
**Session Goal:** Review completed work and design the next major phase of development.

---

## Session Summary

This was a **planning and design session** rather than implementation. Conducted comprehensive review of Sessions 001-003 and designed Phase 4 based on logical progression and identified gaps.

---

## Review of Completed Work

### Sessions 001-003 Accomplishments

**Session 001: Foundation (15-Oct)**
- ✅ Complete modular architecture (Parser → Detector → Generator → Processor)
- ✅ ONNX Runtime integration with CUDA support
- ✅ Stub implementations for all pipeline stages
- ✅ Clean compilation with intentional warnings
- ✅ CLI interface accepting PDF and model paths

**Session 002: Model Conversion (15-Oct)**
- ✅ Python tooling for PyTorch → ONNX conversion
- ✅ Successfully converted DocLayout-YOLO (75MB ONNX)
- ✅ Pdfium library installation and configuration
- ✅ First end-to-end pipeline test (empty output expected)
- ✅ Resolved library path issues (`LD_LIBRARY_PATH`)

**Session 003: Real Inference (15-16-Oct)**
- ✅ Image preprocessing (resize, RGB, normalize, tensorize)
- ✅ ONNX inference execution with GPU acceleration
- ✅ YOLO post-processing (confidence filtering, coordinate conversion)
- ✅ Detecting 5-22 layout elements per page on real PDFs
- ✅ Fixed borrow checker conflicts with explicit drops
- ✅ Mutability cascade handled correctly throughout pipeline

### Current System Capabilities

**What Works:**
1. PDF → Image rendering at 150 DPI
2. Image → Tensor preprocessing (1024x1024, normalized)
3. ONNX model inference on GPU (0.3-0.5s per page)
4. YOLO output parsing ([1, 300, 6] format)
5. Confidence filtering (threshold: 0.25)
6. Coordinate denormalization to pixel coords
7. Class ID → Name mapping

**What's Missing:**
1. ❌ Non-Maximum Suppression (duplicate detections remain)
2. ❌ Detection type breakdown display
3. ❌ Reading order sorting
4. ❌ Visual validation (can't see what's detected)
5. ❌ Real markdown generation (stub returns empty/placeholders)
6. ❌ OCR integration
7. ❌ Table extraction
8. ❌ Formula recognition
9. ❌ REST API

---

## Phase 4 Design

### Design Document

Created: `slop/walkthroughs/08_phase_4_design.md`

**Comprehensive 3-hour implementation plan** covering:

### Objectives

1. **Non-Maximum Suppression (NMS)**
   - Eliminate duplicate overlapping detections
   - IoU-based filtering (threshold: 0.45)
   - Class-aware suppression (only suppress same class)
   - Expected: 10-30% reduction in detection count

2. **Detection Breakdown Display**
   - Count elements by type (text, title, table, figure, equation)
   - Show per-page breakdown in console output
   - Provide quick insight into document structure

3. **Reading Order Sorting**
   - Top-to-bottom, left-to-right flow
   - Multi-column detection (gap threshold: 50px)
   - Ensures natural content flow in output

4. **Visual Debugging Tool**
   - Draw bounding boxes on page images
   - Color-coded by element type
   - Label with class name + confidence
   - Save to `output/annotated/page_NNN.png`

5. **Basic Markdown Generation**
   - Convert sorted detections to structured markdown
   - Placeholder text (awaiting OCR integration)
   - Proper document structure (titles, paragraphs, tables, figures)
   - Include metadata comments (bbox coordinates, confidence)

### Architecture Additions

**New Files:**
- `src/utils/mod.rs` - Utils module declaration
- `src/utils/visualization.rs` - BBox drawing on images
- `src/assets/DejaVuSans.ttf` - Embedded font for labels

**Modified Files:**
- `src/models/layout.rs` - Add NMS, IoU calculation, reading order sort
- `src/pipeline/processor.rs` - Add breakdown display, visualization integration
- `src/pipeline/markdown_generator.rs` - Implement real generation logic
- `src/main.rs` - Enhanced CLI with `--viz`, `--output` flags
- `Cargo.toml` - Add `imageproc` and `rusttype` dependencies

### Implementation Stages

**Stage 1: NMS (30 min)**
- IoU calculation between two boxes
- NMS algorithm with suppression tracking
- Integration into `detect()` pipeline

**Stage 2: Detection Breakdown (15 min)**
- HashMap-based counting by class name
- Formatted console output per page

**Stage 3: Reading Order (45 min)**
- Y-coordinate primary sort (top-to-bottom)
- X-coordinate secondary sort (left-to-right)
- Multi-column handling with gap threshold

**Stage 4: Visualization (60 min)**
- Image loading and RGB conversion
- Rectangle and text drawing with imageproc
- Color mapping by class type
- File I/O to save annotated images

**Stage 5: Markdown Generation (45 min)**
- Element type → Markdown format mapping
- Paragraph accumulation logic
- Metadata comment generation
- Proper spacing and structure

### CLI Enhancement

New usage pattern:
```bash
pdf-mash document.pdf --viz --output result.md
```

Flags:
- `--model <path>` - Custom model path
- `--viz` - Enable visualization output
- `--output <file>` - Save markdown to file

---

## Design Rationale

### Why These Features Together?

**Problem:** Raw detection output is:
- Cluttered with duplicates
- Unordered (random detection order)
- Invisible (no way to verify accuracy)
- Unusable (can't convert to readable format)

**Solution:** Phase 4 addresses all four issues:
1. **NMS** fixes duplicates at the source
2. **Reading order** makes output logical
3. **Visualization** enables validation
4. **Markdown** makes output usable

These features are **interdependent** and should be implemented together as a cohesive quality improvement phase.

### Why Not OCR Yet?

**Reasoning:**
1. Need to validate layout detection quality first
2. OCR depends on accurate bounding boxes
3. Visual debugging requires working before adding OCR
4. Can generate structured markdown without text content
5. OCR adds significant complexity (3 models: det, rec, cls)

**Next Phase (005)** will integrate OCR after detection quality is proven.

---

## Technical Highlights from Review

### Rust Lessons Learned (Sessions 001-003)

**Borrow Checker:**
- Mutability cascades upward through call stack
- Explicit `drop()` releases borrows early
- Cloning small data (tensors) acceptable for borrow resolution
- Can't hide mutability behind immutable references

**ONNX Runtime:**
- Session requires `&mut self` (maintains internal state)
- Output tensors borrow session mutably
- Clone tensor data before processing to avoid conflicts
- Execution providers configured via builder pattern

**Module System:**
- `pub mod` in parent exposes child modules
- Full paths needed for cross-module imports
- `use crate::module::submodule::Type` for absolute paths

**Error Handling:**
- `anyhow::Result` simplifies return types
- `?` operator elegant error propagation
- Context can be added with `.context()` method

### ML Model Integration Patterns

**Preprocessing Pipeline:**
```
DynamicImage
  → resize (1024x1024)
  → to_rgb8
  → Array4<f32> [1,3,H,W] (NCHW)
  → normalize (/255.0)
  → Value::from_array
  → ONNX inference
```

**Post-processing Pipeline:**
```
ONNX Output [1, 300, 6]
  → parse detections (cx, cy, w, h, conf, class_id)
  → confidence filter (>0.25)
  → center→corner coords
  → denormalize (0-1 → pixels)
  → class ID → name mapping
  → Vec<BBox>
```

**Coordinate Systems:**
- Model output: Normalized [0, 1], center format (cx, cy, w, h)
- Application: Pixel coords, corner format (x1, y1, x2, y2)
- Conversion crucial for correct bbox placement

---

## Performance Observations

### Session 003 Test Results

**Test PDF:** 15-page medical research article

**Per-Page Processing:**
- PDF rendering: <0.1s
- ONNX inference: 0.3-0.5s (GPU)
- Post-processing: <0.01s
- **Total:** ~0.4-0.6s per page

**Detection Distribution:**
- Minimum: 5 elements (sparse reference pages)
- Maximum: 22 elements (dense content with tables)
- Average: ~13 elements per page
- Total: 198 detections across 15 pages

**Detection Quality:**
- ✅ Higher counts on content-rich pages (expected)
- ✅ Lower counts on reference pages (expected)
- ⚠️ Need to verify types (text vs table vs figure)
- ⚠️ Likely has duplicates (no NMS yet)

### Phase 4 Performance Impact

**Expected Additions:**
- NMS: +0.01s per page (negligible)
- Reading order sort: <0.001s per page (trivial)
- Visualization: +2-3s per page (optional, `--viz` flag)
- Markdown generation: <0.01s per page (string operations)

**Total pipeline:** Still <0.5s per page without visualization, ~3s with.

---

## Risk Assessment

### Low Risk Items
- ✅ NMS - Standard algorithm, well-understood
- ✅ Reading order - Simple sorting logic
- ✅ Markdown generation - String formatting

### Medium Risk Items
- ⚠️ Visualization - Font loading, image drawing dependencies
- ⚠️ CLI enhancement - Argument parsing can be fragile

### Mitigation Strategies
- Use embedded font (include in binary) to avoid external dependencies
- Test on multiple PDFs (single-column, multi-column, complex layouts)
- Add comprehensive error messages for CLI argument issues

---

## Dependencies Added

### New Cargo Dependencies

```toml
imageproc = "0.25"  # Image drawing primitives
rusttype = "0.9"    # Font rendering for labels
```

### New Assets

```
src/assets/
└── DejaVuSans.ttf  # Embedded font for visualization
```

Download command:
```bash
wget https://github.com/dejavu-fonts/dejavu-fonts/raw/master/ttf/DejaVuSans.ttf \
  -O pdf-mash/src/assets/DejaVuSans.ttf
```

---

## Success Criteria for Phase 4

### Quantitative Metrics

1. **NMS Effectiveness:** Reduce detection count by 15-25% on test PDF
2. **Processing Speed:** Maintain <0.5s per page (excluding visualization)
3. **Visualization Quality:** All bboxes visible and correctly positioned
4. **Reading Order Accuracy:** Manual inspection shows logical flow

### Qualitative Validation

1. **Visual Debugging:** Can identify detection errors by inspecting annotated images
2. **Markdown Structure:** Output reflects document hierarchy (titles, sections, figures)
3. **Detection Insights:** Breakdown reveals document composition at a glance
4. **Usability:** CLI interface intuitive and helpful

### Testing Checklist

- [ ] Test on single-column document
- [ ] Test on multi-column document
- [ ] Test on document with tables
- [ ] Test on document with figures
- [ ] Test on document with equations
- [ ] Verify NMS removes duplicates
- [ ] Verify reading order correct in multi-column
- [ ] Verify visualization colors match classes
- [ ] Verify markdown structure logical
- [ ] Verify CLI flags work as expected

---

## Files to Create/Modify (Phase 4)

### New Files (3)
1. `src/utils/mod.rs` - Module declaration
2. `src/utils/visualization.rs` - BBox drawing (~150 lines)
3. `src/assets/DejaVuSans.ttf` - Font file

### Modified Files (5)
1. `src/models/layout.rs` - Add NMS, IoU, reading order (~100 lines added)
2. `src/pipeline/processor.rs` - Add breakdown, viz integration (~50 lines added)
3. `src/pipeline/markdown_generator.rs` - Implement real generation (~80 lines added)
4. `src/main.rs` - CLI enhancement (~40 lines added)
5. `Cargo.toml` - Add 2 dependencies

**Total:** ~420 lines of new Rust code

---

## Next Steps

### Immediate Action Items

1. **Read Phase 4 design document** (`slop/walkthroughs/08_phase_4_design.md`)
2. **Implement Stage 1 (NMS)** - Start with IoU calculation
3. **Test NMS** - Verify reduction in detection count
4. **Proceed through stages** - One at a time, test each
5. **Document results** - Create `session_005.md` with outcomes

### After Phase 4 Completion

**Phase 5 will focus on OCR integration:**
- PaddleOCR model download and conversion
- Text detection within layout regions
- Text recognition and confidence filtering
- Integration into markdown generation
- Multi-language support validation

With Phase 4's visual debugging in place, Phase 5's OCR results will be easier to validate and troubleshoot.

---

## Documentation Quality Note

Phase 4 design document is **production-ready** with:
- Complete code examples (copy-paste ready)
- Dependency installation commands
- Testing procedures for each stage
- Common issues and solutions
- Clear success criteria
- Realistic time estimates

This represents the **gold standard** for walkthrough documentation quality going forward.

---

## Session Outcome

✅ **Success:** Comprehensive Phase 4 design completed

**Deliverables:**
1. Detailed review of Sessions 001-003 (this document)
2. Complete Phase 4 design document (`08_phase_4_design.md`)
3. Clear implementation roadmap with time estimates
4. Identified dependencies and assets needed
5. Testing strategy and success criteria

**Readiness for Implementation:**
- All design decisions made
- Code patterns established
- Dependencies identified
- Risks assessed and mitigated
- Ready to start Stage 1 immediately

---

**End of Session 004**

**Session duration:** ~45 minutes (review + design)
**Lines of code written:** 0 (design session)
**Documents created:** 2 (this journal + Phase 4 design)
**Next session:** Begin Phase 4 implementation, starting with NMS
**Estimated Phase 4 completion:** 3-4 hours total across 1-2 sessions

**Key Insight:** Taking time to design comprehensively before implementation prevents thrashing and ensures coherent feature sets. Phase 4 addresses four interconnected problems (duplicates, order, visibility, usability) as a unified solution rather than piecemeal fixes.
