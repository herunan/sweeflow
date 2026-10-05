#!/bin/bash
# End-to-end latency benchmark. Opens a Mac Terminal window that redraws ~100 times a second,
# lets sweeflow stream it for a while, then reports the host's per-frame capture→present times
# (encode on the Mac → presented on Linux → acknowledged back) and the frame rate.
# Needs the host's debugPort. Usage: bench/latency.sh [user@mac] [seconds]
set -euo pipefail
MAC=${1:-you@your-mac.local}
SECS=${2:-30}
ssh "$MAC" bash -s "$SECS" <<'REMOTE'
set -euo pipefail
SECS=$1
LOG=~/Library/Logs/sweeflow.log
PORT=$(grep -o "debug port listening on 127.0.0.1:[0-9]*" "$LOG" | tail -1 | cut -d: -f2)
dbg() { echo "$*" | nc -w3 127.0.0.1 "$PORT"; }
before=$(dbg windows | { grep Terminal || true; } | cut -d" " -f1 | sort)
printf '#!/bin/sh\nwhile :; do date +%%T.%%N; sleep 0.01; done\n' > /tmp/sweeflow-bench.command
chmod +x /tmp/sweeflow-bench.command
open -a Terminal /tmp/sweeflow-bench.command
wid=""
for _ in $(seq 50); do
  wid=$(comm -13 <(echo "$before") <(dbg windows | { grep Terminal || true; } | cut -d" " -f1 | sort) | head -1)
  [ -n "$wid" ] && break
  sleep 0.2
done
[ -n "$wid" ] || { echo "bench window didn't appear"; exit 1; }
start=$(( $(wc -l < "$LOG" | tr -d " ") + 1 ))
sleep "$SECS"
pkill -f sweeflow-bench.command || true
dbg close "$wid" >/dev/null
tail -n "+$start" "$LOG" | { grep "stream $wid: .* fps" || true; } | sed -E 's/.*: ([0-9.]+) fps.*p50 ([0-9.-]+), p95 ([0-9.-]+), max ([0-9.-]+); .*encode ([0-9.-]+).*/\1 \2 \3 \4 \5/' |
  awk -v wid="$wid" '{fps+=$1; p50+=$2; p95+=$3; if ($4>mx) mx=$4; enc+=$5; n++}
    END { if (!n) { print "no stats for window " wid; exit 1 }
          printf "window %s, %d samples of 5 s\n  frame rate  %.1f fps\n  latency     p50 %.1f ms, p95 %.1f ms, worst %.1f ms\n  encode      %.1f ms\n", wid, n, fps/n, p50/n, p95/n, mx, enc/n }'
REMOTE
