#!/bin/sh
set -eu

REPO="home-still/home"
TOOL="hs"
INSTALL_DIR="${HOME}/.local/bin"

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

# Get latest version from GitHub API
VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | cut -d'"' -f4)"

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

# Check if INSTALL_DIR is in PATH
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo ""
        echo "Add ${INSTALL_DIR} to your PATH:"
        echo "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.bashrc"
        ;;
esac