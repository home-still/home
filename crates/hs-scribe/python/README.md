# Python Model Conversion Tools

This directory contains Python utilities for converting ML models to ONNX format for use in the pdf-mash Rust application.

## Setup

```bash
cd python
uv sync
```

## Usage

### Convert DocLayout-YOLO Model

```bash
uv run python convert_to_onnx.py /path/to/model.pt
```

The script will:
1. Load the PyTorch model
2. Export to ONNX format
3. Save as `model.onnx` in the same directory

### Environment

- Python 3.10+
- Uses `uv` for dependency management
- Dependencies defined in `pyproject.toml`

## Models Directory Structure

Model files should be placed in `../models/`:
- Keep `.onnx` files for the Rust application
- `.pt` and `.pth` files are gitignored
- Downloaded model repos are gitignored

## Dependencies

Key packages:
- `torch` - PyTorch for loading models
- `onnx` - ONNX format handling
- `onnxscript` - ONNX conversion utilities
- `doclayout-yolo` - DocLayout-YOLO model architecture
