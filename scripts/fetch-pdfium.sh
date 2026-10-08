#!/usr/bin/env sh
# Download a prebuilt PDFium library into vendor/pdfium/ (from bblanchon/pdfium-binaries).
# Usage: scripts/fetch-pdfium.sh [chromium-build]   e.g. scripts/fetch-pdfium.sh 8086
set -eu

BUILD="${1:-8086}"
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) ASSET=pdfium-linux-x64.tgz ;;
    Linux-aarch64) ASSET=pdfium-linux-arm64.tgz ;;
    Darwin-arm64) ASSET=pdfium-mac-arm64.tgz ;;
    Darwin-x86_64) ASSET=pdfium-mac-x64.tgz ;;
    *) echo "unsupported platform $(uname -s)-$(uname -m); download manually from" \
        "https://github.com/bblanchon/pdfium-binaries/releases" >&2; exit 1 ;;
esac

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/vendor/pdfium"
URL="https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F$BUILD/$ASSET"

mkdir -p "$DEST"
echo "Downloading $URL"
curl -fL "$URL" | tar xz -C "$DEST"
echo "PDFium installed in $DEST/lib"
