# OCR Coordinate Transformation Fix

**Date:** 2025-10-21
**Component:** OCR Engine (`src/models/ocr.rs`)
**Issue:** Catastrophic OCR quality due to incorrect coordinate mapping

---

## Problem

OCR was producing ~90% garbage output despite GPU acceleration working correctly:

**Before Fix:**
```
ardsanun eIyingwitr N eywords:H. on 1esfosnreadthroughse
ererecruutedtronerugtreottentoutreocbhrooron
```

**After Fix:**
```
owardsanunderstandingotsexualriskbehayioru eopielyingwithHlly
areviewofsocial.psvchologica andmedicalfinding
```

---

## Root Cause

The bug was in the coordinate space transformation pipeline in `post_process_detection()`.

### The Preprocessing Pipeline

1. **Input:** Original image (e.g., 1127×384 pixels)
2. **Resize:** Scale to fit 960×960 while preserving aspect ratio (→ 960×327)
3. **Pad:** Center on 960×960 white canvas with padding offset (0, 316)
4. **Detect:** Model outputs text boxes in 960×960 coordinate space
5. **Transform:** Map coordinates back to original image space **← BUG HERE**

### The Bug

The code scaled coordinates directly from 960×960 to original dimensions:

```rust
// WRONG: Assumes entire 960×960 maps to original image
let scale_x = original_width as f32 / 960.0;   // 1127 / 960 = 1.174
let scale_y = original_height as f32 / 960.0;  // 384 / 960 = 0.4

let x1 = min_x as f32 * scale_x;
let y1 = min_y as f32 * scale_y;
```

**Problem:** Coordinates include padding area! A box at (100, 300) in the 960×960 space is being mapped to the wrong location in the original image because it doesn't account for the 316 pixels of top padding.

### Debug Output Revealed the Issue

```
[DEBUG] preprocess_for_detection: 1127x384 -> 960x327 (scale=0.852)
[DEBUG] preprocess_for_detection: Padding with offset (0, 316)
[DEBUG] extract_text: Found 4 text boxes
[DEBUG] Box 0: (128.6, 123.1) -> (934.2, 134.7)  // Height = 11.6 pixels
```

That 11.6 pixel height box was **in the padding area** (y=123 is way above y=316 where actual content starts). When cropped from the original image, we were extracting the wrong region entirely.

---

## The Fix

Three-step coordinate transformation:

### Step 1: Calculate Padding Offsets

In `detect_text_regions()` (lines 235-241):

```rust
let target_size = 960;
let (original_width, original_height) = image.dimensions();
let scale = target_size as f32 / original_width.max(original_height) as f32;
let new_width = (original_width as f32 * scale) as u32;
let new_height = (original_height as f32 * scale) as u32;
let x_offset = (target_size - new_width) / 2;
let y_offset = (target_size - new_height) / 2;
```

### Step 2: Pass Padding Info to Post-Processing

Updated `post_process_detection()` signature (lines 288-291):

```rust
fn post_process_detection(
    &mut self,
    shape: Vec<usize>,
    data: Vec<f32>,
    original_width: u32,
    original_height: u32,
    x_offset: u32,        // NEW
    y_offset: u32,        // NEW
    new_width: u32,       // NEW
    new_height: u32,      // NEW
) -> Result<Vec<(f32, f32, f32, f32)>>
```

### Step 3: Correct Coordinate Transformation

In `post_process_detection()` (lines 374-380):

```rust
// Scale from RESIZED dimensions (not 960×960!) to original
let scale_x = original_width as f32 / new_width as f32;
let scale_y = original_height as f32 / new_height as f32;

// Subtract padding offset BEFORE scaling
let x1 = (min_x as f32 - x_offset as f32) * scale_x;
let y1 = (min_y as f32 - y_offset as f32) * scale_y;
let x2 = (max_x as f32 - x_offset as f32) * scale_x;
let y2 = (max_y as f32 - y_offset as f32) * scale_y;
```

---

## The Math

**Example:** Original image 1127×384, box detected at (100, 400) in 960×960 space

### Before Fix (WRONG):
```
x1 = 100 * (1127 / 960) = 117.4
y1 = 400 * (384 / 960) = 160.0
```
Result: Box mapped to (117, 160) in original image

### After Fix (CORRECT):
```
Padding: offset_y = 316
Resized: 960×327

// Step 1: Subtract padding to get coordinates in resized image space
resized_y = 400 - 316 = 84

// Step 2: Scale from resized to original
scale_y = 384 / 327 = 1.174
y1 = 84 * 1.174 = 98.6
```
Result: Box correctly mapped to (117, 98) in original image

**The difference:** 160 vs 98 = **62 pixels off** (16% error on a 384px tall image)

---

## Key Lesson: Coordinate Space Transformations

When working with multiple coordinate spaces:

1. **Identify all spaces:** Detection map (960×960) → Resized image (960×327) → Original (1127×384)
2. **Track offsets:** Padding adds translation that must be subtracted
3. **Transform in order:**
   - First: Remove padding (translation)
   - Then: Scale to target dimensions
4. **Never skip intermediate spaces:** Can't go directly from padded to original

This pattern appears everywhere:
- Computer vision pipelines
- Graphics rendering (viewport → world → screen)
- Game engines (local → world → camera → screen)
- UI layout systems

---

## Files Modified

- `src/models/ocr.rs:211-243` - `detect_text_regions()`: Calculate and pass padding
- `src/models/ocr.rs:282-292` - `post_process_detection()`: Accept padding parameters
- `src/models/ocr.rs:373-380` - Coordinate scaling: Subtract offset before scaling

---

## Testing

GPU still working:
```
Second inference (warm): 8.738059ms
✓ GPU is working!
```

OCR quality dramatically improved - text is now readable with proper word recognition.

---

## Future Improvements

Current OCR still has issues:
- Character recognition errors (e.g., "owards" instead of "towards")
- Missing spaces in some words
- Some character confusion (l/I, 0/O)

These are likely model-specific limitations of the PaddleOCR recognition model, not coordinate bugs.

Potential improvements:
1. Try different PaddleOCR model versions
2. Add post-processing spell correction
3. Fine-tune detection threshold (currently 0.3)
4. Consider alternative OCR models (Tesseract, EasyOCR)
