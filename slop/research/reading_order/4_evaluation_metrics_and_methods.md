# Evaluation Metrics and Methods for Reading Order Detection

## Primary Reading Order Metrics

### BLEU Score (Bilingual Evaluation Understudy)
- **Range**: 0-1 (or 0-100%)
- **Purpose**: Measures n-gram overlap between predicted and ground truth sequences
- **Variants**: BLEU-1, BLEU-2, BLEU-3, BLEU-4 (1-gram to 4-gram)
- **Calculation**: Page-level BLEU averaged across all pages
- **Interpretation**:
  - 0.988 (98.8%): Near-perfect (XY-Cut++)
  - 0.900-0.950: Excellent
  - 0.800-0.900: Good
  - <0.800: Needs improvement
- **Benchmarks**:
  - XY-Cut++: 98.8% (DocBench-100)
  - LayoutReader: +0.2847 improvement over baseline
- **Limitation**: Treats all positional errors equally (1 position vs 10 positions same penalty)
- **Best For**: Overall sequence correctness evaluation

### Average Relative Distance (ARD)
- **Purpose**: Quantifies how far elements deviate from correct positions
- **Unit**: Average positions off from ground truth
- **Interpretation**:
  - Lower is better
  - Measures severity of errors, not just presence
  - Estimates manual correction effort
- **Benchmarks**:
  - LayoutReader: Reduces ARD by 6.71 vs. baselines
  - Good systems: ~6.23 ARD per document (ReadingBank)
- **Advantage**: Differentiates between minor reorderings and major disruptions
- **Use Case**: Production systems prioritizing error severity understanding

### Position Error Rate (PER)
- **Range**: 0-100%
- **Definition**: Percentage of incorrectly positioned blocks
- **Interpretation**:
  - Lower is better
  - 0%: Perfect reading order
  - 50%+: Poor performance
- **Benchmarks**:
  - Learning-to-rank: 5.4% PER
  - MLP baselines: 56.5% PER
  - Heuristic XY-Cut: 61.7% PER
- **Advantage**: Intuitive block-level granularity
- **Best For**: Layout-centric evaluation where individual element placement matters

## Ranking Correlation Metrics

### Spearman's Footrule Distance
- **Purpose**: Measures ranking correlation via sum of absolute position differences
- **Range**: 0 (identical) to maximum (complete disagreement)
- **Formula**: Sum of |predicted_rank - true_rank| for all elements
- **Use Case**: Comparing overall ranking agreement

### Kendall's Tau
- **Purpose**: Pairwise comparison metric
- **Range**: -1 (perfect disagreement) to +1 (perfect agreement)
- **Calculation**: (Concordant pairs - Discordant pairs) / Total pairs
- **Advantages**:
  - Robust to outliers
  - Considers relative order correctness
  - Handles ties well
- **Interpretation**:
  - High Tau + Moderate BLEU = Correct pairwise relationships, different specific sequence
  - Multiple valid reading orders (handwritten documents)
- **Best For**: Documents with ambiguous reading order

## Layout Detection Metrics

### mean Average Precision (mAP)
- **Purpose**: Standard metric for object detection (underlying layout detection)
- **Variants**:
  - **mAP@0.5**: Single IoU threshold of 0.5
  - **mAP@0.5-0.95**: COCO-style, averages across IoU 0.5, 0.55, ..., 0.95
- **Calculation**:
  1. For each class: Precision-Recall curve
  2. Average Precision (AP): Area under PR curve
  3. mAP: Mean of AP across all classes
- **Benchmarks**:
  - Human (DocLayNet): 82-83% mAP (inter-annotator agreement)
  - YOLOv5x6: 76.8% mAP
  - GLAM: 68.6% mAP
  - GLAM + YOLOv5x6 ensemble: 80.8% mAP (SOTA)
  - LayoutLMv3: 95.1% mAP (PubLayNet)
  - DocLayout-YOLO: 77.4% mAP (DocLayNet), 65.6% mAP (D4LA)
- **Per-Class Performance (DocLayNet)**:
  - High: Text (88.1%), Table (86.3%), List-item (86.2%)
  - Low: Page-footer (61.1%), Formula (66.2%)
- **Gap Analysis**: 10% gap between SOTA (76.8%) and human (82-83%) indicates:
  - Genuine ambiguity in layout interpretation
  - Limitations in current architectures
  - Annotation inconsistency

### AP50 (Average Precision at IoU 0.5)
- **Purpose**: Simplified mAP variant using single threshold
- **Benchmarks**:
  - DocLayout-YOLO on DocLayNet: 93.0% AP50
  - DocLayout-YOLO on D4LA: 81.7% AP50
- **Use**: Faster to compute, easier to interpret than mAP@0.5-0.95

## Token Classification and IE Metrics

### F1 Score
- **Formula**: 2 × (Precision × Recall) / (Precision + Recall)
- **Range**: 0-1 (or 0-100%)
- **Benchmarks**:
  - LayoutLM on FUNSD: 79.27% F1 (form understanding)
  - LayoutLMv3 on FUNSD: 90.29% F1
  - LayoutLM on SROIE: 95.24% F1 (receipt parsing)
  - ROPE improvement: +8.4% F1-score over GCN baselines
- **Sensitivity**: Highly sensitive to reading order errors
- **Issue**: Correct extractions in wrong sequence tank performance
- **Solution**: Reading Order Independent Metrics

### Reading Order Independent Metrics
- **Purpose**: Match entities independent of sequence
- **Rationale**: Some applications don't require strict ordering
- **Method**: Entity matching by content/position, ignoring sequence
- **Use Case**: Information extraction where order doesn't matter for downstream task

## Document Understanding Benchmarks

### ICDAR Competition Metrics

#### ICDAR 2024 - Reading Documents Through Aria Glasses (Task B)
- **Challenge**: Reading order from low-resolution AR glass inputs
- **Metric**: BLEU score
- **Benchmark**: Winner achieved 0.0939 BLEU on difficult data
- **Insight**: AR/low-res significantly degrades reading order detection

#### ICDAR 2024 - Handwritten Document Recognition
- **Metrics**:
  - **PCRR**: Page-level Character Recognition Rate
  - **PWRR**: Page-level Word Recognition Rate
- **Benchmark**: Winner achieved 77.44% PCRR, 50.55% PWRR
- **Gap Analysis**: 27% difference between character and word accuracy
  - Suggests boundary detection and word formation challenges
  - Reading order preservation difficult in handwriting recognition

### OmniDocBench Evaluation
- **Coverage**:
  - Text extraction accuracy
  - Formula recognition
  - Table parsing
  - Reading order detection
- **Document Types**: 9 types, 4 layout types, 3 languages
- **Findings**:
  - Existing methods struggle with document diversity
  - MinerU VLM: State-of-the-art on benchmark
  - Surpasses Gemini 2.5 Pro, GPT-4o, Qwen2.5-VL-72B

### DocVQA (Document Visual Question Answering)
- **Purpose**: Measures document understanding via QA
- **Benchmark Impact**:
  - Idefics3-8B: +13.7 points over Idefics2
  - moondream2: +103% improvement with Docmatix training
- **Relevance**: Reading order critical for answering sequential questions

## Computational Efficiency Metrics

### Inference Speed (Latency)
- **Unit**: Seconds per page or Frames per second (FPS)
- **Benchmarks**:
  - **Ultra-fast**: GLAM 0.010s (100 pages/sec)
  - **Very fast**: Surya detection 0.108s, MinerU 0.21s
  - **Fast**: Surya layout 0.273s, Surya reading order 0.4s
  - **Moderate**: LayoutLMv3 0.687s
  - **Slow**: CPU-only processing 3.3s+
- **Speed Comparisons**:
  - GLAM vs LayoutLMv3: 68.7× faster
  - GLAM vs YOLOv5x6: 5.6× faster
- **FPS Benchmarks**:
  - DocLayout-YOLO: 85.5 FPS
  - GLAM: 243 pages/second

### Memory Consumption

#### VRAM Requirements (FP32)
- LayoutLM-base: 4-6GB
- LayoutLMv2: 6-8GB
- LayoutLMv3: 6-8GB
- Donut-base: 8-10GB
- YOLO models: 2-4GB
- Surya full stack: 20GB (max batch)

#### VRAM Requirements (INT8 Quantized)
- LayoutLM-base: 2-3GB (50% reduction)
- LayoutLMv2: 3-4GB
- Donut-base: 4-5GB
- YOLO models: 1-2GB

#### CPU RAM Requirements
- LayoutLM-base: ~8GB
- LayoutLMv2: ~12GB
- Donut-base: ~16GB
- Surya full stack: ~32GB
- Rule: ~2× GPU VRAM for similar CPU performance

### Throughput (Pages per Minute)
- **CPU-only**: 5-15 pages/min
- **GPU (consumer)**: 30-100 pages/min
- **GPU (production)**: 200+ pages/min per GPU

### Model Size
- **Ultra-lightweight**: GLAM 4M params
- **Lightweight**: PARAGRAPH2GRAPH 19.95M params
- **Medium**: DiT-BASE 86M params, LayoutLM 110M params
- **Large**: LayoutLMv3 133M params, LayoutLMv2 200M params
- **Very large**: DiT-LARGE 304M params
- **Comparison**: PARAGRAPH2GRAPH 6.7× smaller than LayoutLMv3

## Accuracy-Speed Tradeoff

### Approach Comparison

| Method | Accuracy | Speed | Memory | Use Case |
|--------|----------|-------|--------|----------|
| Seq2seq LayoutReader | Very High | Slow (0.687s) | High | Offline max accuracy |
| Parallel LayoutLMv3 | High | Moderate (0.13s) | Moderate | Production balance |
| Graph Neural Network | Competitive | Fast (0.01s) | Low | Real-time, interpretable |
| Heuristic XY-Cut | Poor (61.7% PER) | Very Fast | Minimal | Simple docs, prototyping |
| XY-Cut++ | Very High (98.8%) | Fast | Low | Multi-column production |

### Optimization Impact

#### Model Compilation (Surya on A10)
- Detection: +3.3% faster
- Layout: +0.9% faster
- Table recognition: +11.5% faster
- Note: Modest but compounds at scale

#### Quantization (INT8)
- Size: 2-4× smaller
- Speed: 2-3× faster (CPU with AVX512)
- Memory: ~2.2× reduction
- Accuracy: <1% loss with proper calibration

#### ONNX Runtime Optimization
- Operator fusion, constant folding
- Execution providers (CUDA, TensorRT, DirectML)
- Session tuning, I/O binding
- Typical speedup: 2× additional

## Evaluation Best Practices

### Metric Selection
1. **Primary**: BLEU (overall sequence correctness)
2. **Secondary**: ARD (error severity), PER (block-level)
3. **Robustness**: Kendall's Tau (pairwise correctness)
4. **Downstream**: Task-specific (F1, VQA accuracy)

### Cross-Dataset Evaluation
- **Purpose**: Measure generalization
- **Finding**: DocLayNet-trained models generalize better than PubLayNet/DocBank
- **Reason**: Dataset diversity > dataset size

### Human Baseline Establishment
- **DocLayNet**: 82-83% mAP (inter-annotator agreement)
- **Purpose**: Realistic performance ceiling
- **Insight**: 10% gap shows genuine ambiguity vs. model limitation

### Layout Complexity Stratification
- **Single-column**: High performance expected
- **Double-column**: Slight degradation acceptable
- **3+ columns**: Notable accuracy drop normal
- **Complex layouts**: Benchmark challenging cases separately

### Error Analysis Categories
1. **Local reordering**: Adjacent elements swapped (minor)
2. **Column confusion**: Multi-column reading order failure (major)
3. **Section skipping**: Missed entire blocks (critical)
4. **Caption-figure mismatch**: Wrong association (moderate)
5. **Table intrusion**: Reading order breaks at tables (common)

### Ablation Studies (LayoutReader findings)
- **Layout-only vs. text-only**:
  - Layout improvement: 0.27 BLEU
  - Text improvement: 0.16 BLEU
  - **Finding**: Layout information > text content for reading order

### Learning Curve Analysis
- **mAP vs. log(data size)**: Linear relationship
- **Plateau point**: ~80% of full dataset
- **Implication**: Focus on diversity over absolute size

## Benchmark Leaderboards

### DocLayNet Layout Detection
1. GLAM + YOLOv5x6 ensemble: 80.8% mAP
2. YOLOv5x6: 76.8% mAP
3. DocLayout-YOLO: 77.4% mAP
4. GLAM: 68.6% mAP
5. Human baseline: 82-83% mAP

### DocBench-100 Reading Order
1. XY-Cut++: 98.8% BLEU
2. Previous SOTA: ~74-75% BLEU (24% improvement)

### OmniDocBench Document Parsing
1. Mathpix (commercial): #1
2. MinerU: #2 (open-source leader)
3. Gemini 2.5 Pro: Lower
4. GPT-4o: Lower
5. Qwen2.5-VL-72B: Lower

### PubLayNet Layout Detection
- LayoutLMv3: 95.1% mAP
- DiT: 94.9% mAP
- Note: Homogeneous dataset, poor generalization

## Future Directions in Evaluation

### Challenges
1. **Multiple valid orders**: Some layouts have ambiguous reading flow
2. **Universal metrics**: BLEU penalizes creative but valid orderings
3. **Task-specific relevance**: Reading order importance varies by application
4. **Complexity quantification**: Need metrics for layout difficulty

### Proposed Solutions
1. **Reading Order Independent Metrics**: For order-agnostic tasks
2. **Human feedback integration**: Active learning for ambiguous cases
3. **Difficulty-adjusted scoring**: Weight by layout complexity
4. **Multi-reference evaluation**: Allow multiple valid ground truths
5. **Application-specific benchmarks**: RAG, TTS, translation-specific metrics
