#!/bin/sh
# Run the JVM on Talaria: QEMU microvm + disk-jvm.img, all output goes to serial.
#   run.sh [secs] [extra talaria.*]      env: SMP=N MEM=MB
# Example: SMP=4 MEM=3072 ./run.sh 300 "talaria.strace=400"
set -u
# THE LOG TAIL IS A PARAMETER, NOT A CONSTANT. The gates (`kernel/validate.sh`)
# look for evidence in THIS output, and the `stat:` diagnostic block grew to
# fifty lines and pushed the results and the exit code out of the tail: a
# healthy workload looked like a failure. A human needs the tail, a gate needs
# the whole log — hence TAIL=.
TAIL=${TAIL:-60}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
K=${KERNEL:-kernel/target/x86_64-talaria/release/talaria}
SECS=${1:-120}; EXTRA=${2:-}
SMP=${SMP:-4}; MEM=${MEM:-3072}
LOG=services/jvm/build/serial.log
: > "$REPO/$LOG"
docker run --rm -i --device /dev/kvm -v "$REPO":/w talaria-qemu2 bash -s "$K" "$SECS" "$EXTRA" "$SMP" "$MEM" "$LOG" <<'INNER'
set -u; K=$1; SECS=$2; EXTRA=$3; SMP=$4; MEM=$5; LOG=$6
mkdir -p /w/services/jvm/build
timeout $((SECS + 15)) qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host \
  -overcommit cpu-pm=on -smp "$SMP" -m "$MEM" \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=proc talaria.smp=$SMP $EXTRA" \
  -display none -serial "file:/w/$LOG" -no-reboot \
  -drive id=d0,file=/w/services/jvm/disk-jvm.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
QP=$!
# wait for the workload to finish, or for the timeout
for i in $(seq 1 "$SECS"); do
  sleep 1
  grep -qa "BENCH DONE\|the process has finished\|CRASH DUMP" "/w/$LOG" && { echo "the workload finished in $i s"; sleep 5; break; }
done
kill $QP 2>/dev/null; wait $QP 2>/dev/null
INNER
echo "--- serial (tail) ---"; tail -"$TAIL" "$REPO/$LOG"
