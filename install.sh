#!/bin/bash
set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Function to print error and exit
error_exit() {
    echo -e "${RED}Error: $1${NC}" >&2
    exit 1
}

# Check for required commands
command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 || error_exit "curl or wget is required"
command -v unzip >/dev/null 2>&1 || error_exit "unzip is required"

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    linux)
        case "$ARCH" in
            x86_64|amd64)
                URL="https://nightly.link/meanwhile131/deepseek-cli-rs/workflows/rust/main/deepseek-ubuntu-latest.zip"
                BIN_NAME="deepseek"
                ;;
            aarch64|arm64)
                URL="https://nightly.link/meanwhile131/deepseek-cli-rs/workflows/rust/main/deepseek-ubuntu-24.04-arm.zip"
                BIN_NAME="deepseek"
                ;;
            *)
                error_exit "Unsupported architecture: $ARCH"
                ;;
        esac
        ;;
    darwin)
        case "$ARCH" in
            x86_64|amd64)
                URL="https://nightly.link/meanwhile131/deepseek-cli-rs/workflows/rust/main/deepseek-macos-26-intel.zip"
                BIN_NAME="deepseek"
                ;;
            aarch64|arm64)
                URL="https://nightly.link/meanwhile131/deepseek-cli-rs/workflows/rust/main/deepseek-macos-latest.zip"
                BIN_NAME="deepseek"
                ;;
            *)
                error_exit "Unsupported architecture: $ARCH"
                ;;
        esac
        ;;
    *)
        error_exit "Unsupported OS: $OS"
        ;;
esac

echo -e "${GREEN}Detected OS: $OS, Arch: $ARCH${NC}"
echo -e "Downloading from: $URL"

# Create temporary directory
TMP_DIR=$(mktemp -d)
cd "$TMP_DIR"

# Download zip file
if command -v curl >/dev/null 2>&1; then
    curl -L -o deepseek.zip "$URL" || error_exit "Download failed"
else
    wget -O deepseek.zip "$URL" || error_exit "Download failed"
fi

# Extract zip
unzip -q deepseek.zip || error_exit "Extraction failed"

# Find the binary (could be named deepseek or deepseek.exe, but for Unix it's deepseek)
if [ -f "deepseek" ]; then
    BINARY_PATH="$TMP_DIR/deepseek"
elif [ -f "target/release/deepseek" ]; then
    BINARY_PATH="$TMP_DIR/target/release/deepseek"
else
    # Try to find any executable named deepseek
    BINARY_PATH=$(find "$TMP_DIR" -type f -name "deepseek" -perm +111 | head -1)
    [ -z "$BINARY_PATH" ] && error_exit "Could not find deepseek binary in extracted files"
fi

# Determine installation directory
if [ -w "/usr/local/bin" ]; then
    INSTALL_DIR="/usr/local/bin"
    USE_SUDO=""
else
    if [ -w "$HOME/.local/bin" ]; then
        INSTALL_DIR="$HOME/.local/bin"
        USE_SUDO=""
    else
        echo -e "${YELLOW}No write permission to /usr/local/bin or ~/.local/bin. Attempting with sudo...${NC}"
        INSTALL_DIR="/usr/local/bin"
        USE_SUDO="sudo"
    fi
fi

# Create install directory if it doesn't exist
if [ ! -d "$INSTALL_DIR" ]; then
    if [ -n "$USE_SUDO" ]; then
        sudo mkdir -p "$INSTALL_DIR"
    else
        mkdir -p "$INSTALL_DIR"
    fi
fi

# Install binary
echo -e "Installing to $INSTALL_DIR/deepseek"
if [ -n "$USE_SUDO" ]; then
    sudo cp "$BINARY_PATH" "$INSTALL_DIR/deepseek"
    sudo chmod +x "$INSTALL_DIR/deepseek"
else
    cp "$BINARY_PATH" "$INSTALL_DIR/deepseek"
    chmod +x "$INSTALL_DIR/deepseek"
fi

# Cleanup
cd /
rm -rf "$TMP_DIR"

# Check if installation directory is in PATH
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    echo -e "${YELLOW}Warning: $INSTALL_DIR is not in your PATH.${NC}"
    echo -e "You may want to add it to your shell profile:"
    echo -e "  export PATH=\"\$PATH:$INSTALL_DIR\""
fi

echo -e "${GREEN}Installation complete! Run 'deepseek' to see usage.${NC}"
