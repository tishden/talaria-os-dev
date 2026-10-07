#!/bin/sh
# Run ClickHouse on Talaria — on the normal kernel. The ClickHouse binary is not relocatable and
# occupies 2 MB..526 MB, where the kernel's image is LOADED; since the image runs at its high alias
# and a strict process space keeps none of its identity (docs/ARM64-PLAN.md §10.16 stage 7), the
# binary takes those addresses. The high-base build it needed before is gone.
#   run.sh [secs] [extra talaria.*]     env: SMP=N MEM=MB KERNEL=path
set -u
# THE LOG TAIL IS A PARAMETER, NOT A CONSTANT. The gates (`kernel/validate.sh`)
# look for evidence in THIS output, and the `stat:` diagnostic block grew to
# fifty lines and pushed the results and the exit code out of the tail: a
# healthy workload looked like a failure. A human needs the tail, a gate needs
# the whole log — hence TAIL=.
TAIL=${TAIL:-40}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
K=${KERNEL:-kernel/target/x86_64-talaria/release/talaria}
SECS=${1:-180}; EXTRA=${2:-}
# Large pages are requested by THE IMAGE'S MANIFEST itself (`huge=2`, see mkdisk.sh):
# a property of the workload, not of the stand, so the command line has no business with it.
HUGE=${HUGE:-}
SMP=${SMP:-2}; MEM=${MEM:-3072}
LOG=services/clickhouse/build/serial.log
: > "$REPO/$LOG"
docker run --rm -i --device /dev/kvm -v "$REPO":/w talaria-qemu2 bash -s "$K" "$SECS" "$EXTRA" "$SMP" "$MEM" "$LOG" "$HUGE" <<'INNER'
set -u; K=$1; SECS=$2; EXTRA=$3; SMP=$4; MEM=$5; LOG=$6; HUGE=$7
mkdir -p /w/services/clickhouse/build
timeout $((SECS + 20)) qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host \
  -overcommit cpu-pm=on -smp "$SMP" -m "$MEM" \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=proc talaria.smp=$SMP $EXTRA $HUGE" \
  -display none -serial "file:/w/$LOG" -no-reboot \
  -drive id=d0,file=/w/services/clickhouse/disk-ch.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
QP=$!
for i in $(seq 1 "$SECS"); do
  sleep 1
  grep -qa "the process has finished\|exit_group\|CRASH DUMP\|all the .* task" "/w/$LOG" && { echo "the workload finished in $i s"; sleep 4; break; }
done
kill $QP 2>/dev/null; wait $QP 2>/dev/null
INNER
echo "--- serial (tail) ---"; tail -"$TAIL" "$REPO/$LOG"
