# Datasets and Benchmarks for Reading Order Detection

## Reading Order Datasets

### ReadingBank (2021)
- **Size**: 500,000 document images
- **Authors**: Microsoft Research
- **Split**: 400K/50K/50K (train/validation/test)
- **Links**:
  - [GitHub Repository](https://github.com/doc-analysis/ReadingBank)
  - [Papers with Code](https://paperswithcode.com/dataset/readingbank)
  - [arXiv Paper](https://arxiv.org/abs/2108.11591)
- **Generation Method**: Automated extraction from Microsoft WORD XML metadata
- **License**: Apache 2.0
- **Document Types**: Receipts, forms, multi-column layouts, invoices
- **Contact**: Lei Cui (lecu@microsoft.com), Furu Wei (fuwei@microsoft.com)
- **Key Feature**: Embedded reading order in document structure enables automated annotation at scale

### DocBench-100 (2024)
- **Size**: 100 pages (30 complex, 70 regular layouts)
- **Purpose**: Evaluating layout ordering techniques
- **Annotations**: Block-level reading order annotations
- **Paper**: XY-Cut++ (arXiv:2504.10258)
- **Novel Contribution**: Benchmark specifically designed for reading order evaluation

## Layout Analysis Datasets

### DocLayNet (2022)
- **Size**: 80,863 manually annotated pages
- **Multiple Annotations**: 7,059 pages with 2 annotations, 1,591 pages with 3 annotations
- **Total Annotations**: 91,104 annotation instances
- **Conference**: ACM SIGKDD 2022 (28th Conference)
- **arXiv**: [2206.01062](https://arxiv.org/abs/2206.01062)
- **Links**:
  - [GitHub](https://github.com/DS4SD/DocLayNet)
  - [ACM PDF](https://dl.acm.org/doi/pdf/10.1145/3534678.3539043)
  - [HuggingFace v1.1](https://huggingface.co/datasets/ds4sd/DocLayNet-v1.1)
  - [HuggingFace v1.2](https://huggingface.co/datasets/ds4sd/DocLayNet-v1.2)
  - [HuggingFace Large](https://huggingface.co/datasets/pierreguillou/DocLayNet-large)
- **Document Categories** (6 types):
  - Financial Reports (large free-style layouts)
  - Manuals (large free-style layouts)
  - Scientific Articles
  - Laws & Regulations
  - Patents
  - Government Tenders
- **Labels** (11 categories):
  - Caption, Footnote, Formula, List-item
  - Page-footer, Page-header, Picture
  - Section-header, Table, Text, Title
- **Annotation Process**:
  - 40 annotators (32 selected experts)
  - 6 months duration
  - 100-page annotation guideline
- **Human Baseline**: 82-83% mAP (inter-annotator agreement)
- **Best Model**: 76.8% mAP (YOLOv5x6), ~10% gap from human performance
- **Key Advantage**: Document-wise train/test splits prevent template overfitting

### PubLayNet (2019)
- **Size**: 360,000+ document images (over 1 million PDF articles processed)
- **Source**: PubMed Central Open Access Subset (commercial use collection)
- **Award**: Best Paper Award at ICDAR 2019
- **Organization**: IBM Research Australia
- **arXiv**: [1908.07836](https://arxiv.org/abs/1908.07836)
- **Links**:
  - [GitHub](https://github.com/ibm-aur-nlp/PubLayNet)
  - [Papers with Code](https://paperswithcode.com/dataset/publaynet)
  - [IBM Data Asset Exchange](https://community.ibm.com/accelerators/catalog/content/PubLayNet)
  - [Dataset Preview](https://dax-cdn.cdn.appdomain.cloud/dax-publaynet/1.0.0/data-preview/PubLayNet.html)
- **Labels** (5 categories):
  - Text, Title, List, Table, Figure
- **Generation**: Automated XML-to-PDF matching
- **Annotation Types**: Bounding boxes and polygonal segmentations
- **Performance**: 93%+ mAP on in-domain data
- **Limitation**: Poor transfer to diverse layouts (domain-specific)
- **Document Type**: Scientific articles and research papers

### DocBank (2020)
- **Size**: 500,000 pages
- **Source**: arXiv LaTeX documents
- **Labels** (13 categories):
  - abstract, author, caption, equation, figure
  - footer, list, paragraph, section, table
  - title, reference, date
- **Links**:
  - [GitHub](https://github.com/doc-analysis/DocBank)
  - [Microsoft Research](https://www.microsoft.com/en-us/research/publication/docbank-a-benchmark-dataset-for-document-layout-analysis/)
  - [Papers with Code](https://paperswithcode.com/dataset/docbank)
- **Annotation Level**: Token-level annotations
- **Generation**: Automated from LaTeX source
- **Advantage**: Fine-grained information extraction
- **Limitation**: Limited to academic paper templates

### HRDoc (2023)
- **Title**: HRDoc: Dataset and Baseline Method toward Hierarchical Reconstruction of Document Structures
- **Size**: 2,500 multi-page documents (nearly 2 million semantic units)
- **Conference**: AAAI 2023
- **arXiv**: [2303.13839](https://arxiv.org/abs/2303.13839)
- **Links**:
  - [GitHub Repository](https://github.com/jfma-USTC/HRDoc)
  - [AAAI Paper](https://ojs.aaai.org/index.php/AAAI/article/view/25277)
  - [Papers with Code](https://paperswithcode.com/paper/hrdoc-dataset-and-baseline-method-toward)
- **Annotation Type**: Line-level with categories and relations
- **Source**: Rule-based extractors + human annotators
- **Key Feature**: Hierarchical structure (not just flat layout)
- **Tasks Supported**: Page object detection, reading order, hierarchical reconstruction

### Comp-HRDoc (2024)
- **Title**: Comprehensive benchmark for hierarchical document structure analysis
- **Organization**: Microsoft
- **Links**:
  - [GitHub](https://github.com/microsoft/CompHRDoc)
  - [Related Paper](https://arxiv.org/html/2401.11874v2)
  - [ScienceDirect](https://www.sciencedirect.com/science/article/abs/pii/S0031320324005879)
- **Size**: 42,000 documents
- **Categories**: 14 hierarchical categories
- **Relation Types**: 3 types (reading order, parent-child, reference)
- **Tasks Evaluated**:
  - Page object detection
  - Reading order prediction
  - Table of contents extraction
  - Hierarchical structure reconstruction

### GraphDoc (2025)
- **Size**: 80,000 document images
- **Annotations**: 4.13 million relation annotations
- **Relation Categories** (8 types):
  - Spatial: Up, Down, Left, Right
  - Logical: Parent, Child, Sequence, Reference
- **arXiv**: [2502.02501](https://arxiv.org/abs/2502.02501)
- **Conference**: ICLR 2025
- **Base**: Built upon DocLayNet
- **Task**: Graph-based Document Structure Analysis (gDSA)
- **Key Innovation**: Scene Graph Generation techniques adapted for documents

## Synthetic Datasets

### DocSynth300K (2024)
- **Size**: 300,000 synthetic document images
- **Purpose**: Pre-training for DocLayout-YOLO
- **Size on Disk**: ~113GB
- **Release Date**: October 23, 2024
- **Links**:
  - [HuggingFace](https://huggingface.co/datasets/juliozhao/DocSynth300K)
  - [GitHub Code](https://github.com/opendatalab/DocLayout-YOLO)
- **Generation**: Mesh-candidate BestFit algorithm (2D bin packing)
- **Performance Impact**:
  - 81.7% AP50 on D4LA (65.6% mAP)
  - 93.0% AP50 on DocLayNet (77.4% mAP)
- **Advantage**: Large-scale diverse pre-training without manual annotation
- **Limitation**: Lacks real-world layout quirks and degradations
- **Best Practice**: Synthetic pre-training + real-world fine-tuning

## Benchmark Collections

### OmniDocBench (2025)
- **Conference**: CVPR 2025
- **Size**: 1,355 PDF pages
- **arXiv**: [2412.07626](https://arxiv.org/abs/2412.07626)
- **Links**:
  - [GitHub](https://github.com/opendatalab/OmniDocBench)
  - [CVPR Paper PDF](https://openaccess.thecvf.com/content/CVPR2025/papers/Ouyang_OmniDocBench_Benchmarking_Diverse_PDF_Document_Parsing_with_Comprehensive_Annotations_CVPR_2025_paper.pdf)
- **Document Types** (9):
  - Academic papers, Financial reports, Newspapers
  - Textbooks, Handwritten notes, and 4 others
- **Layout Types**: 4 distinct layout patterns
- **Languages**: 3 language types
- **Block-level Elements** (15 categories): 20,000+ annotations
- **Span-level Elements** (4 categories): 80,000+ annotations
- **Evaluation Coverage**:
  - Text extraction
  - Formula recognition
  - Table parsing
  - Reading order detection
- **Key Finding**: Highlights limitations in handling document diversity

### DocStructBench (2024)
- **Purpose**: Complex document structure benchmark
- **Associated**: DocLayout-YOLO project
- **Performance**: 78.8% mAP (DocLayout-YOLO)
- **Challenge**: Complex academic papers with nested structures

### D4LA (Document Dataset for Layout Analysis)
- **Performance (DocLayout-YOLO)**: 81.7% AP50, 65.6% mAP with pre-training
- **Coverage**: Diverse document layout types

## Dataset Comparison Summary

| Dataset | Size | Annotation | Domain | Reading Order | License |
|---------|------|------------|--------|---------------|---------|
| ReadingBank | 500K | Automated (WORD XML) | General | ✓ Yes | Apache 2.0 |
| DocLayNet | 80K | Manual (3 per page) | 6 diverse types | Partial | CDLA-Permissive |
| PubLayNet | 360K | Automated (XML) | Scientific | No | CC BY-NC-SA |
| DocBank | 500K | Automated (LaTeX) | Scientific | No | CC BY-NC-SA |
| HRDoc | 2.5K docs (2M units) | Hybrid | General | ✓ Yes | - |
| Comp-HRDoc | 42K | Manual | General | ✓ Yes | - |
| GraphDoc | 80K | Manual (relations) | General (DocLayNet) | ✓ Yes | - |
| DocSynth300K | 300K | Synthetic | General | No | - |
| OmniDocBench | 1.4K | Comprehensive | 9 types | ✓ Yes | - |

## Key Insights

### Scale vs. Diversity Trade-off
- Large automated datasets (ReadingBank, PubLayNet, DocBank): High volume, limited diversity
- Manually annotated datasets (DocLayNet, HRDoc): Lower volume, higher diversity, better generalization

### Annotation Quality
- Human baseline (DocLayNet): 82-83% mAP shows inherent ambiguity in layout interpretation
- Multi-annotator agreement critical for establishing performance ceiling

### Dataset Design Principles
1. **Document-wise splits** prevent template overfitting
2. **Diversity over size**: 80% of full data often sufficient if diverse
3. **Synthetic pre-training + real fine-tuning** balances efficiency and robustness
4. **Hierarchical annotations** enable richer understanding beyond flat layouts

### Learning Curves
- mAP increases linearly with log(data size)
- Performance plateaus around 80% of full dataset
- DocLayNet-trained models generalize better than PubLayNet/DocBank models
