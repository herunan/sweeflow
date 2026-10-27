#!/bin/bash
# Draws the Mac's icons from the logo (assets/sweeflow-mac-*.svg, see assets/tools/gen-logo.py)
# into mac/Resources: the app icon's sizes (made an .icns by bundle.sh with iconutil) and the
# menu-bar icon (PDF). Needs rsvg-convert; run where that is (Linux), and commit the PNGs.
set -euo pipefail
cd "$(dirname "$0")"
A=../assets
mkdir -p Resources/AppIcon.iconset
for s in 16 32 128 256 512; do
  rsvg-convert -w $s -h $s "$A/sweeflow-mac-app.svg" -o "Resources/AppIcon.iconset/icon_${s}x${s}.png"
  rsvg-convert -w $((s * 2)) -h $((s * 2)) "$A/sweeflow-mac-app.svg" -o "Resources/AppIcon.iconset/icon_${s}x${s}@2x.png"
done
# A PDF: vector, so sharp at any size the menu bar draws it.
rsvg-convert -f pdf "$A/sweeflow-mac-menu.svg" -o Resources/MenuIcon.pdf
