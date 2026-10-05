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
# The container sees /dev/kvm only when it is handed in (the x86 test.sh does the same).
KVMDEV=""; [ -c /dev/kvm ] && KVMDEV="--device /dev/kvm"
KVM=${TAL_KVM:-0}
# boot SECS APPEND: one boot of the image with 4 cores, the console into $LOG. 4 cores: the
# secondaries and the IPIs are part of what is judged. TCG on a loaded runner reaches the
# default sequence's last line in ~8 s; the probes take longer (the bbm one ~30 s). The
# kernel powers the machine off at its end (`talaria.poweroff=1`), so SECS is a ceiling,
# not the boot's length — under KVM the whole gate is a minute rather than the sum of them.
boot() {
  QEMU="timeout $1 qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp 4 -m 1024 -nographic \
    -kernel /w/$IMG -append \"console=ttyAMA0 $2 talaria.smp=4 talaria.poweroff=1\""
  if command -v qemu-system-aarch64 >/dev/null 2>&1; then
    sh -c "$(echo "$QEMU" | sed "s|/w/|$REPO/|")" > "$LOG" 2>&1
  else
    docker run --rm $KVMDEV -v "$REPO:/w" -w /w talaria-qemu-aarch64 sh -c "$QEMU" > "$LOG" 2>&1
  fi
}

# bootd SECS APPEND: as `boot`, with the TFS1 disk of the arm64 set on virtio-blk (the tree's
# virtio-mmio windows; `mkfs.py <img> user/aarch64` builds it from the binaries present).
DISK=kernel/target/disk-a64.img
bootd() {
  QEMU="timeout $1 qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp 4 -m 1024 -nographic \
    -kernel /w/$IMG -append \"console=ttyAMA0 $2 talaria.smp=4 talaria.poweroff=1\" \
    -drive id=d0,file=/w/$DISK,format=raw,if=none -device virtio-blk-device,drive=d0"
  if command -v qemu-system-aarch64 >/dev/null 2>&1; then
    sh -c "$(echo "$QEMU" | sed "s|/w/|$REPO/|g")" > "$LOG" 2>&1
  else
    docker run --rm $KVMDEV -v "$REPO:/w" -w /w talaria-qemu-aarch64 sh -c "$QEMU" > "$LOG" 2>&1
  fi
}

# bootx SECS APPEND DISK [MEM]: as `boot`, with DISK (a path under the repository) on
# virtio-blk and MEM megabytes (1024 by default).
bootx() {
  QEMU="timeout $1 qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp 4 -m ${4:-1024} -nographic \
    -kernel /w/$IMG -append \"console=ttyAMA0 $2 talaria.smp=4 talaria.poweroff=1\" \
    -drive id=d0,file=/w/$3,format=raw,if=none -device virtio-blk-device,drive=d0"
  if command -v qemu-system-aarch64 >/dev/null 2>&1; then
    sh -c "$(echo "$QEMU" | sed "s|/w/|$REPO/|g")" > "$LOG" 2>&1
  else
    docker run --rm $KVMDEV -v "$REPO:/w" -w /w talaria-qemu-aarch64 sh -c "$QEMU" > "$LOG" 2>&1
  fi
}

# bootrsp SECS APPEND DISK PORT CLIENT...: a boot whose console is a TCP line — on arm64 the
# gdb stub shares the console's UART (`arch/aarch64/gdb.rs`) — and CLIENT (any command; the
# word PORT in it is replaced by the port) talking to it; the client's output into $LOG.
# DISK (a path under the repository, or "") goes on virtio-blk.
bootrsp() {
  local secs=$1 app=$2 disk=$3 port=$4 drv="" a; shift 4
  [ -n "$disk" ] && drv="-drive id=d0,file=/w/$disk,format=raw,if=none -device virtio-blk-device,drive=d0"
  QEMU="exec timeout $secs qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp 4 -m 1024 -display none \
    -serial tcp:HOST:$port,server=on,wait=on -kernel /w/$IMG -append \"console=ttyAMA0 $app talaria.smp=4\" $drv"
  if command -v qemu-system-aarch64 >/dev/null 2>&1; then
    sh -c "$(echo "$QEMU" | sed -e "s|/w/|$REPO/|g" -e 's/HOST/127.0.0.1/')" >/dev/null 2>&1 & QP=$!
  else
    docker run --rm --name "tal-rsp-$$" -p "127.0.0.1:$port:$port" $KVMDEV -v "$REPO:/w" -w /w talaria-qemu-aarch64 \
      sh -c "$(echo "$QEMU" | sed 's/HOST/0.0.0.0/')" >/dev/null 2>&1 & QP=$!
  fi
  sleep 3 # the listener up (docker's proxy accepts and RESETS a connection before QEMU listens)
  a=""; for w in "$@"; do w=$(printf '%s' "$w" | sed "s/PORT/$port/g"); a="$a '$w'"; done
  eval "timeout $((secs - 5)) $a" > "$LOG" 2>&1
  docker kill "tal-rsp-$$" >/dev/null 2>&1; kill $QP 2>/dev/null; wait $QP 2>/dev/null
}
RSP="python3 $REPO/kernel/tools/rsp-probe.py"
# A gdb that knows aarch64 (for the core dumps and the live stub).
GDB=""; for g in gdb-multiarch aarch64-linux-gnu-gdb gdb; do
  command -v $g >/dev/null 2>&1 && $g --configuration 2>/dev/null | grep -q aarch64 && { GDB=$g; break; }
done

PASS=0; FAIL=0
# The console of a boot that failed a check is KEPT (kernel/target/a64-fail/N.log): a trap
# seen once in thirty runs is diagnosed from its dump, and the next boot overwrites $LOG.
FAILDIR="$REPO/kernel/target/a64-fail"; rm -rf "${FAILDIR:?}"
keepfail() {
  mkdir -p "$FAILDIR" && cp "$LOG" "$FAILDIR/$((PASS+FAIL)).log" 2>/dev/null && echo "      the console: kernel/target/a64-fail/$((PASS+FAIL)).log"
}
assert() {
  if grep -qE "$1" "$LOG"; then echo "  PASS  $2"; PASS=$((PASS+1)); else echo "  FAIL  $2 (no /$1/)"; FAIL=$((FAIL+1)); keepfail; fi
}
# assertcount PAT N NAME: the pattern must appear at least N times (two processes, two lines).
assertcount() {
  local n; n=$(grep -cE "$1" "$LOG")
  if [ "$n" -ge "$2" ]; then echo "  PASS  $3"; PASS=$((PASS+1)); else echo "  FAIL  $3 ($n of $2 /$1/)"; FAIL=$((FAIL+1)); fi
}
# assertnot PAT NAME: the pattern must NOT appear (a trap or a panic anywhere is a failure).
assertnot() {
  if grep -qE "$1" "$LOG"; then echo "  FAIL  $2 (found /$1/)"; FAIL=$((FAIL+1)); grep -E "$1" "$LOG" | head -3 | cut -c1-160 | sed 's/^/      /'; keepfail; else echo "  PASS  $2"; PASS=$((PASS+1)); fi
}

boot 30 "talaria.test=bringup"
echo "== arm64 bring-up (QEMU virt, gicv3+its, 4 cores) =="
assert 'talaria: boot \(aarch64\), EL1, dtb=0x'                       'boot: the entry runs at EL1 with a device tree in x0'
assert 'the MMU is on: sctlr=0x[0-9a-f]+ ttbr0=0x'                    'boot: the early identity map is on'
assert 'the device tree: [0-9]+ bytes at 0x.*; 1 RAM range\(s\), 4 cpu\(s\), 32 virtio-mmio window\(s\)' 'dtb: RAM, 4 cpus, 32 virtio windows parsed'
assert 'gic v3: distributor 0x8000000, redistributors 0x80a0000 .* its 0x8080000' 'dtb: the GIC and its ITS found'
assert 'timer ppis: secure 29 phys 30 virt 27 hyp 26; psci hvc'          'dtb: the timer PPIs and the PSCI conduit'
assert 'pcie ecam at 0x4010000000'                                    'dtb: the ECAM window'
assert 'start_info at 0x[0-9a-f]+ \(1 memmap entries\)'                'boot: the tree became a PVH start_info'
assert 'cmdline = "console=ttyAMA0 talaria.test=bringup talaria.smp=4 talaria.poweroff=1"'   'cmdline: read through the shared read_cmdline'
assert 'RAM in total 1024 MB; the frame allocator: 0x4[0-9a-f]+\.\.0x80000000' 'mem: the shared allocator took the map'
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
echo "== the shared kernel on arm64: boot_shared, the scheduler, the tick (docs/ARM64-PLAN.md §10.1, stage 3.3) =="
boot 40 "talaria.test=slice"
assert 'slice-test: ALL OK'                                              'slice: the shared timer self-test through the whole boot'
assertnot '\[trap\]|PANIC|BOOT REFUSED'                                  'slice: no trap, no panic, no refusal'
# The guard of a forked task's kernel stack goes into the CHILD's space (and the kernel's
# reference one): it went into the parent's, and the child ran on the stack unguarded.
boot 30 "talaria.test=kstackguard"
assert 'kstackguard: ALL OK'                                              'kstackguard: a stack for another space is guarded there and in the kernel space, inherited by later spaces, released with no hole'
boot 30 "talaria.test=ipi"
assert 'the AP is up — the datapath on core 1, the BSP = the control plane' 'ipi: a secondary came up through PSCI under the scheduler'
assert 'sched: the first entry'                                           'ipi: the tick entered the shared scheduler'
assert '\[task B: housekeeping\] tick [3-9]'                              'ipi: the shared scheduler switches two kernel tasks on the tick'
assert 'IPI->AP=[2-9]'                                                     'ipi: IPIs reach the secondary'
assertnot '\[trap\]|PANIC'                                               'ipi: no trap, no panic'

# Ring 3 (stage 3.4): the binaries come from `make -C kernel/user aarch64` and are not in git,
# so these boots run where they were built; a CI runner without them skips the block and says so.
if [ -f "$REPO/kernel/user/aarch64/mhello" ] && [ -f "$REPO/kernel/user/aarch64/hello.elf" ]; then
  echo "== ring 3 on arm64: the embedded binaries (kernel/user/aarch64) =="
  boot 30 "talaria.test=user"
  assert "Hello from Talaria's userspace \(EL0, a Linux syscall\)!"           'user: hello.elf prints through write() from EL0'
  assert '\[syscall\] exit\(the code=7\)'                                    'user: and exits with its code'
  boot 30 "talaria.test=musl"
  assert 'musl static binary -> Talaria OS!'                                  'musl: a static musl binary starts (TLS, auxv) and prints'
  assert '\[syscall\] exit_group\(the code=5\)'                              'musl: and exits with its code'
  assertnot '\[trap\]|PANIC|\[fault\]'                                        'musl: no trap, no fault'
  boot 40 "talaria.test=printf"
  assert 'Talaria: the line 2, malloc=0x'                                     'printf: stdio and malloc (brk/mmap) in a musl process'
  assert '\[syscall\] exit_group\(the code=0\)'                              'printf: and a clean exit'
  boot 90 "talaria.test=pthreads"
  assert 'PTHREADS: counter=200000 \(we expected 200000\) -> OK'              'pthreads: four threads through clone and futex, preempted by the tick, agree on the count'
  assert 'sched: all the 5 task\(s\) have finished'                           'pthreads: the process and its threads are reaped'
  assertnot '\[trap\]|PANIC|\[fault\]'                                        'pthreads: no trap, no fault'
  # The C++ set: a libstdc++ built against the cross musl (the `aarch64-linux-musl-cross`
  # toolchain, `make -C kernel/user aarch64` builds the four when its g++ is on the PATH).
  if [ -f "$REPO/kernel/user/aarch64/cpptest" ]; then
    echo "== ring 3 on arm64: C++ (static libstdc++ over musl) =="
    boot 60 "talaria.test=cpp"
    assert 'Talaria runs C..! sum.sq 1..10.=385'                               'cpp: STL, exceptions (the unwinder over .eh_frame) and static initialisers'
    boot 90 "talaria.test=cpp2"
    assert 'C.. threads: atomic=200000 guarded=200000'                         'cpp2: std::thread + mutex + chrono'
    boot 60 "talaria.test=cpp3"
    assert 'iostream.map.sort: talaria=3'                                      'cpp3: iostream + map + sort'
    boot 60 "talaria.test=cppdtor"
    assert 'the global destructor with the futex path has worked'              'cppdtor: the global destructors (a thread and a condvar after main)'
    assert 'exit_group.the code=0. — the process has finished'                 'cppdtor: a clean exit_group(0)'
    assertnot '\[trap\]|PANIC|\[fault\]|an unknown number'                    'cppdtor: no trap, no fault, no unknown syscall'
  else
    echo "  SKIP  cpp: no kernel/user/aarch64/cpptest (the aarch64-linux-musl-cross g++ is needed to build the C++ set)"
  fi
  (cd "$REPO/kernel" && python3 mkfs.py "$REPO/$DISK" user/aarch64 >/dev/null 2>&1)
  echo "== the disk on arm64: virtio-blk over the tree's virtio-mmio, processes from it =="
  bootd 40 "talaria.test=fs"
  assert 'TFS1 is mounted — [0-9]+ files on virtio-blk'                        'fs: the block device found in a virtio-mmio window, the FS mounted'
  assert 'Hello from the Talaria disk! \(virtio-blk \+ TFS1\)'                'fs: a file read from the disk'
  bootd 60 "talaria.test=procdisk"
  assertcount 'musl static binary -> Talaria OS!' 2                          'procdisk: both processes from the disk print'
  assert 'sched: all the 2 task\(s\) have finished'                           'procdisk: two processes in separate address spaces run to their end'
  assertnot '\[trap\]|PANIC|\[fault\]'                                        'procdisk: no trap, no fault — the spaces do not share a stack'
  # ── the rest of the embedded set (docs/ARM64-PLAN.md §10.3, step 1): the sources that carried
  # x86 asm or x86-only syscall names, built from the SAME files through kernel/user/tal_arch.h.
  # `an unknown number` in any of these logs is a syscall the arm64 table or the dispatcher
  # does not have — the plan's "zero ENOSYS over the set".
  BAD='\[trap\]|PANIC|an unknown number'
  echo "== ring 3 on arm64: the ported sources (efault, shmrace, smp2, smp4, futexlat) =="
  boot 60 "talaria.test=efault"
  assert 'efault: ALL OK'                                                     'efault: pointers the process does not own -> EFAULT (the kernel image, its frames, unmapped, non-canonical, read-only), valid buffers still work'
  assertnot "$BAD"                                                           'efault: the kernel survives every bad pointer'
  boot 60 "talaria.test=efault talaria.uacheck=0"
  assert '\[trap\] sync from EL1h'                                           'efault A/B: with talaria.uacheck=0 the same program brings the kernel down (the gate tests what it claims)'
  boot 90 "talaria.test=shmrace talaria.shmdelay=2000 talaria.stat=3000"
  assert 'shmrace: ALL OK'                                                    'shmrace: four threads create shm at once — one name gives ONE object, one SysV key one segment'
  assert 'stat: shm: creators waited for the tables [1-9][0-9]* times'        'shmrace: the race really happened (the kernel counted the waits)'
  assertnot "$BAD"                                                           'shmrace: no trap, no unknown syscall'
  boot 90 "talaria.test=shmrace talaria.shmdelay=2000 talaria.shmlock=0"
  assert 'shmrace: FAIL one name -> one object'                               'shmrace A/B: with the tables lock off the same program gets TWO objects for one name'
  # libtsync (docs/ARM64-PLAN.md §10.10): with no user-mode monitor tier every wait goes to the
  # kernel, timed ones on a borrowed deadline — the set that found the level-sensitive timer's
  # interrupt storm (its MPSC phase never finished before `trap.rs` silenced the timer).
  boot 240 "talaria.test=tsync talaria.futexhang=3"
  assert 'TSYNC: ALL OK'                                                      'tsync: libtsync on arm64 — mutex/MCS/condvar/SPSC/MPSC/timedwait on 2 cores, every wait in the kernel'
  assertnot "$BAD|\[futex\] IT HAS HUNG"                                      'tsync: no trap, no hung waiter'
  # Its fibers (an AArch64 context switch: x19-x30, d8-d15, sp) and its benches.
  boot 120 "talaria.test=fiber"
  assert 'FIBER: ALL OK'                                                      'fiber: green threads on arm64 — the switch, the condvar handoff, an SPSC of 500k in order'
  assertnot "$BAD"                                                           'fiber: no trap'
  boot 200 "talaria.test=syncbench talaria.futexhang=3"
  assert 'SYNCBENCH: ALL OK'                                                  'syncbench: the RTT, the mutex, the spin and tsync SPSC phases finish on arm64'
  assertnot "$BAD|\[futex\] IT HAS HUNG"                                      'syncbench: no trap, no hung waiter'
  # The process cycle of the freestanding `launcher` (raw svc: clone(SIGCHLD) -> execve -> wait4).
  bootd 90 "talaria.mode=procdemo"
  assert '\[launcher\] child exited, code=5'                                  'procdemo: fork (clone) + execve(/mhello) + wait4 from a freestanding ring-3 program — the exit code comes back'
  assert '\[launcher\] full process cycle .fork\+execve\+wait4. on Talaria OK' 'procdemo: the launcher finishes'
  boot 200 "talaria.test=piperace"
  assert 'piperace: the pipe carried 4194304 bytes in order across cores'     'piperace: a pipe between threads on two cores carries 4 MB byte-exact'
  assert 'piperace: the eventfd summed 300000 increments from 3 threads exactly' 'piperace: an eventfd written from three cores loses no increment'
  assertnot "$BAD"                                                           'piperace: no trap'
  # A waiter of a mixed epoll set (a pipe, an eventfd, a signalfd) PARKED by talaria.netmix=
  # with a 2 s ceiling: a missing kick reads as a ~2000 ms wake (the kick-less kernel: 1970).
  # Also the epoll_wait timeout on a machine with no NIC (it was zero cycles: an instant 0).
  boot 120 "talaria.test=mixwait talaria.netmix=2000000000"
  assert 'mixwait: ALL OK'                                                    'mixwait: a parked mixed-set waiter is woken by a pipe, an eventfd and a signal at once, not by its 2 s ceiling'
  assert 'mixed-set waiters parked [1-9][0-9]* \(woken [1-9][0-9]*, by the ceiling 0\)' 'mixwait: the waiters really parked, and no park ended by the ceiling'
  assertnot "$BAD"                                                           'mixwait: no trap'
  # A syscall that moves the thread to another core keeps every register (a ClickHouse thread
  # moved by its name came back with x0..x18 and lr zero; the old kernel fails this on x1..x17, lr).
  boot 60 "talaria.test=migregs"
  assert 'migregs: sched_setaffinity -> 0, the core 0 -> [1-3]'               'migregs: sched_setaffinity moves the thread off the boot core'
  assert 'migregs: ALL OK'                                                    'migregs: x1..x17 and lr survive the move (a syscall preserves all but x0)'
  # The ID registers from EL0: AT_HWCAP's CPUID bit promises Linux's emulation of `mrs` (OpenSSL
  # reads MIDR at load; Alpine's PostgreSQL died on it with a SIGILL before).
  boot 60 "talaria.test=idregs"
  assert 'idregs: ALL OK'                                                     'idregs: MIDR/MPIDR/ID_AA64* read from EL0 as Linux answers them (no SVE, SME, MTE or PAC shown)'
  assertnot "$BAD"                                                           'idregs: no trap'
  # A dynamic ELF: a PIE + ld-musl-aarch64 + libz through dlopen (services/build-aarch64-db.sh dyn).
  if [ -f "$REPO/kernel/target/disk-a64-dyn.img" ]; then
    bootx 90 "talaria.test=dyn" kernel/target/disk-a64-dyn.img
    assert 'dyn-hello: zlibVersion=1\.'                                       'dyn: a PIE through PT_INTERP (ld-musl-aarch64), threads, dlopen of libz'
    assert 'dyn-hello: ALL OK'                                                'dyn: the dynamic hello finishes'
  else echo "  SKIP  dyn: there is no kernel/target/disk-a64-dyn.img (services/build-aarch64-db.sh dyn)"; fi
  boot 120 "talaria.test=smp2"
  if [ "$KVM" = 1 ]; then
    assert 'SMP2: the parallel speedup [12]\.[0-9]+ .the thread on an AP, the syscalls from the AP. ALL OK' 'smp2: sched_setaffinity migrates a thread onto a secondary, the speedup is ~2x'
  else
    assert 'SMP2: t1=[0-9]+ both=[0-9]+ speedup=[0-9.]+ wctr=100000000 tid=[0-9]+ cpu=1' 'smp2: the thread migrates onto a secondary and makes its syscalls from there (the speedup is for the stand to judge)'
  fi
  assertnot "$BAD"                                                           'smp2: no trap, no unknown syscall'
  boot 150 "talaria.test=smp4"
  assert 'SMP4: getaffinity ret=8 mask=0xf ncpu=4'                            'smp4: sched_getaffinity gives the online mask'
  assert 'SMP4: the threads are on the cores 1/2/3'                           'smp4: three threads on the secondaries 1..3'
  [ "$KVM" = 1 ] && assert 'SMP4: the parallel speedup [234]\.[0-9]+ on 4 cores .* ALL OK' 'smp4: the speedup on four cores is > 2.5x'
  assertnot "$BAD"                                                           'smp4: no trap, no unknown syscall'
  boot 200 "talaria.test=futexlat talaria.futexhang=3"
  assert 'FUTEXLAT futex cross-core: RTT p50=[0-9]+ ns'                       'futexlat: a cross-core futex ping-pong through an unmodified musl'
  assert 'FUTEXLAT: ALL OK'                                                   'futexlat: all four phases finish (a thread that EXITS on a secondary gives the core back)'
  assertnot "$BAD|\\[futex\\] IT HAS HUNG"                                             'futexlat: no trap, no hung waiter'

  # ── the modes that start FROM an ext2 disk (step 2) and the process model (step 3). The images
  # are built here from kernel/user/aarch64, as test.sh builds x86's; a test that writes gets a copy.
  TD=kernel/target/a64-disks; mkdir -p "$REPO/$TD"
  U="$REPO/kernel/user/aarch64"
  mkimg() { # mkimg NAME SIZE MANIFEST FILES... -> $TD/NAME.img
    local name=$1 size=$2 man=$3 d rc; shift 3
    d=$(mktemp -d) || return 1
    for b in "$@"; do cp "$U/$b" "$d/" 2>/dev/null; done
    echo hello > "$d/hello.txt"; mkdir -p "$d/dir"; echo inside > "$d/dir/inside.txt"; ln -s hello.txt "$d/link.txt"; ln -s dir "$d/dirlink"
    [ -n "$man" ] && printf "$man" > "$d/cell.manifest"
    "$REPO/kernel/mkext2.sh" "$d" "$REPO/$TD/$name.img" "$size" >/dev/null 2>&1; rc=$?
    rm -rf "${d:?}"
    [ $rc = 0 ] && [ -s "$REPO/$TD/$name.img" ]
  }
  if command -v mkfs.ext2 >/dev/null 2>&1 \
     && mkimg ext2 64M 'binary=/sigtest\nargs=sigtest\nenv=HOME=/\n' sigtest execmt fputest memrace mbench fsinfo schedapi forksrv forktls \
     && mkimg ms 64M '' mapshared && mkimg core 32M '' crashme \
     && mkimg coremt 32M 'binary=/crashme\nargs=crashme mt\nrestart=0\n' crashme \
     && mkimg crashr 32M 'binary=/crashme\nargs=crashme mt\nrestart=1\n' crashme \
     && mkimg forkap 32M 'binary=/crashme\nargs=crashme forkap\n' crashme \
     && mkimg peek 32M 'binary=/crashme\nargs=crashme peek\nrestart=0\n' crashme; then
    run() { cp "$REPO/$TD/$3.img" "$REPO/$TD/run.img"; bootx "$1" "$2" "$TD/run.img" "${4:-}"; }
    echo "== the ext2 disk on arm64: the disk-borne modes (sigtest, mbench, schedapi, mapshared, fputest, memrace) =="
    run 150 "talaria.test=sigtest" ext2
    assert 'sigtest: ALL OK'                                                  'sigtest: fd tables, signals (handler, mask, SIGCHLD, sigwait, signalfd), fork/COW, MAP_SHARED, SysV shm, eventfd/timerfd/epoll ET, procfs, fstatat — the arm64 stat, epoll_event and sigcontext layouts under real code'
    assertnot "$BAD|\[fault\]"                                              'sigtest: no trap, no fault, no unknown syscall'
    run 150 "talaria.mode=proc" ext2
    assert 'sigtest: ALL OK'                                                  'proc-mode: a process by /cell.manifest (the launch path of the databases)'
    assertnot "$BAD|\[fault\]"                                              'proc-mode: no trap, no fault, no unknown syscall'
    run 150 "talaria.test=mbench" ext2
    assert 'RESULT mbench syscall [0-9]+ ns'                                  'mbench: the syscall primitive is timed (the number is for the stand)'
    assert 'mbench: ALL OK'                                                   'mbench: the primitives of the hot loop with the binary that goes to Linux'
    assertnot "$BAD"                                                         'mbench: no trap, no unknown syscall'
    run 150 "talaria.test=schedapi" ext2
    assert 'schedapi: ALL OK'                                                 'schedapi: yield gives the core away, membarrier, the binding to cores and back, the rlimits are the real walls'
    assertnot "$BAD"                                                         'schedapi: no trap (the vDSO clock reads the counter from EL0 on EVERY core)'
    run 150 "talaria.test=mapshared" ms
    assert 'mapshared: ALL OK'                                                'mapshared: mmap(MAP_SHARED, fd) — read/write and the mapping see the same bytes, msync reaches the disk'
    assertnot "$BAD"                                                         'mapshared: no trap, no unknown syscall'
    run 150 "talaria.test=fputest" ext2
    assert 'fputest: ok the v survived a signal handler'                      'fputest: v16..v31 survive a signal handler that clobbers them'
    assert 'fputest: ok x0, x1, x8, x16, x17 and x30 survived a signal handler' 'fputest: the registers a syscall return may forget come back from rt_sigreturn'
    assert 'fputest: 8 threads on 4 cores'                                    'fputest: twice as many threads as cores — every core preempts'
    assert 'fputest: ALL OK'                                                  'fputest: the FP/SIMD state survives a syscall, a signal and preemption on four cores'
    assertnot "$BAD|is spoilt"                                               'fputest: not a single thread saw foreign registers'
    run 200 "talaria.test=memrace" ext2
    assert 'memrace: ALL OK'                                                  'memrace: parallel mmap/mprotect/DONTNEED/munmap on four cores do not spoil the memory'
    assertnot "$BAD|AN UNEXPECTED SIGSEGV"                                   'memrace: no trap, no unexpected fault'
    echo "== processes on arm64: execve, fork over a large space, children on the secondaries =="
    run 150 "talaria.test=execmt" ext2
    assert "exec: the process's threads finished: 3"                          'execmt: the exec takes ALL the sister threads off their cores (the reschedule SGI)'
    assert 'execmt: ALL OK'                                                   'execmt: a multithreaded execve — the new image is alive, argv/env arrived, threads start again'
    assertnot "$BAD|did not stop within"                                     'execmt: no trap, the sisters stopped in time'
    run 400 "talaria.test=forktls" ext2 4096
    assert 'forktls: matched 100, did not match 0'                            'forktls: a hundred forks of a 1.5 GB address space — the TPIDR_EL0 and the TLS of every child are intact'
    assert 'forktls: ALL OK'                                                  'forktls: the outcome'
    assertnot "$BAD|THE PARENT is spoilt|ATTENTION: the COW refcount table|\[fault\]" 'forktls: the parent is not spoilt, no fault over a hundred forks'
    assert 'mem: the high area 0x100000000\.\.0x140000000 \(1024 MB\)'       'forktls: the guest RAM above 4 GB (virt: 1..5 GB) is the high area the forks run over'
    # The main node's memory above 4 GB (`mem::high_init`): 850 MB of a c7g.large's
    # 3995 was all a workload got before. 4 GB crosses 4 GB; 17 GB reaches past the
    # static refcount table's 16 GB, as a c7g.large's RAM (16-18.9 GiB) does.
    run 120 "talaria.test=highmem" ext2 4096
    assert 'highmem: ALL OK'                                                  'highmem: the workload frames above 4 GB — identity, their own free list, the refcount, the AS leaf, MAP_FIXED refused'
    run 120 "talaria.test=highmem" ext2 17408
    assert 'mem: the COW refcount table: [0-9]+ KB at 0x[0-9a-f]+, it covers up to 0x480000000' 'highmem: RAM past 16 GB grows the refcount table to the top'
    assert 'highmem: ALL OK'                                                  'highmem: the same on 17 GB (the area 14 GB, the grown table)'
    assertnot "$BAD|\[fault\]"                                              'highmem: no trap'
    # A c7g.large's shape: 850 MB below 4 GB, the rest above. forktls's 1.5 GB went into
    # 2 MB pages from the LOW memory and the next page table found no frame — a panic one
    # syscall in, on the real machine; reproduced here by cutting the low allocator.
    run 400 "talaria.test=forktls talaria.lowmem=850" ext2 5120
    assert 'forktls: ALL OK'                                                  'forktls on a c7g.large shape (850 MB low): the huge pages come from the high area, the kernel keeps its tables'
    assertnot "$BAD|\[fault\]|there is no frame"                          'forktls on a c7g.large shape: no panic for a page table'
    run 150 "talaria.mode=proc" forkap
    assert 'crashme: forkap ALL OK'                                           'forkap: 40 children on the secondaries are born, work and are reaped by waitpid (an ignored SIGCHLD does not interrupt the wait)'
    assert "it has left the tables of the finished process onto the kernel.s CR3" 'forkap: an idling secondary leaves the tables of a dead process'
    assertnot "$BAD"                                                         'forkap: freeing the spaces of dead processes does not bring the machine down'
    echo "== crashes on arm64: the core dump, the supervisor, the isolation =="
    run 90 "talaria.test=core talaria.core=/core.elf" core
    assert '\[core\] [0-9]+ B into /core.elf: threads [0-9]+, memory segments' 'coredump: a fatal signal to the workload writes an ELF core dump (the process dies, the machine lives)'
    assert 'the stack workload \([2-8] frame'                                 'coredump: the stack of the crashed workload is in the log (the x29 chain)'
    assert 'tpidr=0x[0-9a-f]+'                                                'coredump: the crash line names the thread pointer'
    assertnot 'THE TLS IS BROKEN|\[trap\]|PANIC'                            'coredump: a healthy TLS is not called broken; the kernel is intact'
    if command -v debugfs >/dev/null 2>&1; then
      CORE=$(mktemp -d); debugfs -R "dump /core.elf $CORE/core.elf" "$REPO/$TD/run.img" >/dev/null 2>&1
      host() { if eval "$1"; then echo "  PASS  $2"; PASS=$((PASS+1)); else echo "  FAIL  $2"; FAIL=$((FAIL+1)); fi; }
      RE=readelf; command -v aarch64-linux-gnu-readelf >/dev/null 2>&1 && RE=aarch64-linux-gnu-readelf
      host '[ -s "$CORE/core.elf" ]'                                                       'core (host): /core.elf is on the volume after the crash'
      host '$RE -h "$CORE/core.elf" 2>/dev/null | grep -q "AArch64"'                        'core (host): an ELF core for AArch64 (e_machine 183)'
      host '$RE -n "$CORE/core.elf" 2>/dev/null | grep -q NT_PRSTATUS'                      'core (host): the note NT_PRSTATUS is in place'
      if [ -n "$GDB" ]; then
        host '$GDB -batch -nx -ex bt "$U/crashme" "$CORE/core.elf" 2>/dev/null | grep -qE "main \(.*\) at crashme.c"' 'core (host): gdb opens the dump and prints a bt with names (main () at crashme.c)'
        host '$GDB -batch -nx -ex "info registers pc" "$U/crashme" "$CORE/core.elf" 2>/dev/null | grep -q "^pc "'       'core (host): gdb reads the registers from the dump'
      else echo "  SKIP  core (host): no gdb that knows aarch64"; fi
      rm -rf "${CORE:?}"
    else echo "  SKIP  core (host): no debugfs"; fi
    # The KERNEL's own crash: a deliberate data abort at EL1 three calls deep. The registers
    # first (they always reach the console), then the block x86 prints — what the abort was,
    # the stack, the x29 chain — and the kernel's core dump, opened by the host's gdb.
    run 90 "talaria.test=corekernel talaria.core=/core.elf" core
    assert '\[trap\] sync from EL1h: .*data abort, same EL'                  'corekernel: the registers of the fatal exception are printed first'
    assert 'CRASH DUMP: sync from EL1h — data abort, same EL'                 'corekernel: the crash dump block follows them'
    assert 'the abort: FAR=0x00007effdead0000 \(not mapped \(a translation fault\), level [0-3], a write\)' 'corekernel: the abort is decoded (the address, not mapped, a write)'
    assert 'the stack kernel \([4-8] frame'                                   'corekernel: the call stack of the kernel is in the log (the x29 chain through the three test frames)'
    assert '\[core\] [0-9]+ B into /core.elf: threads 1, memory segments'     'corekernel: a fault IN THE KERNEL writes an ELF core dump onto the disk'
    assert 'the kernel has been halted'                                       'corekernel: the dump ends, the machine stops (no second fault inside it)'
    if command -v debugfs >/dev/null 2>&1 && [ -n "${GDB:-}" ]; then
      CORE=$(mktemp -d); debugfs -R "dump /core.elf $CORE/core.elf" "$REPO/$TD/run.img" >/dev/null 2>&1
      KELF="$REPO/${IMG%.img}"
      host '$GDB -batch -nx -ex bt "$KELF" "$CORE/core.elf" 2>/dev/null | grep -q "core_fault_l1"' 'corekernel (host): gdb opens the kernel'"'"'s dump at the faulting function'
      host '$GDB -batch -nx -ex bt "$KELF" "$CORE/core.elf" 2>/dev/null | grep -q "core_fault_l3"' 'corekernel (host): and unwinds the kernel stack from the dump (three frames up)'
      rm -rf "${CORE:?}"
    else echo "  SKIP  corekernel (host): no debugfs or no gdb that knows aarch64"; fi
    run 120 "talaria.mode=cell talaria.core=/core.elf" coremt
    assert '\[core\] [0-9]+ B into /core.elf: threads 4,'                     'coredump-mt: the crash of one thread of a cell on four cores writes a dump with ALL the threads'
    assert "supervise: the workload.s threads were killed by signal 11"       'coredump-mt: the crash of one thread = the crash of the process (the supervisor learns it on the boot core)'
    assertnot "the main thread is alive after the sister.s crash|\[trap\]|PANIC" 'coredump-mt: the sisters do not go on working; the kernel is intact'
    run 150 "talaria.mode=cell talaria.core=/core.elf" crashr
    assertcount "supervise: the workload.s threads were killed by signal 11" 2 'restart-mt: a crash on four cores -> a restart -> the second instance reaches the supervisor too'
    assert 'the restart limit .1. is exhausted'                               'restart-mt: the supervisor ran both instances and halted'
    assertnot 'the journal is busy with a foreign|\[trap\]|PANIC'            'restart-mt: no journal transaction taken away by the killed thread, the kernel intact'
    for m in cell proc; do
      run 90 "talaria.mode=$m" peek
      assert 'the signal 11 to the thread t[0-9]+: the rip 0x[0-9a-f]+, the address 0x48000000' "isolation ($m): a read of the RAM through the identity window from EL0 -> SIGSEGV"
      assertnot 'READ .* the window is OPEN|\[trap\]|PANIC'                  "isolation ($m): the workload did not read the memory of the kernel"
    done
    # LIVE DEBUGGING ON SMP (x86's gdb-smp, over the console's line here): a stop before the
    # workload's first instruction, an hbreak on a thread spinning on a secondary, a watchpoint
    # on the counter it writes — three stops, the value one more each time (the resume steps
    # over the store with the registers off: arm64 has no resume flag) — ^C, detach, and the
    # workload goes on.
    SPIN=$(nm "$U/crashme" 2>/dev/null | awk '$3=="spinner"{print $1}')
    SPINS=$(nm "$U/crashme" 2>/dev/null | awk '$3=="spins"{print $1}')
    if command -v python3 >/dev/null 2>&1 && [ -n "$SPIN" ] && [ -n "$SPINS" ] \
       && mkimg gdbsmp 32M 'binary=/crashme\nargs=crashme live\nrestart=0\n' crashme; then
      echo "== the gdb stub on SMP (arm64: the console's line) =="
      cp "$REPO/$TD/gdbsmp.img" "$REPO/$TD/run.img"
      bootrsp 150 "talaria.mode=proc talaria.gdb=break" "$TD/run.img" 1252 $RSP PORT '?' \
        "Z1,$SPIN,4" c "z1,$SPIN,4" "Z2,$SPINS,8" c "m$SPINS,8" c "m$SPINS,8" c "m$SPINS,8" "z2,$SPINS,8" \
        'c!' INTR qfThreadInfo D SLEEP INTR "m$SPINS,8" 'c!' INTR "m$SPINS,8" D
      assert '^first: T05thread:1;core:0;'                                      'gdb-smp: talaria.gdb=break stops before the workload'"'"'s first instruction'
      assert "^Z1,$SPIN,4 -> OK"                                                'gdb-smp: an hbreak (the breakpoint registers) is set on the workload'"'"'s code'
      assert '^c -> T05thread:[2-9];core:[123];'                                'gdb-smp: it catches the spinning thread ON A SECONDARY (the registers are every core'"'"'s)'
      assert "^m$SPINS,8 -> 0000000000000000"                                   'gdb-smp: a write watchpoint stops before the first store'
      assert "^m$SPINS,8 -> 0100000000000000"                                   'gdb-smp: the resume steps over the store and the next one stops (no resume flag on arm64)'
      assert "^m$SPINS,8 -> 0200000000000000"                                   'gdb-smp: and again — one stop per store'
      assert '^INTR -> T05thread:'                                              'gdb-smp: ^C from gdb stops the running machine (the console'"'"'s poll)'
      assert '^qfThreadInfo -> m1,2,3'                                          'gdb-smp: the threads of the workload are listed'
      assert '^D -> OK'                                                         'gdb-smp: detach answers'
      # After the detach: ^C again, the counter read, a second of running, ^C, read — it moved.
      V1=$(grep "^m$SPINS,8 -> " "$LOG" | sed -n 4p); V2=$(grep "^m$SPINS,8 -> " "$LOG" | sed -n 5p)
      if [ -n "$V1" ] && [ -n "$V2" ] && [ "$V1" != "$V2" ]; then echo "  PASS  gdb-smp: after the detach the workload goes on (the counter moved under a re-attach)"; PASS=$((PASS+1))
      else echo "  FAIL  gdb-smp: after the detach the counter did not move ('$V1' / '$V2')"; FAIL=$((FAIL+1)); keepfail; fi
    else echo "  SKIP  gdb-smp: no python3, no crashme symbols or no image"; fi
  else
    echo "== the ext2 disk on arm64: SKIPPED — no mkfs.ext2 (e2fsprogs) on the host =="
  fi
  # ── the file system and PCIe (docs/ARM64-PLAN.md §10.3 step 2): the ext2/jbd2 block on
  # virtio-blk AND on NVMe — the config space through the ECAM window the device tree names,
  # the BARs assigned by the kernel (no firmware ran before it), MSI-X through the ITS.
  # bootp SECS APPEND DISK DEV [UNTIL]: DISK (a path under the repository, NOT copied — a
  # test of two boots keeps its volume) on DEV (virtio|nvme). With UNTIL the machine is
  # KILLED as soon as the pattern is on the console: a power cut, no sync, no power-off.
  bootp() {
    local dev="-device virtio-blk-device,drive=d0" po="talaria.poweroff=1" run
    [ "$4" = nvme ] && dev="-device nvme,drive=d0,serial=tal001"
    [ -n "${5:-}" ] && po=""
    QEMU="qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp 4 -m 1024 -display none -serial file:\$O \
      -kernel /w/$IMG -append \"console=ttyAMA0 $2 talaria.smp=4 $po\" \
      -drive id=d0,file=/w/$3,format=raw,if=none $dev"
    run='O=$(mktemp); '"$QEMU"' >/dev/null 2>&1 & q=$!; i=0
      while [ $i -lt $((SECS * 2)) ] && kill -0 $q 2>/dev/null; do
        [ -n "$UNTIL" ] && grep -aqE "$UNTIL" $O 2>/dev/null && break; sleep 0.5; i=$((i+1)); done
      kill -9 $q 2>/dev/null; wait $q 2>/dev/null; cat $O; rm -f $O'
    if command -v qemu-system-aarch64 >/dev/null 2>&1; then
      SECS=$1 UNTIL="${5:-}" sh -c "$(echo "$run" | sed "s|/w/|$REPO/|g")" > "$LOG" 2>&1
    else
      docker run --rm $KVMDEV -e SECS="$1" -e UNTIL="${5:-}" -v "$REPO:/w" -w /w talaria-qemu-aarch64 sh -c "$run" > "$LOG" 2>&1
    fi
  }
  plain() { # plain NAME: an ext2 volume with hello.txt alone (the tests below write their own files)
    local d rc; d=$(mktemp -d) || return 1; echo hello > "$d/hello.txt"
    "$REPO/kernel/mkext2.sh" "$d" "$REPO/$TD/$1.img" 64M >/dev/null 2>&1; rc=$?; rm -rf "${d:?}"; [ $rc = 0 ]
  }
  if [ -s "$REPO/$TD/ext2.img" ] && plain fsb && plain jrn; then
    for DEV in virtio nvme; do
      PCI=""; [ $DEV = nvme ] && PCI="talaria.pci=1 "
      echo "== the file system on arm64: ext2 and the journal on $DEV =="
      cp "$REPO/$TD/ext2.img" "$REPO/$TD/fs.img"
      bootp 120 "${PCI}talaria.test=ext2" "$TD/fs.img" $DEV
      [ $DEV = nvme ] && assert 'pci: ECAM 0x[0-9a-f]+ \(the device tree\)'    'pci: the config space through the ECAM window the device tree names (above 4 GB, mapped on demand)'
      [ $DEV = nvme ] && assert 'nvme: 00:[0-9a-f]+\.0 BAR0=0x1[0-9a-f]+ .* polling' 'nvme: the controller is up — the BAR assigned by the kernel from the host bridge window, the admin and IO queues'
      assert 'ext2-test: ALL OK'                                              "ext2 ($DEV): the RW FS (create/write/pread/ftruncate/rename/unlink/mkdir/rmdir/getdents64)"
      assertnot "$BAD"                                                       "ext2 ($DEV): no trap, no unknown syscall"
      # ext2v reads what TWO earlier boots left on the volume: /t/hello.txt of the ext2 test and
      # /t/app.log that sigtest appends and leaves in the write-back cache until its exit.
      bootp 150 "${PCI}talaria.test=sigtest" "$TD/fs.img" $DEV
      assert 'sigtest: ALL OK'                                                "sigtest ($DEV): the process model from this volume"
      bootp 120 "${PCI}talaria.test=ext2v" "$TD/fs.img" $DEV
      assert 'ext2v: .*PERSISTED OK'                                          "ext2v ($DEV): the data survived a reboot (the write-back cache was flushed)"
      cp "$REPO/$TD/fsb.img" "$REPO/$TD/fs.img"
      bootp 150 "${PCI}talaria.test=fsbench" "$TD/fs.img" $DEV
      assert 'fsbench: ALL OK'                                                "fsbench ($DEV): an fsync of 512 blocks — the coalescing into ranges, a repeated fsync is almost free"
      assert 'fsbench: the queue: [0-9]+ commands submitted, the greatest depth ([2-9]|[1-9][0-9]+)' "fsbench ($DEV): the block layer has a DEPTH (the commands fly in a batch)"
      assertnot "$BAD"                                                       "fsbench ($DEV): no trap, no unknown syscall"
      for JM in "" " talaria.ext2=journal=data"; do
        cp "$REPO/$TD/jrn.img" "$REPO/$TD/fs.img"
        bootp 120 "${PCI}talaria.test=journal1$JM" "$TD/fs.img" $DEV 'journal1: COMMITTED|PANIC|\[trap\]'
        assert 'journal1: COMMITTED'                                          "journal ($DEV${JM:+, journal=data}): the transaction is committed, then the power is CUT (no checkpoint)"
        bootp 120 "${PCI}talaria.test=journal2$JM" "$TD/fs.img" $DEV
        assert 'journal2: .*RECOVERED OK'                                     "journal ($DEV${JM:+, journal=data}): after the cut the replay of the journal restores the ${JM:+DATA of the file and the }metadata"
        if command -v e2fsck >/dev/null 2>&1; then
          if e2fsck -fn "$REPO/$TD/fs.img" >/dev/null 2>&1; then echo "  PASS  journal ($DEV${JM:+, journal=data}) (host): e2fsck -fn is clean after the replay"; PASS=$((PASS+1)); else echo "  FAIL  journal ($DEV${JM:+, journal=data}) (host): e2fsck -fn found errors after the replay"; FAIL=$((FAIL+1)); fi
        fi
      done
    done
    echo "== the file system on arm64: the rest of the block on virtio-blk (symlinks, no journal, a cut transaction, htree) =="
    cp "$REPO/$TD/ext2.img" "$REPO/$TD/fs.img"
    bootp 120 "talaria.test=symlink" "$TD/fs.img" virtio
    assert 'symlink-test: ALL OK'                                             'symlink: a read through a link, a link in the middle of a path, readlink, stat/lstat, a creation by the kernel, ELOOP'
    cp "$REPO/$TD/ext2.img" "$REPO/$TD/fs.img"
    bootp 120 "talaria.test=ext2 talaria.ext2=nojournal" "$TD/fs.img" virtio
    assert 'the journal is off by the flag'                                   'nojournal: the mode without transactions is on'
    assert 'ext2-test: ALL OK'                                                'nojournal: the RW FS works without a journal'
    cp "$REPO/$TD/jrn.img" "$REPO/$TD/fs.img"
    bootp 150 "talaria.test=journalbig1 talaria.ext2=journal=data" "$TD/fs.img" virtio 'journal1: COMMITTED|PANIC|\[trap\]'
    assert 'the transactions [2-9]'                                           'journal=data: a transaction larger than the journal is CUT into parts'
    assert 'journal1: COMMITTED'                                              'journal=data: the cut transaction is committed'
    assertnot 'THE JOURNAL HAS BEEN OVERFLOWED'                               'journal=data: not a single part moved over the journal'"'"'s end'
    bootp 150 "talaria.test=journalbig2 talaria.ext2=journal=data" "$TD/fs.img" virtio
    assert 'journal2: .*RECOVERED OK'                                         'journal=data: the file from the cut transaction is restored in full (1500 blocks)'
    if command -v e2fsck >/dev/null 2>&1; then
      DXD=$(mktemp -d); mkdir -p "$DXD/big"; i=1; while [ $i -le 3000 ]; do echo x > "$DXD/big/file_$i.txt"; i=$((i+1)); done
      if DIRIDX=1 "$REPO/kernel/mkext2.sh" "$DXD" "$REPO/$TD/dx.img" 64M >/dev/null 2>&1; then
        bootp 400 "talaria.test=htree talaria.ext2=dxcheck" "$TD/dx.img" virtio
        assert 'htree: ALL OK'                                                'htree: 3000 names in an indexed directory — the search by the dir_index matches the enumeration'
        assert 'the leaves. splits [1-9]'                                     'htree: the growth of a directory by 3000 names splits the leaves and edits the index'
        assert 'has been indexed \(an htree was built from scratch'           'htree: a linear directory that has outgrown a block is indexed by the kernel'
        assertnot "$BAD"                                                     'htree: no trap, no unknown syscall'
        if e2fsck -fn "$REPO/$TD/dx.img" >/dev/null 2>&1; then echo "  PASS  htree (host): e2fsck -fn is clean — the tree after the kernel's splits is correct"; PASS=$((PASS+1)); else echo "  FAIL  htree (host): e2fsck -fn found errors in the directory"; FAIL=$((FAIL+1)); fi
      else echo "  SKIP  htree: the indexed image was not built"; fi
      # The same on a volume whose hash is UNSIGNED (mkfs's default on an arm64 host; forced
      # here so that the verdict does not depend on the host): the kernel hashes with the
      # version +3 and must write the superblock's raw version into its own dx_root — the
      # first KVM stand (an arm64 host) found a 4 there and e2fsck refused the tree.
      if HASHU=1 DIRIDX=1 "$REPO/kernel/mkext2.sh" "$DXD" "$REPO/$TD/dxu.img" 64M >/dev/null 2>&1; then
        bootp 400 "talaria.test=htree talaria.ext2=dxcheck" "$TD/dxu.img" virtio
        assert 'htree: ALL OK'                                                'htree (unsigned hash): 3000 names found by the index, 3000 added'
        assert 'has been indexed \(an htree was built from scratch'           'htree (unsigned hash): the kernel builds an index on the unsigned volume'
        if e2fsck -fn "$REPO/$TD/dxu.img" >/dev/null 2>&1; then echo "  PASS  htree (unsigned hash, host): e2fsck -fn is clean — the kernel's dx_root carries the raw hash version"; PASS=$((PASS+1)); else echo "  FAIL  htree (unsigned hash, host): e2fsck -fn found errors (the dx_root's hash version?)"; FAIL=$((FAIL+1)); fi
      else echo "  SKIP  htree (unsigned hash): the image was not built"; fi
      rm -rf "${DXD:?}"
    else echo "  SKIP  htree: no e2fsck on the host"; fi
    echo "== MSI-X on arm64: the ITS (talaria.test=its|msix) =="
    cp "$REPO/$TD/fsb.img" "$REPO/$TD/fs.img"
    bootp 90 "talaria.pci=1 talaria.test=its" "$TD/fs.img" nvme
    assert 'its: 0x[0-9a-f]+ is up: .*the commands were taken'                'its: the tables, the command queue and LPIs on the boot core'
    assert 'the MSI-X entry holds the address of the translation register'   'its: a device'"'"'s MSI-X entry is written and read back'
    assert 'its: OK   the admin completions arrived as LPIs on core 0'        'its: an NVMe admin completion arrives as an LPI and reaches the vector'"'"'s handler'
    assert 'its: OK   after the re-route \(MOVI\) the LPIs arrive on core 1'  'its: a re-route moves the interrupts to the other core (and not one stays on core 0)'
    assert 'its: ALL CHECKS PASSED'                                           'its: the summary'
    assertnot '\[trap\]|PANIC|its: FAILURE'                                  'its: no trap, no failed check'
    bootp 90 "talaria.pci=1 talaria.test=msix" "$TD/fs.img" nvme
    assert 'msix: OK   the redirection leaves the address alone: the route is a command to the ITS' 'msix: the shared routing gate on arm64 — the route is the ITS'"'"'s'
    assert 'msix: OK   ALL the table.s vectors have been moved'               'msix: the re-route covers the whole table'
    assert 'msix: ALL CHECKS PASSED'                                          'msix: the summary (the rule "not to a dedicated core" included)'
  else
    echo "== the file system on arm64: SKIPPED — the ext2 images were not built =="
  fi
  # ── the UEFI block (docs/ARM64-PLAN.md §4.2, the product path): edk2 -> the arm64 stub ->
  # the kernel, the machine described by ACPI alone (edk2 publishes no device tree next to
  # its tables) — the MADT's GIC entries, the GTDT, the SPCR, PSCI from the FADT, the ECAM
  # from the MCFG, the BARs the firmware assigned. The stub carries the kernel inside:
  # `kernel/uefi-stub/build.sh aarch64` after every kernel build, else the block is skipped.
  EFI=kernel/uefi-stub/target/aarch64-unknown-uefi/release/talaria-uefi-stub.efi
  FW=/usr/share/edk2/aarch64/QEMU_EFI.silent.fd
  # bootu SECS APPEND [DISK]: through the firmware; DISK (a copy is made) on NVMe.
  bootu() {
    local drv=""
    [ -n "${3:-}" ] && { cp "$REPO/$3" "$REPO/$TD/uefi.img"; drv="-drive id=d0,file=/w/$TD/uefi.img,format=raw,if=none -device nvme,drive=d0,serial=tal001"; }
    QEMU="timeout $1 qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp 4 -m 1024 -nographic -bios $FW \
      -kernel /w/$EFI -append \"console=ttyAMA0 $2 talaria.smp=4 talaria.poweroff=1\" $drv"
    if command -v qemu-system-aarch64 >/dev/null 2>&1; then
      sh -c "$(echo "$QEMU" | sed "s|/w/|$REPO/|g")" > "$LOG" 2>&1
    else
      docker run --rm $KVMDEV -v "$REPO:/w" -w /w talaria-qemu-aarch64 sh -c "$QEMU" > "$LOG" 2>&1
    fi
  }
  if [ -f "$REPO/$EFI" ] && [ "$REPO/$EFI" -nt "$REPO/${IMG%.img}" ]; then
    echo "== UEFI on arm64: edk2 -> the stub -> the kernel, the machine described by ACPI =="
    bootu 120 "talaria.test=musl"
    assert 'uefi-stub: Talaria UEFI boot \(aarch64\)'                        'uefi: the firmware starts the stub (the command line from LoadOptions)'
    assert 'talaria: boot \(aarch64\), EL[12], the UEFI stub.s block='       'uefi: the stub leaves the firmware, turns the MMU off and enters the Image with its boot block'
    assert 'talaria: ACPI \(the RSDP 0x[0-9a-f]+\): MADT true, GTDT true, SPCR true, FADT true; 4 cpu\(s\)' 'uefi-acpi: the cores, the GIC, the timer, the console and PSCI come from the ACPI tables (no device tree)'
    assert 'the wall clock.s base is the firmware.s clock \(UEFI GetTime\)'   'uefi: the wall clock starts from the firmware'"'"'s time'
    assert 'musl static binary -> Talaria OS!'                                'uefi: a musl process in ring 3 behind the firmware path'
    assertnot '\[trap\]|PANIC|FATAL'                                         'uefi: no trap, no refusal of the stub'
    bootu 200 "talaria.test=smp4"
    if [ "$KVM" = 1 ]; then
      assert 'SMP4: the parallel speedup [234]\.[0-9]+ on 4 cores .* ALL OK'  'uefi-smp4: four cores brought up through PSCI as the FADT names it, the speedup'
    else
      assert 'SMP4: the threads are on the cores 1/2/3'                       'uefi-smp4: four cores brought up through PSCI as the FADT names it (the MADT'"'"'s MPIDRs)'
    fi
    if [ -s "$REPO/$TD/ext2.img" ]; then
      bootu 150 "talaria.pci=1 talaria.test=ext2" "$TD/ext2.img"
      assert 'pci: ECAM 0x[0-9a-f]+ \(MCFG\)'                                 'uefi-pci: the ECAM window from the ACPI MCFG'
      assert 'nvme: 00:[0-9a-f]+\.0 BAR0=0x[0-9a-f]{9,} '                     'uefi-nvme: the BAR the FIRMWARE assigned (64-bit, above 4 GB) is respected and mapped'
      assert 'ext2-test: ALL OK'                                              'uefi-nvme: ext2 RW through NVMe behind the firmware path (EC2'"'"'s EBS path)'
      bootu 120 "talaria.pci=1 talaria.test=its" "$TD/ext2.img"
      assert 'its: ALL CHECKS PASSED'                                         'uefi-its: MSI-X through the ITS the MADT names'
      assertnot '\[trap\]|PANIC|its: FAILURE'                                'uefi-its: no trap, no failed check'
    fi
  else
    echo "== UEFI on arm64: SKIPPED — no fresh stub (kernel/uefi-stub/build.sh aarch64 after the kernel build) =="
  fi
  # ── the network (docs/ARM64-PLAN.md §10.3 step 1): virtio-net over the tree's virtio-mmio
  # windows. The guest and its clients share a tap, so the block runs INSIDE the stand image
  # (`kernel/test-aarch64-net.sh`), which needs NET_ADMIN and /dev/net/tun; its PASS/FAIL
  # lines are printed as they are and its totals are added to the gate's.
  if command -v docker >/dev/null 2>&1 && [ -c /dev/net/tun ] \
     && docker run --rm talaria-qemu-aarch64 sh -c 'command -v ip && command -v socat && command -v python3' >/dev/null 2>&1; then
    E2=""; [ -s "$REPO/kernel/target/a64-disks/ext2.img" ] && E2=kernel/target/a64-disks/ext2.img
    NETLOG=$(mktemp)
    docker run --rm $KVMDEV --cap-add NET_ADMIN --device /dev/net/tun -v "$REPO:/w" -w /w \
      -e TAL_IMG="$IMG" -e TAL_ACCEL="$ACCEL" -e TAL_KVM="$KVM" -e TAL_DISK="$DISK" -e TAL_EXT2="$E2" \
      -e TAL_SVC="$([ -d "$REPO/kernel/target/a64-services" ] && echo kernel/target/a64-services)" \
      talaria-qemu-aarch64 bash kernel/test-aarch64-net.sh > "$NETLOG" 2>&1
    grep -vE '^NET TOTAL' "$NETLOG"
    NP=$(sed -n 's/^NET TOTAL: PASS=\([0-9]*\) FAIL=\([0-9]*\)$/\1/p' "$NETLOG"); NF=$(sed -n 's/^NET TOTAL: PASS=\([0-9]*\) FAIL=\([0-9]*\)$/\2/p' "$NETLOG")
    if [ -n "$NP" ] && [ -n "$NF" ]; then PASS=$((PASS+NP)); FAIL=$((FAIL+NF)); else echo "  FAIL  net: the network block did not report its totals"; FAIL=$((FAIL+1)); fi
    rm -f "$NETLOG"
  else
    echo "== the network on arm64: SKIPPED — it needs docker, /dev/net/tun and iproute+socat in the talaria-qemu-aarch64 image =="
  fi
else
  echo "== ring 3 on arm64: SKIPPED — no kernel/user/aarch64 binaries here (make -C kernel/user aarch64) =="
fi

# PHASE 7's first step: the small open-source services, cross-built for arm64 by
# services/build-aarch64.sh into kernel/target/a64-services (unmodified sources, static musl).
SV=kernel/target/a64-services
if [ -f "$REPO/$SV/disk-lua.img" ]; then
  echo "== open source on arm64: Lua, SQLite, QuickJS, BusyBox from a TFS1 disk =="
  for spec in "lua|Lua OK on Talaria|fib.30. = 832040" "sqlite|SQLite 3.46.1 on Talaria OS|rows=4 sum=12.0 avg=3.0" \
              "quickjs|QuickJS OK on Talaria|fib.28. = 317811" "busybox|busybox 1.36.1 on Talaria OS|sum.i.2,1..1000.=333833500"; do
    m=${spec%%|*}; r=${spec#*|}; p1=${r%%|*}; p2=${r#*|}
    [ -f "$REPO/$SV/disk-$m.img" ] || { echo "  SKIP  $m: no $SV/disk-$m.img"; continue; }
    bootx 150 "talaria.mode=$m" "$SV/disk-$m.img"
    assert "$p1"                                                             "opensource ($m): the unmodified program runs to its verdict in EL0"
    assert "$p2"                                                             "opensource ($m): and computes the right answer"
    assertnot '\[trap\]|PANIC|an unknown number|\[fault\]'                    "opensource ($m): no trap, no fault, no unknown syscall"
  done
else echo "== open source on arm64: SKIPPED — no $SV (services/build-aarch64.sh with the aarch64 musl cross) =="; fi

echo "== observability on arm64: PMUv3, the profile with stacks, the gdb stub =="
# Under TCG the instructions counter is modelled only with -icount; with it every
# instruction is one cycle, so the IPC is exactly 1.00 — a deterministic judge of the
# counters and of the 32-bit widening across a wrap (talaria.pmunarrow=1).
A0=$ACCEL; IC=0; [ "$ACCEL" = "-cpu max" ] && { ACCEL="$ACCEL -icount shift=0"; IC=1; }
boot 60 "talaria.test=wakeprobe"
assert 'wakeprobe: a store under wfe: the median wake after 30 us, 100 us, 300 us, 1 ms of sleep: [0-9]+ [0-9]+ [0-9]+ [0-9]+ ns' 'wakeprobe: a store wakes a wfe sleeper, timed after four lengths of sleep'
assert 'wakeprobe: an SGI under wfe: the median wake after .* [0-9]+ [0-9]+ [0-9]+ [0-9]+ ns' 'wakeprobe: an SGI wakes a wfe sleeper, timed'
assert 'wakeprobe: an SGI under wfi: the median wake after .* [0-9]+ [0-9]+ [0-9]+ [0-9]+ ns' 'wakeprobe: an SGI wakes a wfi sleeper, timed'
assert 'wakeprobe: OK'                                                     'wakeprobe: every round woke'
boot 120 "talaria.test=pmu"
assert 'talaria: the PMU is on \(PMUv3'                                    'pmu: PMUv3 found and programmed (cycles + event counters)'
assert 'pmu: [0-9]+ iterations \(rdpmc\): cycles=[0-9]+ instrs=[1-9][0-9]* IPC=' 'pmu: cycles AND retired instructions counted'
[ $IC = 1 ] && assert 'IPC=1\.00'                                          'pmu: under -icount the IPC is exactly 1.00'
assert 'pmu: OK'                                                           'pmu: the verdict'
# The machine's counters into the gate's output: on the stand that is the record of what a
# Neoverse core's PMU looks like through KVM (the version, the width, the events found).
grep -E 'talaria: the PMU|pmu: [0-9]+ iterations|pmu: LLC|pmu: branches' "$LOG" | cut -c1-200 | sed 's/^/      /'
boot 120 "talaria.test=pmu talaria.pmunarrow=1"
assert 'the width: 32-bit, widened by the reader'                          'pmu (narrow): the no-CHAIN path is taken on request'
assert 'pmu: across the 32-bit wrap: instrs=[0-9]+ cycles=[0-9]+ — the widening holds' 'pmu (narrow): a counter set below 2^32 is read correctly across its wrap'
ACCEL=$A0
boot 60 "talaria.test=dirtyprot"
assert 'dirtyprot: OK'                                                     'paging: a written page made read-only keeps its dirty bit (protect, and arm64'"'"'s encoding of a read-only page)'
boot 90 "talaria.test=obs"
assert 'unwind: the unwinding checks all passed'                           'obs: the unwinder on a synthetic chain'
assert 'kstat cache: the PMU — [0-9]+ GP \+ 1 fixed counters'              'obs: kstat names the counters'
assert 'obs: ALL OK'                                                       'obs: every observability command runs to the end'
assert 'kstat sync: the wait primitive is wfe'                             'obs: kstat names the arm64 wait tier'
boot 90 "talaria.test=console"
assert 'perf: SUMMARY  IPC '                                               'console: perf computes the IPC from the counters'
assert 'perf: SUMMARY  IPC [0-9]+\.[0-9]+, LLC misses - \(no counter\)'      'console: an event without a counter is "-", not a zero (QEMU declares no cache event)'
if [ -f "$REPO/kernel/user/aarch64/profdeep" ]; then
  boot 120 "talaria.test=profdeep"
  assert 'profdeep: ALL OK'                                                'profdeep: the workload'"'"'s stack unwinds on a live workload (five levels, x29 chain)'
  assert 'SAMPLE 3 [0-9]+ [0-9]+ [^ ]+ 0x[0-9a-f]+ 0x[0-9a-f]+ 0x[0-9a-f]+'  'profdeep: perf record dumps STACKS of ring 3 (the frame'"'"'s x29 and its EL0 mode read right)'
  assert 'perf: WORKLOAD .ring 3. — [0-9]+ samples, average depth [3-8]'   'profdeep: perf top shows the stack depth'
  assertnot 'obs: FAIL|THERE ARE FAILURES|\[trap\]|PANIC'                  'profdeep: no failed check, no trap'
  boot 120 "talaria.test=obslive"
  assert 'obs-live: ALL OK'                                                'obslive: time, syscalls by thread, switches and the profile on a live workload'
fi
if command -v python3 >/dev/null 2>&1; then
  # The stub on the console's line: the registers (gdb's aarch64-core: 268 bytes = 536 hex),
  # the threads, two software steps, an hbreak set and cleared, detach.
  bootrsp 60 "talaria.mode=gdb" "" 1251 $RSP PORT '?' qSupported g qfThreadInfo s s 'Z1,40080000,4' 'z1,40080000,4' D
  assert '^first: T05thread:1;core:0;'                                      'gdb-stub: the machine stops in the stub and says so over the console'"'"'s line'
  assert '^qSupported -> PacketSize=400'                                    'gdb-stub: qSupported'
  assert '^g -> [0-9a-f]{536}$'                                             'gdb-stub: g gives x0..x30, sp, pc, cpsr'
  assert '^qfThreadInfo -> m1'                                              'gdb-stub: the thread list'
  assertcount '^s -> T05thread:1;core:0;' 2                                 'gdb-stub: the software step stops after one instruction (twice)'
  assertcount ' -> OK$' 3                                                   'gdb-stub: hbreak (Z1/z1) and detach (D) answer OK'
  if [ -n "$GDB" ]; then
    bootrsp 60 "talaria.mode=gdb" "" 1253 $GDB -batch -nx -ex 'set remotetimeout 20' -ex 'target remote :PORT' -ex 'bt 3' \
      -ex 'info registers pc cpsr' -ex stepi -ex 'info threads' -ex detach "$REPO/${IMG%.img}"
    assert '#0 .* in talaria::runmode::run'                                 'gdb-stub (host gdb): attaches over the console'"'"'s line and names the stopped function'
    assert '^cpsr +0x[0-9a-f]+ +\[ .*EL=1'                                  'gdb-stub (host gdb): the registers, cpsr decoded'
    assert '^\* 1 +Thread 1'                                                'gdb-stub (host gdb): the threads'
    assert 'Inferior 1 \(Remote target\) detached'                          'gdb-stub (host gdb): detach'
  else echo "  SKIP  gdb-stub (host gdb): no gdb that knows aarch64"; fi
else echo "  SKIP  gdb-stub: no python3"; fi

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
