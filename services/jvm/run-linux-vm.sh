#!/bin/sh
# The JVM in a Linux GUEST: the same QEMU/KVM as Talaria's, the same JDK, the same
# Bench.class, the same number of cores and memory.
#
# Why separate from run-linux.sh: that one runs Linux NATIVELY (docker with a cpuset)
# while Talaria runs under a hypervisor. For latency that is tolerable (Talaria wins
# DESPITE the handicap), but not for compute: a TLB miss under a hypervisor walks
# the nested tables, and the gap in sieve/matmul (~20 %) is explained by the stand,
# not by the system. The suite caught the same mistake on ClickHouse, where it cost
# four test cases (see services/clickhouse/linux-init.c).
#
#   run-linux-vm.sh [Bench arguments]   env: SMP=4 MEM=3072 SECS=400
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
# The main class + its argument — exactly as in the Talaria manifest
# (JVM_MAIN="Bench all" in mkdisk.sh). A bare "all" is taken by java as a CLASS
# NAME and fails with ClassNotFoundException.
ARGS=${*:-Bench all}
SMP=${SMP:-4}; MEM=${MEM:-3072}; SECS=${SECS:-400}
JDK=$(ls -d "$HERE"/build/jdk-* 2>/dev/null | grep -v tar.gz | head -1)
[ -n "$JDK" ] || { echo "no JDK in $HERE/build (see README)"; exit 1; }
[ -f "$HERE/build/app/Bench.class" ] || { echo "no build/app/Bench.class — build-bench.sh"; exit 1; }

TREE=$(mktemp -d); chmod 755 "$TREE"
mkdir -p "$TREE/opt" "$TREE/app" "$TREE/tmp" "$TREE/lib" "$TREE/etc"
cp -a "$JDK" "$TREE/opt/jdk"
cp "$HERE/build/app/Bench.class" "$TREE/app/"

# THE MUSL LOADER. The JDK build is alpine/musl, i.e. a dynamic ELF with the
# interpreter /lib/ld-musl-x86_64.so.1. Without it the kernel answers execve with
# "No such file or directory" — about the INTERPRETER, not about java itself, and
# from the outside it looks like "the binary is missing". The loader is THE SAME one
# the Talaria image gets (services/jvm/mkdisk.sh), otherwise two runtimes would be compared.
MUSL="$REPO/kernel/user/dyn/ld-musl-x86_64.so.1"
if [ "${TAL_MUSL:-1}" = 1 ] && [ -f "$MUSL" ]; then
    cp "$MUSL" "$TREE/lib/ld-musl-x86_64.so.1"
else
    docker run --rm alpine:3.20 cat /lib/ld-musl-x86_64.so.1 > "$TREE/lib/ld-musl-x86_64.so.1"
fi
cp "$TREE/lib/ld-musl-x86_64.so.1" "$TREE/lib/libc.musl-x86_64.so.1"
printf 'root:x:0:0:root:/:/bin/sh\njava:x:1000:1000:java:/:/bin/sh\n' > "$TREE/etc/passwd"
printf 'root:x:0:\njava:x:1000:\n' > "$TREE/etc/group"
printf '127.0.0.1 localhost\n' > "$TREE/etc/hosts"

# The flags are THE SAME ones the Talaria manifest gets (mkdisk.sh): otherwise two
# different JVMs would be compared, not two systems.
JVM_ARGS=${JVM_ARGS:-"-XX:+UseSerialGC -Xmx256m -XX:-UsePerfData"}
{
    echo "tune=lowlat"
    echo "exec=/opt/jdk/bin/java"
    echo "arg=java"
    for a in $JVM_ARGS; do echo "arg=$a"; done
    echo "arg=-cp"; echo "arg=/app"
    for a in $ARGS; do echo "arg=$a"; done
    echo "env=HOME=/"
    echo "env=PATH=/opt/jdk/bin"
    echo "env=JAVA_HOME=/opt/jdk"
    echo "halt=1"
} > "$TREE/.spec"

IR="$HERE/build/initramfs-jvm.gz"
"$REPO/bench/suite/lib/mk-linux-guest.sh" "$TREE" "$TREE/.spec" "$IR" virtio_net virtio_pci virtio_mmio >/dev/null
rm -rf "$TREE"

LOG="$HERE/build/serial-linux-vm.log"
rm -f "$LOG"
docker run --rm --device /dev/kvm -v "$REPO":/w talaria-qemu2 \
  timeout "$SECS" qemu-system-x86_64 -M pc -enable-kvm -cpu host \
    -overcommit cpu-pm=on -smp "$SMP" -m "$MEM" \
    -kernel /w/bench/linux-guest/vmlinuz -initrd "/w/${IR#$REPO/}" \
    -append "console=ttyS0 quiet" -display none -serial "file:/w/${LOG#$REPO/}" -no-reboot \
    >/dev/null 2>&1 || true
grep -a "^RESULT " "$LOG" 2>/dev/null | tr -d '\r' || echo "no results (see $LOG)"
