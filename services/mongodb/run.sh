#!/bin/sh
# Run MongoDB on Talaria (tap network 10.0.0.2:27017) + a wire-protocol (OP_MSG) probe from the qemu container.
# run.sh [secs] [append]; WAIT=N waits N s after the probe (e.g. WAIT=40 ./run.sh 100 "talaria.stop=30"
# — a clean mongod shutdown on SIGTERM, the next start needs no WiredTiger recovery). Serial log -> build/serial.log
set -u
# THE LOG TAIL IS A PARAMETER, NOT A CONSTANT. The gates (`kernel/validate.sh`)
# look for evidence in THIS output, and the `stat:` diagnostic block grew to
# fifty lines and pushed the results and the exit code out of the tail: a
# healthy workload looked like a failure. A human needs the tail, a gate needs
# the whole log — hence TAIL=.
TAIL=${TAIL:-30}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
SECS=${1:-60}; EXTRA=${2:-}
docker run --rm -i -e WAIT="${WAIT:-}" --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -v "$REPO":/w talaria-qemu2 bash -s "$K" "$SECS" "$EXTRA" <<'INNER'
set -u; K=$1; SECS=$2; EXTRA=$3
mkdir -p /w/services/mongodb/build   # without the directory QEMU cannot open the serial file and exits silently
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 1024 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=proc $EXTRA" -display none -serial file:/w/services/mongodb/build/serial.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/w/services/mongodb/disk-mongo.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
QP=$!
CLI="python3 /w/services/mongodb/mongoprobe.py"
for i in $(seq 1 $SECS); do
  sleep 1
  if $CLI 10.0.0.2 27017 ping >/dev/null 2>&1; then echo "mongod: the server answers after $i s"; break; fi
done
$CLI 10.0.0.2 27017 demo 2>&1 | head -30
[ -n "${WAIT:-}" ] && { echo "waiting $WAIT s (talaria.stop / clean shutdown)"; sleep "$WAIT"; }
kill $QP 2>/dev/null; wait $QP 2>/dev/null
INNER
echo "--- serial (tail) ---"; tail -"$TAIL" "$REPO/services/mongodb/build/serial.log" | cut -c1-200
