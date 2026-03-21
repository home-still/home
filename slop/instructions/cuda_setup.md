# CUDA Setup for ONNX Runtime GPU Acceleration

## What We're Using

**ONNX Runtime 1.22.0** with CUDA 12 execution provider
- GPU acceleration for layout detection and OCR inference
- 3-5x speedup over CPU-only execution
- RTX 3090 target hardware (24GB VRAM)

## Requirements

| Component | Version | Notes |
|-----------|---------|-------|
| ort crate | 2.0.0-rc.10 | Requires API version 22 |
| ONNX Runtime | 1.22.0+ | First version with API v22 |
| CUDA libs | 12.x | cuBLAS, cuFFT, cuDNN, etc. |
| NVIDIA Driver | 535+ | Supports CUDA 12 |

### Version Compatibility

The `ort` crate version determines the required ONNX Runtime:

| ort Version | API Version | Min ONNX Runtime |
|-------------|-------------|------------------|
| 2.0.0-rc.10 | 22 | 1.22.0 |
| 2.0.0-rc.8 | 21 | 1.21.0 |
| 2.0.0-rc.4 | 20 | 1.20.0 |

**Error symptom:** `The requested API version [22] is not available`

## Installation (Arch Linux with CUDA 13 Driver)

Arch Linux ships with CUDA 13.x, but ONNX Runtime requires CUDA 12.x libraries. We install CUDA 12 libs alongside the existing driver.

### Step 1: Install ONNX Runtime GPU

```bash
cd /tmp

# Download ONNX Runtime 1.22.0 (or latest 1.22.x/1.23.x)
wget https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-linux-x64-gpu-1.22.0.tgz

# Create installation directory
sudo mkdir -p /opt/onnxruntime-gpu

# Extract
sudo tar -xzf onnxruntime-linux-x64-gpu-1.22.0.tgz -C /opt/onnxruntime-gpu --strip-components=1

# Verify
ls /opt/onnxruntime-gpu/lib/libonnxruntime.so*
```

### Step 2: Install CUDA 12 Libraries

ONNX Runtime GPU links against CUDA 12 libs. On Arch with CUDA 13, we extract them from NVIDIA RPMs:

```bash
cd /tmp
mkdir -p cuda12-libs
sudo mkdir -p /opt/cuda-12-libs

# Download required CUDA 12.6 libraries from NVIDIA RHEL9 repo
wget https://developer.download.nvidia.com/compute/cuda/repos/rhel9/x86_64/libcublas-12-6-12.6.4.1-1.x86_64.rpm
wget https://developer.download.nvidia.com/compute/cuda/repos/rhel9/x86_64/libcufft-12-6-11.3.0.4-1.x86_64.rpm
wget https://developer.download.nvidia.com/compute/cuda/repos/rhel9/x86_64/libcurand-12-6-10.3.7.77-1.x86_64.rpm
wget https://developer.download.nvidia.com/compute/cuda/repos/rhel9/x86_64/libcusparse-12-6-12.5.4.2-1.x86_64.rpm
wget https://developer.download.nvidia.com/compute/cuda/repos/rhel9/x86_64/libcusolver-12-6-11.7.1.2-1.x86_64.rpm
wget https://developer.download.nvidia.com/compute/cuda/repos/rhel9/x86_64/libcudnn9-cuda-12-9.6.0.74-1.x86_64.rpm
wget https://developer.download.nvidia.com/compute/cuda/repos/rhel9/x86_64/cuda-cudart-12-6-12.6.77-1.x86_64.rpm
wget https://developer.download.nvidia.com/compute/cuda/repos/rhel9/x86_64/cuda-nvrtc-12-6-12.6.77-1.x86_64.rpm

# Extract all RPMs (Arch uses bsdtar, not rpm2cpio)
for rpm in *.rpm; do bsdtar -xf "$rpm" -C cuda12-libs; done

# Install libraries
sudo cp -a cuda12-libs/usr/local/cuda-12.6/targets/x86_64-linux/lib/* /opt/cuda-12-libs/
sudo cp -a cuda12-libs/usr/lib64/* /opt/cuda-12-libs/ 2>/dev/null
```

### Step 3: Configure Environment

Add to your shell profile (`~/.bashrc`, `~/.zshrc`):

```bash
export LD_LIBRARY_PATH="/opt/onnxruntime-gpu/lib:/opt/cuda-12-libs:$LD_LIBRARY_PATH"
```

Or for project-specific use, the `.cargo/config.toml` already sets:

```toml
[env]
ORT_STRATEGY = "system"
ORT_LIB_LOCATION = "/opt/onnxruntime-gpu/lib"
LD_LIBRARY_PATH = "/opt/onnxruntime-gpu/lib"
```

**Note:** You still need CUDA 12 libs in the path at runtime.

## Verification

```bash
# Set environment
export LD_LIBRARY_PATH="/opt/onnxruntime-gpu/lib:/opt/cuda-12-libs:$LD_LIBRARY_PATH"

# Build and test
cd /home/ladvien/pdf_masher/pdf-mash
cargo clean  # Important after library changes
cargo test

# Expected output:
# CUDA EP available: true
# test test_process_simple_pdf ... ok
```

## Troubleshooting

### Error: API version not available

```
The requested API version [22] is not available, only API versions [1, 20] are supported
```

**Cause:** ONNX Runtime version too old.
**Fix:** Install ONNX Runtime 1.22.0 or later.

### Error: Library not found (libcublasLt.so.12, libcufft.so.11, etc.)

```
Failed to load library libonnxruntime_providers_cuda.so with error: libcublasLt.so.12: cannot open shared object file
```

**Cause:** CUDA 12 libraries not installed or not in path.
**Fix:**
1. Install CUDA 12 libs (Step 2 above)
2. Add `/opt/cuda-12-libs` to `LD_LIBRARY_PATH`

### Error: Stale build artifacts

```
version `VERS_1.19.2' not found
```

**Cause:** Old build linked against previous ONNX Runtime version.
**Fix:** `cargo clean && cargo build`

### Error: CUDA installer needs libxml2.so.2

```
./cuda-installer: error while loading shared libraries: libxml2.so.2
```

**Cause:** NVIDIA's CUDA toolkit installer expects older libraries.
**Fix:** Don't use the .run installer. Extract RPMs with bsdtar instead.

## Alternative: AUR Package

On Arch Linux, you can try:

```bash
yay -S cuda-12.5
```

This installs a complete CUDA 12.5 toolkit to `/opt/cuda-12.5/`.

## Project Configuration

### Cargo.toml

```toml
[dependencies]
ort = { version = "2.0.0-rc.10", features = ["cuda"] }
```

### .cargo/config.toml

```toml
[env]
ORT_STRATEGY = "system"
ORT_LIB_LOCATION = "/opt/onnxruntime-gpu/lib"
LD_LIBRARY_PATH = "/opt/onnxruntime-gpu/lib"

[build]
rustflags = ["-C", "target-cpu=native"]
```

## Performance

### GPU vs CPU (RTX 3090)

| Operation | CPU | GPU | Speedup |
|-----------|-----|-----|---------|
| Layout Detection (1 page) | ~800ms | ~150ms | 5.3x |
| OCR Recognition (1 region) | ~100ms | ~25ms | 4x |
| Full page pipeline | ~3s | ~800ms | 3.7x |

### VRAM Usage

- Layout model: ~500MB
- OCR models: ~200MB
- Working memory: ~500MB
- **Total**: ~1.2GB per inference session

## References

- [ONNX Runtime Releases](https://github.com/microsoft/onnxruntime/releases)
- [NVIDIA CUDA Toolkit Downloads](https://developer.nvidia.com/cuda-downloads)
- [ort crate documentation](https://docs.rs/ort/2.0.0-rc.10/ort/)
- [CUDA Compatibility Guide](https://docs.nvidia.com/deploy/cuda-compatibility/)

---

**Last updated:** 2025-12-17
**Tested on:** Arch Linux, NVIDIA RTX 3090, Driver 580.105.08
**Current status:** Working with ONNX Runtime 1.22.0 + CUDA 12.6 libs
