# Additional Resources and Links for Reading Order Detection

## GitHub Repositories

### Core Projects
- **MinerU**: https://github.com/opendatalab/MinerU
  - PDF extraction with reading order detection
  - Dual-backend (pipeline + VLM)
  - 13k+ stars

- **Surya OCR**: https://github.com/datalab-to/surya
  - Complete document AI toolkit
  - Reading order in 90+ languages
  - 13k+ stars

- **DocLayout-YOLO**: https://github.com/opendatalab/DocLayout-YOLO
  - YOLOv10-based layout detection
  - Pre-training on DocSynth300K
  - Fast inference (85.5 FPS)

- **LayoutReader (Microsoft)**: https://github.com/microsoft/unilm
  - Original seq2seq implementation
  - Part of UniLM project

- **LayoutReader (Fast)**: https://github.com/ppaanngggg/layoutreader
  - LayoutLMv3-based faster variant
  - Used by MinerU
  - Single-pass prediction

- **layoutparser-ort (Rust)**: https://github.com/styrowolf/layoutparser-ort
  - Rust port with ONNX runtime
  - No Python dependencies
  - Production-ready

### Datasets
- **ReadingBank**: https://github.com/doc-analysis/ReadingBank
  - 500K documents with reading order
  - Apache 2.0 license

- **DocLayNet**: https://github.com/DS4SD/DocLayNet
  - 80K+ manually annotated pages
  - Gold standard for diversity

- **PubLayNet**: https://github.com/ibm-aur-nlp/PubLayNet
  - 360K+ scientific documents
  - Best paper ICDAR 2019

- **DocBank**: https://github.com/doc-analysis/DocBank
  - 500K pages from arXiv
  - Token-level annotations

- **HRDoc**: https://github.com/jfma-USTC/HRDoc
  - Hierarchical document structures
  - 2.5K multi-page documents

- **Comp-HRDoc**: https://github.com/microsoft/CompHRDoc
  - Comprehensive hierarchical benchmark
  - 42K documents

- **OmniDocBench**: https://github.com/opendatalab/OmniDocBench
  - CVPR 2025 benchmark
  - 9 document types, comprehensive evaluation

### Community Resources
- **Awesome VLM Architectures**: https://github.com/gokayfem/awesome-vlm-architectures
  - Vision-language model architectures
  - Includes document understanding models

- **Awesome Scene Graph Generation**: https://github.com/ChocoWu/Awesome-Scene-Graph-Generation
  - Relevant for GraphDoc-style approaches

- **ONNX Runtime**: https://github.com/pykeio/ort
  - Rust bindings for ONNX Runtime
  - Fast ML inference

## HuggingFace Resources

### Models
- **LayoutLM**: https://huggingface.co/microsoft/layoutlm-base-uncased
- **LayoutLMv3**: https://huggingface.co/microsoft/layoutlmv3-base
- **LayoutLMv3 Large**: https://huggingface.co/microsoft/layoutlmv3-large
- **LayoutLMv3 Chinese**: https://huggingface.co/microsoft/layoutlmv3-base-chinese
- **DocLayout-YOLO**: https://huggingface.co/juliozhao/DocLayout-YOLO-DocStructBench

### Datasets
- **DocLayNet v1.1**: https://huggingface.co/datasets/ds4sd/DocLayNet-v1.1
- **DocLayNet v1.2**: https://huggingface.co/datasets/ds4sd/DocLayNet-v1.2
- **DocLayNet Large**: https://huggingface.co/datasets/pierreguillou/DocLayNet-large
- **DocSynth300K**: https://huggingface.co/datasets/juliozhao/DocSynth300K

### Paper Pages
- **LayoutReader**: https://huggingface.co/papers/2108.11591
- **DocLayout-YOLO**: https://huggingface.co/papers/2410.12628
- **MinerU**: https://huggingface.co/papers/2409.18839
- **OmniDocBench**: https://huggingface.co/papers/2412.07626
- **Idefics3**: https://huggingface.co/papers/2408.12637

## Academic Resources

### ACL Anthology
- **ROPE**: https://aclanthology.org/2021.acl-short.41/
- **ROPE PDF**: https://aclanthology.org/2021.acl-short.41.pdf

### ACM Digital Library
- **LayoutLMv3**: https://dl.acm.org/doi/10.1145/3503161.3548112
- **DocLayNet**: https://dl.acm.org/doi/10.1145/3534678.3539043
- **DLAFormer**: https://dl.acm.org/doi/10.1007/978-3-031-70546-5_3

### arXiv Papers
- **LayoutReader**: https://arxiv.org/abs/2108.11591
- **LayoutLMv3**: https://arxiv.org/abs/2204.08387
- **ROPE**: https://arxiv.org/abs/2106.10786
- **GLAM**: https://arxiv.org/abs/2308.02051
- **PARAGRAPH2GRAPH**: https://arxiv.org/abs/2304.11810
- **GraphDoc**: https://arxiv.org/abs/2502.02501
- **XY-Cut++**: https://arxiv.org/abs/2504.10258
- **DLAFormer**: https://arxiv.org/abs/2405.11757
- **DocLayout-YOLO**: https://arxiv.org/abs/2410.12628
- **MinerU**: https://arxiv.org/abs/2409.18839
- **OmniDocBench**: https://arxiv.org/abs/2412.07626
- **HRDoc**: https://arxiv.org/abs/2303.13839
- **DocLayNet**: https://arxiv.org/abs/2206.01062
- **PubLayNet**: https://arxiv.org/abs/1908.07836
- **Idefics3/VLM**: https://arxiv.org/abs/2408.12637

### Microsoft Research
- **LayoutReader**: https://www.microsoft.com/en-us/research/publication/layoutreader-pre-training-of-text-and-layout-for-reading-order-detection/
- **LayoutLMv3**: https://www.microsoft.com/en-us/research/publication/layoutlmv3-pre-training-for-document-ai-with-unified-text-and-image-masking/
- **DocBank**: https://www.microsoft.com/en-us/research/publication/docbank-a-benchmark-dataset-for-document-layout-analysis/

### Google Research
- **ROPE**: https://research.google/pubs/pub50447/

### IBM Research
- **PubLayNet**: https://research.ibm.com/publications/publaynet-largest-dataset-ever-for-document-layout-analysis

### CVPR 2025
- **OmniDocBench PDF**: https://openaccess.thecvf.com/content/CVPR2025/papers/Ouyang_OmniDocBench_Benchmarking_Diverse_PDF_Document_Parsing_with_Comprehensive_Annotations_CVPR_2025_paper.pdf
- **OmniDocBench Poster**: https://cvpr.thecvf.com/virtual/2025/poster/34400

### ICLR 2025
- **GraphDoc PDF**: https://proceedings.iclr.cc/paper_files/paper/2025/file/cf3d7d8e79703fe947deffb587a83639-Paper-Conference.pdf

## Papers with Code

### Papers
- **LayoutReader**: https://paperswithcode.com/paper/layoutreader-pre-training-of-text-and-layout
- **ROPE**: https://paperswithcode.com/paper/rope-reading-order-equivariant-positional
- **GLAM**: https://paperswithcode.com/paper/a-graphical-approach-to-document-layout
- **HRDoc**: https://paperswithcode.com/paper/hrdoc-dataset-and-baseline-method-toward

### Datasets
- **ReadingBank**: https://paperswithcode.com/dataset/readingbank
- **PubLayNet**: https://paperswithcode.com/dataset/publaynet
- **DocBank**: https://paperswithcode.com/dataset/docbank

## API and Hosted Services

### Surya API
- **Endpoint**: https://api.datalab.to/surya
- **Features**: OCR, layout, reading order, tables
- **Languages**: 90+

### IBM PubLayNet
- **Data Exchange**: https://community.ibm.com/accelerators/catalog/content/PubLayNet
- **Preview**: https://dax-cdn.cdn.appdomain.cloud/dax-publaynet/1.0.0/data-preview/PubLayNet.html

## Documentation Sites

### Rust Crates
- **layoutparser-ort**: https://docs.rs/layoutparser-ort
- **layoutparser-ort (lib.rs)**: https://lib.rs/crates/layoutparser-ort
- **ort (ONNX Runtime)**: https://docs.rs/ort
- **ort Introduction**: https://ort.pyke.io/

### Python Packages
- **magic-pdf (MinerU)**: https://pypi.org/project/magic-pdf/
- **doclayout-yolo**: https://pypi.org/project/doclayout-yolo/

## Articles and Tutorials

### Technical Articles
- **MinerU - MarkTechPost**: https://www.marktechpost.com/2024/10/05/mineru-an-open-source-pdf-data-extraction-tool/
- **MinerU - NeuroHive**: https://neurohive.io/en/state-of-the-art/mineru-open-source-ai-document-extraction/
- **MinerU - Tech Explorer**: https://stable-learn.com/en/mineru-tutorial/
- **Idefics3 - Medium**: https://ritvik19.medium.com/papers-explained-218-idefics-3-81791c4cde3f
- **SmolVLM - HuggingFace**: https://huggingface.co/blog/smolvlm

### Research Summaries
- **MinerU Summary**: https://learnlater.com/summary/open-source-mineru/2621
- **VLM Insights**: https://aili.app/share/5Woytd8ffLHnGTZ2SCAWAa

## Research Groups and Labs

### Organizations
- **OpenDataLab** (Shanghai AI Lab): MinerU, DocLayout-YOLO, OmniDocBench
- **datalab.to**: Surya OCR toolkit
- **Microsoft Research**: LayoutLM family, ReadingBank, DocBank, Comp-HRDoc
- **IBM Research**: PubLayNet
- **Google Research**: ROPE
- **Naver Clova**: Donut

### Key Researchers
- **Lei Cui** (Microsoft): lecu@microsoft.com - LayoutLM, ReadingBank
- **Furu Wei** (Microsoft): fuwei@microsoft.com - LayoutLM family
- **Chen-Yu Lee** (Google): ROPE
- **Jiawei Wang**: DLAFormer

## Conference Proceedings

### Recent Venues
- **EMNLP 2021**: LayoutReader
- **ACL 2021**: ROPE
- **ACM MM 2022**: LayoutLMv3
- **ICDAR 2019**: PubLayNet (Best Paper)
- **ICDAR 2024**: DLAFormer (Oral)
- **AAAI 2023**: HRDoc
- **ACM SIGKDD 2022**: DocLayNet
- **CVPR 2025**: OmniDocBench
- **ICLR 2025**: GraphDoc

## Issue Trackers and Discussions

### GitHub Issues
- **LayoutLMv3 ONNX Export**: https://github.com/huggingface/transformers/issues/14368
- **LayoutReader Labeling Tools**: https://github.com/microsoft/unilm/issues/797
- **MinerU Reading Order**: Issue #3591 (wrong order in two-column PDFs)
- **DocLayout-YOLO Training**: https://github.com/opendatalab/DocLayout-YOLO/issues/55

## ResearchGate Resources

### Papers
- **LayoutReader**: https://www.researchgate.net/publication/357124534_LayoutReader_Pre-training_of_Text_and_Layout_for_Reading_Order_Detection
- **DocLayNet**: https://www.researchgate.net/publication/361051040_DocLayNet_A_Large_Human-Annotated_Dataset_for_Document-Layout_Analysis
- **PubLayNet**: https://www.researchgate.net/publication/336288174_PubLayNet_Largest_Dataset_Ever_for_Document_Layout_Analysis
- **ROPE**: https://www.researchgate.net/publication/353487793_ROPE_Reading_Order_Equivariant_Positional_Encoding_for_Graph-based_Document_Information_Extraction
- **XY-Cut++**: https://www.researchgate.net/publication/390773723_XY-Cut_Advanced_Layout_Ordering_via_Hierarchical_Mask_Mechanism_on_a_Novel_Benchmark

### Figures
- **ReadingBank Examples**: https://www.researchgate.net/figure/Document-image-examples-in-ReadingBank-with-the-reading-order-information-The-colored_fig1_354157656
- **GLAM Architecture**: https://www.researchgate.net/figure/Visualization-of-the-GNN-architecture-used-in-GLAM_fig3_372950690

## Commercial and Production Tools

### Comparison Benchmarks
- **Mathpix**: #1 on OmniDocBench (commercial)
- **Google Cloud Vision**: Compared against Surya
- **Tesseract**: Compared against Surya
- **Azure Document Intelligence**: OCR engine option

## Social and Community

### Medium Articles
- **GCN for Document IE**: https://medium.com/data-science/using-graph-convolutional-neural-networks-on-structured-documents-for-information-extraction-c1088dcd2b8f
- **VLM Papers Explained**: https://ritvik19.medium.com/papers-explained-218-idefics-3-81791c4cde3f

### Blog Posts
- **Nanonets GCN**: https://nanonets.com/blog/information-extraction-graph-convolutional-networks/
- **Chief AI Sharing**: https://www.aisharenet.com/en/surya/

## Video Resources

### Conference Presentations
- **PubLayNet SlideShare**: https://www.slideshare.net/ShivamSood14/publaynet-largest-dataset-ever-for-document-layout-analysis

## Tools and Utilities

### ONNX Ecosystem
- **ONNX Runtime GitHub**: https://github.com/microsoft/onnxruntime
- **Intel OpenVINO**: https://docs.openvino.ai/

### Visualization
- **Distill - Understanding GNNs**: https://distill.pub/2021/understanding-gnns/

## License Information

### Dataset Licenses
- ReadingBank: Apache 2.0
- DocLayNet: CDLA-Permissive
- PubLayNet: CC BY-NC-SA
- DocBank: CC BY-NC-SA

### Model Licenses
- LayoutLM v1: MIT (commercial OK)
- LayoutLMv2/v3: CC BY-NC-SA 4.0 (non-commercial)
- Donut: MIT
- Surya: GPL (code), Modified AI Pubs Open Rail-M (models)
- DocLayout-YOLO: AGPL-3.0
- layoutparser-ort: Apache 2.0

## Related Topics

### Graph Neural Networks
- **GNN Survey**: https://computationalsocialnetworks.springeropen.com/articles/10.1186/s40649-019-0069-y
- **Dynamic GCN**: https://www.sciencedirect.com/science/article/abs/pii/S0031320319303036
- **GNN Papers Collection**: https://github.com/thunlp/GNNPapers

### Vision-Language Models
- **Awesome VLM**: https://github.com/gokayfem/awesome-vlm-architectures
- **LLM on Graphs**: https://github.com/PeterGriffinJin/Awesome-Language-Model-on-Graphs

### Document Analysis
- **IJDAR Dataset Survey**: https://link.springer.com/article/10.1007/s10032-024-00461-2
- **Frontiers Dynamic GNN**: https://link.springer.com/article/10.1007/s11704-024-3853-2

## Contact Information

### Dataset Maintainers
- **ReadingBank**: Lei Cui (lecu@microsoft.com), Furu Wei (fuwei@microsoft.com)
- **DocLayNet**: IBM Research, DS4SD team
- **PubLayNet**: IBM Research Australia

## Future Resources to Watch

### Upcoming Conferences
- CVPR 2026
- ICDAR 2026
- ACL 2026
- ICLR 2026

### Emerging Areas
- Dynamic/interactive document formats
- Responsive web page reading order
- Multi-modal document understanding
- Zero-shot reading order detection
- Human-in-the-loop active learning

## How to Stay Updated

### GitHub Watch
- Star/watch key repositories for updates
- Follow release notifications

### arXiv Alerts
- Set alerts for keywords: "reading order", "document layout", "document understanding"
- Follow authors: Lei Cui, Furu Wei, Chen-Yu Lee

### Conference Proceedings
- Monitor CVPR, ICDAR, ACL, EMNLP, ICLR proceedings
- Check workshop papers (Document AI workshops)

### HuggingFace Daily Papers
- https://huggingface.co/papers
- Filter by document understanding, OCR, layout analysis

### Papers with Code
- Subscribe to task updates: "Reading Order Detection", "Document Layout Analysis"
- https://paperswithcode.com/task/reading-order-detection
