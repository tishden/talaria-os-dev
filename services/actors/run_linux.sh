#!/bin/sh
# Run THE SAME actor-bench in a Linux guest: the host's vmlinuz (bench/linux-guest)
# + an initramfs {init, actor-bench}, QEMU -M pc -smp 4 — the same VM discipline as
# Talaria's (run.sh). Result: build/results-linux.txt.
# usage: run_linux.sh [timeout_secs] ["actor-bench args"]   (default all)
set -eu
D=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$D/../.." && pwd)
SECS=${1:-360}; ARGS=${2:-all}
LK=$REPO/bench/linux-guest/vmlinuz
[ -f "$LK" ] || { echo "no $LK — bench/linux-guest/mk_guest.sh"; exit 1; }
[ -f "$D/build/actor-bench" ] || { echo "no build/actor-bench — run build.sh first"; exit 1; }

# init (static musl) + initramfs
docker run --rm -v "$REPO":/w talaria-alpine-build sh -c \
  'gcc -O2 -static -o /w/services/actors/build/linux-init /w/services/actors/linux-init.c'
IR=$(mktemp -d)
cp "$D/build/linux-init" "$IR/init"; chmod +x "$IR/init"
cp "$D/build/actor-bench" "$IR/actor-bench"
mkdir -p "$IR/proc" "$IR/dev"
(cd "$IR" && find . | cpio -o -H newc --quiet | gzip -1) > "$D/build/initramfs-actors.gz"
rm -rf "$IR"

docker run --rm -i --device /dev/kvm -v "$REPO":/w talaria-qemu2 bash -s "$SECS" "$ARGS" <<'INNER'
set -u; SECS=$1; ARGS=$2
rm -f /w/services/actors/build/serial-linux.log
taskset -c 4-7 qemu-system-x86_64 -M pc -enable-kvm -cpu host -smp 4 -m 1024 \
  -kernel /w/bench/linux-guest/vmlinuz -initrd /w/services/actors/build/initramfs-actors.gz \
  -append "console=ttyS0 quiet args=$ARGS" \
  -display none -serial file:/w/services/actors/build/serial-linux.log -no-reboot \
  >/tmp/q.log 2>&1 &
QP=$!
# vCPU threads -> their own physical cores 0-3 (the same discipline as run.sh)
sleep 2
for t in /proc/$QP/task/*; do
  c=$(cat "$t/comm" 2>/dev/null)
  case "$c" in "CPU "*) n=${c#CPU }; n=${n%%/*}; taskset -pc "$n" "${t##*/}" >/dev/null 2>&1 ;; esac
done
for i in $(seq 1 "$SECS"); do
  sleep 1
  grep -aq "BENCH DONE\|bench exited" /w/services/actors/build/serial-linux.log 2>/dev/null && { echo "benchmark finished in $i s"; break; }
  kill -0 $QP 2>/dev/null || break
done
kill $QP 2>/dev/null; wait $QP 2>/dev/null
INNER
grep -a "MACH\|RESULT\|SKIP" "$D/build/serial-linux.log" | tee "$D/build/results-linux.txt"
