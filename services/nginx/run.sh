#!/bin/sh
# Run nginx on Talaria (tap network 10.0.0.2:8080) + an HTTP probe from the host.
#   run.sh [secs] [extra talaria.*]      env: SMP=N MEM=MB
set -u
# THE LOG TAIL IS A PARAMETER, NOT A CONSTANT. The gates (`kernel/validate.sh`)
# look for evidence in THIS output, and the `stat:` diagnostic block grew to
# fifty lines and pushed the results and the exit code out of the tail: a
# healthy workload looked like a failure. A human needs the tail, a gate needs
# the whole log — hence TAIL=.
TAIL=${TAIL:-30}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
K=${KERNEL:-kernel/target/x86_64-talaria/release/talaria}
SECS=${1:-30}; EXTRA=${2:-}; SMP=${SMP:-1}; MEM=${MEM:-512}
mkdir -p "$REPO/services/nginx/build"
docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -v "$REPO":/w talaria-qemu2 bash -s "$K" "$SECS" "$EXTRA" "$SMP" "$MEM" <<'INNER'
set -u; K=$1; SECS=$2; EXTRA=$3; SMP=$4; MEM=$5
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp "$SMP" -m "$MEM" \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=proc talaria.smp=$SMP $EXTRA" \
  -display none -serial file:/w/services/nginx/build/serial.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/w/services/nginx/disk-nginx.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
QP=$!
for i in $(seq 1 "$SECS"); do
  sleep 1
  if timeout 3 bash -c 'exec 3<>/dev/tcp/10.0.0.2/8080' 2>/dev/null; then echo "nginx listens after $i s"; break; fi
done
echo "--- GET /nop ---"
( printf 'GET /nop HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n'; sleep 1 ) \
  | timeout 5 bash -c 'cat >&3; cat <&3' 3<>/dev/tcp/10.0.0.2/8080 2>/dev/null | head -8 | tr -d '\r'
echo "--- GET / ---"
( printf 'GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n'; sleep 1 ) \
  | timeout 5 bash -c 'cat >&3; cat <&3' 3<>/dev/tcp/10.0.0.2/8080 2>/dev/null | head -12 | tr -d '\r'
kill $QP 2>/dev/null; wait $QP 2>/dev/null
INNER
echo "--- serial (tail) ---"; tail -"$TAIL" "$REPO/services/nginx/build/serial.log"
