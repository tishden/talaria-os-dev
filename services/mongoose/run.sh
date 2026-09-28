#!/bin/sh
# Run the mongoose web server on Talaria with a tap network.
# From the repository root: services/mongoose/run.sh
set -u
REPO=$(cd "$(dirname "$0")/../.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
DISK=services/mongoose/disk-mongoose.img
[ -f "$REPO/$K" ] || { echo "no kernel — build kernel/ first"; exit 1; }
[ -f "$REPO/$DISK" ] || { echo "no disk — python3 mkdisk.py disk-mongoose.img"; exit 1; }
docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -v "$REPO":/w talaria-qemu2 bash -s "$K" "$DISK" <<'INNER'
set -u
K=$1; DISK=$2
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=mongoose" -display none -serial file:/tmp/o.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/w/$DISK,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
QP=$!; sleep 6
echo "=== a few HTTP requests ==="
for p in / /index.html /api/status "/hello?x=1"; do
  echo "--- GET $p ---"
  timeout 5 curl -sS -m 4 "http://10.0.0.2:80$p" 2>&1 | head -3
done
echo "=== keep-alive (2 requests on one connection) ==="
timeout 5 curl -sS -m 4 http://10.0.0.2:80/a http://10.0.0.2:80/b 2>&1 | grep -ci mongoose | sed 's/^/Mongoose responses: /'
echo "=== unknown syscalls (if any) ==="
grep -cE 'an unknown number|unknown syscall' /tmp/o.log | sed 's/^/lines: /'
grep -E 'an unknown number|unknown syscall' /tmp/o.log | sort -u | head
kill $QP 2>/dev/null
INNER
