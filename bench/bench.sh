#!/bin/sh
# Runs inside the talaria-qemu2 container:
#   docker run --rm --device /dev/kvm --device /dev/net/tun \
#     --cap-add NET_ADMIN --cap-add SYS_NICE -v <repo>:/w talaria-qemu2 sh /w/bench/bench.sh
# Matrix: talaria guest (KVM+virtio+tap) vs Linux kernel-stack baselines.
set -e
KERNEL=/w/kernel/target/x86_64-unknown-none/release/talaria
PING=/w/bench/udp_ping
ECHO=/w/bench/udp_echo
N=${N:-20000}
WARM=${WARM:-2000}
QEMU_CPUS=${QEMU_CPUS:-1,2}
SRV_CPU=${SRV_CPU:-2}
CLI_CPU=${CLI_CPU:-3}

echo "### talaria guest: QEMU microvm + KVM + virtio-mmio + tap"
ip tuntap add tap0 mode tap
ip addr add 10.0.0.1/24 dev tap0
ip link set tap0 up
ip neigh replace 10.0.0.2 lladdr 52:54:00:12:34:56 dev tap0

taskset -c "$QEMU_CPUS" qemu-system-x86_64 \
  -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
  -kernel "$KERNEL" -append console=ttyS0 \
  -display none -serial file:/tmp/serial.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 &
QPID=$!
sleep 3
$PING 10.0.0.2 7777 "$N" "$WARM" "$CLI_CPU" 64
echo "--- guest-side telemetry (last stats lines) ---"
sleep 6
kill $QPID 2>/dev/null || true
wait $QPID 2>/dev/null || true
grep "stats:" /tmp/serial.log | tail -4

echo
echo "### baseline A: Linux kernel stack, loopback (busy-poll echo)"
$ECHO 7777 "$SRV_CPU" &
EPID=$!
sleep 0.5
$PING 127.0.0.1 7777 "$N" "$WARM" "$CLI_CPU" 64
kill $EPID 2>/dev/null || true

echo
echo "### baseline B: Linux kernel stack, veth pair across netns (busy-poll echo)"
ip netns add peer
ip link add veth0 type veth peer name veth1
ip link set veth1 netns peer
ip addr add 10.1.0.1/24 dev veth0
ip link set veth0 up
ip netns exec peer ip addr add 10.1.0.2/24 dev veth1
ip netns exec peer ip link set veth1 up
ip netns exec peer ip link set lo up
ip netns exec peer $ECHO 7777 "$SRV_CPU" &
E2PID=$!
sleep 0.5
$PING 10.1.0.2 7777 "$N" "$WARM" "$CLI_CPU" 64
kill $E2PID 2>/dev/null || true
echo DONE
