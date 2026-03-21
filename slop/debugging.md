# PDF OCR Pipeline Debugging Journal

## Session: 2024-12-01 - Word Segmentation Fixes

### Problem Statement
PDF OCR output has concatenated words like `schizophrenicpatients` because PaddleOCR recognition doesn't output spaces (dictionary has no space character).

### Solution: instant-segment for Word Segmentation

**Dependencies added to Cargo.toml:**
```toml
instant-segment = "0.8"
smartstring = "0.2"  # Must match instant-segment's version
```

**Dictionary files downloaded to `models/dictionaries/`:**
- `unigrams.txt` (5MB) - format: `word\tfrequency`
- `bigrams.txt` (5.5MB) - format: `word1 word2\tfrequency`

Source: https://github.com/grantjenks/wordsegment (Python port has the data files)

---

## Fix 1: `needs_dictionary` Check Bug

**File:** `pdf-mash/src/ocr/postprocessor.rs` line 36-38

**Problem:** The check includes `enable_segmentation`, but instant-segment uses its own unigrams/bigrams files, not SymSpell's dictionary.

**Before:**
```rust
let needs_dictionary = config.enable_segmentation
    || config.enable_spelling_correction
    || config.enable_confusable_correction;
```

**After:**
```rust
let needs_dictionary =
    config.enable_spelling_correction || config.enable_confusable_correction;
```

---

## Fix 2: Handle Punctuation in Tokens

**File:** `pdf-mash/src/ocr/postprocessor.rs`

**Problem:** OCR output has punctuation embedded in tokens like `RabochJ,FaltusF.Sexualityof`. The segmenter only works on pure alphabetic strings.

**Solution:** Split tokens into alpha runs and non-alpha characters, segment only the alpha runs, reassemble.

**Replace `segment_words` function with three functions:**

```rust
fn segment_words(&self, text: &str) -> String {
    let Some(segmenter) = &self.segmenter else {
        return String::from(text);
    };

    let mut search = Search::default();

    text.split_whitespace()
        .map(|token| self.segment_token(token, segmenter, &mut search))
        .collect::<Vec<_>>()
        .join(" ")
}

fn segment_token(&self, token: &str, segmenter: &Segmenter, search: &mut Search) -> String {
    // Split token into alpha runs and non-alpha chars
    // e.g., "RabochJ,FaltusF." -> process "RabochJ", keep ",", process "FaltusF", keep "."
    let mut result = String::new();
    let mut current_alpha = String::new();

    for ch in token.chars() {
        if ch.is_alphabetic() {
            current_alpha.push(ch);
        } else {
            if !current_alpha.is_empty() {
                result.push_str(&self.segment_alpha_run(&current_alpha, segmenter, search));
                current_alpha.clear();
            }
            result.push(ch);
        }
    }

    if !current_alpha.is_empty() {
        result.push_str(&self.segment_alpha_run(&current_alpha, segmenter, search));
    }

    result
}

fn segment_alpha_run(&self, run: &str, segmenter: &Segmenter, search: &mut Search) -> String {
    if !Self::needs_segmentation(run) {
        return String::from(run);
    }

    let lower = run.to_lowercase();
    match segmenter.segment(&lower, search) {
        Ok(words) => words.collect::<Vec<_>>().join(" "),
        Err(_) => String::from(run),
    }
}
```

---

## Fix 3: Update Test

**File:** `pdf-mash/src/ocr/postprocessor.rs` (end of file)

Change:
```rust
let processor = Postprocessor::new(config, Some("...")).unwrap();
```
To:
```rust
let processor = Postprocessor::new(config, None).unwrap();
```

Add test cases for punctuation:
```rust
("RabochJ,FaltusF.Sexualityof", "RabochJ,FaltusF.sexuality of"),
("lives(1,2).Butnewerrese", "lives(1,2).but newer rese"),
```

---

## Results After Fixes

**Before:**
```
RabochJ,FaltusF.Sexualityof act psychiatry scan
andof50controlwomer
schizophrenicpatients
```

**After:**
```
RabochJ,FaltusF.sexuality of act psychiatry scan
andof50control women
schizophrenic patients
```

---

## Remaining Issues to Address

1. **Partial words from line breaks** - e.g., `perience` (missing `ex` from previous line)
2. **Over-segmentation of proper nouns** - e.g., `charles university` for `CharlesUniversity`
3. **Medical terminology** - uncommon words get split incorrectly
4. **OCR recognition errors** - not a segmentation issue, but affects output quality

---

## Iteration 1: Basic Segmentation Working

**Status:** Fixes 1-3 applied and tested

**Successes:**
- `schizophrenicpatients` -> `schizophrenic patients` ✓
- `anorexianervosa` -> `anorexia nervosa` ✓
- `sexualadaptation` -> `sexual adaptation` ✓
- `measurementoffemal` -> `measurement of female` ✓

**New Issues Identified:**

1. **Short alpha runs around numbers not segmented:**
   - `andof50control` splits into `andof` (5 chars) + `50` + `control` (7 chars)
   - Both alpha runs are below 8-char threshold, so not segmented
   - Current threshold: `len >= 8 && alpha_count >= 6`

2. **Over-segmentation of compound terms:**
   - `Czechoslovakia` -> `czech oslo v` (wrong!)
   - `CharlesUniversity` -> `charles university` (acceptable but loses proper noun case)

3. **Partial words from line breaks:**
   - `perience` appears without `ex` from previous line
   - This is a layout/OCR issue, not segmentation

---

## Iteration 2: Tuning Segmentation Threshold

**Threshold 6:** Over-segmented names like `RabochJ` -> `ra both j` (wrong)
**Threshold 7:** Still over-segmenting

---

## Iteration 3: Reject Single-Char Segmentation Results

**Fix:** In `segment_alpha_run`, reject segmentations that produce any single-char words:
```rust
if result.iter().any(|w| w.len() == 1) {
    return String::from(run);  // Keep original
}
```

**Result:**
- `RabochJ` preserved (would produce "j") ✓
- `Sexualityofwomenwith` -> `sexuality of women with` ✓
- `schizophrenicpatients` -> `schizophrenic patients` ✓
- `anorexianervosa` -> `anorexia nervosa` ✓

**Remaining Issues:**
1. Short alpha runs around numbers not segmented: `andof50control`
2. All output lowercased (case not preserved)
3. Layout issues causing partial words from multi-column PDFs

---

## Current State Analysis

**What's Working Well:**
- Long concatenated words segmented correctly
- Author name+initial patterns preserved (e.g., `RabochJ`)
- Punctuation handled correctly
- Medical terms like `schizophrenic`, `anorexia` work

**Root Cause of Remaining Issues:**
The garbage text like `andof50control` isn't a segmentation problem - it's a **layout detection problem**. The PDF has two columns and the OCR is reading across columns instead of down each column.

---

## Session: 2024-12-02 - Wide Line OCR Fixes

### Problem: Truncated Line OCR Output

**Symptom:** Lines showing partial text like:
- `Thesexualdevelopmentar` (should be full line)
- `andof50controlwomer` (should be full line)

**Root Cause:** `extract_single_line()` only used `chunks[0]` when preprocessor created multiple chunks for wide lines.

---

## Fix 4: Handle All Chunks in extract_single_line

**File:** `pdf-mash/src/ocr/pipeline.rs`

**Problem:** Wide lines (>480px) were being chunked, but only first chunk was used!

**Before:**
```rust
fn extract_single_line(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
    let chunks = self.preprocessor.prepare(image)?;
    if chunks.is_empty() { return Ok(RecognitionResult::empty()); }
    // BUG: Only uses first chunk, rest discarded!
    self.recognizer.recognize(&chunks[0].image)
}
```

**After:**
```rust
fn extract_single_line(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
    let chunks = self.preprocessor.prepare(image)?;
    if chunks.is_empty() { return Ok(RecognitionResult::empty()); }

    // Single chunk - recognize directly
    if chunks.len() == 1 {
        return self.recognizer.recognize(&chunks[0].image);
    }

    // Multiple chunks - recognize all and merge
    let mut chunk_results = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let result = self.recognizer.recognize(&chunk.image)?;
        chunk_results.push(result);
    }
    Ok(self.merge_chunks(&chunk_results))
}
```

---

## Fix 5: Smart Overlap Detection for Chunk Merging

**File:** `pdf-mash/src/ocr/pipeline.rs`

**Problem:** Fixed character-count overlap removal didn't work - OCR produces different results for same visual region in different chunks.

**Solution:** Find actual text overlap by looking for longest common suffix/prefix:

```rust
fn find_overlap(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let max_overlap = a_chars.len().min(b_chars.len()).min(25);

    // Try exact match first
    for overlap in (3..=max_overlap).rev() {
        let a_suffix: String = a_chars[a_chars.len() - overlap..].iter().collect();
        let b_prefix: String = b_chars[..overlap].iter().collect();
        if a_suffix == b_prefix {
            return overlap;
        }
    }

    // Fuzzy match for OCR variations (allow 20% difference)
    for overlap in (8..=max_overlap).rev() {
        let differences = a_suffix.iter().zip(b_prefix.iter())
            .filter(|(a, b)| a != b).count();
        if differences <= overlap / 5 {
            return overlap;
        }
    }
    0
}
```

---

## Fix 6: Increase max_recognition_width

**File:** `pdf-mash/src/ocr/config.rs`

**Problem:** Even with proper chunk merging, overlap artifacts occurred due to OCR variations.

**Solution:** Increase `max_recognition_width` from 480 to 1200 pixels to avoid chunking most lines.

```rust
impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            // PP-OCRv4 supports ~320 chars at 48px height = ~1920px
            max_recognition_width: 1200,  // Was 480
            target_height: 48,
            chunk_overlap: 120,  // Was 80
            ...
        }
    }
}
```

---

## Final Results

**Before (garbled, truncated):**
```
the sexual development ar andof50controlwomer and3sexo logical ques
```

**After (clean, full lines):**
```
the sexual development and life of30adult women with anorexia nervosa
and of 50 control women was investigated using a structured interview
and 3 sexological questionnaires.heterosexual development was found to...
```

**Key Improvements:**
- Full line OCR without truncation ✓
- Word segmentation working ✓
- No more overlap artifacts ✓
- References readable ✓

---

## Remaining Minor Issues

1. Some word segmentation edge cases (e.g., `andof50` not split)
2. Case normalization (all lowercase from segmentation)
3. Minor OCR glitches (rare)

---

## Summary of All Fixes

| Fix | File | Problem | Solution |
|-----|------|---------|----------|
| 1 | postprocessor.rs | needs_dictionary included segmentation | Remove enable_segmentation from check |
| 2 | postprocessor.rs | Punctuation breaks segmentation | Split into alpha runs, segment each |
| 3 | postprocessor.rs | Over-segmentation of names | Reject single-char results |
| 4 | pipeline.rs | Only first chunk used | Process all chunks |
| 5 | pipeline.rs | Fixed overlap removal | Smart overlap detection |
| 6 | config.rs | Too much chunking | Increase max width to 1200 |
| 7 | postprocessor.rs | Blocked valid "a"/"I" words | Allow specific single-char words |
| 8 | postprocessor.rs | Lost case after segmentation | Added restore_case function |

---

## Session: 2024-12-02 - Geometry-Based Word Detection (FAILED EXPERIMENT)

### Hypothesis
Instead of post-processing segmentation, detect word boundaries using vertical projection profile (gaps between characters), then OCR each word separately and reconstruct spacing from geometric positions.

### Implementation Added

**File:** `pdf-mash/src/ocr/pipeline.rs`

```rust
/// A detected word bounding box within a line
#[derive(Debug, Clone)]
pub struct WordBox {
    pub x1: u32,
    pub x2: u32,
    pub gap_before: u32,
}

/// Detect word boundaries using vertical projection profile
fn detect_words_by_projection(&self, image: &DynamicImage) -> Vec<WordBox> {
    // Calculate vertical projection (count dark pixels per column)
    // Find gaps (columns with near-zero dark pixels)
    // Convert gaps to word boxes
}

/// Extract text from a line using geometry-based word detection
fn extract_line_with_word_detection(&mut self, image: &DynamicImage) -> Result<RecognitionResult> {
    let word_boxes = self.detect_words_by_projection(image);
    // Crop each word, resize to target_height, recognize
    // Join results with spaces based on gap sizes
}
```

### Results: FAILED

**Output was garbled:**
```
RAW OCR: 'Se)e' (conf: 0.32) [1866x112]
RAW OCR: 'esse2exia AcScSc998491' (conf: 0.66)
RAW OCR: 'exeee)adsWissexVa 5eSvesia1sgsecediew...' (conf: 0.68)
```

### Root Cause Analysis

1. **PP-OCRv4 is trained on full text lines, not isolated words**
   - The recognition model expects context from surrounding characters
   - Isolated word fragments lose this context and produce garbage

2. **Vertical projection is too sensitive**
   - Gaps within characters (e.g., between stems of 'm', 'w') detected as word boundaries
   - Results in too many "words" (40+ per line instead of 5-6)

3. **Cropped word images are too small**
   - Many detected "words" are just 1-3 characters
   - PP-OCRv4 can't reliably recognize such short sequences

### Conclusion

**Geometry-based per-word OCR does NOT work with PP-OCRv4.**

The correct approach is:
1. Full-line OCR (produces concatenated text without spaces)
2. Post-processing with instant-segment for word segmentation
3. Case restoration to preserve original capitalization

### Code Status

The geometry detection code remains in `pipeline.rs` but is **NOT CALLED**:
- `detect_words_by_projection()` - vertical projection word detection
- `extract_line_with_word_detection()` - per-word OCR pipeline

Could potentially be useful for:
- Different OCR backends trained on words (e.g., CRNN word models)
- Detecting table cells or structured content
- Validating whether text needs segmentation

---

## Fix 7: Allow "a" and "I" as Valid Single-Char Words

**File:** `pdf-mash/src/ocr/postprocessor.rs`

**Problem:** Rejecting all single-char segmentation results blocked valid "a" and "I" words.

**Before:**
```rust
if result.iter().any(|w| w.len() == 1) {
    return String::from(run);
}
```

**After:**
```rust
if result.iter().any(|w| w.len() == 1 && *w != "a" && *w != "i") {
    return String::from(run);
}
```

---

## Fix 8: Case Restoration After Segmentation

**File:** `pdf-mash/src/ocr/postprocessor.rs`

**Problem:** instant-segment outputs lowercase, losing original capitalization.

**Before:**
- Input: `"HelloWorld"`
- Segmentation: `["hello", "world"]`
- Output: `"hello world"` ❌

**After:**
```rust
fn restore_case(original: &str, segments: &[&str]) -> String {
    let orig_chars: Vec<char> = original.chars().collect();
    let mut result = String::new();
    let mut orig_idx = 0;

    for (seg_idx, segment) in segments.iter().enumerate() {
        if seg_idx > 0 { result.push(' '); }
        for _ in segment.chars() {
            if orig_idx < orig_chars.len() {
                result.push(orig_chars[orig_idx]);
                orig_idx += 1;
            }
        }
    }
    result
}
```

- Input: `"HelloWorld"`
- Segmentation: `["hello", "world"]`
- Output: `"Hello World"` ✓

---

## Current Output Quality

**Sample Results:**
```
RAW: 'Sexualityofwomenwithanorexianervosa'
CORRECTED: 'Sexuality of women with anorexia nervosa' ✓

RAW: 'AcceptedforpublicationFebruary9,1991'
CORRECTED: 'Accepted for publication February9,1991' ✓

RAW: 'schizophrenicpatients'
CORRECTED: 'schizophrenic patients' ✓
```

**Remaining Edge Cases:**
- `andof50control` - short alpha runs around numbers not segmented
- Some proper names over-segmented: `JiriRaboch` → `Jiri a both`

---

## Session: 2024-12-02 - DBNet Geometry Approach (ALSO FAILED)

### Hypothesis
Use the existing DBNet line detector on cropped line images to detect word boxes, then use geometry (box positions and gaps) to insert spaces into full-line OCR output.

### Implementation
```rust
fn detect_word_boxes(&mut self, line_image: &DynamicImage) -> Vec<(f32, f32, f32, f32)>
fn insert_spaces_by_geometry(&self, text: &str, boxes: &[(f32, f32, f32, f32)]) -> String
fn extract_line_with_geometry(&mut self, image: &DynamicImage) -> Result<RecognitionResult>
```

### Results: ALSO FAILED

DBNet only detected 2-6 boxes per line (should be ~10-15 for typical text):
```
GEOMETRY: 2 word boxes for 31 chars  // "Sexualityofwomenwithanorexianervosa"
```

When proportional character allocation was applied:
```
"connection betweent hed iseasea ndse xuality"  // mid-word breaks
```

### Root Cause
DBNet is trained to detect **text lines/blocks**, not individual words. When run on a cropped line, it finds sub-regions but not word boundaries.

---

## Final Conclusion: instant-segment is the Right Approach

Both geometry approaches failed:
1. **Vertical projection**: Too sensitive, detects gaps within characters
2. **DBNet on lines**: Too coarse, finds text regions not words

**What works:**
- Full-line OCR with PP-OCRv4 (preserves context)
- instant-segment post-processing for word segmentation
- Case restoration to preserve original capitalization
- Allow "a"/"i" as valid single-char words

This is comparable to MinerU's approach - they also rely on post-processing rules rather than geometry-based word detection.

---

## Final Working Pipeline

1. **Layout detection** (DocLayout-YOLO) → text regions
2. **Line detection** (DBNet) → individual lines within regions
3. **Recognition** (PP-OCRv4) → concatenated text without spaces
4. **Post-processing** (instant-segment) → word segmentation + case restoration

No geometry-based spacing. The code is clean and simple.
