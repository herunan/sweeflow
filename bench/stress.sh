#!/bin/bash
# Stress test: many Mac windows at once, the pointer sweeping across them, clicks that raise one
# after another, focus hopping, a resize storm and apps launching meanwhile, while the host's
# own figures say how long input waited. Opens its own windows (TextEdit, Terminal, Finder) and
# closes them again; only apps that weren't running before are started, and only those are quit.
# Needs the host's debug port (set here if it's off, and switched back off after).
# Usage: bench/stress.sh [user@mac] [windows] [seconds per phase]
#   → prints a summary; the host log of the run is left in /tmp/sweeflow-stress.log on the Mac,
#     profiler samples in /tmp/sample-*.txt.
set -euo pipefail
MAC=${1:-you@your-mac.local}
WINDOWS=${2:-16}
SECS=${3:-20}
ssh -o BatchMode=yes "$MAC" bash -s "$WINDOWS" "$SECS" <<'REMOTE'
set -uo pipefail
WINDOWS=$1; SECS=$2
LOG=~/Library/Logs/sweeflow.log
CFG=~/Library/Application\ Support/sweeflow/config.json
DIR=/tmp/sweeflow-stress; rm -rf "$DIR"; mkdir -p "$DIR"

# The debug port: on for the run.
was=$(plutil -extract debugPort raw "$CFG" 2>/dev/null || echo 0)
if [ "$was" = 0 ]; then
  plutil -replace debugPort -integer 44199 "$CFG"
  pkill -x SweeflowHost || true    # its launch agent starts it again
  echo "debug port switched on; waiting for the client to come back"
fi
PORT=""
dbg() { echo "$*" | nc -w10 127.0.0.1 "$PORT"; }
ids() { dbg windows | cut -d" " -f1 | grep -E '^[0-9]+$' | sort; }
started=()   # apps this run started (pids), quit at the end
start_app() {   # start_app NAME [FILE...]: only if it isn't running already
  pgrep -xq "$1" && return 1
  open -a "$@"; for _ in $(seq 50); do pgrep -xq "$1" && break; sleep 0.1; done
  started+=("$(pgrep -x "$1" | head -1)")
}
close_ours() {   # closing goes through each app in turn and can take seconds: wait until they've gone
  for _ in 1 2 3; do
    left=$(comm -13 "$DIR/before" <(ids))
    [ -z "$left" ] && return 0
    for w in $left; do dbg close "$w" >/dev/null; done
    for _ in $(seq 20); do sleep 2; [ -z "$(comm -13 "$DIR/before" <(ids))" ] && return 0; done
  done
  echo "   (some test windows are still open: $(comm -13 "$DIR/before" <(ids) | tr '\n' ' '))"
}
restore() {
  pkill -f "$DIR/busy" 2>/dev/null || true
  if [ -n "$PORT" ] && [ -f "$DIR/before" ]; then close_ours; fi
  for p in "${started[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  if [ "$was" = 0 ]; then plutil -replace debugPort -integer 0 "$CFG"; pkill -x SweeflowHost || true; fi
}
trap restore EXIT
for _ in $(seq 60); do
  PORT=$(grep -o "debug port listening on 127.0.0.1:[0-9]*" "$LOG" | tail -1 | cut -d: -f2)
  [ -n "$PORT" ] && [ -n "$(ids)" ] && break
  sleep 1
done
[ -n "$(ids)" ] || { echo "no debug port / no client connected / no windows"; exit 1; }
start=$(( $(wc -l < "$LOG" | tr -d " ") + 1 ))
mark() { echo "$(date +%s) $*" >> "$DIR/phases"; echo "== $*"; }
HOST=$(pgrep -x SweeflowHost)
cpu() { ps -o %cpu= -p "$HOST" -p "$(pgrep -x WindowServer)" | tr '\n' ' '; }
profile() { ( sleep "$2"; sample "$HOST" 8 1 -file "/tmp/sample-$1.txt" >/dev/null 2>&1 ) & }

# Sends debug-port commands in turn at the rate asked (one connection each, as the port wants).
cat > "$DIR/drive.pl" <<'PL'
use IO::Socket::INET; use Time::HiRes qw(time sleep);
my ($port, $secs, $hz, @cmds) = @ARGV; my $end = time + $secs; my ($n, $slow, $worst) = (0, 0, 0);
while (time < $end) {
  my $c = $cmds[$n % @cmds]; my $t = time;
  my $s = IO::Socket::INET->new(PeerAddr => "127.0.0.1", PeerPort => $port, Timeout => 20) or next;
  print $s "$c\n"; my $r = <$s>; close $s;
  my $ms = (time - $t) * 1000; $worst = $ms if $ms > $worst; $slow++ if $ms > 100; $n++;
  my $wait = 1 / $hz - (time - $t); sleep $wait if $wait > 0;
}
printf "   %d commands answered, %d took over 100 ms, slowest %.0f ms\n", $n, $slow, $worst;
PL

ids > "$DIR/before"
echo "   open before the test: $(dbg windows | cut -d" " -f2 | sort | uniq -c | awk '{printf "%s %s, ", $1, $2}')"
for a in Calculator Chess Dictionary "Font Book"; do pgrep -xq "$a" && echo "$a"; done > "$DIR/apps-before"
mark "baseline ($(wc -l < "$DIR/before" | tr -d ' ') windows already open)"
sleep 10

mark "opening $WINDOWS windows"
profile open 2
docs=(); for i in $(seq $((WINDOWS / 2))); do
  seq 400 | sed "s/^/stress window $i line /" > "$DIR/doc$i.txt"; docs+=("$DIR/doc$i.txt")
done
start_app TextEdit "${docs[@]}" || echo "   (TextEdit is in use: left alone, fewer windows)"
for i in $(seq $((WINDOWS / 4))); do
  printf '#!/bin/sh\nwhile :; do date +%%T.%%N; sleep 0.02; done\n' > "$DIR/busy$i.command"; chmod +x "$DIR/busy$i.command"
done
start_app Terminal "$DIR"/busy*.command || open -a Terminal "$DIR"/busy*.command
for i in $(seq $((WINDOWS / 4))); do mkdir -p "$DIR/folder$i"; open "$DIR/folder$i"; done
sleep 12; wait
ours=$(comm -13 "$DIR/before" <(ids))
all=$(ids)
echo "   $(echo "$all" | wc -l | tr -d ' ') windows streamed now, $(echo "$ours" | wc -l | tr -d ' ') of them the test's; cpu host/WindowServer: $(cpu)"
[ -n "$ours" ] || { echo "the test's windows didn't appear"; exit 1; }

mark "idle with all windows"
sleep "$SECS"
echo "   cpu host/WindowServer: $(cpu)"
# How long the window server takes to say what's in front (a press on a covered window waits for it).
for w in $(echo "$all" | head -3); do echo "   window server: $(dbg front "$w" 200 200 | grep -oE "asked now: [a-z]+ in [0-9]+ ms|last look \([0-9]+ ms old\)" | tr '\n' ' ')"; done

mark "pointer sweeping across every window (200 moves a second)"
moves=(); for w in $all; do for x in 60 120 180 240; do moves+=("move $w $x 200"); done; done
profile sweep 3
perl "$DIR/drive.pl" "$PORT" "$SECS" 200 "${moves[@]}"
wait; echo "   cpu host/WindowServer: $(cpu)"

# Clicks: the debug port answers at once, so how long each press waited to land is read from
# the host's log afterwards ("click on … held … ms"; presses that went straight through log nothing).
ln() { wc -l < "$LOG" | tr -d " "; }
held() {   # held FROM TO SENT: the presses between those log lines that had to wait
  sed -n "$1,$2p" "$LOG" | grep -E "click on [0-9]+ held" | sed -E 's/.*held ([0-9]+) ms.*/\1/' | sort -rn |
    awk -v sent="$3" '{ s += $1; n++; if (n == 1) m = $1 } END { if (n) printf "   %d presses, %d of them waited: %.0f ms on average, longest %.0f ms\n", sent, n, s / n, m; else printf "   %d presses, none waited\n", sent }'
}
mark "clicks that raise one window after another (10 a second)"
clicks=(); for w in $ours; do clicks+=("click $w 200 200"); done
profile clicks 3
from=$(ln)
sent=$(perl "$DIR/drive.pl" "$PORT" "$SECS" 10 "${clicks[@]}" | grep -oE "^ +[0-9]+" | tr -d " ")
wait; sleep 1; held "$from" "$(ln)" "$sent"; echo "   cpu host/WindowServer: $(cpu)"

mark "clicks inside one window (20 a second)"
one=$(echo "$ours" | head -1)
same=(); for x in 150 200 250 300; do same+=("click $one $x 220"); done
sleep 1; from=$(ln)
sent=$(perl "$DIR/drive.pl" "$PORT" "$SECS" 20 "${same[@]}" | grep -oE "^ +[0-9]+" | tr -d " ")
sleep 1; held "$from" "$(ln)" "$sent"; echo "   cpu host/WindowServer: $(cpu)"

if dbg focus 0 | grep -q "unknown command"; then
  echo "== (this host has no focus/resize debug commands: those two phases skipped)"
else
  mark "focus hopping from window to window (10 a second)"
  focuses=(); for w in $all; do focuses+=("focus $w"); done
  perl "$DIR/drive.pl" "$PORT" "$SECS" 10 "${focuses[@]}"
  echo "   cpu host/WindowServer: $(cpu)"

  mark "every test window resized over and over, as a tiling drag does (60 a second)"
  sizes=(); for h in 500 520 540 560 580 600 580 560 540 520; do for w in $ours; do sizes+=("resize $w 700 $h"); done; done
  profile resize 3
  perl "$DIR/drive.pl" "$PORT" "$SECS" 60 "${sizes[@]}"
  wait; echo "   cpu host/WindowServer: $(cpu)"
fi

mark "apps launching while the pointer sweeps"
( for a in Calculator Chess Dictionary "Font Book"; do pgrep -xq "$a" || open -a "$a"; sleep 2; done ) &
perl "$DIR/drive.pl" "$PORT" "$SECS" 100 "${moves[@]}"
wait; sleep 5
for a in Calculator Chess Dictionary "Font Book"; do
  # Started above (not in `started`: that ran in a subshell), and not there before the run.
  grep -qx "$a" "$DIR/apps-before" 2>/dev/null || started+=("$(pgrep -x "$a" | head -1)")
done

mark "closing the test windows"
pkill -f "$DIR/busy" || true
close_ours
mark "end"

tail -n "+$start" "$LOG" > /tmp/sweeflow-stress.log
cp "$DIR/phases" /tmp/sweeflow-stress.phases
echo
echo "what the host logged during the run ($(wc -l < /tmp/sweeflow-stress.log | tr -d ' ') lines):"
grep "session queue: work waits" /tmp/sweeflow-stress.log | sed -E 's/.*waits ([0-9.]+) ms on average, worst ([0-9.]+) ms/\1 \2/' |
  awk '{ if ($1 > a) a = $1; if ($2 > w) w = $2; n++ } END { printf "  session queue                worst wait %.0f ms, worst 10 s average %.1f ms (%d reports of a wait over 2 ms)\n", w, a, n }'
for k in "accessibility action took" "resize of [0-9]+ took" "looked up for capture in" "still taken in"; do
  grep -E "$k" /tmp/sweeflow-stress.log | sed -E 's/.* ([0-9.]+) ms.*/\1/' | sort -rn |
    awk -v k="$k" '{ s += $1; n++; if (n == 1) m = $1 } END { if (n) printf "  %-28s %d times, %.0f ms in all, slowest %.0f ms\n", k, n, s, m }'
done
for k in "raising " "moved out from under" "the app made it" "no first frame" "buttons \[" "encoder [0-9]+x" "client hello"; do
  printf "  %-28s %s times\n" "$k" "$(grep -cE "$k" /tmp/sweeflow-stress.log)"
done
REMOTE
