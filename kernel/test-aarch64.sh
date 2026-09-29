#!/bin/sh
# The arm64 bring-up's gate (docs/ARM64-PLAN.md §6): boot the aarch64 kernel once in QEMU
# `virt` with four cores and assert the verdicts the bring-up prints. Under TCG on an x86
# host every assertion here is DETERMINISTIC — the machine's logic, not its timing — so
# this is the set a hosted CI runner can judge; the timing gates and the memory-ordering
# soak stay with the KVM stand (§1). Run from the host:
#   kernel/test-aarch64.sh            (docker image talaria-qemu-aarch64, or a host qemu-system-aarch64)
# The kernel comes from kernel/build-aarch64.sh; TAL_AARCH64_IMG= names another image.
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
IMG=${TAL_AARCH64_IMG:-kernel/target/aarch64-talaria/release/talaria.img}
[ -f "$REPO/$IMG" ] || { echo "there is no $IMG — run kernel/build-aarch64.sh first"; exit 1; }
LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT
ACCEL="-cpu max"
[ "$(uname -m)" = "aarch64" ] && [ -c /dev/kvm ] && ACCEL="-enable-kvm -cpu host"
# 4 cores: the secondaries and the IPIs are part of what is judged. 30 s is generous —
# TCG on a loaded runner reaches the last line in ~8 s.
QEMU="timeout 30 qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp 4 -m 1024 -nographic \
  -kernel /w/$IMG -append \"console=ttyAMA0 talaria.mode=cell talaria.smp=4\""
if command -v qemu-system-aarch64 >/dev/null 2>&1; then
  sh -c "$(echo "$QEMU" | sed "s|/w/|$REPO/|")" > "$LOG" 2>&1
else
  docker run --rm -v "$REPO:/w" -w /w talaria-qemu-aarch64 sh -c "$QEMU" > "$LOG" 2>&1
fi

PASS=0; FAIL=0
assert() {
  if grep -qE "$1" "$LOG"; then echo "  PASS  $2"; PASS=$((PASS+1)); else echo "  FAIL  $2 (no /$1/)"; FAIL=$((FAIL+1)); fi
}
# assertnot PAT NAME: the pattern must NOT appear (a trap or a panic anywhere is a failure).
assertnot() {
  if grep -qE "$1" "$LOG"; then echo "  FAIL  $2 (found /$1/)"; FAIL=$((FAIL+1)); grep -E "$1" "$LOG" | head -3 | cut -c1-160 | sed 's/^/      /'; else echo "  PASS  $2"; PASS=$((PASS+1)); fi
}

echo "== arm64 bring-up (QEMU virt, gicv3+its, 4 cores) =="
assert 'talaria: boot \(aarch64\), EL1, dtb=0x'                       'boot: the entry runs at EL1 with a device tree in x0'
assert 'the MMU is on: sctlr=0x[0-9a-f]+ ttbr0=0x'                    'boot: the early identity map is on'
assert 'the device tree: [0-9]+ bytes at 0x.*; 1 RAM range\(s\), 4 cpu\(s\), 32 virtio-mmio window\(s\)' 'dtb: RAM, 4 cpus, 32 virtio windows parsed'
assert 'gic v3: distributor 0x8000000, redistributors 0x80a0000 .* its 0x8080000' 'dtb: the GIC and its ITS found'
assert 'timer ppis: secure 29 phys 30 virt 27 hyp 26; psci hvc'          'dtb: the timer PPIs and the PSCI conduit'
assert 'pcie ecam at 0x4010000000'                                    'dtb: the ECAM window'
assert 'start_info at 0x[0-9a-f]+ \(1 memmap entries\)'                'boot: the tree became a PVH start_info'
assert 'cmdline = "console=ttyAMA0 talaria.mode=cell talaria.smp=4"'   'cmdline: read through the shared read_cmdline'
assert 'RAM in total 1024 MB; the frame allocator: 0x40[0-9a-f]+\.\.0x80000000' 'mem: the shared allocator took the map'
assert 'free frames = [0-9]{6}'                                        'mem: frames are handed out'
assert 'vdso: the clock in user space is up .*__kernel_clock_gettime at \+0x' 'vdso: the image is mapped and exports the clock call'
assert 'dirty probe on 0x[0-9a-f]+: hardware DBM \(TCR.HD\) .*\(as expected\)' 'paging: the dirty bit through DBM'
assert 'our own page tables are active \(GB1 on 2M'                    'paging: the shared tables are active, the kernel gigabyte is GB1'
assert 'the frame allocator starts at 0x[0-9a-f]+ — above the supervisor leaf 0x[0-9a-f]+: OK' 'paging: the allocator is above the supervisor leaf'
assert 'paging probe on 0x[0-9a-f]+: .*the word survived \(as expected\)' 'paging: PROT_NONE and back through enc/dec'
assert 'psci 1\.[0-9] through hvc'                                     'psci: the firmware answers'
assert 'gic v3 is up: 256 SPIs; this core.s redistributor found'       'gic: the distributor and this core'
assert 'the tick: PPI 27 at 1000 us — [0-9]+ ticks in 50 ms on cpu0 \(as expected\)' 'timer: the tick re-arms itself'
assert '4 of 4 cores up'                                              'smp: the secondaries came up through PSCI'
assert 'cpu1: 1 SGI\(s\) received, [0-9]+ tick\(s\) of its own'        'smp: cpu1 took an IPI and its own ticks'
assert 'cpu3: 1 SGI\(s\) received, [0-9]+ tick\(s\) of its own'        'smp: cpu3 took an IPI and its own ticks'
assert 'el0 probe: back in EL1 — 3 svc round trip\(s\), ended by brk #0 \(as expected\)' 'el0: three svc round trips and a controlled brk'
assert 'vdso probe: EL0 read [0-9]+\.[0-9]{9} s; .* \(as expected\)'   'el0: the vDSO clock read from EL0 is inside the kernel bracket'
assert 'arm64 phases 1-3\(a\) stop here'                               'the bring-up reached its end'
assertnot '\[trap\]|PANIC|NOT as expected|NOT inside|NOT the'          'no trap, no panic, no unexpected verdict'
echo "TOTAL (arm64): PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = 0 ]
