#!/bin/bash
# Builds a Sweeflow.dmg for other Macs: release build, signed with the fixed "Sweeflow Release"
# identity (so macOS keeps people's permissions across updates), packed in a disk image.
# Usage: release.sh VERSION BUILD   → prints the .dmg path last
set -euo pipefail
cd "$(dirname "$0")"
VERSION="$1"; BUILD="$2"
REL="$HOME/Library/Application Support/sweeflow-release"
KEYCHAIN="$REL/release.keychain-db"
KC_PASS="sweeflow-release"
SIGN_NAME="Sweeflow Release"
OUT="$PWD/dist"
mkdir -p "$REL" "$OUT"

echo "==> building $VERSION ($BUILD)" >&2
swift build -c release --arch arm64 2>&1 | grep -E "error|Build complete" >&2 || true
BIN=".build/arm64-apple-macosx/release/SweeflowHost"
[ -x "$BIN" ] || BIN=".build/release/SweeflowHost"
[ -x "$BIN" ] || { echo "build failed" >&2; exit 1; }

# One identity for every release. Its key is the thing to keep safe (release.p12, backed up
# off this Mac by release-mac.sh): a new one would make macOS ask everyone for permissions again.
if [ ! -f "$KEYCHAIN" ]; then
    if [ ! -f "$REL/release.p12" ]; then
        echo "==> creating the release signing identity" >&2
        tmp=$(mktemp -d)
        cat > "$tmp/cs.cnf" <<CNF
[req]
distinguished_name=dn
x509_extensions=v3
prompt=no
[dn]
CN=$SIGN_NAME
[v3]
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
CNF
        openssl req -x509 -config "$tmp/cs.cnf" -newkey rsa:2048 -nodes -keyout "$tmp/k.pem" -out "$tmp/c.pem" -days 36500 2>/dev/null
        openssl pkcs12 -export -inkey "$tmp/k.pem" -in "$tmp/c.pem" -out "$REL/release.p12" -passout pass:$KC_PASS \
            -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1
        chmod 600 "$REL/release.p12"
        rm -rf "$tmp"
    fi
    security create-keychain -p "$KC_PASS" "$KEYCHAIN"
    security set-keychain-settings "$KEYCHAIN"
    security import "$REL/release.p12" -k "$KEYCHAIN" -P "$KC_PASS" -T /usr/bin/codesign >/dev/null
    security set-key-partition-list -S apple-tool:,apple: -s -k "$KC_PASS" "$KEYCHAIN" >/dev/null
fi
security unlock-keychain -p "$KC_PASS" "$KEYCHAIN"
if ! security list-keychains -d user | grep -q release.keychain; then
    security list-keychains -d user -s $(security list-keychains -d user | tr -d '"') "$KEYCHAIN"
fi

echo "==> bundling and signing" >&2
STAGE=$(mktemp -d)
./bundle.sh "$BIN" "$STAGE/Sweeflow.app" "$VERSION" "$BUILD"
codesign --force --timestamp=none --options runtime --keychain "$KEYCHAIN" -s "$SIGN_NAME" \
    -i dev.sweeflow.host "$STAGE/Sweeflow.app"
codesign --verify --strict "$STAGE/Sweeflow.app" >&2
ln -s /Applications "$STAGE/Applications"

echo "==> making the disk image" >&2
DMG="$OUT/Sweeflow-$VERSION.dmg"
rm -f "$DMG"
hdiutil create -quiet -volname Sweeflow -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"
echo "$DMG"
