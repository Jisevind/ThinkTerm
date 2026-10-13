#!/bin/sh
# Acceptance checks for the iOS app on a booted simulator, against the
# probe (ios/tools/probe.sh up), with the app built by ios/build.sh:
#   1. the desktop changes the layout while the phone is cut off;
#   2. the server restarts;
#   3. the app goes to the background and comes back;
#   4. a flood of output, and the longest the main thread stalls in it.
# The app runs its --accepttest flow; this script does what a desktop,
# a network or a person would, and asks the app what it shows.
#
#   SIM_NAME   the booted simulator (default "iPhone 17")
#   STALL_MS   the longest main-thread stall allowed in the flood (250)
set -u
cd "$(dirname "$0")/../.."
SIM_NAME=${SIM_NAME:-iPhone 17}
STALL_MS=${STALL_MS:-250}
BUNDLE=com.roversx.thinkterm.ios
APP=target/xcode/Build/Products/Debug-iphonesimulator/ThinkTerm.app
REQUEST=/tmp/ttp-ssh/accept-request
SSH=/tmp/ttp-ssh
SRV=/tmp/ttp-srv
BIN=/tmp/ttp-bin

UDID=$(xcrun simctl list devices booted | grep -F "$SIM_NAME (" | head -1 | sed -E 's/.*\(([0-9A-F-]+)\).*/\1/')
[ -n "$UDID" ] || { echo "no booted simulator named $SIM_NAME" >&2; exit 2; }
[ -d "$APP" ] || { echo "no app at $APP: run ios/build.sh" >&2; exit 2; }
[ -x $BIN/thinkterm ] || { echo "no probe: run ios/tools/probe.sh up" >&2; exit 2; }
LOG=$(mktemp "${TMPDIR:-/tmp}/thinkterm-accept.XXXXXX")
passed=0
failed=0

# The probe's server, as a desktop on its machine would use it.
srv() { env -i HOME=$SRV PATH=/usr/bin:/bin $BIN/thinkterm cli --no-auto-start "$@"; }

# The proxies serving this probe's sessions: under its own sshd (by the
# pid file, checked), never another probe's of the same name.
probe_proxies() {
  listener=$(cat $SSH/sshd.pid 2>/dev/null) || return 0
  ps -ww -o command= -p "$listener" 2>/dev/null | grep -q "$SSH/sshd_config" || return 0
  ps -ax -ww -o pid=,ppid=,command= | awk -v root="$listener" -v cmd="$BIN/thinkterm cli --prefer-mux proxy" '
    { pid[NR] = $1; ppid[NR] = $2; $1 = ""; $2 = ""; sub(/^ +/, ""); line[NR] = $0 }
    END {
      under[root] = 1
      do {
        grew = 0
        for (i = 1; i <= NR; i++) if (under[ppid[i]] && !under[pid[i]]) { under[pid[i]] = 1; grew = 1 }
      } while (grew)
      for (i = 1; i <= NR; i++) if (under[pid[i]] && index(line[i], cmd) == 1) print pid[i]
    }'
}

result() {
  if [ "$1" = ok ]; then passed=$((passed + 1)); echo "PASS $2"
  else failed=$((failed + 1)); echo "FAIL $2"; fi
}

# Ask the app something; its answer lands in ANSWER ("timeout" if none).
asked=0
ask() {
  asked=$((asked + 1))
  echo "$asked $*" > $REQUEST.new && mv $REQUEST.new $REQUEST
  ANSWER=timeout
  for _ in $(seq 1 150); do
    if grep -aq "^ACCEPT ack $asked " "$LOG"; then
      # Read again a moment later: the console may have been mid-line.
      sleep 0.1
      line=$(grep -a "^ACCEPT ack $asked " "$LOG" | head -1 | tr -d '\r')
      ANSWER=${line#"ACCEPT ack $asked "}
      return 0
    fi
    sleep 0.2
  done
  return 1
}

# key=value out of the last answer.
field() { echo "$ANSWER" | tr ' ' '\n' | sed -n "s/^$1=//p"; }

# Until the app is attached again, for up to $1 seconds by the clock (an
# app that stopped answering costs that, not a wait for every question).
connected() {
  end=$(($(date +%s) + $1))
  while [ "$(date +%s)" -lt $end ]; do
    ask state
    [ "$(field connected)" = yes ] && return 0
    sleep 0.5
  done
  return 1
}

# Until the app no longer is: a cut that happened.
cut_off() {
  end=$(($(date +%s) + $1))
  while [ "$(date +%s)" -lt $end ]; do
    ask state
    [ "$(field connected)" = no ] && return 0
    sleep 0.5
  done
  return 1
}

# Until the app's rows show $1 (output only: the command line differs).
shows() {
  end=$(($(date +%s) + 20))
  while [ "$(date +%s)" -lt $end ]; do
    ask expect "$1"
    [ "$ANSWER" = found ] && return 0
    sleep 0.5
  done
  return 1
}

# The server's own count: tabs in the window holding tab $1, and its panes.
# (The app's tab, from its views; the pane on its status line is the one it
# first attached to, which a reconnect to a new server does not change.)
server_counts() {
  srv list --format json | python3 -I -c '
import json, sys
panes = json.load(sys.stdin)
tab = int(sys.argv[1]) if sys.argv[1].isdigit() else None
window = next((p["window_id"] for p in panes if p["tab_id"] == tab), None)
tabs = {p["tab_id"] for p in panes if p["window_id"] == window}
in_tab = [p for p in panes if p["tab_id"] == tab]
print(len(tabs), len(in_tab))' "$1"
}

# The app's view against the server's, for up to 15 s while it settles:
# tabs in its window, and a bar for each pane of its tab when it is split
# (a lone pane has none on the phone). SERVER says what the server had,
# once it was asked: a check cut short before then reports that.
SERVER="server not asked"
same_layout() {
  for _ in $(seq 1 30); do
    ask state
    set -- $(server_counts "$(field tab)")
    SERVER="server tabs=${1:-?} panes=${2:-?}"
    bars=${2:-0}
    [ "$bars" -gt 1 ] || bars=0
    if [ "$(field connected)" = yes ] && [ "$(field tabs)" = "${1:-}" ] && [ "$(field bars)" = "$bars" ]; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

rm -f $REQUEST
xcrun simctl install "$UDID" "$APP"
xcrun simctl launch --console-pty --terminate-running-process "$UDID" $BUNDLE --accepttest > "$LOG" 2>&1 &
console=$!
trap 'kill $console 2>/dev/null; xcrun simctl terminate "$UDID" $BUNDLE 2>/dev/null; rm -f $REQUEST; ios/tools/probe.sh sshd-up >/dev/null 2>&1' EXIT

if connected 60; then result ok "connects: $ANSWER"; else result no "connects: $ANSWER"; echo "log: $LOG"; exit 1; fi

# 1. Cut off, the desktop splits the phone's pane and opens a tab beside it.
pane=$(field pane)
# Found before the sshd goes: its sessions outlive it, with another parent.
proxies=$(probe_proxies)
ios/tools/probe.sh sshd-down
for pid in $proxies; do kill "$pid"; done
# Each step must have happened, or the check proves nothing.
[ -n "$proxies" ] && cut_off 15 && cut=yes || cut=no
srv split-pane --pane-id "$pane" --right >/dev/null && srv spawn --pane-id "$pane" >/dev/null && changed=yes || changed=no
sleep 2
ios/tools/probe.sh sshd-up
if [ $cut = yes ] && [ $changed = yes ] && connected 40 && same_layout; then
  result ok "layout changed while cut off: $ANSWER ($SERVER)"
else
  result no "layout changed while cut off: cut=$cut changed=$changed, $ANSWER ($SERVER)"
fi

# 2. The server restarts; the phone finds the new one, a fresh layout.
pid=$(cat $SRV/.local/share/thinkterm/pid-debug)
if ps -ww -o command= -p "$pid" | grep -q "^$BIN/thinkterm-mux-server"; then kill "$pid" && stopped=yes || stopped=no; else stopped=no; fi
sleep 2
# A new server, not the old one still answering.
if [ $stopped = yes ] && connected 40 && same_layout \
   && [ "$(cat $SRV/.local/share/thinkterm/pid-debug 2>/dev/null)" != "$pid" ]; then
  result ok "server restart: $ANSWER ($SERVER)"
else
  result no "server restart: stopped=$stopped, $ANSWER ($SERVER)"
fi

# 3. To the background and back. The console reaches the log in bursts:
# its lines are waited for.
logged() {
  for _ in $(seq 1 30); do
    tail -n +"$mark" "$LOG" | grep -aq "$1" && return 0
    sleep 0.5
  done
  return 1
}
mark=$(($(wc -l < "$LOG") + 1))
xcrun simctl launch "$UDID" com.apple.Preferences >/dev/null 2>&1
logged 'core: shown: false' && hidden=yes || hidden=no
sleep 2
xcrun simctl launch "$UDID" $BUNDLE >/dev/null 2>&1
if [ $hidden = yes ] && logged 'core: shown: true' && connected 40 \
   && ask type 'echo BG-$((6*7))' && shows BG-42; then
  result ok "background and back: told hidden and shown, typing works"
else
  result no "background and back: hidden=$hidden, $ANSWER"
fi

# 4. A flood of output: the main thread keeps its frames, the end arrives.
# Measured from before it starts: the app reads requests on the main
# thread, so one sent after would be read only once a stall had passed.
stall=
if ask measure && ask type 'seq 1 200000; echo FLOOD-$((40+2))'; then
  sleep 8
  ask measured && stall=$(field longest_gap_ms)
fi
if [ -n "$stall" ] && [ "$stall" -lt "$STALL_MS" ]; then
  result ok "flood: longest main-thread stall ${stall} ms"
else
  result no "flood: longest main-thread stall ${stall:-?} ms (limit $STALL_MS)"
fi
if shows FLOOD-42 && ask type 'echo AFTER-$((5*5))' && shows AFTER-25; then
  result ok "flood: the end arrives and typing works after"
else
  result no "flood: end or typing after: $ANSWER"
fi

echo "accept: $passed passed, $failed failed (log: $LOG)"
[ "$failed" -eq 0 ]
