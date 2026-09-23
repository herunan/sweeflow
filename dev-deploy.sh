#!/bin/bash
# Rebuild everything, push the Mac host (sources are embedded at build time) and restart the
# local client service. Usage: ./dev-deploy.sh [user@mac]
set -euo pipefail
cd "$(dirname "$0")/linux"
cargo build --release 2>&1 | grep -E "^(error|warning: unused)|Finished" || true
./target/release/sweeflow setup "${1:-you@your-mac.local}"
./target/release/sweeflow install
