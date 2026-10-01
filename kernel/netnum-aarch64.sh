#!/bin/bash
# The network numbers of the arm64 stand (docs/ARM64-PLAN.md §10.3 step 1: "`tcp_bench`/
# `udp_ping` numbers on the stand"): the guest on a tap, the meters next to it. It runs
# INSIDE the stand image, as `kernel/test-aarch64-net.sh` does:
#   docker run --rm --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net --cap-add NET_ADMIN \
#     -v "$PWD:/w" -w /w -e TAL_ACCEL="-enable-kvm -cpu host" talaria-qemu-aarch64 bash kernel/netnum-aarch64.sh
# The meters are the arm64 builds of kernel/user/{udp_ping,tcp_bench}.c — static Linux
# binaries, so on an arm64 host they run as they are (under TCG on an x86 box there is
# nothing to run them with, and nothing to measure: the script says so and stops).
# What is printed: the UDP round trip against `userver` (the socket syscalls from EL0) in
# three configurations — polling, the card's line with the parking, the line without the
# parking — and the TCP request rate against `diskserver` (epoll + a file from the disk).
set -u
IMG=${TAL_IMG:-kernel/target/aarch64-talaria/release/talaria.img}
ACCEL=${TAL_ACCEL:--cpu max}
DISK=${TAL_DISK:-kernel/target/disk-a64.img}
N=${TAL_N:-20000}
U=/w/kernel/user/aarch64
[ "$(uname -m)" = aarch64 ] || { echo "netnum: the meters are arm64 binaries and this host is $(uname -m) — the numbers are the stand's"; exit 0; }
[ -x $U/udp_ping ] && [ -x $U/tcp_bench ] || { echo "netnum: no $U/udp_ping or tcp_bench (make -C kernel/user aarch64)"; exit 1; }
VHOST=""; [ -c /dev/vhost-net ] && case "$ACCEL" in *kvm*) VHOST=",vhost=on";; esac
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
O=/tmp/o.log
bootn() { # bootn SMP APPEND [DISK]
  rm -f $O; local drv=""
  [ -n "${3:-}" ] && { cp "/w/$3" /tmp/run.img; drv="-drive id=d0,file=/tmp/run.img,format=raw,if=none -device virtio-blk-device,drive=d0"; }
  qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp "$1" -m 1024 -display none -serial file:$O \
    -kernel "/w/$IMG" -append "console=ttyAMA0 $2 talaria.smp=$1" \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no$VHOST \
    -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 $drv >/tmp/q.log 2>&1 &
  Q=$!
}
waitfor() { local i; for i in $(seq 1 $(($2 * 2))); do grep -aqE "$1" $O 2>/dev/null && return 0; kill -0 $Q 2>/dev/null || return 1; sleep 0.5; done; return 1; }
stopq() { kill $Q 2>/dev/null; wait $Q 2>/dev/null; }
echo "== netnum: vhost '${VHOST:-off}', accel '$ACCEL', $N round trips =="
for cfg in "" "talaria.netirq=1" "talaria.netirq=1 talaria.netpark=0"; do
  bootn 1 "talaria.mode=userver $cfg"
  if waitfor "userver: the UDP echo" 60; then
    sleep 1
    $U/udp_ping 10.0.0.2 9998 200 >/dev/null 2>&1          # the ARP and the first-touch faults are not the number
    echo "udp [${cfg:-polling}]: $($U/udp_ping 10.0.0.2 9998 $N 2>&1 | tail -1)"
  else echo "udp [${cfg:-polling}]: the guest did not come up"; tail -3 $O; fi
  stopq
done
for cfg in "" "talaria.netirq=1"; do
  bootn 1 "talaria.mode=diskserver $cfg" "$DISK"
  if waitfor "the workload.s first syscall" 60; then
    sleep 2
    $U/tcp_bench 10.0.0.2 80 200 >/dev/null 2>&1
    echo "tcp [${cfg:-polling}]: $($U/tcp_bench 10.0.0.2 80 $N 2>&1 | tail -1)"
  else echo "tcp [${cfg:-polling}]: the guest did not come up"; tail -3 $O; fi
  stopq
done
