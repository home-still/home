# Legacy Python scripts

These scripts belong to an earlier version of the conversion pipeline. The
current scribe server does not use them, and nothing in the build or the code
runs them.

| Script | What it does |
|---|---|
| `convert_to_onnx.py` | Exports a DocLayout-YOLO PyTorch model (`.pt`) to ONNX. |
| `download_paddle_models.py` | Fills the PaddleOCR model cache in `~/.paddleocr/`. |
| `download_dictionaries.py` | Downloads the SymSpell English frequency dictionaries. |

None of them produces the models the scribe server loads. It loads two ONNX
files from `~/.home-still/models/`:

| File | Model | Setting |
|---|---|---|
| `pp-doclayoutv3.onnx` | PP-DocLayout-V3 layout detection | `HS_SCRIBE_LAYOUT_MODEL_PATH` |
| `slanet-plus.onnx` | SLANet-Plus table structure | `HS_SCRIBE_TABLE_MODEL_PATH` |

See the [scribe server settings](../../../docs/reference/configuration.md#scribe_server).
