# DocLayout-YOLO Coordinate Transformation Fix - 2025-10-22

## THE BUG

The bounding boxes from DocLayout-YOLO were completely misaligned with document content due to **incorrect coordinate space transformation**.

### Example Input Data
- **Original image**: 1268×1654 pixels
- **Resized to**: 785×1024 (maintains aspect ratio)
- **Padded to**: 1024×1024 (adds 119px left padding, 0px top padding)
- **Raw model output**: cx=188.18, cy=457.76, w=488.58, h=841.44

### The Problem

The code had THREE fundamental errors:

1. **Wrong assumption about coordinate format** (lines 112-117):
   ```rust
   let cx = data[base_idx]; // center x (normalized 0-1)  ← WRONG!
   ```
   The comments claimed coordinates were "normalized 0-1", but model outputs are actually **PIXEL VALUES in 1024×1024 space**!

2. **Confused transformation logic** (lines 159-186):
   The code attempted multiple different transformation approaches, showing confusion about the correct method.

3. **Incorrect transformation order**:
   The code tried scaling using "gain" and complex center/size transformations instead of the simple, correct approach.

## THE ROOT CAUSE

DocLayout-YOLO (like all YOLO models) outputs coordinates in **preprocessed image space**, not original space:

```
Original (1268×1654) → Resize (785×1024) → Pad (1024×1024)
                                                    ↑
                                          Model outputs HERE
```

To transform back to original:
1. **Subtract padding offset** (1024 space → resized 785×1024 space)
2. **Scale by resized-to-original ratio** (resized → original)
3. **Clip to valid region** (handle boxes extending into padding)

## THE FIX

### Correct Transformation Formula

```rust
// Step 1: Convert center+size to corners in 1024 space
let x1_padded = cx - w / 2.0;
let x2_padded = cx + w / 2.0;

// Step 2: Subtract padding offset to get resized space coordinates  
let x1_resized = x1_padded - x_offset as f32;
let x2_resized = x2_padded - x_offset as f32;

// Step 3: Scale from resized dimensions to original dimensions
let scale_x = original_width / new_width as f32;
let x1_original = x1_resized * scale_x;
let x2_original = x2_resized * scale_x;

// Step 4: Clip to valid image region
let x1 = x1_original.max(0.0);
let x2 = x2_original.min(original_width);
```

### Math Example

Given:
- Original: 1268×1654
- Resized: 785×1024
- Padding offset: (119, 0)
- Raw detection: cx=188.18, w=488.58

**Step-by-step transformation:**

```
1. Corners in 1024 space:
   x1_padded = 188.18 - 488.58/2 = -55.11
   x2_padded = 188.18 + 488.58/2 = 432.47

2. After offset subtraction (785×1024 space):
   x1_resized = -55.11 - 119 = -174.11
   x2_resized = 432.47 - 119 = 313.47

3. After scaling to original (1268×1654):
   scale_x = 1268 / 785 = 1.615
   x1_original = -174.11 * 1.615 = -281.08
   x2_original = 313.47 * 1.615 = 506.25

4. After clipping:
   x1 = max(-281.08, 0) = 0.0
   x2 = min(506.25, 1268) = 506.25
```

**Why negative values?** Wide boxes can extend into the padding region. This is NORMAL and expected!

## VERIFICATION

### Confirmed Against Official Implementation

Checked the official DocLayout-YOLO Python code in:
`/models/model_tools/lib/python3.13/site-packages/doclayout_yolo/utils/ops.py`

The `scale_boxes()` function does EXACTLY this:
```python
# Subtract padding
boxes[..., 0] -= pad[0]
boxes[..., 1] -= pad[1]

# Scale by gain
boxes[..., :4] /= gain

# Clip to bounds
return clip_boxes(boxes, img0_shape)
```

This confirms our fix matches the official implementation!

## FILES MODIFIED

**File**: `/mnt/datadrive_m2/pdf_masher/pdf-mash/src/models/layout.rs`

**Lines changed**: 107-166 (the `post_process_yolo` loop body)

### Before (BUGGY):
- Comments claimed normalized coordinates (wrong!)
- Used complex "gain" calculation
- Attempted to scale center AND size separately
- Confused transformation order

### After (FIXED):
- Clear documentation: coordinates are PIXELS
- Simple 4-step transformation
- Subtract padding → Scale → Clip
- Matches official YOLO implementation

## KEY LESSONS

1. **Read the model documentation carefully**: Don't assume coordinate formats!
2. **Check reference implementations**: The Python code showed the correct approach
3. **Coordinate transformations have ORDER**: Padding subtraction before scaling
4. **Negative coordinates are normal**: Boxes can extend into padding regions
5. **Always clip final coordinates**: Boxes must be within [0, width] × [0, height]

## Testing Required

To verify this fix works:

1. Run layout detection on a test document page image
2. Generate annotated output image with bounding boxes drawn
3. Verify boxes align correctly with document elements
4. Check that all 10 class types are detected correctly:
   - title, plain text, abandon, figure, figure_caption
   - table, table_caption, table_footnote, isolate_formula, formula_caption

## Related Issues

This is similar to the OCR coordinate fix documented in:
`/mnt/datadrive_m2/pdf_masher/slop/journal/2025-10-21-ocr-coordinate-transformation-fix.md`

Both issues stemmed from incorrect handling of letterbox preprocessing transformations.
