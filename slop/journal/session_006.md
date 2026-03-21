# Session 006: Missing Class Names Discovery & Fix

**Date:** 2025-10-18
**Session Goal:** Resolve unknown_5, unknown_6, unknown_7 detection classes by researching DocLayout-YOLO's actual class mapping.

---

## Session Summary

**Critical Discovery:** The class mapping in `layout.rs` was **completely wrong**! All 10 classes were misidentified. Successfully extracted correct mapping from PyTorch model and updated both class names array and markdown generator to handle all 10 document element types.

**Session Duration:** ~45 minutes
**Methodology:** Model inspection → Class extraction → Code update → Testing

---

## Problem Statement

From Session 005 TODO:
- Model detecting `unknown_5`, `unknown_6`, `unknown_7` on pages 3-8
- These dominated dense content pages (9-10 detections per page)
- Suggested they were important classes, not errors

**Root Cause Hypothesis:** Class names array didn't match model's actual output classes.

---

## Investigation Process

### Step 1: Research DocLayout-YOLO Documentation

**Attempted:**
- HuggingFace model page (juliozhao/DocLayout-YOLO-DocStructBench)
- arXiv paper 2410.12628
- GitHub repo (opendatalab/DocLayout-YOLO)

**Found:** 10 distinct categories listed in paper:
1. Title
2. Plain Text
3. Abandoned Text
4. Figure
5. Figure Caption
6. Table
7. Table Caption
8. Table Footnote
9. Isolated Formula
10. Formula Caption

**Missing:** Exact index mapping (which is class 0, 1, 2, etc.)

---

### Step 2: Extract Class Names from PyTorch Model

**Approach:** Load model using `doclayout_yolo` package and inspect `model.names` attribute.

**Command:**
```bash
uv run python -c "
from doclayout_yolo import YOLOv10
model = YOLOv10('/mnt/datadrive_m2/pdf_masher/models/DocLayout-YOLO-DocStructBench/doclayout_yolo_docstructbench_imgsz1024.pt')
for idx, name in model.names.items():
    print(f'{idx}: {name}')
"
```

**Output:**
```
0: title
1: plain text
2: abandon
3: figure
4: figure_caption
5: table
6: table_caption
7: table_footnote
8: isolate_formula
9: formula_caption
```

---

## Critical Discovery

**Original (WRONG) Mapping:**
```rust
let class_names = vec![
    String::from("text"),      // 0 - WRONG
    String::from("title"),     // 1 - WRONG
    String::from("figure"),    // 2 - WRONG
    String::from("table"),     // 3 - WRONG
    String::from("equation"),  // 4 - WRONG
    // Missing: 5-9
];
```

**Correct Mapping:**
```rust
let class_names = vec![
    String::from("title"),           // 0 ✓
    String::from("plain text"),      // 1 ✓
    String::from("abandon"),         // 2 ✓
    String::from("figure"),          // 3 ✓
    String::from("figure_caption"),  // 4 ✓
    String::from("table"),           // 5 ✓
    String::from("table_caption"),   // 6 ✓
    String::from("table_footnote"),  // 7 ✓
    String::from("isolate_formula"), // 8 ✓
    String::from("formula_caption"), // 9 ✓
];
```

**Impact of Wrong Mapping:**
- What we thought were "figures" were actually "abandons"
- What we thought were "tables" were actually "figures"
- Classes 5-9 showed as "unknown" because they didn't exist in our array
- Detection breakdown was completely misleading
- Markdown output would have been structurally incorrect

---

## Implementation

### Fix 1: Update Class Names Array

**File:** `pdf-mash/src/models/layout.rs:38-49`

**Change:** Replaced 5-element incorrect array with 10-element correct array.

**Key Detail:** Class 1 is `"plain text"` (with space), not `"plain_text"` (underscore). Model has inconsistent naming convention.

**Test:** `cargo check` - compiled successfully.

---

### Fix 2: Update Markdown Generator for All 10 Classes

**File:** `pdf-mash/src/pipeline/markdown_generator.rs:20-111`

**Changes:**

1. **Renamed classes:**
   - `"text"` → `"plain text"`
   - `"equation"` → `"isolate_formula"`

2. **Added new classes:**
   - `"abandon"` - Skip with `continue` (page numbers, headers, footers)
   - `"figure_caption"` - Italic caption after figures
   - `"table_caption"` - Bold caption before/above tables
   - `"table_footnote"` - Small text (<sub>) below tables
   - `"formula_caption"` - Italic equation numbers/labels

3. **Fixed missing content:**
   - `"figure"` case was flushing paragraph but not outputting figure image
   - `"isolate_formula"` case was flushing but not outputting LaTeX block
   - Added proper markdown for both

**Markdown Format Patterns:**

| Class | Markdown Output | Notes |
|-------|----------------|-------|
| title | `## [Title - conf: X.XX]` | h2 header with confidence |
| plain text | `[Text block]` | Accumulates into paragraphs |
| abandon | *(skip)* | Filtered out with `continue` |
| figure | `![Figure placeholder](figure.png)` | Image syntax |
| figure_caption | `*Figure: [Caption awaiting OCR]*` | Italic below figure |
| table | `\| Table \|\n\|-------\|\n\| ... \|` | Markdown table |
| table_caption | `**Table: [Caption awaiting OCR]**` | Bold before table |
| table_footnote | `<sub>[Footnote awaiting OCR]</sub>` | Small text |
| isolate_formula | `$$\n[LaTeX equation]\n$$` | LaTeX block |
| formula_caption | `*Equation: [Caption]*` | Italic equation label |

**Test:** `cargo check` - compiled successfully with no warnings.

---

## Testing Results

### Detection Breakdown (Corrected)

**Before (Session 005 - WRONG):**
```
Page 3: unknown_6: 9, unknown_5: 1, unknown_7: 1, figure: 2
```

**After (Session 006 - CORRECT):**
```
Page 1:
  • plain text: 4
  • abandon: 3
  • title: 2

Page 3:
  • table_caption: 9
  • table_footnote: 1
  • table: 1
  • abandon: 2

Page 5:
  • table_caption: 7
  • table_footnote: 1
  • table: 1
  • abandon: 3
```

**Validation:** Pages 3-8 are table-heavy (medical research article with data tables). Detection now makes sense!

---

### Markdown Output Quality

**File:** `output/corrected_classes_output.md` (331 lines)

**Structure Observed:**
- ✅ Titles appear as h2 headers with confidence scores
- ✅ Plain text blocks accumulated into paragraphs
- ✅ Tables with proper markdown syntax
- ✅ Table captions appear before tables (bold)
- ✅ Table footnotes appear after tables (small text)
- ✅ No "abandon" elements in output (page numbers filtered)
- ✅ Document flow logical (top-to-bottom)

**Sample Output:**
```markdown
## [Title - conf: 0.90]

[Text block] [Text block] [Text block]

**Table: [Caption awaiting OCR]**

| Table |
|-------|
| [Table content awaiting extraction] |

<sub>[Footnote awaiting OCR]</sub>

[Text block] [Text block]
```

**Quality:** Structure is accurate. Content placeholders ready for Phase 5 OCR integration.

---

## Key Insights

### Technical Lessons

**1. Never Trust Documentation Alone**
- DocLayout-YOLO docs showed class types but not indices
- PyTorch model metadata is ground truth
- Always verify with actual model inspection

**2. Model Naming Inconsistencies Are Real**
- 9 classes use underscores: `figure_caption`, `table_caption`
- 1 class uses space: `"plain text"` (not `plain_text`)
- No clear naming convention followed
- Must match exactly or detections fail silently

**3. Wrong Mappings Create Silent Failures**
- Code compiled and ran fine with wrong classes
- Detection counts looked plausible
- Only visual inspection revealed structural issues
- Type systems can't catch semantic errors

**4. "Abandon" Class is Clever Design**
- Automatically identifies page metadata (numbers, headers, footers)
- Filters out non-content elements
- Saves OCR effort on irrelevant text
- Genius preprocessing for document extraction

---

### Rust Patterns Used

**1. Match Exhaustiveness**
```rust
match bbox.class_name.as_str() {
    "title" => { ... }
    "plain text" => { ... }
    // ... 8 more ...
    _ => { ... }  // Catches future unknown classes
}
```

**2. Continue for Filtering**
```rust
"abandon" => {
    continue;  // Skip this iteration entirely
}
```
Cleaner than wrapping all other cases in `if` conditions.

**3. State Machine Pattern**
```rust
let mut current_paragraph = String::new();
// Accumulate text
current_paragraph.push_str("[Text block] ");
// Flush when structural element appears
if !current_paragraph.is_empty() {
    markdown.push_str(&current_paragraph);
    current_paragraph.clear();
}
```

---

## Files Modified

**1. `pdf-mash/src/models/layout.rs`**
- Lines 38-49: Replaced class_names array
- Added all 10 correct class names
- Fixed "plain text" (space, not underscore)

**2. `pdf-mash/src/pipeline/markdown_generator.rs`**
- Lines 20-111: Complete match statement rewrite
- Added 6 new class handlers
- Fixed 2 missing content outputs (figure, isolate_formula)
- Removed 2 obsolete handlers (text, equation)

**Total Changes:** ~30 lines modified, full correctness achieved

---

## Validation Checklist

- [x] All 10 class names match model output exactly
- [x] Markdown generator handles all 10 classes
- [x] "abandon" elements correctly filtered
- [x] Figure outputs image markdown
- [x] Formula outputs LaTeX blocks
- [x] Table structure with captions and footnotes
- [x] No compiler warnings
- [x] Test PDF produces logical markdown structure
- [x] Detection breakdown shows table-heavy pages correctly

---

## Performance Impact

**None.** This was purely a correctness fix, not a performance change.

**Detection counts unchanged:**
- Before: 140 detections (post-NMS)
- After: 140 detections (same, just correctly labeled)

---

## Remaining Issues

### Issue 1: OCR Integration Still Needed

**Status:** Phase 5 (next session)

**Blockers:** None - detection quality now validated

**Plan:**
1. Integrate PaddleOCR for text extraction
2. Replace `[Text block]` with actual recognized text
3. Replace `[Caption awaiting OCR]` with real captions

---

### Issue 2: Figure/Formula Captions May Appear Out of Order

**Status:** Low priority

**Observation:** Reading order sort uses Y-coordinate + X-coordinate, which may place captions before/after their elements incorrectly in complex layouts.

**Mitigation:** Post-processing pass to group captions with nearest structural element (future enhancement).

---

## Next Steps

### Immediate: Update TODO.md

Mark "Add Missing Class Names" as ✅ RESOLVED.

### Phase 5: OCR Integration

**Now that classes are correct, we can:**
1. Download PaddleOCR models (det, rec, cls)
2. Convert to ONNX format
3. Implement OCR engine in `src/models/ocr.rs`
4. Run OCR within detected bounding boxes
5. Replace placeholders with actual text

**Estimated Effort:** 3-4 hours

**Readiness:** HIGH - detection quality confirmed with correct class labels.

---

## Documentation Quality Note

This session demonstrates the importance of:
- **Verification over assumption** - Don't trust docs, inspect models
- **Incremental testing** - Caught issue early through visual inspection
- **Proper logging** - Detection breakdown revealed the problem
- **Complete fixes** - Updated both class array AND markdown generator

---

## Session Outcome

✅ **Success:** All 10 DocLayout-YOLO classes correctly identified and implemented!

**Deliverables:**
1. Correct class mapping extracted from PyTorch model
2. Updated class_names array in layout.rs
3. Complete markdown generator with all 10 classes
4. Corrected detection breakdown output
5. Properly structured markdown output (331 lines)
6. This comprehensive session journal

**Code Quality:**
- No compiler warnings
- All classes handled exhaustively
- Proper filtering of "abandon" elements
- Clean, readable match statement

**Readiness for Phase 5:**
- Detection quality validated visually
- Class labels semantically correct
- Markdown structure ready for text insertion
- Foundation solid for OCR integration

---

**End of Session 006**

**Session duration:** ~45 minutes
**Lines of code modified:** ~30
**Files modified:** 2
**Critical bugs fixed:** 1 (wrong class mapping)
**Classes corrected:** 10/10
**Tests passed:** All (compilation + visual inspection)
**Documentation created:** This journal + pending TODO update

**Final State:** ✅ Class mapping 100% correct - ready for OCR Phase!

**Key Takeaway:** Silent semantic errors are harder to find than compilation errors. Wrong class mappings produced plausible but incorrect results. Model inspection revealed the truth. Always verify ML model outputs against actual model metadata, not documentation.

**Lesson:** Incremental testing with visual validation catches issues that unit tests might miss. The visualizations from Session 005 made it obvious something was wrong when we saw the detection breakdown.
