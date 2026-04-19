#!/usr/bin/env bash
set -euo pipefail

REPO="bravo1goingdark/recon"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# Detect platform
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *)      echo "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
  x86_64)  arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)       echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

TARGET="${arch}-${os}"

# Get latest release tag
if [ -z "${VERSION:-}" ]; then
  VERSION="$(curl -sL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | head -1 | cut -d'"' -f4)"
  if [ -z "$VERSION" ]; then
    echo "Could not determine latest version. Set VERSION=vX.Y.Z manually."
    exit 1
  fi
fi

URL="https://github.com/$REPO/releases/download/$VERSION/recon-$TARGET.tar.gz"

echo "Installing recon $VERSION for $TARGET..."
echo "  From: $URL"
echo "  To:   $INSTALL_DIR/recon"

# Download and extract
mkdir -p "$INSTALL_DIR"
curl -sL "$URL" | tar xz -C "$INSTALL_DIR"
chmod +x "$INSTALL_DIR/recon"

echo ""
echo "Installed recon to $INSTALL_DIR/recon"
echo ""

# Check PATH
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
  echo "Add to your PATH:"
  echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
  echo ""
fi

# Print MCP config snippet
echo "Add to .claude/settings.json to wire as MCP server:"
echo ""
cat <<SETTINGS
{
  "mcpServers": {
    "recon": {
      "command": "$INSTALL_DIR/recon",
      "args": ["serve", "--repo", "."]
    }
  }
}
SETTINGS
