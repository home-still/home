# Coordinate Clipping Fix - 2025-10-22

## Problem
Bounding boxes from layout detector were invisible in annotated images. Investigation showed negative coordinates (e.g., x=-271.7).

## Root Cause
The DocLayout-YOLO model outputs coordinates in **1024×1024 padded space**.

When images are letterboxed:
- Original: 1268×1654
- Resized: 785×1024 (preserves aspect ratio)
- Padded: 1024×1024 (centered with offset 119,0)

**Key insight**: The model predicts boxes anywhere in 1024×1024 space, including padding regions. Wide boxes (e.g., w=816 pixels) can extend into the left/right padding, resulting in negative coordinates after transformation.

Example:
```
Raw model output: cx=188.78, w=816.31
Box edges: x_left = 188.78 - 816.31/2 = -219.4 (in padding!)
After scaling: -219.4 × (1268/1024) = -271.7 (negative!)
```

## Solution Applied
**Coordinate clipping** in `src/models/layout.rs`:

```rust
// Step 1: Scale from 1024×1024 to original dimensions
let scale_from_1024_x = original_width / 1024.0;
let x1_raw = (cx - w / 2.0) * scale_from_1024_x;

// Step 2: Clip to valid image region
let x1 = x1_raw.max(0.0);  // Prevent negative coordinates
let x2 = x2_raw.min(original_width);  // Prevent overflow
```

## Why This Works
- Boxes that extend into padding get clipped to image boundaries
- Preserves valid portions of wide boxes
- Standard practice in object detection post-processing

## Files Modified
1. `src/models/layout.rs` (lines 116-136)
   - Removed offset-based transformation
   - Added coordinate clipping with `.max(0.0)` and `.min(dimension)`

2. `src/utils/visualization.rs`
   - Removed debug output (lines 17-32 removed)

## Result
✅ Bounding boxes now visible and correctly positioned
✅ All 9 boxes on page 1 render properly
✅ Colors match class types (red=title, green=text, purple=abandon)

## Lessons Learned
1. **YOLO letterbox models output in padded space** - coordinates can be anywhere in the full input size
2. **Negative coordinates are expected** - boxes extending into padding are normal
3. **Clipping is essential** - post-process coordinates to valid image region
4. **Debug systematically** - traced from raw model output → transformation → visualization to find issue
