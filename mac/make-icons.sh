#!/bin/bash
# Draws the Mac's icons from the logo (assets/sweeflow-app.svg, assets/sweeflow-mac-menu.svg, see assets/tools/gen-logo.py)
# into mac/Resources: the app icon's sizes (made an .icns by bundle.sh with iconutil) and the
# menu-bar icon (PDF). Needs rsvg-convert; run where that is (Linux), and commit the PNGs.
set -euo pipefail
cd "$(dirname "$0")"
A=../assets
mkdir -p Resources/AppIcon.iconset
for s in 16 32 128 256 512; do
  rsvg-convert -w $s -h $s "$A/sweeflow-app.svg" -o "Resources/AppIcon.iconset/icon_${s}x${s}.png"
  rsvg-convert -w $((s * 2)) -h $((s * 2)) "$A/sweeflow-app.svg" -o "Resources/AppIcon.iconset/icon_${s}x${s}@2x.png"
done
# A PDF: vector, so sharp at any size the menu bar draws it.
rsvg-convert -f pdf "$A/sweeflow-mac-menu.svg" -o Resources/MenuIcon.pdf
# Its state badges: the same symbols as the Linux tray icon's (Breeze, LGPL), paused and not
# connected.
B=/usr/share/icons/breeze
rsvg-convert -f pdf "$B/status/16/media-playback-paused-symbolic.svg" -o Resources/BadgePaused.pdf
rsvg-convert -f pdf "$B/actions/16/network-disconnect-symbolic.svg" -o Resources/BadgeDisconnected.pdf
