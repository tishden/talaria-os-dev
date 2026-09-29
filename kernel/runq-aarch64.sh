#!/bin/sh
# Boot the arm64 kernel once in QEMU `virt` and print the console (the arm64 twin of
# runq.sh). On an x86 host this is TCG — slow, single-CPU semantics, a TSO machine
# underneath — good for "does it boot", useless for timing and for the memory-ordering
# audit (docs/ARM64-PLAN.md §1). KVM is used when /dev/kvm exists AND the host is arm64.
#   kernel/runq-aarch64.sh [seconds] [extra qemu args...]
#   TAL_APPEND="console=ttyAMA0 talaria.mode=cell" kernel/runq-aarch64.sh   # the command line
# The QEMU comes from the docker image talaria-qemu-aarch64 (services/docker/
# Dockerfile.qemu-aarch64) unless qemu-system-aarch64 is on the host's PATH.
set -u
SECS=${1:-20}; shift 2>/dev/null || true
APPEND=${TAL_APPEND:-console=ttyAMA0 talaria.mode=none}
REPO=$(cd "$(dirname "$0")/.." && pwd)
IMG=kernel/target/aarch64-talaria/release/talaria.img
[ -f "$REPO/$IMG" ] || { echo "no $IMG — run kernel/build-aarch64.sh first"; exit 1; }
ACCEL="-cpu max"
[ "$(uname -m)" = "aarch64" ] && [ -c /dev/kvm ] && ACCEL="-enable-kvm -cpu host"
QEMU="timeout $SECS qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp 1 -m 1024 -nographic \
  -kernel /w/$IMG -append \"$APPEND\" $*"
if command -v qemu-system-aarch64 >/dev/null 2>&1; then
  sh -c "$(echo "$QEMU" | sed "s|/w/|$REPO/|")"
else
  docker run --rm -v "$REPO:/w" -w /w talaria-qemu-aarch64 sh -c "$QEMU"
fi
