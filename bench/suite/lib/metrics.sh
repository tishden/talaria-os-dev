#!/bin/sh
# A snapshot of the stand's counters — taken from what the system already counts itself.
#
#   metrics.sh <stand> <file> [label]
#
# Talaria: the console on top of the live workload (kstat/perf/netstat/top) — the snapshot
#   is taken from the scheduler tick and does not stop the workload.
# Linux:   statd in the guest's init (see guest-init.c) — perf events opened on
#   the workload's task, plus /proc of the task itself and of the system.
#
# Both paths are NON-INVASIVE: on neither side is the workload rebuilt for the measurement,
# recompiled, or started under a tracer.
set -eu
STAND=${1:?stand}
OUT=${2:?file}
TAG=${3:-}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=${REPO:-$(cd "$HERE/../../.." && pwd)}
GUEST_IP=${GUEST_IP:-10.0.0.2}
SOCK=${CONSOLE_SOCK:-bench/suite/results/console.sock}

{
    echo "### snapshot $STAND $TAG $(date -Is)"
    if [ "$STAND" = talaria ]; then
        # netstat next to kstat: when the server stops ACCEPTING connections,
        # the counters look healthy (the listener is in place, the task count is the same), and
        # the difference is visible only in the state of the listener's sockets.
        python3 "$HERE/talaria-console.py" "$REPO/$SOCK" kstat stat netstat perf 2>&1 || echo "the console is unavailable"
    else
        python3 - "$GUEST_IP" <<'PY' 2>&1 || echo "statd is unavailable"
import socket, sys
s = socket.create_connection((sys.argv[1], 9999), timeout=5)
s.settimeout(5)
buf = b""
while True:
    c = s.recv(65536)
    if not c:
        break
    buf += c
sys.stdout.write(buf.decode("utf-8", "replace"))
PY
    fi
    echo "### end of snapshot"
} >> "$OUT"
