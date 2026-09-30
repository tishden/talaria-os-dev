#!/bin/sh
# The arm64 bring-up's gate (docs/ARM64-PLAN.md §6): boot the aarch64 kernel once in QEMU
# `virt` with four cores and assert the verdicts the bring-up prints. Under TCG on an x86
# host every assertion here is DETERMINISTIC — the machine's logic, not its timing — so
# this is the set a hosted CI runner can judge; the timing gates and the memory-ordering
# soak stay with the KVM stand (§1). Run from the host:
#   kernel/test-aarch64.sh            (docker image talaria-qemu-aarch64, or a host qemu-system-aarch64)
# The kernel comes from kernel/build-aarch64.sh; TAL_AARCH64_IMG= names another image.
#   TAL_KVM=1 kernel/test-aarch64.sh   — on the KVM stand: the timing lines are judged too
#   (the tick count in 50 ms must be the ~50 a real timer gives; TCG gives 48-49).
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
IMG=${TAL_AARCH64_IMG:-kernel/target/aarch64-talaria/release/talaria.img}
[ -f "$REPO/$IMG" ] || { echo "there is no $IMG — run kernel/build-aarch64.sh first"; exit 1; }
LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT
ACCEL="-cpu max"
[ "$(uname -m)" = "aarch64" ] && [ -c /dev/kvm ] && ACCEL="-enable-kvm -cpu host"
KVM=${TAL_KVM:-0}
# boot SECS APPEND: one boot of the image with 4 cores, the console into $LOG. 4 cores: the
# secondaries and the IPIs are part of what is judged. TCG on a loaded runner reaches the
# default sequence's last line in ~8 s; the probes take longer (the bbm one ~30 s).
boot() {
  QEMU="timeout $1 qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp 4 -m 1024 -nographic \
    -kernel /w/$IMG -append \"console=ttyAMA0 $2 talaria.smp=4\""
  if command -v qemu-system-aarch64 >/dev/null 2>&1; then
    sh -c "$(echo "$QEMU" | sed "s|/w/|$REPO/|")" > "$LOG" 2>&1
  else
    docker run --rm -v "$REPO:/w" -w /w talaria-qemu-aarch64 sh -c "$QEMU" > "$LOG" 2>&1
  fi
}

PASS=0; FAIL=0
assert() {
  if grep -qE "$1" "$LOG"; then echo "  PASS  $2"; PASS=$((PASS+1)); else echo "  FAIL  $2 (no /$1/)"; FAIL=$((FAIL+1)); fi
}
# assertnot PAT NAME: the pattern must NOT appear (a trap or a panic anywhere is a failure).
assertnot() {
  if grep -qE "$1" "$LOG"; then echo "  FAIL  $2 (found /$1/)"; FAIL=$((FAIL+1)); grep -E "$1" "$LOG" | head -3 | cut -c1-160 | sed 's/^/      /'; else echo "  PASS  $2"; PASS=$((PASS+1)); fi
}

boot 30 "talaria.mode=cell"
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
if [ "$KVM" = 1 ]; then
  assert 'the tick: PPI 27 at 1000 us — (49|50|51) ticks in 50 ms on cpu0'  'timer: ~50 ticks in 50 ms under KVM (a real 1 ms timer)'
else
  assert 'the tick: PPI 27 at 1000 us — [0-9]+ ticks in 50 ms on cpu0 \(as expected\)' 'timer: the tick re-arms itself'
fi
assert '4 of 4 cores up'                                              'smp: the secondaries came up through PSCI'
assert 'cpu1: 1 SGI\(s\) received, [0-9]+ tick\(s\) of its own'        'smp: cpu1 took an IPI and its own ticks'
assert 'cpu3: 1 SGI\(s\) received, [0-9]+ tick\(s\) of its own'        'smp: cpu3 took an IPI and its own ticks'
assert 'el0 probe: back in EL1 — 3 svc round trip\(s\), ended by brk #0 \(as expected\)' 'el0: three svc round trips and a controlled brk'
assert 'vdso probe: EL0 read [0-9]+\.[0-9]{9} s; .* \(as expected\)'   'el0: the vDSO clock read from EL0 is inside the kernel bracket'
assert 'arm64 phases 1-3\(a\) stop here'                               'the bring-up reached its end'
assertnot '\[trap\]|PANIC|NOT as expected|NOT inside|NOT the'          'no trap, no panic, no unexpected verdict'

# The probes TCG cannot JUDGE but can RUN (docs/ARM64-PLAN.md §9): here they prove the
# machinery — the job on a secondary core, the guarded load and its fixup, the EL0 state
# machines — and their verdict lines; the numbers and the weak-memory answers are the
# stand's. The ring gets fewer slots than the stand would (TCG is slow).
echo "== the probes (talaria.test=ring|icache|bbm|svccost) =="
boot 90 "talaria.test=ring talaria.ringn=${TAL_RINGN:-100000}"
assert 'ring probe: [0-9]+ slots in [0-9]+ ms — 0 corrupted, 0 with a wrong length, [0-9]+ slots/s \(as expected\)' 'ring: the SPSC protocol between two cores, no corrupted slot'
assertnot '\[trap\]|PANIC|NOT ' 'ring: no trap, no unexpected verdict'
boot 60 "talaria.test=icache"
assert 'icache probe: A answered 0xa \(as expected\)'                      'icache: the first probe runs'
assert 'icache probe: B \(rewritten WITH sync\) answered 0xb \(as expected\)' 'icache: the rewritten frame answers anew after icache::sync'
assert 'icache probe: C \(rewritten WITHOUT sync\) answered 0x'            'icache: the unsynchronised rewrite is observed (not judged)'
assertnot '\[trap\]|PANIC|NOT ' 'icache: no trap, no unexpected verdict'
boot 120 "talaria.test=bbm"
assert 'bbm probe: 48 regions, [0-9]+ ms; the reader saw [0-9]+ reads, [0-9]+ faults \(PROT_NONE phases\), 0 WRONG values, 0 TLB conflict aborts \(as expected\)' 'bbm: splits and rights flips under a neighbour, no wrong value, no conflict abort'
assertnot '\[trap\]|PANIC|NOT ' 'bbm: no trap, no unexpected verdict'
boot 90 "talaria.test=svccost"
assert 'svccost probe: WITH the FP/SIMD save: [0-9]+ ns per svc round trip'   'svccost: the entry with the FP/SIMD save is timed'
assert 'svccost probe: WITHOUT the FP/SIMD save: [0-9]+ ns per svc round trip — the save costs [0-9]+ ns \(numbers taken\)' 'svccost: and without it'
assertnot '\[trap\]|PANIC|NOT ' 'svccost: no trap, no unexpected verdict'
echo "TOTAL (arm64): PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = 0 ]
