#!/bin/bash
# Lays out Sweeflow.app around a built binary. Shared by install.sh (development) and release.sh.
# Usage: bundle.sh BINARY APP_PATH VERSION BUILD
set -euo pipefail
BIN="$1"; APP="$2"; VERSION="$3"; BUILD="$4"
BUNDLE_ID="dev.sweeflow.host"
AGENT_LABEL="dev.sweeflow.agent"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Library/LaunchAgents"
cp "$BIN" "$APP/Contents/MacOS/SweeflowHost"
# Icons (drawn by make-icons.sh): the app's, and the menu-bar item's.
RES="$(cd "$(dirname "$0")" && pwd)/Resources"
cp "$RES/MenuIcon.pdf" "$APP/Contents/Resources/"
iconutil -c icns "$RES/AppIcon.iconset" -o "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleName</key><string>Sweeflow</string>
  <key>CFBundleDisplayName</key><string>Sweeflow</string>
  <key>CFBundleExecutable</key><string>SweeflowHost</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$BUILD</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>LSUIElement</key><true/>
  <key>NSScreenCaptureUsageDescription</key><string>Sweeflow streams your app windows to your Linux desktop.</string>
  <key>NSAudioCaptureUsageDescription</key><string>Sweeflow plays your Mac apps' sound on your Linux desktop.</string>
  <key>NSLocalNetworkUsageDescription</key><string>Sweeflow lets your Linux computer find this Mac and connect to it.</string>
  <key>NSBonjourServices</key><array><string>_sweeflow._udp</string><string>_sweeflow-pair._udp</string></array>
</dict></plist>
PLIST
# Started at login (and kept running) once Sweeflow registers it with SMAppService.
cat > "$APP/Contents/Library/LaunchAgents/$AGENT_LABEL.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$AGENT_LABEL</string>
  <key>BundleProgram</key><string>Contents/MacOS/SweeflowHost</string>
  <key>AssociatedBundleIdentifiers</key><array><string>$BUNDLE_ID</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict></plist>
PLIST
