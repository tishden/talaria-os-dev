#!/bin/sh
# qgdb.sh — the same as qrun.sh, but the machine is ready for LIVE debugging:
#   * -M pc: microvm has no COM2, and the gdb stub speaks exactly through it;
#   * COM2 -> the host's 127.0.0.1:1234 (gdb connects there);
#   * the disk is NVMe: on -M pc Talaria's virtio is mmio only, while NVMe is there (EC2).
# Cores come from SMP= (default 4): stopping in the debugger parks every core
# (all-stop), and hardware breakpoints are armed on all of them.
#   ./qgdb.sh [seconds]              then in the console: gdb break
#   GDBARGS=talaria.gdb=break ./qgdb.sh   stop before the first instruction
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
SECS=${1:-600}
IMG=${IMG:-disk.img}
SMP=${SMP:-4}
GDBARGS=${GDBARGS:-}
docker run --rm -i -p 127.0.0.1:1234:1234 --device /dev/kvm -v "$REPO":/w -v "$HERE":/e talaria-qemu2 sh -c '
  timeout '"$SECS"' qemu-system-x86_64 -M pc -enable-kvm -cpu host -smp '"$SMP"' -m 1024 \
    -kernel /w/'"$K"' -append "console=ttyS0 talaria.mode=cell talaria.pci=1 talaria.smp='"$SMP"' '"$GDBARGS"'" \
    -display none -serial stdio -serial tcp:0.0.0.0:1234,server,nowait -no-reboot \
    -drive id=d0,file=/e/'"$IMG"',format=raw,if=none -device nvme,drive=d0,serial=tal0
  true'
