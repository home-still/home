# Implementation Tools and Libraries for Reading Order Detection

## Complete Document AI Frameworks

### MinerU
- **Organization**: Shanghai AI Laboratory, OpenDataLab
- **GitHub**: [opendatalab/MinerU](https://github.com/opendatalab/MinerU)
- **PyPI**: [magic-pdf](https://pypi.org/project/magic-pdf/)
- **Paper**: [arXiv:2409.18839](https://arxiv.org/abs/2409.18839)
- **Architecture**: Dual-backend system
  - **Pipeline Backend**: Atomic models (DocLayout-YOLO + PaddleOCR + LayoutReader)
  - **VLM Backend**: 1.2B parameter multimodal model (v2.0+)
- **Reading Order Integration**:
  - Pipeline: `ppaanngggg/layoutreader` package (LayoutLMv3-based)
  - VLM: Native reading order learning (emergent behavior)
- **Performance**:
  - Speed: 0.21s per page (GPU), 3.3s per page (CPU)
  - Accuracy: 2nd on OmniDocBench (open-source)
- **Versions**:
  - v0.9.0 (Oct 2024): LayoutReader integration
  - v2.0 (Jun 2025): VLM backend
  - v2.5 (Sep 2025): Enhanced VLM model
- **Known Limitations**:
  - 3+ column layouts
  - Vertical text (improving in v2.1.0+)
  - Comic books, art albums, handwritten notes
- **Output**: content_list.json with ordered blocks + Markdown
- **Languages**: Multi-language support (84 languages in OCR)

### Surya
- **Organization**: datalab.to
- **GitHub**: [datalab-to/surya](https://github.com/datalab-to/surya)
- **API**: [api.datalab.to/surya](https://api.datalab.to/surya)
- **Stars**: 13,000+ on GitHub
- **Features**:
  - OCR (90+ languages)
  - Layout analysis
  - Reading order detection
  - Table recognition
  - Line detection
- **Architecture**:
  - Text Detection: Modified EfficientViT
  - Recognition: Modified Donut with grouped query attention + MoE layers
  - Reading Order: Seq2seq models on custom datasets
  - Decoding: UTF-16
- **Performance (A10 GPU)**:
  - Detection: 0.108s per page (batch 36, 16GB VRAM)
  - Layout: 0.273s per page (batch 32, 7GB VRAM, 88% accuracy)
  - Reading Order: 0.4s per page (88% accuracy)
  - Table Recognition: 0.022s per page (batch 64, 10GB VRAM)
  - OCR: 0.62s per page (batch 512, 20GB VRAM)
- **Compilation** (optional speedup):
  - `COMPILE_DETECTOR=true`: +3.3% faster
  - `COMPILE_LAYOUT=true`: +0.9% faster
  - `COMPILE_TABLE_REC=true`: +11.5% faster
  - `COMPILE_ALL=true`: Enable all
- **License**:
  - Code: GPL
  - Models: Modified AI Pubs Open Rail-M
  - Commercial: Free for startups <$2M revenue
- **Languages**: Works with any language for layout/reading order

### DocLayout-YOLO
- **GitHub**: [opendatalab/DocLayout-YOLO](https://github.com/opendatalab/DocLayout-YOLO)
- **Paper**: [arXiv:2410.12628](https://arxiv.org/abs/2410.12628)
- **PyPI**: [doclayout-yolo](https://pypi.org/project/doclayout-yolo/)
- **HuggingFace**: [juliozhao/DocLayout-YOLO-DocStructBench](https://huggingface.co/juliozhao/DocLayout-YOLO-DocStructBench)
- **Base Model**: YOLOv10
- **Pre-training**: DocSynth300K (300K synthetic documents)
- **Performance**:
  - D4LA: 81.7% AP50, 65.6% mAP
  - DocLayNet: 93.0% AP50, 77.4% mAP
  - DocStructBench: 78.8% mAP
  - Speed: 85.5 FPS
- **Innovation**:
  - Global-to-Local Adaptive Perception (multi-scale detection)
  - Mesh-candidate BestFit algorithm (2D bin packing for synthesis)
- **SDK Usage**:
  ```python
  from ultralytics import YOLOv10
  model = YOLOv10("path/to/model.pt")
  results = model.predict(
      image,
      imgsz=1024,
      conf=0.2,
      device='cuda'
  )
  ```
- **Categories Detected**: Text, Title, List-item, Section-header, Page-header, Page-footer, Table, Picture, Formula
- **License**: AGPL-3.0

## Pre-trained Models

### LayoutLM Family (Microsoft)
- **LayoutLM (v1)**:
  - Model: `microsoft/layoutlm-base-uncased`
  - Parameters: 110M
  - License: MIT (commercial use OK)
  - ONNX Support: ✓ Full official support
  - Memory: 4-6GB VRAM (FP32), 2-3GB (INT8)

- **LayoutLMv2**:
  - Model: `microsoft/layoutlmv2-base-uncased`
  - Parameters: 200M
  - License: CC BY-NC-SA 4.0 (non-commercial only)
  - ONNX Support: Partial (community workarounds)
  - Memory: 6-8GB VRAM (FP32), 3-4GB (INT8)

- **LayoutLMv3**:
  - Model: `microsoft/layoutlmv3-base`
  - Parameters: 133M (44M fewer than v2)
  - License: CC BY-NC-SA 4.0 (non-commercial only)
  - ONNX Support: Partial (custom config required)
  - Memory: Similar to v2
  - Issue: [GitHub #14368](https://github.com/huggingface/transformers/issues/14368)

- **LayoutXLM**:
  - Model: `microsoft/layoutxlm-base`
  - Languages: 53 languages (multilingual)
  - License: CC BY-NC-SA 4.0

### YOLO-based Models (ONNX-ready)
- **YOLOv10 DocLayNet**:
  - Model: `Oblix/yolov10m-doclaynet_ONNX_document-layout-analysis`
  - Format: Pre-exported ONNX
  - Runtime: ONNX Runtime, Transformers.js

- **YOLOv8 DocLayNet**:
  - Model: `Oblix/yolov8x-doclaynet_ONNX`
  - Format: Pre-exported ONNX
  - Deployment: JavaScript/TypeScript ready

### Other Models
- **Donut**:
  - Model: `naver-clova-ix/donut-base`
  - Parameters: 200M+
  - License: MIT
  - Feature: OCR-free document understanding
  - Memory: 8-10GB VRAM (FP32), 4-5GB (INT8)

## ONNX Deployment

### Export Process (LayoutLM v1)
```bash
# Install dependencies
pip install transformers[onnx]

# Export to ONNX
python -m transformers.onnx \
    --model=microsoft/layoutlm-base-uncased \
    onnx/
```

### Quantization (Hugging Face Optimum)
```bash
# Install Optimum
pip install optimum[onnxruntime]

# Dynamic quantization (no calibration data)
optimum-cli onnxruntime quantize \
    --onnx_model model_dir \
    --avx512_vnni \
    --output quantized_model
```

**Quantization Benefits**:
- Size: 2-4× smaller
- Speed: 2-3× faster on CPU (with AVX512)
- Memory: ~2.2× reduction
- Accuracy: <1% loss with proper calibration

### ONNX Runtime Optimization
- **Operator Fusion**: Combine multiple ops
- **Constant Folding**: Pre-compute static values
- **Execution Providers**:
  - CUDA (NVIDIA GPU)
  - TensorRT (NVIDIA optimized)
  - DirectML (Windows GPU)
- **Session Options**: Thread tuning, memory pattern optimization
- **I/O Binding**: Reduce copy overhead
- **Typical Speedup**: 2× additional with careful tuning

### Alternative: Intel OpenVINO
```bash
# Install OpenVINO
pip install openvino-dev

# Convert ONNX to IR format
mo --input_model model.onnx \
   --output_dir openvino_model/
```

## Rust Integration

### layoutparser-ort
- **Crate**: [layoutparser-ort](https://crates.io/crates/layoutparser-ort)
- **Docs**: [docs.rs/layoutparser-ort](https://docs.rs/layoutparser-ort)
- **GitHub**: [styrowolf/layoutparser-ort](https://github.com/styrowolf/layoutparser-ort)
- **Purpose**: Rust port of LayoutParser for ONNX models
- **Runtime**: ONNX via `ort` crate bindings
- **Models Supported**:
  - Detectron2 (LayoutParser-PubLayNet)
  - YOLOX (DocLayNet-trained)
  - Table-Transformer
  - Donut
- **License**: Apache 2.0
- **Advantages**:
  - No Python runtime overhead
  - Deterministic latency
  - Minimal memory footprint
  - Embedded/edge deployment

**Example Usage**:
```rust
use layoutparser_ort::LayoutModel;

let model = LayoutModel::from_pretrained("model.onnx")?;
let bboxes: Vec<BBox> = model.detect(&image)?;
```

### PDF Processing Crates
- **lopdf**:
  - Pure Rust PDF read/write
  - Text extraction, bounding boxes
  - Object stream support
  - Encrypted PDF decryption (auto)
  - Requires: Rust 1.85+

- **pdf-rs/pdf**:
  - Read, alter, write PDFs
  - Experimental write support
  - Hierarchical primitive visualization

- **printpdf**:
  - Create, read, write, render PDFs
  - Pure Rust implementation
  - License: MIT

### Typical Rust Pipeline
```rust
// 1. Load PDF
let doc = lopdf::Document::load("input.pdf")?;

// 2. Extract page images
let page_image = extract_page(&doc, 0)?;

// 3. Initialize layout model
let model = layoutparser_ort::LayoutModel::from_pretrained("layout.onnx")?;

// 4. Detect layout
let bboxes: Vec<BBox> = model.detect(&page_image)?;

// 5. Apply reading order logic
let ordered = sort_reading_order(&bboxes);
```

## Python Libraries

### LayoutReader Implementations
- **Original (Microsoft)**:
  - Seq2seq architecture
  - High accuracy, slow inference (~687ms)
  - Autoregressive decoding

- **ppaanngggg/layoutreader** (Used by MinerU):
  - GitHub: [ppaanngggg/layoutreader](https://github.com/ppaanngggg/layoutreader)
  - Base: LayoutLMv3ForTokenClassification
  - Speed: Faster single-pass prediction
  - Input: Bounding boxes (0-1000 normalized)
  - Output: Sequence indices (reading order)

### ONNX Runtime (Python)
```python
import onnxruntime as ort

# Create session with GPU
session = ort.InferenceSession(
    "model.onnx",
    providers=['CUDAExecutionProvider', 'CPUExecutionProvider']
)

# Configure threads (CPU)
import os
os.environ['OMP_NUM_THREADS'] = str(cpu_count // 2)

# Run inference
outputs = session.run(None, input_dict)
```

### Transformers (HuggingFace)
```python
from transformers import AutoModel

# Load LayoutLM
model = AutoModel.from_pretrained("microsoft/layoutlm-base-uncased")

# Load LayoutLMv3
model = AutoModel.from_pretrained("microsoft/layoutlmv3-base")
```

## Deployment Patterns

### Docker Container
```dockerfile
FROM nvidia/cuda:11.8-cudnn8-runtime-ubuntu22.04

# Pre-download models during build
RUN python -c "from transformers import AutoModel; \
               AutoModel.from_pretrained('microsoft/layoutlm-base-uncased')"

# Configure threads
ENV OMP_NUM_THREADS=8

# Use persistent volume for cache
VOLUME /root/.cache/huggingface
```

### FastAPI REST Endpoint
```python
from fastapi import FastAPI
import onnxruntime as ort

app = FastAPI()
session = ort.InferenceSession("model.onnx")

@app.post("/predict")
async def predict(image: bytes):
    result = session.run(None, preprocess(image))
    return {"reading_order": result}
```

### Kubernetes Deployment
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: reading-order-service
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: service
        image: reading-order:latest
        resources:
          limits:
            nvidia.com/gpu: 1
            memory: 16Gi
```

### Monitoring (Prometheus + Grafana)
**Key Metrics**:
- Inference latency (p50, p95, p99)
- Throughput (pages/second)
- GPU/CPU utilization
- Memory consumption
- Queue depth
- Error rates

### Scaling Strategies
- **Horizontal**: Multiple replicas (stateless design)
- **Vertical**: Multiple GPUs + batch processing
- **Auto-scaling**: Based on queue depth
- **Cost Optimization**:
  - Spot instances for batch jobs
  - CPU fallback for low-priority
  - INT8/INT4 quantization (2-4× VRAM reduction)

## Hardware Recommendations

### Minimum (CPU-only)
- CPU: 8+ cores (Intel i7/AMD Ryzen 7)
- RAM: 16GB
- Storage: SSD
- Throughput: 5-15 pages/minute

### Recommended (GPU)
- GPU: NVIDIA RTX 3060 (12GB) or better
- CPU: 4+ cores
- RAM: 16GB
- VRAM: 8-12GB minimum
- Throughput: 30-100 pages/minute

### Production (High-throughput)
- GPU: NVIDIA A10 (24GB), A100 (40/80GB), H100
- CPU: 16+ cores
- RAM: 64+ GB
- Multi-GPU: Parallel processing
- Throughput: 200+ pages/minute per GPU

## Performance Comparison

| Tool | Speed (GPU) | Speed (CPU) | Languages | Reading Order | License |
|------|-------------|-------------|-----------|---------------|---------|
| MinerU | 0.21s/page | 3.3s/page | 84 | ✓ Yes | Apache 2.0 |
| Surya | 0.4s/page (RO) | - | 90+ | ✓ Yes | GPL/Modified Rail-M |
| DocLayout-YOLO | 85.5 FPS | - | Any | No (layout only) | AGPL-3.0 |
| LayoutReader (original) | 0.687s/page | - | Any | ✓ Yes | CC BY-NC-SA 4.0 |
| GLAM | 0.010s/page | - | Any | Partial | - |

## License Considerations

### Commercial-Friendly
- LayoutLM v1: MIT ✓
- MinerU: Apache 2.0 ✓
- Donut: MIT ✓
- layoutparser-ort: Apache 2.0 ✓

### Non-Commercial Only
- LayoutLMv2/v3: CC BY-NC-SA 4.0 ✗
- LayoutXLM: CC BY-NC-SA 4.0 ✗

### Limited Commercial
- Surya: Free for startups <$2M revenue
- DocLayout-YOLO: AGPL-3.0 (copyleft)

**Recommendation**: For production commercial use, either:
1. Use MIT/Apache 2.0 licensed models
2. Train custom models on permissive datasets
3. License commercial alternatives
