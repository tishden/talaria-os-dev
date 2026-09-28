#!/bin/sh
# Bring the stand's VM up or down. Shared plumbing for all workloads of the suite.
#
# IMPORTANT: the QEMU container runs with --network host, so the tap lives in the
# HOST's network namespace and the load generator sees the guest directly
# (10.0.0.2). Without that the tap would stay inside the container and only a
# process started there could reach the guest.
#
# THE GENERATOR AND THE GUEST ON DIFFERENT HOST CORES (`VM_CPUSET=`, `GEN_CPUSET=`).
#
# A network waiter in Talaria SPINS through the stack, i.e. the guest occupies exactly `smp`
# host cores regardless of the rate, and the generator runs with `--spin` and occupies one
# more. On a working machine without a layout they fight each other and the
# desktop: a measurement (postgresql/select1, SMP=4, 8 host cores)
# gave p99 of 550-744 µs at low rates and 102 MS at 19.7 thousand req/s — i.e.
# the stand's own jitter was 5-600 times larger than the 100 µs under study, and locally
# NOTHING could be resolved.
#
#   VM_CPUSET=0-2 GEN_CPUSET=4-5 bench/suite/run-local.sh postgresql
#
# The layout cannot be derived automatically: only the person sitting at the host knows
# how many of its cores are free. Without the variables the behaviour is exactly as before.
#   vm_up_talaria  <image> <mode> <smp> <mem MB> <log> [extra cmdline]
#   vm_up_linux    <initramfs> <smp> <mem MB> <log> [extra cmdline]
#   vm_wait_port   <ip> <port> <seconds>
#   vm_down
set -u
: "${REPO:?REPO is not set}"
TAP=${TAP:-tap0}
GUEST_IP=${GUEST_IP:-10.0.0.2}
CONSOLE_SOCK=${CONSOLE_SOCK:-bench/suite/results/console.sock}
HOST_IP=${HOST_IP:-10.0.0.1}
VM_CID=""

# The tap is created by the CONTAINER with --network host: it has NET_ADMIN, and
# the interface appears in the host's network namespace. That is how the stand comes up
# without sudo on a working machine — only access to docker is needed.
# A run killed halfway leaves the VM alive, and tap0 stays busy: from the outside
# it looks like "QEMU does not start", while the cause is the previous run.
# So bringing the stand up begins with cleaning up after ourselves.
vm_kill_stale() {
    # ONLY BY OUR OWN LABEL. There used to be a filter by IMAGE here as well
    # (`ancestor=talaria-qemu2`), and that turned out to be dangerous: the kernel regression
    # (`kernel/test.sh`) runs from the same image, and a suite run started next to it
    # silently killed its container halfway. From the outside it looked like
    # "the regression fell apart": some gates did not run at all, and the host checks
    # failed on missing artifacts. The `tlbench=1` label is set by every run of
    # the suite, so there is no need for us to kill anything foreign.
    stale=$(docker ps -aq --filter label=tlbench=1 2>/dev/null | sort -u | tr "\n" " ")
    # IMPORTANT: `docker rm -f` without arguments returns an error, and with `set -e` in
    # the caller that silently kills the whole run — which is exactly what happened.
    case "$stale" in
        *[!\ ]*) echo "removing the VM of the previous run: $(echo $stale | wc -w)"
                 docker rm -f $stale >/dev/null 2>&1 || true ;;
    esac
    return 0
}

SUITE_TLGEN_PAT=${SUITE_TLGEN_PAT:-bench/suite/tlgen/tlgen}

# GENERATORS OF THE PREVIOUS RUN. tlgen runs with `--spin`, i.e. it occupies a core
# fully; a hung one (or one left behind by a killed run) does not merely hang but
# EATS A CORE and poisons every later measurement. It was caught the expensive way: an instance
# left from a run four hours earlier had been spinning all that time, and the numbers
# after it read as "the stand has become noisy".
vm_kill_stale_gen() {
    pids=$(pgrep -f "$SUITE_TLGEN_PAT" 2>/dev/null | tr "\n" " ")
    case "$pids" in
        *[!\ ]*) echo "removing generators of the previous run: $(echo $pids | wc -w)"
                 kill $pids 2>/dev/null || true
                 sleep 1
                 kill -9 $(pgrep -f "$SUITE_TLGEN_PAT" 2>/dev/null | tr "\n" " ") 2>/dev/null || true ;;
    esac
    return 0
}
vm_tap_up() {
    vm_kill_stale
    vm_kill_stale_gen
    docker run --rm --network host --cap-add NET_ADMIN --device /dev/net/tun \
        talaria-qemu2 sh -c "
            ip link show $TAP >/dev/null 2>&1 || ip tuntap add $TAP mode tap
            ip addr replace $HOST_IP/24 dev $TAP
            ip link set $TAP up" || return 1
    ip -br addr show "$TAP"
}

vm_tap_down() {
    docker run --rm --network host --cap-add NET_ADMIN --device /dev/net/tun \
        talaria-qemu2 sh -c "ip link del $TAP" >/dev/null 2>&1 || true
}

# THE DISK AS ON EC2. The local image lies in the host's tmpfs and writes gigabytes per
# second; a gp3 volume on EC2 gives 125 MB/s and 3000 IOPS. The write tail that is
# visible ONLY on EC2 (a one-second stall of a mongodb insert) does not reproduce
# locally until the disk is throttled to the same shelf. The knob:
#   DISK_THROTTLE=gp3      -> throttling.bps-total=125000000,iops-total=3000
#   DISK_THROTTLE="<throttling.* options, comma-separated>"  -> passed verbatim
# It is applied to BOTH guests identically — systems are compared, not media.
vm_drive_throttle() {
    case "${DISK_THROTTLE:-}" in
        "") ;;
        gp3) echo ",throttling.bps-total=125000000,throttling.iops-total=3000" ;;
        *)   echo ",${DISK_THROTTLE}" ;;
    esac
}

vm_up_talaria() {
    img=$1; mode=$2; smp=$3; mem=$4; log=$5; extra=${6:-}
    kern=${KERNEL:-kernel/target/x86_64-talaria/release/talaria}
    # The console is not a file but a unix socket with a journal: that way it can also be WRITTEN to.
    # That is exactly where Talaria's counters are taken from (kstat/perf/netstat), and the kernel
    # console works on top of the running workload (USER-GUIDE §9.1).
    rm -f "$REPO/$CONSOLE_SOCK"
    # The log file is created by the CONTAINER (it is root), so an old file is removed:
    # otherwise the second run fails with "permission denied", and QEMU without serial
    # exits silently and the stand looks like "the guest did not come up".
    rm -f "$log"
    VM_CID=$(docker run -d --label tlbench=1 --network host --device /dev/kvm \
        --device /dev/net/tun --device /dev/vhost-net --cap-add NET_ADMIN \
        ${VM_CPUSET:+--cpuset-cpus=$VM_CPUSET} \
        -v "$REPO":/w talaria-qemu2 \
        qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host \
          -overcommit cpu-pm=on -smp "$smp" -m "$mem" \
          -kernel "/w/$kern" \
          -append "console=ttyS0 talaria.mode=$mode talaria.smp=$smp talaria.ip=$GUEST_IP/24 talaria.gw=$HOST_IP $extra" \
          -display none -no-reboot \
          -chardev "socket,id=c0,path=/w/$CONSOLE_SOCK,server=on,wait=off,logfile=/w/$log" \
          -serial chardev:c0 \
          -netdev "tap,id=n0,ifname=$TAP,script=no,downscript=no,vhost=on" \
          -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
          -drive "id=d0,file=/w/$img,format=raw,if=none$(vm_drive_throttle)" -device virtio-blk-device,drive=d0)
    echo "$VM_CID" > /tmp/tlbench-vm.cid
    vm_check_started || return 1
    # The console socket is created by QEMU as root (srwxr-xr-x), while connecting to a unix
    # socket requires WRITE PERMISSION. We open it once at bring-up so that the
    # counters can then be read straight from the host without starting a container for every
    # snapshot: an extra docker run between measurements is noise in the measurement itself.
    docker run --rm -v "$REPO":/w talaria-qemu2 chmod 666 "/w/$CONSOLE_SOCK" >/dev/null 2>&1 || true
}

# A DISK IS MANDATORY FOR THE LINUX GUEST WHEREVER THE WORKLOAD WRITES.
#
# The guest boots from an initramfs, i.e. its root is TMPFS, MEMORY. While that
# remained so, the database on Linux wrote its journal into RAM while on Talaria it wrote
# to a real image on the host's disk, and MEDIA were compared rather than systems:
# the mongodb insert tail of 77-186 ms against 0.24 on Linux was explained by exactly that
# (a device barrier costs 46 ms on this disk, and there are 422 such barriers per run).
# The sixth argument is the image for the data directory; `mount=` in the spec mounts it.
vm_up_linux() {
    initramfs=$1; smp=$2; mem=$3; log=$4; extra=${5:-}; disk=${6:-}
    rm -f "$log"
    dsk=""
    [ -z "$disk" ] || dsk="-drive id=d0,file=/w/$disk,format=raw,if=none$(vm_drive_throttle) -device virtio-blk-pci,drive=d0"
    VM_CID=$(docker run -d --label tlbench=1 --network host --device /dev/kvm \
        --device /dev/net/tun --device /dev/vhost-net --cap-add NET_ADMIN \
        ${VM_CPUSET:+--cpuset-cpus=$VM_CPUSET} \
        -v "$REPO":/w talaria-qemu2 \
        qemu-system-x86_64 -M pc -enable-kvm -cpu host \
          -overcommit cpu-pm=on -smp "$smp" -m "$mem" \
          -kernel "/w/bench/linux-guest/vmlinuz" -initrd "/w/$initramfs" \
          -append "console=ttyS0 quiet $extra" \
          -display none -serial "file:/w/$log" -no-reboot \
          $dsk \
          -netdev "tap,id=n0,ifname=$TAP,script=no,downscript=no,vhost=on" \
          -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56)
    echo "$VM_CID" > /tmp/tlbench-vm.cid
    vm_check_started
}

# QEMU that could not open the tap or serial exits IMMEDIATELY and silently: from the outside
# that is indistinguishable from "the guest is booting slowly", and the minute of waiting for the port is then
# explained by anything except a busy tap0. So we check explicitly.
vm_check_started() {
    sleep 1
    if [ -z "$(docker ps -q --filter id=$VM_CID)" ]; then
        echo "QEMU did not start:"
        docker logs "$VM_CID" 2>&1 | head -5 | sed 's/^/  /'
        docker rm -f "$VM_CID" >/dev/null 2>&1
        VM_CID=""
        return 1
    fi
    return 0
}

vm_wait_port() {
    ip=$1; port=$2; secs=$3
    i=0
    while [ "$i" -lt "$secs" ]; do
        if timeout 1 bash -c "exec 3<>/dev/tcp/$ip/$port" 2>/dev/null; then
            echo "port $ip:$port opened after $i s"; return 0
        fi
        i=$((i + 1)); sleep 1
    done
    echo "port $ip:$port did not open within $secs s"; return 1
}

vm_down() {
    cid=${VM_CID:-$(cat /tmp/tlbench-vm.cid 2>/dev/null || true)}
    [ -n "$cid" ] && docker rm -f "$cid" >/dev/null 2>&1
    rm -f /tmp/tlbench-vm.cid
    VM_CID=""
}
