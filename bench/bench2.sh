#!/bin/sh
# v0.2 matrix: talaria vs Linux guest on the SAME QEMU/tap transport,
# QEMU userspace vs vhost-net datapath, vCPU thread pinned+FIFO.
#   docker run --rm --privileged -v <repo>:/w talaria-qemu2 sh /w/bench/bench2.sh
# CPU plan (i7-3820, HT pairs 0/4 1/5 2/6 3/7): vcpu=2, qemu misc=1, client=3.
set -e
KERNEL=/w/kernel/target/x86_64-unknown-none/release/talaria
LGK=/w/bench/linux-guest/vmlinuz
LGI=/w/bench/linux-guest/initramfs.gz
PING=/w/bench/udp_ping
ECHO=/w/bench/udp_echo
N=${N:-20000}
WARM=${WARM:-2000}
# CPU plan overridable via env (defaults: local i7-3820, HT pairs n/n+4)
VCPU_CPU=${VCPU_CPU:-2}
VHOST_CPU=${VHOST_CPU:-6} # HT sibling of the vCPU core: shared L1/L2
MISC_CPUS=${MISC_CPUS:-0,1,4,5}
CLI_CPU=${CLI_CPU:-3}
SRV_CPU=${SRV_CPU:-$VCPU_CPU}

# Busy-poll SCHED_FIFO tasks hit RT throttling (95%/s -> 50ms holes);
# disable it for the run (global sysctl, needs --privileged).
echo -1 > /proc/sys/kernel/sched_rt_runtime_us 2>/dev/null \
    && echo "rt throttling: off" || echo "rt throttling: could not disable"

ip tuntap add tap0 mode tap
ip addr add 10.0.0.1/24 dev tap0
ip link set tap0 up

pin_qemu() { # $1 = qemu pid; needs --pid=host to see the vhost kthread
    sleep 1.5
    for t in /proc/$1/task/*; do
        tid=${t##*/}
        case "$(cat $t/comm)" in
        CPU*KVM*)
            taskset -pc $VCPU_CPU $tid >/dev/null
            chrt -f -p 55 $tid || true
            ;;
        *)
            taskset -pc $MISC_CPUS $tid >/dev/null
            ;;
        esac
    done
    # vhost worker is a host kernel thread named vhost-<qemu pid>
    for tid in $(grep -l "^vhost-$1$" /proc/[0-9]*/comm 2>/dev/null | cut -d/ -f3); do
        case "$tid" in
        *[!0-9]*) continue ;;
        esac
        taskset -pc $VHOST_CPU $tid >/dev/null
        chrt -f -p 50 $tid || true
        echo "pinned vhost tid $tid -> cpu $VHOST_CPU"
    done
}

run_talaria() { # $1 = extra netdev opts, $2 = label, $3 = extra kernel args
    ip neigh replace 10.0.0.2 lladdr 52:54:00:12:34:56 dev tap0
    qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
        -kernel "$KERNEL" -append "console=ttyS0 $3" \
        -display none -serial file:/tmp/serial-$2.log -no-reboot \
        -netdev tap,id=n0,ifname=tap0,script=no,downscript=no$1 \
        -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 &
    QPID=$!
    pin_qemu $QPID
    sleep 2
    echo "### talaria guest [$2]"
    $PING 10.0.0.2 7777 "$N" "$WARM" "$CLI_CPU" 64
    sleep 6
    kill $QPID 2>/dev/null || true
    wait $QPID 2>/dev/null || true
    grep "stats" /tmp/serial-$2.log | tail -2
    echo
}

run_linux() { # $1 = extra netdev opts, $2 = label
    ip neigh replace 10.0.0.2 lladdr 52:54:00:12:34:57 dev tap0
    qemu-system-x86_64 -M pc -enable-kvm -cpu host -smp 1 -m 512 \
        -kernel "$LGK" -initrd "$LGI" \
        -append "console=ttyS0 rdinit=/init quiet 8250.nr_uarts=1" \
        -display none -serial file:/tmp/serial-$2.log -no-reboot \
        -netdev tap,id=n0,ifname=tap0,script=no,downscript=no$1 \
        -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:57,ioeventfd=on &
    QPID=$!
    sleep 8 # kernel boot + module load
    pin_qemu $QPID
    sleep 1
    echo "### linux guest (host 5.14 kernel, busy-poll echo) [$2]"
    $PING 10.0.0.2 7777 "$N" "$WARM" "$CLI_CPU" 64
    kill $QPID 2>/dev/null || true
    wait $QPID 2>/dev/null || true
    echo
}

run_talaria "" "qemu-user"
run_talaria ",vhost=on" "vhost"
run_talaria ",vhost=on" "l1-smoltcp-vhost" "talaria.mode=smoltcp"
run_linux "" "qemu-user"
run_linux ",vhost=on" "vhost"

echo "### reference: Linux loopback (busy-poll echo)"
$ECHO 7777 $SRV_CPU &
EPID=$!
sleep 0.5
$PING 127.0.0.1 7777 "$N" "$WARM" "$CLI_CPU" 64
kill $EPID 2>/dev/null || true
echo
echo "### reference: veth across netns (busy-poll echo)"
ip netns add peer
ip link add veth0 type veth peer name veth1
ip link set veth1 netns peer
ip addr add 10.1.0.1/24 dev veth0
ip link set veth0 up
ip netns exec peer ip addr add 10.1.0.2/24 dev veth1
ip netns exec peer ip link set veth1 up
ip netns exec peer $ECHO 7777 $SRV_CPU &
E2PID=$!
sleep 0.5
$PING 10.1.0.2 7777 "$N" "$WARM" "$CLI_CPU" 64
kill $E2PID 2>/dev/null || true
echo DONE
