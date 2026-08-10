#!/bin/bash
set -euo pipefail

VERSION="v6.9.0"
REPO="tang-vu/ContribAI"
INSTALL_DIR="${CONTRIBAI_INSTALL_DIR:-/usr/local/bin}"

# Detect OS and arch
OS=$(uname -s | tr "[:upper:]" "[:lower:]")
ARCH=$(uname -m)

case "$OS" in
  linux)
    case "$ARCH" in
      x86_64|amd64)
        BINARY="contribai-$VERSION-linux-x86_64"
        ;;
      *) echo "Unsupported Linux architecture: $ARCH"; exit 1 ;;
    esac ;;
  darwin)
    case "$ARCH" in
      arm64|aarch64)
        BINARY="contribai-$VERSION-macos-aarch64"
        ;;
      x86_64|amd64)
        BINARY="contribai-$VERSION-macos-x86_64"
        ;;
      *) echo "Unsupported macOS architecture: $ARCH"; exit 1 ;;
    esac ;;
  *) echo "Unsupported OS: $OS"; exit 1 ;;
esac

URL="https://github.com/$REPO/releases/download/$VERSION/$BINARY"
CHECKSUM_URL="$URL.sha256"

echo "Installing ContribAI $VERSION..."
echo "  OS: $OS | Arch: $ARCH"
echo "  Binary: $BINARY"
echo "  Downloading from: $URL"
echo ""

TEMP_FILE=$(mktemp "${TMPDIR:-/tmp}/contribai.XXXXXX")
CHECKSUM_FILE="$TEMP_FILE.sha256"
trap 'rm -f "$TEMP_FILE" "$CHECKSUM_FILE"' EXIT

curl -fsSL "$URL" -o "$TEMP_FILE"
curl -fsSL "$CHECKSUM_URL" -o "$CHECKSUM_FILE"

EXPECTED_SHA256=$(awk 'NR == 1 { print $1 }' "$CHECKSUM_FILE" | tr '[:upper:]' '[:lower:]')
case "$EXPECTED_SHA256" in
  *[!0-9a-f]*|"") echo "Release checksum is malformed; refusing to install." >&2; exit 1 ;;
esac
if [ "${#EXPECTED_SHA256}" -ne 64 ]; then
  echo "Release checksum is malformed; refusing to install." >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA256=$(sha256sum "$TEMP_FILE" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA256=$(shasum -a 256 "$TEMP_FILE" | awk '{print $1}')
else
  echo "Cannot verify download: sha256sum or shasum is required." >&2
  exit 1
fi

if [ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]; then
  echo "Checksum verification failed; refusing to install." >&2
  echo "  Expected: $EXPECTED_SHA256" >&2
  echo "  Actual:   $ACTUAL_SHA256" >&2
  exit 1
fi

echo "  SHA256 checksum verified."
chmod +x "$TEMP_FILE"

if [ -n "${CONTRIBAI_INSTALL_DIR:-}" ]; then
  mkdir -p "$INSTALL_DIR"
fi

if [ -w "$INSTALL_DIR" ]; then
  mv "$TEMP_FILE" "$INSTALL_DIR/contribai"
else
  echo "Need sudo to install to $INSTALL_DIR"
  sudo mv "$TEMP_FILE" "$INSTALL_DIR/contribai"
fi

echo ""
echo "ContribAI installed successfully!"
echo "Run 'contribai demo' before adding credentials."
