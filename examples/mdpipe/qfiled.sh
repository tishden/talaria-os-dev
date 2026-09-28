#!/bin/sh
# qfiled.sh — boot the image in filed mode and run talaria-file commands
# from the "host" 10.0.0.1 (it lives in the same container as tap0).
#   IMG=disk-crash.img ./qfiled.sh "ls /" "pull /core.mdpipe out/core.mdpipe"
# The token is TALARIA_TOKEN (s3cret by default); it is passed to the node in the cmdline.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
IMG=${IMG:-disk.img}
TOK=${TALARIA_TOKEN:-s3cret}
mkdir -p "$HERE/out"
CMDS=""
for c in "$@"; do CMDS="$CMDS
echo '\$ talaria-file 10.0.0.2 $c'; python3 /w/kernel/tools/talaria-file 10.0.0.2 $c; echo"; done
docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -e TALARIA_TOKEN="$TOK" -v "$REPO":/w -v "$HERE":/e talaria-qemu2 sh -c '
  ip tuntap add tap0 mode tap 2>/dev/null
  ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
    -kernel /w/'"$K"' -append "console=ttyS0 talaria.mode=filed talaria.filedtoken='"$TOK"'" \
    -display none -serial file:/e/filed-serial.log -no-reboot \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
    -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
    -drive id=d0,file=/e/'"$IMG"',format=raw,if=none -device virtio-blk-device,drive=d0 >/dev/null 2>&1 &
  # The "accepting files" line is the listener in the kernel, not the readiness of the
  # path: the first connect may outrun ARP. We wait until the port really answers.
  for i in $(seq 1 60); do
    python3 -c "import socket; socket.create_connection((\"10.0.0.2\", 7373), 1).close()" 2>/dev/null && break
    sleep 0.5
  done
  cd /e'"$CMDS"'
  kill %1 2>/dev/null; true'
