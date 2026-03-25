"""Download PaddleOCR models to local cache."""

from paddleocr import PaddleOCR
import os

print("=" * 60)
print("Downloading PaddleOCR Models")
print("=" * 60)

# Initialize PaddleOCR - this will download models to ~/.paddleocr/
print("\nInitializing PaddleOCR (will download models if not cached)...")
try:
    ocr = PaddleOCR(lang='en')  # Minimal config - just English models
except Exception as e:
    print(f"Error initializing PaddleOCR: {e}")
    print("\nTrying with default config...")
    ocr = PaddleOCR()  # Absolute minimal - all defaults

print("\n✓ Models downloaded successfully!")

# Find model locations
paddle_dir = os.path.expanduser("~/.paddleocr")
print(f"\nModel cache directory: {paddle_dir}")

if os.path.exists(paddle_dir):
    print("\nSearching for model files...")
    for root, dirs, files in os.walk(paddle_dir):
        for file in files:
            if file.endswith(('.pdmodel', '.pdiparams', '.txt')):
                rel_path = os.path.relpath(os.path.join(root, file), paddle_dir)
                print(f"  {rel_path}")

print("\n" + "=" * 60)
print("Next Steps:")
print("=" * 60)
print("1. Models are in Paddle format (.pdmodel, .pdiparams)")
print("2. We'll download pre-converted ONNX models instead")
print("3. Check PaddleOCR GitHub for ONNX versions")
