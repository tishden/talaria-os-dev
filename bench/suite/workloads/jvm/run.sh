#!/bin/sh
# OpenJDK 21 (Temurin, musl) as a workload of the suite.
#
# The test case is not client-server: the benchmark runs INSIDE the process and prints
# scalar metrics (`RESULT <group> <metric> <value> <unit>`) — pass time,
# gigaflops, allocation rate, speedup on four cores,
# the ping-pong RTT between threads. They cannot be reduced to the percentile scheme: for
# half of them "more" means "better", and the ratio would read backwards. So the
# summary is its own — lib/scalars.py, where the direction is given explicitly per metric name.
#
#   run.sh <talaria|linux> <results directory> [label]
#   env: SMP=4 MEM=3072
#
# Both sides: ONE JDK directory, ONE Bench.class, the same JVM flags, the same number of
# cores. Linux is Alpine in docker with a cpuset on the same physical cores.
set -eu
STAND=${1:?stand}
OUT=${2:?directory}
HERE=$(cd "$(dirname "$0")" && pwd)
SUITE=$(cd "$HERE/../.." && pwd)
REPO=$(cd "$SUITE/../.." && pwd)
J="$REPO/services/jvm"
SMP=${SMP:-4}; MEM=${MEM:-3072}
mkdir -p "$OUT"

[ -f "$J/disk-jvm.img" ] || { echo "no $J/disk-jvm.img — run services/jvm/mkdisk.sh"; exit 1; }

if [ "$STAND" = talaria ]; then
    echo "   jvm/talaria: SMP=$SMP MEM=$MEM"
    SMP=$SMP MEM=$MEM "$J/run.sh" ${JVM_SECS:-400} >/dev/null 2>&1 || true
    LOG="$J/build/serial.log"
else
    # Linux runs IN THE SAME VM, not natively. Native docker (run-linux.sh) gives
    # Linux an advantage: a TLB miss under a hypervisor walks nested tables, and
    # the compute gap (~20 % on sieve/matmul) is explained by the stand, not by the
    # system. The suite already caught the same mistake on ClickHouse.
    echo "   jvm/linux: the same JDK in AN IDENTICAL VM (SMP=$SMP MEM=$MEM)"
    SMP=$SMP MEM=$MEM SECS=${JVM_SECS:-400} "$J/run-linux-vm.sh" Bench all > "$OUT/.jvm-linux.raw" 2>&1 || true
    LOG="$OUT/.jvm-linux.raw"
fi
grep -a "^RESULT " "$LOG" 2>/dev/null | tr -d '\r' > "$OUT/jvm-$STAND.txt" || true
n=$(wc -l < "$OUT/jvm-$STAND.txt")
echo "    metrics collected: $n"
[ "$n" -gt 0 ] || { echo "    no results, the tail of the log:"; tail -6 "$LOG" 2>/dev/null | sed 's/^/      /'; exit 1; }

# The summary appears once both sides are there.
if [ -s "$OUT/jvm-talaria.txt" ] && [ -s "$OUT/jvm-linux.txt" ]; then
    python3 "$SUITE/lib/scalars.py" "$OUT/jvm-talaria.txt" "$OUT/jvm-linux.txt" | tail -30
fi
