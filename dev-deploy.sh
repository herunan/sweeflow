#!/bin/bash
# Rebuild everything, push the Mac host (sources are embedded at build time) and restart the
# local client service. Usage: ./dev-deploy.sh [user@mac]  (default: the one in .dev-mac)
set -euo pipefail
cd "$(dirname "$0")"
TARGET="${1:-$(cat .dev-mac 2>/dev/null)}"
[ -n "$TARGET" ] || { echo "which Mac? $0 user@mac, or put user@mac in .dev-mac" >&2; exit 1; }
cd linux
cargo build --release
./target/release/sweeflow setup "$TARGET"
./target/release/sweeflow install
