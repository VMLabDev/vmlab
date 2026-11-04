#!/usr/bin/env bash
# Render the app icon and, optionally, refresh the viewer's embedded RGBA copy.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for size in 16 24 32 48 64 128 256; do
  rsvg-convert -w "$size" -h "$size" "$DIR/icon.svg" -o "$DIR/icon-$size.png"
done
if [[ $# -eq 1 ]]; then
  convert "$DIR/icon-64.png" -depth 8 "RGBA:$1"
elif [[ $# -ne 0 ]]; then
  echo "Usage: $0 [viewer-icon.rgba]" >&2
  exit 2
fi
