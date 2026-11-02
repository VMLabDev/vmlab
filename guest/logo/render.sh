#!/usr/bin/env bash
# Render the path-only source without depending on an installed font.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
rsvg-convert "$DIR/wordmark.svg" -o "$work/logo.png"
convert "$work/logo.png" -background black -alpha remove -alpha off \
  -type TrueColor -depth 8 "BMP3:$DIR/Logo.bmp"
convert -size 640x480 xc:black "$work/logo.png" -gravity center -composite \
  -colorspace sRGB -type TrueColor -define jpeg:colorspace=RGB \
  -sampling-factor 4:2:0 -interlace none -strip -quality 90 "$DIR/bootsplash.jpg"
