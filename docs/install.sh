#!/bin/sh
set -eu

REPO="home-still/home"
TOOL="hs"
INSTALL_DIR="${HOME}/.local/bin"
PRE=false

# Parse flags
for arg in "$@"; do
    case "$arg" in
        --pre) PRE=true ;;
        *)     echo "Usage: install.sh [--pre]"; exit 1 ;;
    esac
done

# Detect platform
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}" in
    Darwin) os="apple-darwin" ;;
    Linux)  os="unknown-linux-gnu" ;;
    *)      echo "Unsupported OS: ${OS}"; exit 1 ;;
esac

case "${ARCH}" in
    x86_64)         arch="x86_64" ;;
    arm64|aarch64)  arch="aarch64" ;;
    *)              echo "Unsupported architecture: ${ARCH}"; exit 1 ;;
esac

TARGET="${arch}-${os}"

# Get version from GitHub API (--pre includes release candidates)
if [ "$PRE" = true ]; then
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases?per_page=1" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
else
    VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p')"
fi

if [ -z "${VERSION}" ]; then
    echo "Failed to fetch latest version"
    exit 1
fi

ARCHIVE="${TOOL}-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"

echo "Installing ${TOOL} ${VERSION} for ${TARGET}..."

# Download and extract
mkdir -p "${INSTALL_DIR}"
curl -fsSL "${URL}" | tar -xz -C "${INSTALL_DIR}"
chmod +x "${INSTALL_DIR}/${TOOL}"

echo "Installed ${TOOL} to ${INSTALL_DIR}/${TOOL}"

# Install companion binaries if available for this platform
for COMPANION in hs-distill-server hs-gateway hs-mcp; do
    COMP_ARCHIVE="${COMPANION}-${VERSION}-${TARGET}.tar.gz"
    COMP_URL="https://github.com/${REPO}/releases/download/${VERSION}/${COMP_ARCHIVE}"
    if curl -fsSL -o /dev/null --head "${COMP_URL}" 2>/dev/null; then
        curl -fsSL "${COMP_URL}" | tar -xz -C "${INSTALL_DIR}"
        chmod +x "${INSTALL_DIR}/${COMPANION}"
        echo "Installed ${COMPANION} to ${INSTALL_DIR}/${COMPANION}"
    fi
done

# Check if INSTALL_DIR is in PATH
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo ""
        echo "Add ${INSTALL_DIR} to your PATH:"
        echo "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.bashrc"
        ;;
esac