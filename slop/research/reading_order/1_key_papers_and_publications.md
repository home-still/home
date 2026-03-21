# Key Papers and Publications on Reading Order Detection

## Transformer-Based Models

### LayoutReader (2021)
- **Title**: LayoutReader: Pre-training of Text and Layout for Reading Order Detection
- **Conference**: EMNLP 2021
- **Authors**: Microsoft Research Team
- **arXiv**: [2108.11591](https://arxiv.org/abs/2108.11591)
- **Links**:
  - [Microsoft Research Page](https://www.microsoft.com/en-us/research/publication/layoutreader-pre-training-of-text-and-layout-for-reading-order-detection/)
  - [HuggingFace Paper Page](https://huggingface.co/papers/2108.11591)
  - [Papers with Code](https://paperswithcode.com/paper/layoutreader-pre-training-of-text-and-layout)
  - [GitHub - ReadingBank Dataset](https://github.com/doc-analysis/ReadingBank)
  - [GitHub - Faster Implementation](https://github.com/ppaanngggg/layoutreader)
- **Key Contribution**: First large-scale deep learning approach using seq2seq architecture with LayoutLM-based encoder for reading order prediction
- **Dataset**: Introduced ReadingBank with 500,000 documents

### LayoutLMv3 (2022)
- **Title**: LayoutLMv3: Pre-training for Document AI with Unified Text and Image Masking
- **Conference**: ACM MM 2022 (30th ACM International Conference on Multimedia)
- **Authors**: Yupan Huang, Tengchao Lv, Lei Cui, Yutong Lu, Furu Wei
- **Links**:
  - [ACM Digital Library](https://dl.acm.org/doi/10.1145/3503161.3548112)
  - [arXiv PDF](https://arxiv.org/pdf/2204.08387)
  - [Microsoft Research](https://www.microsoft.com/en-us/research/publication/layoutlmv3-pre-training-for-document-ai-with-unified-text-and-image-masking/)
  - [HuggingFace Model Hub](https://huggingface.co/microsoft/layoutlmv3-base)
  - [GitHub - UniLM Project](https://github.com/microsoft/unilm)
- **Key Innovation**: First multimodal model eliminating CNN backbones, using direct ViT-style linear projection
- **Architecture**: 12 layers, 768 hidden dims, 133M parameters (BASE)
- **Performance**: 90.29% F1 on FUNSD, 95.1% mAP on PubLayNet

### DLAFormer (2024)
- **Title**: DLAFormer: An End-to-End Transformer For Document Layout Analysis
- **Conference**: ICDAR 2024 (Oral)
- **Authors**: Jiawei Wang, Kai Hu, Qiang Huo
- **arXiv**: [2405.11757](https://arxiv.org/abs/2405.11757)
- **Links**:
  - [arXiv HTML](https://arxiv.org/html/2405.11757v1)
  - [ACM Digital Library](https://dl.acm.org/doi/10.1007/978-3-031-70546-5_3)
- **Key Approach**: Unified end-to-end transformer treating reading order as relation prediction
- **Innovation**: Type-wise queries and coarse-to-fine strategy for concurrent processing of multiple tasks
- **Datasets**: DocLayNet and Comp-HRDoc

## Graph Neural Network Approaches

### ROPE (2021)
- **Title**: ROPE: Reading Order Equivariant Positional Encoding for Graph-based Document Information Extraction
- **Conference**: ACL 2021 (59th Annual Meeting, Short Papers)
- **Authors**: Chen-Yu Lee, Chun-Liang Li, Chu Wang, Renshen Wang, Yasuhisa Fujii, Siyang Qin, Ashok Popat, Tomas Pfister
- **Pages**: 314–321
- **arXiv**: [2106.10786](https://arxiv.org/abs/2106.10786)
- **Links**:
  - [ACL Anthology](https://aclanthology.org/2021.acl-short.41/)
  - [Google Research](https://research.google/pubs/pub50447/)
  - [Papers with Code](https://paperswithcode.com/paper/rope-reading-order-equivariant-positional)
  - [PDF](https://aclanthology.org/2021.acl-short.41.pdf)
- **Key Contribution**: Positional encoding for GCNs to capture sequential presentation order
- **Performance**: Improves existing GCNs by up to 8.4% F1-score

### GLAM (2023)
- **Title**: A Graphical Approach to Document Layout Analysis
- **arXiv**: [2308.02051](https://arxiv.org/abs/2308.02051)
- **Links**:
  - [arXiv HTML](https://ar5iv.labs.arxiv.org/html/2308.02051)
  - [Papers with Code](https://paperswithcode.com/paper/a-graphical-approach-to-document-layout)
- **Key Innovation**: Lightweight GNN with only 4M parameters
- **Performance**: 10ms per page (243 pages/second), 68.6% mAP on DocLayNet
- **Speed**: 68.7× faster than LayoutLMv3, 5.6× faster than YOLOv5x6
- **Ensemble**: Combined with YOLOv5x6 achieves 80.8% mAP (new SOTA)

### PARAGRAPH2GRAPH (2023)
- **Title**: PARAGRAPH2GRAPH: A GNN-based framework for layout paragraph analysis
- **arXiv**: [2304.11810](https://arxiv.org/abs/2304.11810)
- **Authors**: Shu Wei, Nuo Xu
- **Date**: April 24, 2023
- **Key Features**: Language-independent, 19.95M parameters, handles arbitrarily long documents
- **Advantage**: No transformer sequence length constraints

### GraphDoc (2025)
- **Title**: Graph-based Document Structure Analysis
- **arXiv**: [2502.02501](https://arxiv.org/abs/2502.02501)
- **Conference**: ICLR 2025
- **Links**:
  - [arXiv HTML](https://arxiv.org/html/2502.02501v1)
  - [ICLR PDF](https://proceedings.iclr.cc/paper_files/paper/2025/file/cf3d7d8e79703fe947deffb587a83639-Paper-Conference.pdf)
- **Dataset**: 80,000 images, 4.13M relation annotations
- **Relations**: 8 types (spatial: Up/Down/Left/Right, logical: Parent/Child/Sequence/Reference)
- **Model**: Document Relation Graph Generator (DRGG)

## Hybrid and Rule-Based Methods

### XY-Cut++ (2024)
- **Title**: XY-Cut++: Advanced Layout Ordering via Hierarchical Mask Mechanism on a Novel Benchmark
- **arXiv**: [2504.10258](https://arxiv.org/abs/2504.10258)
- **Date**: April 2025
- **Links**:
  - [arXiv HTML](https://arxiv.org/html/2504.10258v1)
  - [ResearchGate](https://www.researchgate.net/publication/390773723_XY-Cut_Advanced_Layout_Ordering_via_Hierarchical_Mask_Mechanism_on_a_Novel_Benchmark)
  - [AI Research Paper Details](https://www.aimodels.fyi/papers/arxiv/xy-cut-advanced-layout-ordering-via-hierarchical)
- **Performance**: 98.8% BLEU overall (98.6% complex, 98.9% regular)
- **Improvement**: 24% over baselines
- **Benchmark**: DocBench-100 (100 pages: 30 complex, 70 regular)
- **Innovation**: Pre-masking, multi-granularity segmentation, cross-modal matching

## Production Systems

### MinerU (2024)
- **Title**: MinerU: An Open-Source Solution for Precise Document Content Extraction
- **arXiv**: [2409.18839](https://arxiv.org/abs/2409.18839)
- **Date**: September 27, 2024
- **Organization**: Shanghai AI Laboratory, OpenDataLab
- **Links**:
  - [GitHub](https://github.com/opendatalab/MinerU)
  - [HuggingFace Paper](https://huggingface.co/papers/2409.18839)
  - [PyPI - magic-pdf](https://pypi.org/project/magic-pdf/)
  - [MarkTechPost Article](https://www.marktechpost.com/2024/10/05/mineru-an-open-source-pdf-data-extraction-tool/)
- **Architecture**: Dual-backend (pipeline + VLM)
- **Reading Order**: LayoutReader integration in pipeline backend (v0.9.0+), native in VLM backend
- **Performance**: 0.21s per page with GPU (fastest among open-source tools)

### Surya (2024)
- **Title**: OCR, layout analysis, reading order, table recognition in 90+ languages
- **Organization**: datalab.to
- **Links**:
  - [GitHub](https://github.com/datalab-to/surya)
  - [API](https://api.datalab.to/surya)
- **Features**: Complete document AI toolkit
- **Performance on A10 GPU**:
  - Detection: 0.108s per page
  - Layout: 0.27s per page (88% accuracy)
  - Reading order: 0.4s per page (88% accuracy)
  - Table recognition: 0.022s per page
- **License**: GPL code, Modified AI Pubs Open Rail-M model
- **Stars**: 13k+ GitHub stars

### DocLayout-YOLO (2024)
- **Title**: DocLayout-YOLO: Enhancing Document Layout Analysis through Diverse Synthetic Data and Global-to-Local Adaptive Perception
- **arXiv**: [2410.12628](https://arxiv.org/abs/2410.12628)
- **Date**: October 16, 2024
- **Links**:
  - [GitHub](https://github.com/opendatalab/DocLayout-YOLO)
  - [arXiv HTML](https://arxiv.org/html/2410.12628v1)
  - [HuggingFace Paper](https://huggingface.co/papers/2410.12628)
  - [PyPI](https://pypi.org/project/doclayout-yolo/)
  - [HuggingFace Models](https://huggingface.co/juliozhao/DocLayout-YOLO-DocStructBench)
- **Base**: YOLOv10 with document-specific optimizations
- **Dataset**: Pre-trained on DocSynth300K
- **Performance**: 81.7% AP50 on D4LA, 93.0% AP50 on DocLayNet, 85.5 FPS
- **Innovation**: Global-to-Local Adaptive Perception, Mesh-candidate BestFit algorithm

## Vision-Language Models

### Idefics3-8B (2024)
- **Title**: Building and better understanding vision-language models: insights and future directions
- **arXiv**: [2408.12637](https://arxiv.org/abs/2408.12637)
- **Links**:
  - [arXiv HTML](https://arxiv.org/html/2408.12637v1)
  - [HuggingFace Paper](https://huggingface.co/papers/2408.12637)
  - [HuggingFace Blog - SmolVLM](https://huggingface.co/blog/smolvlm)
- **Architecture**: SigLIP-SO400M vision encoder + Llama 3.1 Instruct
- **Training Data**: Docmatix dataset (2.4M images, 9.5M QA pairs)
- **Improvement**: 13.7-point improvement on DocVQA over Idefics2
- **Innovation**: Pixel shuffle strategy replacing perceiver resampler for better OCR

## Benchmarks and Evaluations

### OmniDocBench (2025)
- **Title**: OmniDocBench: Benchmarking Diverse PDF Document Parsing with Comprehensive Annotations
- **Conference**: CVPR 2025
- **arXiv**: [2412.07626](https://arxiv.org/abs/2412.07626)
- **Links**:
  - [GitHub](https://github.com/opendatalab/OmniDocBench)
  - [CVPR Poster](https://cvpr.thecvf.com/virtual/2025/poster/34400)
  - [CVPR PDF](https://openaccess.thecvf.com/content/CVPR2025/papers/Ouyang_OmniDocBench_Benchmarking_Diverse_PDF_Document_Parsing_with_Comprehensive_Annotations_CVPR_2025_paper.pdf)
- **Dataset**: 1,355 PDF pages
- **Coverage**: 9 document types, 4 layout types, 3 languages
- **Annotations**: 15 block-level elements (20k+), 4 span-level elements (80k+)
- **Documents**: Academic papers, financial reports, newspapers, textbooks, handwritten notes
