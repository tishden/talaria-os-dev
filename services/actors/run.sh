#!/bin/sh
# Run actor-bench on Talaria: QEMU -smp 4 (BSP + 3 APs), cpu-pm=on (mwait in the
# guest), talaria.mode=proc + talaria.smp=4. Result: build/results-talaria.txt.
# usage: run.sh [timeout_secs] [extra-append]   (mkdisk.sh prepares the disk)
set -u
D=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$D/../.." && pwd)
K=${K:-kernel/target/x86_64-talaria/release/talaria}   # path RELATIVE to the repository (a kernel under test: K=services/actors/build/talaria-exp)
SECS=${1:-360}; EXTRA=${2:-}
[ -f "$D/disk-actors.img" ] || { echo "no disk-actors.img — run mkdisk.sh first"; exit 1; }

docker run --rm -i --device /dev/kvm -v "$REPO":/w talaria-qemu2 bash -s "$K" "$SECS" "$EXTRA" <<'INNER'
set -u; K=$1; SECS=$2; EXTRA=$3
mkdir -p /w/services/actors/build   # without the directory QEMU cannot open the serial file and exits silently
rm -f /w/services/actors/build/serial-talaria.log
taskset -c 4-7 qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 4 -overcommit cpu-pm=on -m 1024 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=proc talaria.smp=4 $EXTRA" \
  -display none -serial file:/w/services/actors/build/serial-talaria.log -no-reboot \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  -drive id=d0,file=/w/services/actors/disk-actors.img,format=raw,if=none -device virtio-blk-device,drive=d0 \
  >/tmp/q.log 2>&1 &
QP=$!
# vCPU threads -> their own physical cores (0-3; HT pairs 0-4..3-7): otherwise with
# cpu-pm=on an idle guest looks 100 % busy to the host and competes with desktop noise (p50 -> 10+ µs)
sleep 2
for t in /proc/$QP/task/*; do
  c=$(cat "$t/comm" 2>/dev/null)
  case "$c" in "CPU "*) n=${c#CPU }; n=${n%%/*}; taskset -pc "$n" "${t##*/}" >/dev/null 2>&1 ;; esac
done
for i in $(seq 1 "$SECS"); do
  sleep 1
  grep -aq "BENCH DONE" /w/services/actors/build/serial-talaria.log 2>/dev/null && { echo "benchmark finished in $i s"; break; }
done
kill $QP 2>/dev/null; wait $QP 2>/dev/null
INNER
grep -a "MACH\|RESULT\|SKIP" "$D/build/serial-talaria.log" | tee "$D/build/results-talaria.txt"
