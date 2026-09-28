#!/bin/sh
# qrun.sh — boot the image in QEMU/KVM (docker talaria-qemu2). Serial goes to this
# terminal: it carries both the kernel log and the operator console (type commands right here).
#   ./qrun.sh "<cmdline tail>" [seconds] [cores]
#   IMG=disk-crash.img ./qrun.sh "talaria.mode=cell talaria.core=/core.mdpipe"
# Network: tap0 10.0.0.1 <-> node 10.0.0.2 (inside the container).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
APPEND=${1:-talaria.mode=cell}
SECS=${2:-60}
SMP=${3:-4}
IMG=${IMG:-disk.img}
[ -f "$REPO/$K" ] || { echo "no kernel $K — build it: cd kernel && cargo -Z json-target-spec build --release"; exit 1; }
[ -f "$HERE/$IMG" ] || { echo "no $IMG — run make"; exit 1; }
docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -v "$REPO":/w -v "$HERE":/e talaria-qemu2 sh -c '
  ip tuntap add tap0 mode tap 2>/dev/null
  ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
  timeout '"$SECS"' qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp '"$SMP"' -m 1024 \
    -kernel /w/'"$K"' -append "console=ttyS0 '"$APPEND"'" \
    -display none -serial stdio -no-reboot \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
    -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
    -drive id=d0,file=/e/'"$IMG"',format=raw,if=none -device virtio-blk-device,drive=d0
  true'
