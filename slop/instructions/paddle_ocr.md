# PaddleOCR Setup - PP-OCRv5 English Models

## What We're Using

**PP-OCRv5** - Latest PaddleOCR recognition model (2025 release)
- 11% improvement over PP-OCRv4 for English text
- Pre-converted ONNX format (no paddle2onnx conversion needed)
- Source: `monkt/paddleocr-onnx` on Hugging Face

## Why We Switched

### Original Problem (Walkthrough 036)
We were running Chinese recognition models on English PDFs:
```
Input:  "Freud S The origins of psychoanalysis"
Output: "FreuDSTheonginsotpsvchoanavsis"
```

**Root causes:**
1. Chinese model doesn't understand English word boundaries
2. 150 DPI too low for quality recognition
3. Recognition preprocessing had width issues (1280px clamp on 300 DPI images)

### Solution
1. ✓ Increased DPI: 150 → 300 (processor.rs:37)
2. ✓ Downloaded PP-OCRv5 English ONNX models (this doc)
3. TODO: Add width clamping to recognition preprocessing
4. TODO: Integrate wordninja for word segmentation fallback

## Model Files

Located in: `/mnt/datadrive_m2/pdf_masher/models/`

```
pp-ocrv5_det_en.onnx    # Detection model (84 MB)
pp-ocrv5_rec_en.onnx    # Recognition model (7.5 MB)
en_dict.txt             # Character dictionary (1.4 KB)
```

### Download Commands

```bash
cd /mnt/datadrive_m2/pdf_masher/models

# Detection model
wget https://huggingface.co/monkt/paddleocr-onnx/resolve/main/detection/v5/det.onnx \
  -O pp-ocrv5_det_en.onnx

# Recognition model (English)
wget https://huggingface.co/monkt/paddleocr-onnx/resolve/main/languages/english/rec.onnx \
  -O pp-ocrv5_rec_en.onnx

# Character dictionary
wget https://huggingface.co/monkt/paddleocr-onnx/resolve/main/languages/english/dict.txt \
  -O en_dict.txt
```

## Integration Notes

### Current OCREngine Configuration
Located in: `pdf-mash/src/ocr/mod.rs` (or wherever OCREngine is defined)

**Need to update:**
- Detection model path: `models/pp-ocrv5_det_en.onnx`
- Recognition model path: `models/pp-ocrv5_rec_en.onnx`
- Character dictionary path: `models/en_dict.txt`

### Model Architecture
- **Detection**: Finds bounding boxes for text regions in images
- **Recognition**: Converts cropped text region images → actual text
- **Dictionary**: Maps model output indices → English characters

### ONNX Runtime Backend
PP-OCRv5 supports:
- Paddle Inference (requires full PaddlePaddle stack)
- **ONNX Runtime** (lightweight, what we're using)
- CUDA 12 for GPU acceleration

We're using ONNX Runtime with CPU inference (can add GPU later).

## Troubleshooting

### Why Not Use paddle2onnx?

**We tried.** It was hell:
```
CMake Error: Compatibility with CMake < 3.5 has been removed from CMake.
```

The issue: paddle2onnx 0.9.2 depends on onnx 1.9.0, which has ancient CMake requirements incompatible with modern systems.

**Attempted:**
- `uv pip install paddle2onnx` ❌
- `pip install paddle2onnx --break-system-packages` ❌
- PaddleX CLI method ❌
- Temporary venv in /tmp ❌

**Solution:** Use pre-converted ONNX models from Hugging Face (monkt/paddleocr-onnx).

### Alternative Model Sources

If `monkt/paddleocr-onnx` disappears:
1. **AIPLUX/paddleocr-ppocrv5-onnx** - Another HF repo with PP-OCRv5 ONNX
2. **marsena/paddleocr-onnx-models** - Focused on server detection models
3. **SWHL/RapidOCR** - Contains PP-OCRv4 models
4. **Official PaddleOCR releases** - Download .tar, convert with Docker-based paddle2onnx

### Model Versions

- **v5 (current)**: Latest, 11% improvement for English
- **v4**: Previous version, still good
- **v3**: Older, skip unless v4/v5 unavailable

Don't mix detection/recognition versions (e.g., v5 detection requires v5 recognition).

## Performance Expectations

### English Text Recognition (PP-OCRv5)
- **Clean PDFs**: 95%+ accuracy
- **Scanned documents**: 85-95% (depends on scan quality)
- **Poor scans/handwriting**: 70-85%

### Speed (CPU, single-threaded)
- Detection: ~100-200ms per page (300 DPI)
- Recognition: ~50-100ms per text region
- Post-correction: ~10-20ms per paragraph

Add CUDA for 3-5x speedup on GPU.

## Next Steps

1. **Update OCREngine** to use new model paths
2. **Add width clamping** to recognition preprocessing (see Walkthrough 036, Step 3)
3. **Integrate wordninja** for segmentation fallback (Step 4)
4. **Test on real PDF** and measure accuracy improvement
5. **Benchmark** detection + recognition + post-correction pipeline

## References

- [PP-OCRv5 Documentation](https://paddlepaddle.github.io/PaddleOCR/main/en/version3.x/algorithm/PP-OCRv5/PP-OCRv5.html)
- [monkt/paddleocr-onnx on Hugging Face](https://huggingface.co/monkt/paddleocr-onnx)
- [PP-OCRv5 Blog Post](https://huggingface.co/blog/baidu/ppocrv5)
- [PaddleOCR 3.0 Technical Report](https://arxiv.org/html/2507.05595v1)
- [Walkthrough 036](../walkthroughs/036.md) - OCR quality fix plan

---

**Last updated:** 2025-11-25
**Models downloaded:** 2025-11-25
**Current status:** Models ready, integration pending
