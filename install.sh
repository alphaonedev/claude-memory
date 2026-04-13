#!/bin/sh
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
set -e

REPO="alphaonedev/ai-memory-mcp"
BINARY="ai-memory"
VERSION="latest"

# ---------------------------------------------------------------------------
# Help
# ---------------------------------------------------------------------------
if [ "$1" = "--help" ] || [ "$1" = "-h" ]; then
    cat <<'USAGE'
Usage: install.sh [OPTIONS]

Install the ai-memory binary from GitHub releases.

Options:
  -h, --help    Show this help message
  --dir DIR     Override install directory (default: ~/.cargo/bin or ~/.local/bin)
  --version VER Install a specific version tag (default: latest)

Environment variables:
  AI_MEMORY_INSTALL_DIR   Override install directory

Examples:
  curl -fsSL https://raw.githubusercontent.com/alphaonedev/ai-memory-mcp/main/install.sh | sh
  ./install.sh --dir /usr/local/bin
  AI_MEMORY_INSTALL_DIR=~/bin ./install.sh
USAGE
    exit 0
fi

# ---------------------------------------------------------------------------
# Parse arguments
# ---------------------------------------------------------------------------
while [ $# -gt 0 ]; do
    case "$1" in
        --dir)    shift; AI_MEMORY_INSTALL_DIR="$1" ;;
        --version) shift; VERSION="$1" ;;
        *) echo "Unknown option: $1 (try --help)" >&2; exit 1 ;;
    esac
    shift
done

# ---------------------------------------------------------------------------
# Windows / PowerShell detection
# ---------------------------------------------------------------------------
if [ -n "$PSModulePath" ] || [ -n "$POWERSHELL_DISTRIBUTION_CHANNEL" ]; then
    echo "Error: It looks like you are running inside PowerShell." >&2
    echo "Please use install.ps1 instead:" >&2
    echo "  irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Detect OS
# ---------------------------------------------------------------------------
OS="$(uname -s)"
case "$OS" in
    Linux)              os="unknown-linux-gnu" ;;
    Darwin)             os="apple-darwin" ;;
    MINGW*|MSYS*|CYGWIN*) os="pc-windows-msvc" ;;
    *)
        echo "Error: Unsupported OS: $OS" >&2
        echo "Fallback: cargo install ai-memory" >&2
        exit 1
        ;;
esac

# ---------------------------------------------------------------------------
# Detect architecture
# ---------------------------------------------------------------------------
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|amd64)   arch="x86_64" ;;
    aarch64|arm64)   arch="aarch64" ;;
    *)
        echo "Error: Unsupported architecture: $ARCH" >&2
        echo "Fallback: cargo install ai-memory" >&2
        exit 1
        ;;
esac

TARGET="${arch}-${os}"

# ---------------------------------------------------------------------------
# File extension
# ---------------------------------------------------------------------------
case "$os" in
    *windows*) EXT="zip" ;;
    *)         EXT="tar.gz" ;;
esac

ASSET="ai-memory-${TARGET}.${EXT}"

# ---------------------------------------------------------------------------
# Install directory: prefer ~/.cargo/bin, fall back to ~/.local/bin
# ---------------------------------------------------------------------------
if [ -n "$AI_MEMORY_INSTALL_DIR" ]; then
    INSTALL_DIR="$AI_MEMORY_INSTALL_DIR"
elif [ -d "$HOME/.cargo/bin" ]; then
    INSTALL_DIR="$HOME/.cargo/bin"
else
    INSTALL_DIR="$HOME/.local/bin"
fi

echo "Detected platform: ${TARGET}"
echo "Installing to:     ${INSTALL_DIR}"

# ---------------------------------------------------------------------------
# Build download URLs
# ---------------------------------------------------------------------------
if [ "$VERSION" = "latest" ]; then
    BASE_URL="https://github.com/${REPO}/releases/latest/download"
else
    BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
fi

RELEASE_URL="${BASE_URL}/${ASSET}"
CHECKSUM_URL="${BASE_URL}/${ASSET}.sha256"

# ---------------------------------------------------------------------------
# Create install directory
# ---------------------------------------------------------------------------
mkdir -p "$INSTALL_DIR"

# ---------------------------------------------------------------------------
# Temp directory with cleanup
# ---------------------------------------------------------------------------
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

# ---------------------------------------------------------------------------
# Download helper with exit-code checking
# ---------------------------------------------------------------------------
download() {
    _url="$1"
    _dest="$2"
    _label="$3"

    if command -v curl >/dev/null 2>&1; then
        if ! curl -fsSL "$_url" -o "$_dest" 2>/dev/null; then
            echo "Error: Failed to download ${_label} from:" >&2
            echo "  ${_url}" >&2
            echo "" >&2
            echo "If this is a 404, the release may not exist for your platform." >&2
            echo "Fallback: cargo install ai-memory" >&2
            return 1
        fi
    elif command -v wget >/dev/null 2>&1; then
        if ! wget -q "$_url" -O "$_dest" 2>/dev/null; then
            echo "Error: Failed to download ${_label} from:" >&2
            echo "  ${_url}" >&2
            echo "" >&2
            echo "If this is a 404, the release may not exist for your platform." >&2
            echo "Fallback: cargo install ai-memory" >&2
            return 1
        fi
    else
        echo "Error: curl or wget is required to download files." >&2
        exit 1
    fi
}

# ---------------------------------------------------------------------------
# Download binary archive
# ---------------------------------------------------------------------------
echo "Downloading ${ASSET}..."
if ! download "$RELEASE_URL" "$TMPDIR/$ASSET" "$ASSET"; then
    exit 1
fi

# Print binary archive size
FILESIZE=$(wc -c < "$TMPDIR/$ASSET" | tr -d ' ')
if [ "$FILESIZE" -gt 1048576 ] 2>/dev/null; then
    SIZE_MB=$(echo "scale=1; $FILESIZE / 1048576" | bc 2>/dev/null || echo "$FILESIZE bytes")
    echo "Downloaded: ${SIZE_MB} MB"
else
    echo "Downloaded: ${FILESIZE} bytes"
fi

# ---------------------------------------------------------------------------
# Download and verify SHA256 checksum
# ---------------------------------------------------------------------------
echo "Verifying checksum..."
if download "$CHECKSUM_URL" "$TMPDIR/${ASSET}.sha256" "checksum" 2>/dev/null; then
    EXPECTED_HASH=$(awk '{print $1}' "$TMPDIR/${ASSET}.sha256")
    if command -v sha256sum >/dev/null 2>&1; then
        ACTUAL_HASH=$(sha256sum "$TMPDIR/$ASSET" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        ACTUAL_HASH=$(shasum -a 256 "$TMPDIR/$ASSET" | awk '{print $1}')
    else
        echo "Warning: No sha256sum or shasum found, skipping checksum verification." >&2
        ACTUAL_HASH=""
        EXPECTED_HASH=""
    fi

    if [ -n "$EXPECTED_HASH" ] && [ -n "$ACTUAL_HASH" ]; then
        if [ "$EXPECTED_HASH" != "$ACTUAL_HASH" ]; then
            echo "Error: Checksum mismatch!" >&2
            echo "  Expected: ${EXPECTED_HASH}" >&2
            echo "  Actual:   ${ACTUAL_HASH}" >&2
            echo "" >&2
            echo "The downloaded file may be corrupted. Please try again." >&2
            exit 1
        fi
        echo "Checksum OK: ${ACTUAL_HASH}"
    fi
else
    echo "Warning: Checksum file not available, skipping verification."
fi

# ---------------------------------------------------------------------------
# Extract
# ---------------------------------------------------------------------------
echo "Extracting..."
case "$EXT" in
    tar.gz) tar xzf "$TMPDIR/$ASSET" -C "$TMPDIR" ;;
    zip)    unzip -qo "$TMPDIR/$ASSET" -d "$TMPDIR" ;;
esac

# ---------------------------------------------------------------------------
# Validate extracted binary
# ---------------------------------------------------------------------------
if [ ! -f "$TMPDIR/$BINARY" ]; then
    echo "Error: Expected binary '$BINARY' not found after extraction." >&2
    echo "Archive contents:" >&2
    ls -la "$TMPDIR/" >&2
    echo "" >&2
    echo "Fallback: cargo install ai-memory" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Install binary
# ---------------------------------------------------------------------------
cp "$TMPDIR/$BINARY" "$INSTALL_DIR/$BINARY"
chmod +x "$INSTALL_DIR/$BINARY"

if [ ! -x "$INSTALL_DIR/$BINARY" ]; then
    echo "Error: Installed binary is not executable at $INSTALL_DIR/$BINARY" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# macOS: remove Gatekeeper quarantine attribute
# ---------------------------------------------------------------------------
if [ "$OS" = "Darwin" ]; then
    xattr -d com.apple.quarantine "$INSTALL_DIR/$BINARY" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------
echo ""
echo "Installed ${BINARY} to ${INSTALL_DIR}/${BINARY}"

# Verify installation by running the binary
if "$INSTALL_DIR/$BINARY" stats >/dev/null 2>&1; then
    echo "Verification: binary runs successfully."
else
    echo "Installed successfully (could not run 'ai-memory stats' -- database may not be configured yet)."
fi

# Check if install dir is in PATH
case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo ""
        echo "Note: ${INSTALL_DIR} is not in your PATH."
        echo "Add it with:"
        echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
        ;;
esac

echo ""
echo "Run 'ai-memory --help' to get started."
