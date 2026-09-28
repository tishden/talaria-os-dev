#!/bin/sh
# Quick kernel run in QEMU (docker talaria-qemu2): runq.sh "<append>" <disk.img> [secs] [net] [mem]
# Serial -> stdout. For one-off runs of tests/modes with an arbitrary disk.
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
APP=$1; DISK=$2; SECS=${3:-8}; NET=${4:-}; MEM=${5:-512}
docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -v "$REPO":/w talaria-qemu2 sh -s "$K" "$APP" "$DISK" "$SECS" "$NET" "$MEM" <<'INNER'
set -u; K=$1; APP=$2; DISK=$3; SECS=$4; NET=$5; MEM=$6
NETOPT=""
if [ -n "$NET" ]; then
  ip tuntap add tap0 mode tap 2>/dev/null; ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
  NETOPT="-netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56"
fi
timeout "$SECS" qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m "$MEM" \
  -kernel "/w/$K" -append "console=ttyS0 $APP" -display none -serial stdio -no-reboot $NETOPT \
  -drive id=d0,file="/w/$DISK",format=raw,if=none -device virtio-blk-device,drive=d0 2>&1 | grep -v "^$"
INNER
