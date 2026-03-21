# Permissively-Licensed Reading Order Detection Models (October 2025)

## Performance Comparison Summary

| Model | License | Size | Accuracy | Speed | Reading Order Support | ONNX Export | Best For |
|-------|---------|------|----------|-------|----------------------|-------------|----------|
| **Docling + Granite-Docling** | MIT/Apache 2.0 | 258M | 78% mAP | Fast | ✅ Native | ⚠️ Manual | **Reading order detection** |
| **Qwen2.5-VL-3B** | Apache 2.0 | 3B | SOTA (matches GPT-4o) | Slow | 🔄 Via prompting | ✅ Yes | End-to-end doc understanding |
| **Qwen2.5-VL-7B** | Apache 2.0 | 7B | SOTA | Slower | 🔄 Via prompting | ✅ Yes | Maximum accuracy |
| **Idefics3-8B** | Apache 2.0 | 8B | Excellent (+13.7 DocVQA) | Slower | 🔄 Via prompting | ✅ Yes | Document-specialized VLM |
| **Florence-2** | MIT | Lightweight | Good | Fast | ❌ Not specialized | ✅ Yes | General vision tasks |
| **mPLUG-DocOwl 2** | Apache 2.0 | Unknown | SOTA (10 benchmarks) | Moderate | 🔄 Implicit | ⚠️ TBD | OCR-free document understanding |
| **Donut** | MIT | 200M+ | Good (2022 SOTA) | Moderate | ❌ Not specialized | ✅ Yes | OCR-free (older model) |

**Legend:**
- ✅ Native: Built-in reading order detection module
- 🔄 Via prompting: Can be prompted to output reading order
- 🔄 Implicit: Learns reading order during training
- ❌ Not specialized: Not designed for reading order
- ⚠️ Manual: ONNX export requires manual process (not Optimum supported)
- ⚠️ TBD: ONNX export support unclear

---

## ❌ Non-Permissive Models (AVOID)

| Model | License | Issue |
|-------|---------|-------|
| LayoutReader | CC BY-NC-SA 4.0 | Non-commercial only |
| LayoutLMv2 | CC BY-NC-SA 4.0 | Non-commercial only |
| LayoutLMv3 | CC BY-NC-SA 4.0 | Non-commercial only |
| LayoutXLM | CC BY-NC-SA 4.0 | Non-commercial only |
| GOT-OCR 2.0 | Research only | Cannot use for any real application |
| Surya | GPL + Modified Rail-M | Free for startups <$2M, otherwise paid |
| Qwen2.5-VL-72B | Proprietary Qwen License | Commercial restrictions |

---

## 🏆 Top 3 Recommendations for Rust + ONNX Pipeline

### 1. **Docling (IBM) - BEST FOR READING ORDER**
**License**: MIT (codebase) + Apache 2.0 (models)

**Why it's #1:**
- Only permissively-licensed system with **explicit reading order detection**
- Can port MIT-licensed Python code to Rust
- Production-ready (IBM enterprise backing)
- Presented at AAAI 2025

**Capabilities:**
- Layout detection with RT-DETRv2 (Apache 2.0)
- Reading order inference module (MIT)
- Document conversion to Markdown/JSON
- Page layout, table structure, formulas
- Multi-format support (PDF, DOCX, PPTX, images)

**Performance:**
- 78% mAP on layout detection
- 28ms inference per page (NVIDIA A100)
- Best model: "heron-101" with 20.6-23.9% mAP improvement

**Links:**
- GitHub: https://github.com/docling-project/docling
- Models: https://huggingface.co/ibm-granite/granite-docling-258M
- Layout Models: https://huggingface.co/ds4sd/docling-models
- Website: https://www.docling.ai/

**ONNX Export Status:**
- RT-DETRv2 architecture not yet in HuggingFace Optimum
- Can use `torch.onnx.export` manually
- Issue #2176 tracking RT-DETR ONNX support

**Integration Strategy:**
1. Port reading order logic (MIT code) to Rust
2. Or call Docling Python module via subprocess
3. Or manually export layout models to ONNX with PyTorch

---

### 2. **Qwen2.5-VL-3B (Alibaba) - BEST ACCURACY**
**License**: Apache 2.0

**Why it's #2:**
- SOTA accuracy (matches GPT-4o on documents)
- Fully permissive Apache 2.0
- Can be prompted for reading order
- ONNX export supported

**Capabilities:**
- Enhanced visual recognition
- Precise object localization
- Robust document parsing
- Structured data extraction (invoices, forms, tables)
- Multi-page document understanding

**Performance:**
- Matches GPT-4o and Claude 3.5 Sonnet
- Excels at document and diagram understanding
- Released 2025

**Links:**
- GitHub: https://github.com/QwenLM/Qwen2.5-VL
- 3B Model: https://huggingface.co/Qwen/Qwen2.5-VL-3B-Instruct
- 7B Model: https://huggingface.co/Qwen/Qwen2.5-VL-7B-Instruct
- License: https://github.com/QwenLM/Qwen2.5-VL/blob/main/LICENSE

**Usage for Reading Order:**
```python
prompt = """
Given this document image with the following detected bounding boxes:
[bbox data in JSON format]

Please output the correct reading order as a numbered list,
considering multi-column layouts, tables, and figure captions.
"""
```

**Trade-offs:**
- Larger model (3B params) = slower inference
- End-to-end approach vs. specialized reading order
- Requires GPU for acceptable speed

---

### 3. **Idefics3-8B (HuggingFace) - DOCUMENT SPECIALIST**
**License**: Apache 2.0

**Why it's #3:**
- Specialized for document understanding
- Strong OCR and visual reasoning
- Trained on massive Docmatix dataset
- Apache 2.0 permissive

**Capabilities:**
- Enhanced OCR
- Document understanding (DocVQA +13.7 improvement)
- Visual reasoning
- Uses mPLUG-DocOwl-1.5 techniques
- Pixel shuffle strategy for better spatial awareness

**Training Data:**
- Docmatix: 2.4M images, 9.5M QA pairs (240× larger than previous)
- Dramatic improvement: +103% on DocVQA for smaller models

**Performance:**
- 8B parameters (larger than Qwen2.5-VL-3B)
- 13.7-point improvement over Idefics2 on DocVQA

**Links:**
- Model: https://huggingface.co/HuggingFaceM4/Idefics3-8B-Llama3
- Blog: https://huggingface.co/blog/idefics3
- Paper: https://arxiv.org/abs/2408.12637

**Trade-offs:**
- Even larger (8B params) = slowest of the three
- Best for document-heavy workflows
- Requires substantial VRAM (16GB+)

---

## 🎯 Recommended Implementation Strategy

### Option A: Pure Docling (Simplest)
**Approach:** Use Docling's reading order detection directly

```bash
# Install Docling
pip install docling

# Use as library or CLI
docling input.pdf --output markdown
```

**Pros:**
- Zero model training/export needed
- MIT licensed code (can port to Rust)
- Production-ready
- Fast inference

**Cons:**
- Need Python subprocess from Rust
- Or need to port reading order logic to Rust

---

### Option B: Hybrid XY-Cut++ + VLM Fallback (Best Performance)
**Approach:** Fast heuristics with ML safety net

```rust
pub fn determine_reading_order(
    bboxes: &[BBox],
    image: &DynamicImage,
    complexity_threshold: f32
) -> Result<Vec<BBox>> {

    // Phase 1: Try XY-Cut++ (algorithm, no license issues)
    let ordered = xy_cut_plus_plus(bboxes)?;
    let confidence = estimate_confidence(&ordered, bboxes);

    // Phase 2: Fallback to Qwen2.5-VL-3B for complex layouts
    if confidence < complexity_threshold {
        eprintln!("Complex layout detected, using VLM fallback...");
        ordered = qwen_vl_reading_order(image, bboxes)?;
    }

    Ok(ordered)
}

fn estimate_confidence(ordered: &[BBox], original: &[BBox]) -> f32 {
    // Heuristics: detect multi-column, tables crossing columns, etc.
    let column_count = detect_columns(original).len();
    let has_complex_tables = detect_complex_tables(original);

    if column_count >= 3 || has_complex_tables {
        return 0.5; // Low confidence
    }
    0.9 // High confidence
}
```

**Pros:**
- 98.8% BLEU on most documents (XY-Cut++)
- <10ms for simple layouts
- SOTA accuracy fallback for complex cases
- Fully permissive licenses

**Cons:**
- Need to implement XY-Cut++
- Need to export/integrate Qwen2.5-VL-3B

---

### Option C: Pure VLM (Highest Accuracy)
**Approach:** Always use Qwen2.5-VL-3B or Idefics3-8B

```rust
pub struct ReadingOrderVLM {
    session: ort::Session,
}

impl ReadingOrderVLM {
    pub fn detect_order(&self, image: &DynamicImage, bboxes: &[BBox]) -> Result<Vec<usize>> {
        // 1. Preprocess image + bboxes
        let input = self.prepare_input(image, bboxes)?;

        // 2. Run ONNX inference
        let output = self.session.run(input)?;

        // 3. Parse output (JSON or token sequence)
        let order_indices = self.parse_output(output)?;

        Ok(order_indices)
    }
}
```

**Pros:**
- Highest accuracy on complex documents
- Handles edge cases automatically
- No manual algorithm implementation

**Cons:**
- Slowest (130-400ms per page)
- Requires GPU for production
- Larger VRAM requirements (8-16GB)

---

## 📋 Quick Decision Matrix

**Choose Docling if:**
- ✅ You want the simplest integration
- ✅ You're okay with Python subprocess or porting code
- ✅ You need production-ready NOW
- ✅ Speed is important (28ms inference)

**Choose Qwen2.5-VL-3B if:**
- ✅ You need maximum accuracy
- ✅ You have GPU resources (8GB+ VRAM)
- ✅ You process complex, diverse documents
- ✅ You want end-to-end document understanding

**Choose Idefics3-8B if:**
- ✅ Documents are your primary use case
- ✅ You have substantial GPU (16GB+ VRAM)
- ✅ You want best-in-class document VQA
- ✅ OCR quality is critical

**Choose Hybrid (XY-Cut++ + VLM) if:**
- ✅ You want best of both worlds
- ✅ You have time to implement XY-Cut++
- ✅ Speed matters for 80% of documents
- ✅ Accuracy matters for 20% complex cases

---

## 🔧 ONNX Export Status

### ✅ Ready for ONNX Export:
- **Qwen2.5-VL**: Standard VLM export supported
- **Idefics3**: Standard VLM export supported
- **Florence-2**: Supported (MIT license)
- **Donut**: Supported (MIT license)

### ⚠️ Manual Export Needed:
- **Docling Layout Models**: RT-DETRv2 requires `torch.onnx.export`
  - Not in HuggingFace Optimum yet
  - Tracked in Issue #2176
  - Community workarounds exist

### 📚 ONNX Export Resources:
- HuggingFace Optimum: https://huggingface.co/docs/optimum/en/exporters/onnx/overview
- Manual export: https://huggingface.co/docs/transformers/serialization
- RT-DETR discussion: https://github.com/huggingface/optimum/issues/2176

---

## 🚀 Getting Started with Docling (Recommended)

### Install:
```bash
pip install docling
```

### Basic Usage:
```python
from docling.document_converter import DocumentConverter

converter = DocumentConverter()
result = converter.convert("document.pdf")

# Access reading order
for element in result.document.iterate_items():
    print(f"Order: {element.reading_order}, Type: {element.label}")

# Export to Markdown
markdown = result.document.export_to_markdown()
```

### Examine Reading Order Code:
```bash
# Clone repo
git clone https://github.com/docling-project/docling
cd docling

# Find reading order logic (MIT licensed)
grep -r "reading_order" --include="*.py"
```

### Port to Rust:
The reading order inference logic is MIT licensed, so you can:
1. Study the Python implementation
2. Port the algorithm to Rust
3. Use with your existing `BBox` structures

---

## 📊 Memory Requirements

| Model | FP32 VRAM | INT8 VRAM | CPU RAM | Batch Size |
|-------|-----------|-----------|---------|------------|
| Docling (layout) | 2-4GB | 1-2GB | 4-8GB | 1-8 |
| Qwen2.5-VL-3B | 12GB | 6GB | 24GB | 1-2 |
| Qwen2.5-VL-7B | 28GB | 14GB | 56GB | 1 |
| Idefics3-8B | 32GB | 16GB | 64GB | 1 |
| Florence-2 | 2-4GB | 1-2GB | 4-8GB | 1-8 |

**Your RTX 3090 (24GB):**
- ✅ Can run Qwen2.5-VL-3B with INT8 quantization
- ✅ Can run Docling layout models
- ⚠️ Cannot run Qwen2.5-VL-7B or Idefics3-8B in FP32
- ✅ Can run 7B/8B models with INT8 quantization

---

## 🎓 Additional MIT/Apache 2.0 Resources

### Other Permissive Models:
- **LayoutLM v1**: MIT license (110M params, older but permissive)
- **TrOCR**: MIT license (text recognition)
- **LiLT**: MIT license (layout-aware language model)

### Datasets (Permissive):
- **DocLayNet**: CDLA-Permissive license
- **ReadingBank**: Apache 2.0 (research use noted)

### Frameworks:
- **HuggingFace Transformers**: Apache 2.0
- **ONNX Runtime**: MIT license
- **PyTorch**: BSD-3-Clause

---

## 📞 Support and Community

### Docling:
- GitHub Issues: https://github.com/docling-project/docling/issues
- HuggingFace: https://huggingface.co/ds4sd

### Qwen:
- GitHub Issues: https://github.com/QwenLM/Qwen2.5-VL/issues
- Discussion: https://github.com/QwenLM/Qwen2.5-VL/discussions

### Idefics:
- HuggingFace Discussion: https://huggingface.co/HuggingFaceM4/Idefics3-8B-Llama3/discussions

---

## 🏁 Final Recommendation

**For your Rust + ONNX + RTX 3090 pipeline:**

1. **Start with Docling** (MIT/Apache 2.0)
   - Examine reading order logic
   - Port to Rust or use Python subprocess
   - Fastest path to production

2. **If accuracy isn't sufficient, add Qwen2.5-VL-3B fallback** (Apache 2.0)
   - Export to ONNX with INT8 quantization
   - Use for complex layouts only
   - Fits in 6GB VRAM (quantized)

3. **Implement XY-Cut++ eventually** (Algorithm, no license)
   - Replace Docling's heuristics
   - Achieve 98.8% BLEU
   - <10ms inference

This gives you:
- ✅ Fully permissive licenses
- ✅ Production-ready performance
- ✅ Clear upgrade path
- ✅ Fits your hardware
- ✅ Works with your Rust architecture

---

**Last Updated**: October 2025
**Next Review**: When Docling adds official ONNX export support for RT-DETRv2
