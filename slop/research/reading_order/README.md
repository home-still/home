# Reading Order Detection Research Collection

This directory contains comprehensive research on state-of-the-art reading order detection algorithms for PDF document extraction, compiled on 2025-10-24.

## Document Overview

### Main Research Document
- **[reading_order_sota.md](reading_order_sota.md)** - Comprehensive 120+ page research document covering:
  - Transformer-based models (LayoutReader, LayoutLMv3, DiT, DLAFormer, VLMs)
  - Graph neural network approaches (PARAGRAPH2GRAPH, GLAM, ROPE, GraphDoc)
  - MinerU production implementation details
  - Hybrid and rule-based methods (XY-Cut, XY-Cut++)
  - Training datasets and benchmarks
  - Evaluation metrics and performance comparisons
  - ONNX deployment and Rust integration
  - Computational efficiency analysis
  - Future directions and practical recommendations

### Support Documents

#### 1. [Key Papers and Publications](1_key_papers_and_publications.md)
Organized collection of seminal papers with direct links:
- **Transformer Models**: LayoutReader (EMNLP 2021), LayoutLMv3 (ACM MM 2022), DLAFormer (ICDAR 2024)
- **Graph Methods**: ROPE (ACL 2021), GLAM (2023), PARAGRAPH2GRAPH (2023), GraphDoc (ICLR 2025)
- **Hybrid Approaches**: XY-Cut++ (2024, 98.8% BLEU)
- **Production Systems**: MinerU (2024), Surya (2024), DocLayout-YOLO (2024)
- **VLM Approaches**: Idefics3-8B (2024)
- **Benchmarks**: OmniDocBench (CVPR 2025)

Each entry includes:
- Full citation and conference/journal
- arXiv, ACM, and other direct links
- Key contributions and innovations
- Performance benchmarks
- GitHub repositories

#### 2. [Datasets and Benchmarks](2_datasets_and_benchmarks.md)
Comprehensive dataset catalog with download links:
- **Reading Order Datasets**: ReadingBank (500K), DocBench-100
- **Layout Datasets**: DocLayNet (80K, gold standard), PubLayNet (360K), DocBank (500K)
- **Hierarchical**: HRDoc, Comp-HRDoc, GraphDoc (4.13M relations)
- **Synthetic**: DocSynth300K (300K generated documents)
- **Benchmarks**: OmniDocBench (CVPR 2025), DocStructBench

Includes:
- Dataset sizes and statistics
- Annotation methods and quality
- Document type coverage
- License information
- Performance comparisons
- Best practices for dataset selection

#### 3. [Implementation Tools and Libraries](3_implementation_tools_and_libraries.md)
Production-ready tools and frameworks:
- **Complete Frameworks**: MinerU (0.21s/page), Surya (90+ languages), DocLayout-YOLO (85.5 FPS)
- **Pre-trained Models**: LayoutLM family, YOLO variants, Donut
- **ONNX Deployment**: Export processes, quantization (2-4× speedup), optimization
- **Rust Integration**: layoutparser-ort, PDF processing crates
- **Python Libraries**: LayoutReader implementations, ONNX Runtime
- **Deployment Patterns**: Docker, Kubernetes, FastAPI, monitoring
- **Hardware Recommendations**: CPU-only, GPU consumer, GPU production

Includes code examples, performance metrics, and license considerations.

#### 4. [Evaluation Metrics and Methods](4_evaluation_metrics_and_methods.md)
Complete guide to measuring reading order performance:
- **Primary Metrics**: BLEU, ARD, PER
- **Ranking Metrics**: Spearman's Footrule, Kendall's Tau
- **Layout Metrics**: mAP, AP50, per-class performance
- **IE Metrics**: F1 scores, reading order independent metrics
- **Benchmarks**: ICDAR competitions, OmniDocBench, DocVQA
- **Efficiency Metrics**: Inference speed, memory consumption, throughput
- **Accuracy-Speed Tradeoffs**: Method comparisons and optimization impacts

Includes interpretation guidelines, benchmark leaderboards, and best practices.

#### 5. [Additional Resources and Links](5_additional_resources_and_links.md)
Curated collection of 100+ resources:
- **GitHub Repositories**: Core projects, datasets, community resources
- **HuggingFace**: Models, datasets, paper pages
- **Academic Resources**: ACL, ACM, arXiv, Microsoft Research, Google Research
- **APIs and Services**: Surya API, IBM Data Exchange
- **Documentation**: Rust crates, Python packages
- **Articles and Tutorials**: Technical guides, research summaries
- **Research Groups**: Labs and key researchers
- **License Information**: Dataset and model licenses
- **How to Stay Updated**: Conference alerts, arXiv monitoring

## Quick Reference

### Performance Leaders (2024-2025)

#### Reading Order Accuracy
1. **XY-Cut++**: 98.8% BLEU (hybrid approach)
2. **LayoutReader**: Near-perfect on ReadingBank
3. **MinerU VLM**: SOTA on OmniDocBench

#### Speed Champions
1. **GLAM**: 0.010s/page (243 pages/sec)
2. **MinerU**: 0.21s/page with GPU
3. **DocLayout-YOLO**: 85.5 FPS for layout

#### Production Ready
- **MinerU**: Best balance, dual-backend, 13K+ stars
- **Surya**: Complete toolkit, 90+ languages
- **DocLayout-YOLO**: Fast layout detection

### Key Findings

1. **Accuracy Ceiling**: Human inter-annotator agreement ~82-83% mAP shows inherent ambiguity
2. **Layout > Text**: Layout information contributes more to reading order than text content
3. **Speed Tradeoff**:
   - Seq2seq: Very high accuracy, slow (0.687s)
   - Parallel classification: High accuracy, moderate speed (0.13s)
   - Graph methods: Competitive accuracy, very fast (0.01s)
   - Heuristics: Poor accuracy (61.7% PER), very fast
4. **Generalization**: DocLayNet-trained models generalize better than PubLayNet/DocBank
5. **Dataset Size**: Diversity matters more than size; 80% of data often sufficient

### Recommended Implementations (2025)

**For Maximum Accuracy**:
- Deploy LayoutReader or MinerU VLM backend
- Accept computational cost
- Best for offline processing, quality-critical applications

**For Production Balance**:
- YOLOv5x6 layout + parallel LayoutLMv3 reading order
- 70-80% end-to-end accuracy at 30-100 pages/min
- Best for production services with moderate throughput

**For Multi-Column Documents**:
- XY-Cut++ achieving 98.8% BLEU
- Hybrid heuristic-ML approach
- Best for newspapers, academic papers, complex layouts

**For Edge/Embedded**:
- Graph methods (GLAM: 98 pages/sec, 4M params)
- Rust implementation (layoutparser-ort)
- Best for resource-constrained, real-time requirements

### License Considerations

**Commercial-Friendly (MIT/Apache 2.0)**:
- LayoutLM v1 ✓
- MinerU ✓
- Donut ✓
- layoutparser-ort ✓

**Non-Commercial Only (CC BY-NC-SA 4.0)**:
- LayoutLMv2/v3 ✗
- Train custom models or license alternatives for commercial use

**Limited Commercial**:
- Surya: Free for startups <$2M revenue
- DocLayout-YOLO: AGPL-3.0 (copyleft)

## Research Timeline

- **2019**: PubLayNet (ICDAR Best Paper)
- **2021**: LayoutReader (EMNLP), ROPE (ACL)
- **2022**: LayoutLMv3 (ACM MM), DiT (CVPR), DocLayNet (SIGKDD)
- **2023**: GLAM, PARAGRAPH2GRAPH, HRDoc (AAAI), Comp-HRDoc
- **2024**: XY-Cut++ (98.8% BLEU), DLAFormer (ICDAR), DocLayout-YOLO, MinerU, Surya, Idefics3
- **2025**: GraphDoc (ICLR), OmniDocBench (CVPR)

## Key Contributors

### Organizations
- **Microsoft Research**: LayoutLM family, ReadingBank, DocBank, Comp-HRDoc
- **Shanghai AI Lab (OpenDataLab)**: MinerU, DocLayout-YOLO, OmniDocBench
- **IBM Research**: PubLayNet
- **Google Research**: ROPE
- **datalab.to**: Surya

### Researchers to Follow
- Lei Cui (Microsoft): lecu@microsoft.com
- Furu Wei (Microsoft): fuwei@microsoft.com
- Chen-Yu Lee (Google)
- Jiawei Wang (DLAFormer)

## Future Directions

### Open Research Questions
1. Reading order in 5+ column complex layouts (magazines, newspapers)
2. Optimal token budget allocation in multimodal transformers
3. Universal evaluation metrics for diverse document types
4. Human-in-the-loop active learning for ambiguous cases
5. Dynamic/interactive document formats (responsive web, adaptive PDFs)

### Performance Gaps
- 10-15% gap between SOTA (72-77% mAP) and human baseline (82-83%)
- Represents both realistic ceiling (ambiguity) and opportunity (architecture improvements)

### Emerging Trends
1. **Unified Architectures**: Single model for detection + reading order + hierarchy
2. **Efficiency Improvements**: FastViT-style hybrids, selective computation, dynamic tokens
3. **Zero-shot Capabilities**: VLMs with document instruction tuning
4. **Few-shot Adaptation**: Transfer across document types without fine-tuning

## Using This Research

### For Researchers
- Start with main document for comprehensive overview
- Dive into specific papers via links in document #1
- Use datasets from document #2 for training/evaluation
- Compare against metrics in document #4

### For Practitioners
- Check implementation guide (document #3) for production deployment
- Review license considerations before selecting models
- Use hardware recommendations for infrastructure planning
- Follow deployment patterns for scalability

### For Students
- Read main document sections in order
- Work through key papers chronologically
- Implement simple approaches (XY-Cut) before complex (transformers)
- Use evaluation metrics guide to measure progress

## Updates and Maintenance

**Last Updated**: 2025-10-24

**Sources**: All information sourced from peer-reviewed papers, official documentation, and verified open-source projects.

**Verification**: Links and benchmarks verified as of compilation date. Some links may become outdated; refer to GitHub repos for latest.

**Contributions**: This is a research collection. For corrections or updates, verify against original sources listed in documents #1 and #5.

## Citation

If using this research collection, please cite the original papers. Key citations:

```bibtex
@inproceedings{wang2021layoutreader,
  title={LayoutReader: Pre-training of Text and Layout for Reading Order Detection},
  author={Wang, Zilong and Xu, Yiheng and Cui, Lei and Shang, Jingbo and Wei, Furu},
  booktitle={EMNLP},
  year={2021}
}

@inproceedings{huang2022layoutlmv3,
  title={LayoutLMv3: Pre-training for Document AI with Unified Text and Image Masking},
  author={Huang, Yupan and Lv, Tengchao and Cui, Lei and Lu, Yutong and Wei, Furu},
  booktitle={ACM MM},
  year={2022}
}

@article{pfitzmann2022doclaynet,
  title={DocLayNet: A Large Human-Annotated Dataset for Document-Layout Analysis},
  author={Pfitzmann, Birgit and others},
  journal={ACM SIGKDD},
  year={2022}
}
```

## Related Collections

- [Awesome Document AI](https://github.com/tstanislawek/awesome-document-ai)
- [Awesome OCR](https://github.com/kba/awesome-ocr)
- [Awesome Layout Analysis](https://github.com/Layout-Parser/layout-parser)

---

**Navigation**:
- [Main Research](reading_order_sota.md)
- [Papers](1_key_papers_and_publications.md)
- [Datasets](2_datasets_and_benchmarks.md)
- [Tools](3_implementation_tools_and_libraries.md)
- [Metrics](4_evaluation_metrics_and_methods.md)
- [Resources](5_additional_resources_and_links.md)
