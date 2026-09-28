#!/bin/sh
# Run busybox (the awk applet) on Talaria.
set -u
REPO=$(cd "$(dirname "$0")/../.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
docker run --rm -i --device /dev/kvm -v "$REPO":/w talaria-qemu2 sh -s "$K" <<'INNER'
set -u; K=$1
timeout 15 qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=busybox" -display none -serial file:/tmp/o.log -no-reboot \
  -drive id=d0,file=/w/services/busybox/disk-busybox.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1
cat /tmp/o.log
INNER
