#!/bin/bash
# Builds Sweeflow.app and (re)starts it as a LaunchAgent. Idempotent: safe to re-run after every
# change. On a Mac that has the "Sweeflow Release" key (made by release.sh) it updates the
# released app in /Applications, signed with that key, so macOS keeps its permissions. Elsewhere
# it's a development copy in ~/Applications, signed with a stable local identity.
set -euo pipefail
cd "$(dirname "$0")"

BUNDLE_ID="dev.sweeflow.host"
SUPPORT="$HOME/Library/Application Support/sweeflow"
REL="$HOME/Library/Application Support/sweeflow-release"
AGENT="$HOME/Library/LaunchAgents/$BUNDLE_ID.plist"
LOG="$HOME/Library/Logs/sweeflow.log"

mkdir -p "$SUPPORT" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"

echo "==> building"
swift build -c release 2>&1 | grep -E "error|Compiling|Build complete" || true
BIN=".build/release/SweeflowHost"
[ -x "$BIN" ] || { echo "build failed"; exit 1; }

if [ -z "${SWEEFLOW_DEV_COPY:-}" ] && [ -f "$REL/release.keychain-db" ] &&
   { [ -d /Applications/Sweeflow.app ] || [ ! -d "$HOME/Applications/Sweeflow.app" ]; }; then
    # This Mac makes the releases: update the released, permitted copy in place. (Not when
    # only a development copy is installed here: that's the one macOS has the permissions for,
    # and replacing it with the released app would lose them. SWEEFLOW_DEV_COPY=1 asks for the
    # development copy outright.)
    APP="/Applications/Sweeflow.app"
    KEYCHAIN="$REL/release.keychain-db"
    KC_PASS="sweeflow-release"
    SIGN_NAME="Sweeflow Release"
    # Keep the installed version number; the build number says when it was built.
    VERSION=$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$APP/Contents/Info.plist" 2>/dev/null || echo 0.1.0)
    BUILD="${SWEEFLOW_BUILD:-$(date +%Y%m%d%H%M)}"
    # A development copy from before goes: one Sweeflow, the permitted one.
    rm -rf "$HOME/Applications/Sweeflow.app"
else
    APP="$HOME/Applications/Sweeflow.app"
    KEYCHAIN="$SUPPORT/signing.keychain-db"
    KC_PASS="sweeflow-signing"
    SIGN_NAME="Sweeflow Local Signing"
    VERSION=0.1; BUILD=1
    mkdir -p "$HOME/Applications"
    # A self-signed code-signing identity in a dedicated keychain. macOS privacy permissions
    # (Screen Recording, Accessibility) are tied to the signature, so a stable identity means
    # they survive rebuilds; ad-hoc signatures would lose them every time.
    if [ ! -f "$KEYCHAIN" ]; then
        echo "==> creating local signing identity"
        tmp=$(mktemp -d)
        cat > "$tmp/cs.cnf" <<EOF
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
EOF
        openssl req -x509 -config "$tmp/cs.cnf" -newkey rsa:2048 -nodes -keyout "$tmp/k.pem" -out "$tmp/c.pem" -days 36500 2>/dev/null
        openssl pkcs12 -export -inkey "$tmp/k.pem" -in "$tmp/c.pem" -out "$tmp/id.p12" -passout pass:$KC_PASS \
            -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1
        security create-keychain -p "$KC_PASS" "$KEYCHAIN"
        security set-keychain-settings "$KEYCHAIN"   # no auto-lock
        security import "$tmp/id.p12" -k "$KEYCHAIN" -P "$KC_PASS" -T /usr/bin/codesign >/dev/null
        security set-key-partition-list -S apple-tool:,apple: -s -k "$KC_PASS" "$KEYCHAIN" >/dev/null
        rm -rf "$tmp"
    fi
fi
security unlock-keychain -p "$KC_PASS" "$KEYCHAIN"
# codesign only searches keychains on the search list.
if ! security list-keychains -d user | grep -q "$(basename "$KEYCHAIN")"; then
    security list-keychains -d user -s $(security list-keychains -d user | tr -d '"') "$KEYCHAIN"
fi

echo "==> bundling $APP ($SIGN_NAME, $VERSION build $BUILD)"
STAGE=$(mktemp -d)
./bundle.sh "$BIN" "$STAGE/Sweeflow.app" "$VERSION" "$BUILD"
codesign --force --timestamp=none --options runtime --keychain "$KEYCHAIN" -s "$SIGN_NAME" \
    -i "$BUNDLE_ID" "$STAGE/Sweeflow.app"
launchctl bootout "gui/$(id -u)/$BUNDLE_ID" 2>/dev/null || true
# The app's own login item (SMAppService) goes stale whenever the app is replaced in place
# (launchd then refuses to start it), so a plain LaunchAgent runs it here; the app sees that and
# leaves its own login item alone.
launchctl bootout "gui/$(id -u)/dev.sweeflow.agent" 2>/dev/null || true
pkill -x SweeflowHost 2>/dev/null || true
rm -rf "$APP"
mv "$STAGE/Sweeflow.app" "$APP"
rmdir "$STAGE"

cat > "$AGENT" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$BUNDLE_ID</string>
  <key>ProgramArguments</key><array><string>$APP/Contents/MacOS/SweeflowHost</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>$LOG</string>
  <key>StandardOutPath</key><string>$LOG</string>
</dict></plist>
EOF

echo "==> starting"
launchctl bootstrap "gui/$(id -u)" "$AGENT"
sleep 2
tail -n 5 "$LOG"
echo "==> host fingerprint: $(cat "$SUPPORT/fingerprint" 2>/dev/null || echo '(not yet created)')"
