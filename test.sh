#!/bin/bash
# Runs every test: the Linux client's (cargo test) and the Mac host's (swift test, on the Mac,
# which needs Xcode). Usage: ./test.sh [user@mac]
set -euo pipefail
cd "$(dirname "$0")"
MAC=${1:-you@your-mac.local}
echo "== Linux client"
(cd linux && cargo test --release --quiet 2>&1 | grep -vE "^warning|^\s+(-->|\||=|[0-9]+ \|)|^$")
echo "== Mac host"
rsync -a --delete --exclude .build mac/ "$MAC":sweeflow-test/
ssh "$MAC" 'cd sweeflow-test && swift test 2>&1 | grep -E "Test (Suite|Case).*(passed|failed)|error:|Executed" | grep -vE "Test Case.*passed" | tail -20'
