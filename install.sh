#!/bin/sh
# install.sh — download and install the soma-observe binary
#
# Usage:
#   curl -fsSL https://github.com/chaitugsk07/soma-observe/releases/latest/download/install.sh | sh
#
# After install:
#   export DATABASE_URL="postgres://user:pass@host:5432/dbname"
#   soma-observe
#
# The binary is placed in the first writable directory found in this order:
#   $HOME/.local/bin, /usr/local/bin (requires sudo), or the current directory.

set -e

REPO="chaitugsk07/soma-observe"
BINARY="soma-observe"
BASE_URL="https://github.com/${REPO}/releases/latest/download"

# ---------------------------------------------------------------------------
# Detect OS and architecture
# ---------------------------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}" in
  Linux)
    case "${ARCH}" in
      x86_64)
        ASSET="soma-observe-linux-x86_64"
        ;;
      *)
        echo "Error: unsupported Linux architecture '${ARCH}'." >&2
        echo "Supported: x86_64. Build from source: cargo build --release" >&2
        exit 1
        ;;
    esac
    ;;
  Darwin)
    case "${ARCH}" in
      arm64|aarch64)
        ASSET="soma-observe-macos-aarch64"
        ;;
      x86_64)
        # No separate x86_64 macOS binary yet; try the aarch64 build via Rosetta.
        ASSET="soma-observe-macos-aarch64"
        ;;
      *)
        echo "Error: unsupported macOS architecture '${ARCH}'." >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "Error: unsupported OS '${OS}'." >&2
    echo "Supported: Linux (x86_64), macOS (arm64/x86_64 via Rosetta)." >&2
    echo "Build from source: cargo build --release" >&2
    exit 1
    ;;
esac

DOWNLOAD_URL="${BASE_URL}/${ASSET}"

# ---------------------------------------------------------------------------
# Choose install directory
# ---------------------------------------------------------------------------
if [ -d "${HOME}/.local/bin" ] && echo "${PATH}" | grep -q "${HOME}/.local/bin"; then
  INSTALL_DIR="${HOME}/.local/bin"
elif [ -w "/usr/local/bin" ]; then
  INSTALL_DIR="/usr/local/bin"
else
  # Create ~/.local/bin and add to PATH hint.
  INSTALL_DIR="${HOME}/.local/bin"
  mkdir -p "${INSTALL_DIR}"
fi

INSTALL_PATH="${INSTALL_DIR}/${BINARY}"

# ---------------------------------------------------------------------------
# Download
# ---------------------------------------------------------------------------
echo "Downloading ${BINARY} for ${OS}/${ARCH}..."
echo "  URL: ${DOWNLOAD_URL}"

if command -v curl > /dev/null 2>&1; then
  curl -fsSL --progress-bar "${DOWNLOAD_URL}" -o "${INSTALL_PATH}"
elif command -v wget > /dev/null 2>&1; then
  wget -q --show-progress "${DOWNLOAD_URL}" -O "${INSTALL_PATH}"
else
  echo "Error: neither curl nor wget found. Install one and retry." >&2
  exit 1
fi

chmod +x "${INSTALL_PATH}"

echo ""
echo "Installed: ${INSTALL_PATH}"

# Warn if the install dir is not on PATH.
if ! echo "${PATH}" | grep -q "${INSTALL_DIR}"; then
  echo ""
  echo "Warning: ${INSTALL_DIR} is not in your PATH."
  echo "Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
  echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
fi

echo ""
echo "Next steps:"
echo "  export DATABASE_URL=\"postgres://user:pass@host:5432/dbname\""
echo "  ${BINARY}"
echo ""
echo "Or use docker compose for a self-contained setup:"
echo "  docker compose up -d"
