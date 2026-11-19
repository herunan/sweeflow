#!/bin/bash
# Builds Sweeflow.dmg on a Mac over SSH and brings it back to dist/ (the Mac's download, from the
# website and the GitHub release).
# Usage: ./release-mac.sh VERSION [user@mac]  (default: the one in .dev-mac, a line like you@mac.local)
set -euo pipefail
cd "$(dirname "$0")"
VERSION="${1:?usage: $0 VERSION [user@mac]}"
TARGET="${2:-$(cat .dev-mac 2>/dev/null)}"
[ -n "$TARGET" ] || { echo "which Mac? $0 VERSION user@mac, or put user@mac in .dev-mac" >&2; exit 1; }
BUILD=${BUILD:-$(git rev-list --count HEAD)}  # override for test builds of one commit
REMOTE='~/.sweeflow/release-src'

echo "==> uploading sources to $TARGET"
tar -c --exclude .build --exclude dist -C mac -f - . | ssh "$TARGET" "rm -rf $REMOTE && mkdir -p $REMOTE && tar -x -C $REMOTE -f -"
DMG=$(ssh "$TARGET" "$REMOTE/release.sh $VERSION $BUILD" | tail -1)
mkdir -p dist
scp -q "$TARGET:$DMG" "dist/Sweeflow-$VERSION.dmg"
cp "dist/Sweeflow-$VERSION.dmg" dist/Sweeflow.dmg
echo "$BUILD" > dist/Sweeflow.build   # for the update notice (linux/release/build.sh)
# The signing key never changes; keep a copy off the Mac (not in git).
mkdir -p ~/.local/share/sweeflow-release && chmod 700 ~/.local/share/sweeflow-release
ssh "$TARGET" 'cat "$HOME/Library/Application Support/sweeflow-release/release.p12"' > ~/.local/share/sweeflow-release/release.p12
chmod 600 ~/.local/share/sweeflow-release/release.p12
echo "==> dist/Sweeflow-$VERSION.dmg"
