#!/bin/sh
# Renders the installer window's background (background.svg) at 1x and 2x for dmgbuild, which
# joins them into one picture for normal and Retina screens. Run on Linux (needs rsvg-convert
# and the Fira Sans font); the PNGs are committed, since the Mac that builds the DMG has neither.
set -eu
cd "$(dirname "$0")"
rsvg-convert -w 640 -h 320 background.svg -o background.png
rsvg-convert -w 1280 -h 640 background.svg -o background@2x.png
