#!/usr/bin/env python3
"""
Convert DocLayout-YOLO PyTorch model to ONNX format.

Usage:
    python convert_to_onnx.py <input.pt> [output.onnx]

Example:
    python convert_to_onnx.py ../DocLayout-YOLO-DocStructBench/doclayout_yolo_docstructbench_imgsz1024.pt
"""

import sys
from pathlib import Path
from doclayout_yolo import YOLOv10


def convert_model(input_path: str, output_path: str = None):
    """Convert PyTorch model to ONNX format."""

    input_file = Path(input_path)
    if not input_file.exists():
        print(f"❌ Error: Input file not found: {input_path}")
        sys.exit(1)

    if output_path is None:
        output_path = input_file.with_suffix('.onnx')

    print(f"📥 Loading PyTorch model: {input_file.name}")
    print(f"   Path: {input_file.absolute()}")

    try:
        model = YOLOv10(str(input_file))
    except Exception as e:
        print(f"❌ Failed to load model: {e}")
        sys.exit(1)

    print(f"\n🔄 Exporting to ONNX...")
    print(f"   Settings:")
    print(f"   - Format: ONNX")
    print(f"   - Simplify: True")
    print(f"   - Dynamic shapes: False")
    print(f"   - Input size: 1024x1024")

    try:
        model.export(
            format="onnx",
            simplify=False,  # Skip onnxsim to avoid build issues
            dynamic=False,
            imgsz=1024
        )
    except Exception as e:
        print(f"❌ Export failed: {e}")
        sys.exit(1)

    # The export creates a file with the same name but .onnx extension
    expected_output = input_file.with_suffix('.onnx')

    if expected_output.exists():
        file_size_mb = expected_output.stat().st_size / (1024 * 1024)
        print(f"\n✅ Conversion complete!")
        print(f"   Output: {expected_output.name}")
        print(f"   Size: {file_size_mb:.1f} MB")
        print(f"   Path: {expected_output.absolute()}")
    else:
        print(f"⚠️  Warning: Expected output file not found: {expected_output}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: python convert_to_onnx.py <input.pt> [output.onnx]")
        print("\nExample:")
        print("  python convert_to_onnx.py ../DocLayout-YOLO-DocStructBench/doclayout_yolo_docstructbench_imgsz1024.pt")
        sys.exit(1)

    input_model = sys.argv[1]
    output_model = sys.argv[2] if len(sys.argv) > 2 else None

    convert_model(input_model, output_model)
