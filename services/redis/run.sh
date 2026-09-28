#!/bin/sh
# Run Redis 7.2 on Talaria with a tap network + a RESP probe.
set -u
REPO=$(cd "$(dirname "$0")/../.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -v "$REPO":/w talaria-qemu2 bash -s "$K" <<'INNER'
set -u; K=$1
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=redis" -display none -serial file:/tmp/o.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/w/services/redis/disk-redis.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
QP=$!; sleep 11
( printf 'PING\r\nSET talaria "on bare metal"\r\nGET talaria\r\nINCR hits\r\nHSET user name alice\r\nHGETALL user\r\nLPUSH q a b c\r\nLRANGE q 0 -1\r\nDBSIZE\r\n'; sleep 2 ) \
  | timeout 4 bash -c 'cat >&3; cat <&3' 3<>/dev/tcp/10.0.0.2/6379 | tr -d '\0' | head -40
kill $QP 2>/dev/null
INNER
