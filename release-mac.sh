#!/bin/bash
# Builds Sweeflow.dmg on a Mac over SSH and brings it back: dist/ here, plus the copy the
# "Add a Mac" wizard serves to other Macs (~/.local/share/sweeflow/Sweeflow.dmg).
# Usage: ./release-mac.sh VERSION [user@mac]  (default: your Mac)
set -euo pipefail
cd "$(dirname "$0")"
VERSION="${1:?usage: $0 VERSION [user@mac]}"
TARGET="${2:-you@your-mac.local}"
BUILD=$(git rev-list --count HEAD)
REMOTE='~/.sweeflow/release-src'

echo "==> uploading sources to $TARGET"
tar -c --exclude .build --exclude dist -C mac -f - . | ssh "$TARGET" "rm -rf $REMOTE && mkdir -p $REMOTE && tar -x -C $REMOTE -f -"
DMG=$(ssh "$TARGET" "$REMOTE/release.sh $VERSION $BUILD" | tail -1)
mkdir -p dist ~/.local/share/sweeflow
scp -q "$TARGET:$DMG" "dist/Sweeflow-$VERSION.dmg"
cp "dist/Sweeflow-$VERSION.dmg" ~/.local/share/sweeflow/Sweeflow.dmg
# The copy the Linux build carries (build.rs), so `sweeflow install` sets it up anywhere.
cp "dist/Sweeflow-$VERSION.dmg" dist/Sweeflow.dmg
# The signing key never changes; keep a copy off the Mac (not in git).
mkdir -p ~/.local/share/sweeflow-release && chmod 700 ~/.local/share/sweeflow-release
ssh "$TARGET" 'cat "$HOME/Library/Application Support/sweeflow-release/release.p12"' > ~/.local/share/sweeflow-release/release.p12
chmod 600 ~/.local/share/sweeflow-release/release.p12
echo "==> dist/Sweeflow-$VERSION.dmg (served by the wizard; rebuild linux/ to carry it in the binary)"
