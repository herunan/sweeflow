#!/bin/bash
# Builds a Sweeflow.dmg for other Macs: release build, signed with a Developer ID (so
# macOS keeps people's permissions across updates), notarised by Apple and stapled, so it opens
# without any warning. Packed in a disk image, which is notarised and stapled too.
# Usage: release.sh VERSION BUILD   → prints the .dmg path last
set -euo pipefail
cd "$(dirname "$0")"
VERSION="$1"; BUILD="$2"
REL="$HOME/Library/Application Support/sweeflow-release"
KEYCHAIN="$REL/release.keychain-db"
KC_PASS="sweeflow-release"
NOTARY_PROFILE="sweeflow"   # notarytool store-credentials, kept in the release keychain
OUT="$PWD/dist"
mkdir -p "$REL" "$OUT"

echo "==> building $VERSION ($BUILD)" >&2
swift build -c release --arch arm64 2>&1 | grep -E "error|Build complete" >&2 || true
BIN=".build/arm64-apple-macosx/release/SweeflowHost"
[ -x "$BIN" ] || BIN=".build/release/SweeflowHost"
[ -x "$BIN" ] || { echo "build failed" >&2; exit 1; }

# The Developer ID identity (devid.p12, made with a request from this Mac, backed up off it by
# release-mac.sh), Apple's intermediate certificate beside it, and notarytool's login: all in a
# keychain of their own, so nothing asks for the login keychain's password.
if [ ! -f "$KEYCHAIN" ]; then
    [ -f "$REL/devid.p12" ] || { echo "no $REL/devid.p12: the Developer ID identity" >&2; exit 1; }
    security create-keychain -p "$KC_PASS" "$KEYCHAIN"
    security set-keychain-settings "$KEYCHAIN"
    security import "$REL/devid.p12" -k "$KEYCHAIN" -P "$KC_PASS" -T /usr/bin/codesign >/dev/null
    curl -fsSo "$REL/DeveloperIDG2CA.cer" https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer
    security import "$REL/DeveloperIDG2CA.cer" -k "$KEYCHAIN" >/dev/null
    security set-key-partition-list -S apple-tool:,apple: -s -k "$KC_PASS" "$KEYCHAIN" >/dev/null
fi
security unlock-keychain -p "$KC_PASS" "$KEYCHAIN"
# The identity's name (it has the team's name and ID in it), read rather than written down here.
SIGN_NAME=$(security find-identity -v -p codesigning "$KEYCHAIN" | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)
[ -n "$SIGN_NAME" ] || { echo "no Developer ID Application identity in $KEYCHAIN" >&2; exit 1; }
# A new keychain has no notarytool login yet: store it once (an App Store Connect API key, made
# under Users and Access › Integrations, Developer role):
#   xcrun notarytool store-credentials sweeflow --key AuthKey_….p8 --key-id … --issuer … --keychain "$KEYCHAIN"
# On the search list too (the path has a space, so read it line by line), so codesign finds
# Apple's intermediate certificate that's kept beside the Developer ID one.
if ! security list-keychains -d user | grep -qF "$KEYCHAIN"; then
    keychains=()
    while IFS= read -r k; do
        k=${k#*\"}; k=${k%\"}
        case "$k" in *release.keychain*|*/Application|*/Support/*) ;; *) keychains+=("$k") ;; esac
    done < <(security list-keychains -d user)
    security list-keychains -d user -s "${keychains[@]}" "$KEYCHAIN"
fi

echo "==> bundling and signing" >&2
STAGE=$(mktemp -d)
./bundle.sh "$BIN" "$STAGE/Sweeflow.app" "$VERSION" "$BUILD"
codesign --force --timestamp --options runtime --keychain "$KEYCHAIN" -s "$SIGN_NAME" \
    -i dev.sweeflow.host "$STAGE/Sweeflow.app"
codesign --verify --strict "$STAGE/Sweeflow.app" >&2

# Apple's notary service checks it (a few minutes) and its ticket is stapled on, so the app
# opens without a warning even offline, and once copied out of the disk image by an update.
notarise() {
    xcrun notarytool submit "$1" --keychain-profile "$NOTARY_PROFILE" --keychain "$KEYCHAIN" --wait \
        --output-format json > "$STAGE/notary.json" || true
    if ! grep -q '"status" *: *"Accepted"' "$STAGE/notary.json"; then
        cat "$STAGE/notary.json" >&2
        id=$(sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' "$STAGE/notary.json" | head -1)
        [ -n "$id" ] && xcrun notarytool log "$id" --keychain-profile "$NOTARY_PROFILE" --keychain "$KEYCHAIN" >&2
        echo "notarisation failed" >&2; exit 1
    fi
}
echo "==> notarising the app" >&2
ditto -c -k --keepParent "$STAGE/Sweeflow.app" "$STAGE/Sweeflow.zip"
notarise "$STAGE/Sweeflow.zip"
rm "$STAGE/Sweeflow.zip"
xcrun stapler staple -q "$STAGE/Sweeflow.app" >&2
echo "==> making the disk image" >&2
DMG="$OUT/Sweeflow-$VERSION.dmg"
rm -f "$DMG"
# dmgbuild lays out the window that opens with the disk (background, icon places) without driving
# Finder. Kept in its own Python environment next to the signing key, made the first time.
DMGBUILD="$REL/dmgbuild/bin/dmgbuild"
if [ ! -x "$DMGBUILD" ]; then
    python3 -m venv "$REL/dmgbuild" >&2
    "$REL/dmgbuild/bin/pip" install -q "dmgbuild==1.6.5" >&2
fi
if [ -x "$DMGBUILD" ]; then
    "$DMGBUILD" -s dmg/settings.py -D app="$STAGE/Sweeflow.app" -D here="$PWD/dmg" Sweeflow "$DMG" >&2
else
    echo "    (dmgbuild missing: a plain disk image)" >&2
    ln -s /Applications "$STAGE/Applications"
    hdiutil create -quiet -volname Sweeflow -srcfolder "$STAGE" -ov -format UDZO "$DMG"
fi
echo "==> notarising the disk image" >&2
codesign --force --timestamp --keychain "$KEYCHAIN" -s "$SIGN_NAME" "$DMG"
notarise "$DMG"
xcrun stapler staple -q "$DMG" >&2
spctl --assess --type open --context context:primary-signature -v "$DMG" >&2
rm -rf "$STAGE"
echo "$DMG"
