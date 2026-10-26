#!/bin/sh
# A snapshot of the counters of the stand's Linux side. It is served over TCP the same way as
# on Talaria (the kernel console): one request, one text snapshot.
#
#   statd.sh <workload pid> [port]
#   statd.sh --snap <pid>          — one snapshot to stdout, without the server
#
# Everything printed is what the system already counts itself: the workload is not wrapped
# in anything and is not rebuilt into anything.
set -eu
PID=${1:?the pid of the workload}
PORT=${2:-9999}
snap() {
    echo "### snapshot linux $(date -Is)"
    echo "--- perf ---"
    timeout 3 perf stat -e cycles,instructions,cache-misses,branch-misses,context-switches,cpu-migrations,page-faults \
        -p "$PID" -x, sleep 1 2>&1 | sed 's/^/perf /' || echo "perf is unavailable"
    echo "--- syscalls (a count over one second) ---"
    timeout 3 perf stat -e raw_syscalls:sys_enter -p "$PID" -x, sleep 1 2>&1 | sed 's/^/syscalls /' || true
    # THE WHOLE MACHINE, RING 3 AND THE KERNEL APART (ARM64-PLAN §12 item 3). `-p` above follows
    # the postmaster alone — PostgreSQL's backends are its forked children, so the line read
    # 21 k instructions at IPC 0.26 a second, nothing of the queries. The server's machine runs
    # the workload only, so `-a` divided by the rung's rate is the per-request price to set
    # beside Talaria's (talaria.pmuel0=1 / talaria.pmuev=). A c7g guest's PMU has two event
    # counters and the cycle counter: one set of three a second, never multiplexed.
    # 0x2D L2D_TLB_REFILL, 0x02 L1I_TLB_REFILL, 0x05 L1D_TLB_REFILL (the PMUv3 common events).
    echo "--- perf, the whole machine (one second a set) ---"
    for ev in cycles:u,instructions:u,r2D:u cycles:k,instructions:k,r2D:k r02:u,r05:u r02:k,r05:k \
              raw_syscalls:sys_enter,context-switches,cpu-migrations; do
        timeout 3 perf stat -a -e "$ev" -x, sleep 1 2>&1 | sed 's/^/perfall /' || echo "perfall unavailable $ev"
    done
    echo "--- the task ---"
    sed 's/^/status /' "/proc/$PID/status" 2>/dev/null || true
    sed 's/^/io /'     "/proc/$PID/io"     2>/dev/null || true
    echo "threads $(ls /proc/$PID/task 2>/dev/null | wc -l)"
    # THE PLACEMENT OF THREADS ON CORES: with isolcpus all the server's threads lived on one core
    # (batch 4, mysql) — that must be visible in the snapshot rather than found by hand over ssh.
    echo "psr $(ps -L -o psr= -p "$PID" 2>/dev/null | sort -n | uniq -c | awk '{printf "cpu%s:%s ", $2, $1}')"
    echo "--- the system ---"
    sed 's/^/sys /' /proc/stat
    sed 's/^/snmp /' /proc/net/snmp
    sed 's/^/netstat /' /proc/net/netstat
    sed 's/^/irq /' /proc/interrupts
    echo "### end of snapshot"
}
# THE SERVER IS IN PYTHON, NOT nc. On AL2023 nc/ncat are NOT INSTALLED (the
# nmap-ncat package is not in the base image), and the previous `snap | nc -l` loop on EC2 never
# listened: both forms of nc died instantly, the loop ran snap every ~3 s — i.e.
# `perf stat -p <server>` hung on the workload two thirds of the measurement time, while the runner
# got "Connection refused" and wrote "the snapshot is unavailable" into every suite. The snapshot
# is computed ON REQUEST (one connection, one snap); between requests the server
# does nothing. The order "accept -> compute -> serve" keeps perf on
# the workload only for the ~2 s intended, and only when the runner asked.
#   statd.sh --snap <pid>   — one snapshot to stdout (what is sent over TCP)
[ "$PID" = "--snap" ] && { PID=${2:?pid}; snap; exit 0; }
exec python3 - "$PORT" "$0" "$PID" <<'PY'
import socket, subprocess, sys
port, me, pid = int(sys.argv[1]), sys.argv[2], sys.argv[3]
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", port)); srv.listen(4)
while True:
    c, _ = srv.accept()
    try:
        out = subprocess.run(["/bin/sh", me, "--snap", pid], stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, timeout=30).stdout
        c.sendall(out)
    except Exception as e:
        try: c.sendall(("the snapshot could not be assembled: %r\n" % (e,)).encode())
        except Exception: pass
    finally:
        c.close()
PY
