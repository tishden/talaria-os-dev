# Talaria OS — changelog

The release history. The numbers and their analysis are in
`docs/MEASUREMENTS.md`, the current status in `README.md`, and what is still open
in `docs/ROADMAP.md`.

Versions before 1.0 carry no compatibility contract: the `cell.manifest` format,
the `talaria.*` boot parameters, the Talaria-specific syscalls (1000+, 4600+) and
the libtsync API may change without migration.

---

## Unreleased — the arm64 port (branch `arm64-port`, 2026-09-29 … 2026-10-22)

Talaria runs on AArch64: QEMU `virt`, a `c7g.metal` under KVM, and EC2 Graviton AMIs
(`c7g.large`/`xlarge`, UEFI via edk2, ENA with LLQ, NVMe, the GICv3 ITS). The plan,
the phases, every gate and every number are `docs/ARM64-PLAN.md`; the first arm64
scoreboard against Linux on a Graviton3 is `docs/MEASUREMENTS.md` §10.13-10.15.

### Added

- **The arm64 kernel** (`kernel/build-aarch64.sh`, `kernel/uefi-stub/build.sh aarch64`,
  `kernel/test-aarch64.sh` + `-net`/`-db`/`-pwr`): one source per ring-3 test for both
  machines, `arch/aarch64` beside `arch/x86_64` behind one `arch` facade; the six
  workloads (PostgreSQL, MariaDB, MongoDB, Redis, OpenJDK 21, ClickHouse) on Graviton.
- **The direct map** (ARM64-PLAN §10.16): the kernel image in the higher half with W^X,
  no RAM identity in a process space by default (`talaria.dmap=strict`), the kernel's
  memory from any RAM, a `Pa` newtype for everything a device reads.
- **SNTP on Graviton** (`talaria.ntp=aws`, ±21 us), PMUv3 and the profiler's stacks,
  a gdb stub over the console.
- Gates and instruments from §11: `umark` (the workload names its slow intervals),
  `gatecost`, `wakedef`, `wakesleep`, `sleeplat`/`sleeplath`/`sleeplong`, `pinmove`,
  `txloss` (`talaria.txdrop=N`, the loss gate).

### Fixed (shared code, found by the second machine or by the arm64 scoreboard)

- Three races of the shared code a loaded TCG host found (ENGINEERING-NOTES §6.1); the
  stale root reloaded by the switch stub (PostgreSQL on a `c6i.xlarge`); atomicity in
  pipe/eventfd/signals/the COW refcount; the fork slot.
- The syscall gate priced a read by its buffer, not by what the descriptor could fill
  (an empty `recvfrom` of 1 MiB: 4.3 us → 0.45 us on a Graviton).
- `nanosleep` was a loop of 200-us windows (the Graviton mdpipe tail: p99 36 → 5 us);
  a woken thread waited for the slice, not the preemption threshold; a timed sleep behind
  a running neighbour woke at the core's long tick; the `talaria.test=` modes' secondary
  cores had no tick and starved a second thread; `sched_setaffinity` moved a thread only
  from the boot core.
- The TCP retransmission floor was the RFC's 1 s (smoltcp 0.14, now vendored at Linux's
  200 ms): one lost segment stalled the load generator exactly 1.000 s.

### Gate

See ARM64-PLAN §11 item 6 for the pre-merge run of both regressions and `validate.sh`.

---

## v1.13 (2026-09-28)

The open-source release. The tag points at the tree of this entry (583 files,
history started afresh with one commit); everything after it sits as commits on
top. The performance numbers did not change — they were taken on this same code
and are listed under "the performance programme" below.

### Added

- **A hands-on guide** (`docs/HANDS-ON-GUIDE.md`) plus the example
  `examples/mdpipe`: multithreaded C++ from a build to a post-mortem —
  monitoring, core placement, profiling, live debugging, a core dump, `filed`,
  QEMU and EC2, with real output throughout.
- **Workload pointer validation in syscalls.** One check on entry
  (`user_range_ok()`): the whole range the kernel will read or write must lie in
  the process's own memory, or `-EFAULT` as on Linux, with the kernel untouched
  (gate `talaria.test=efault`; ~10 ns per call with a pointer, and the knob
  `talaria.uacheck=0` exists only to measure that). **SMEP** is enabled where the
  CPU has it (CR4 bit 20 by CPUID 7): ring 0 cannot EXECUTE a workload's page.
  SMAP was rejected with a reason — the kernel copies process memory with
  ordinary loads, and that is what makes a syscall cheap.
- **Host unit tests grew 189 → 264** (`kernel-unit`): ext2 structures and the
  htree hash, journal replay against an IN-MEMORY journal, thread selection and
  the `reqfirst` policy, the stealing rule, wait predicates and parking
  hysteresis, cmdline parsing and the knob registry, AES-128-GCM against known
  vectors, datapath ring arithmetic with wrapping differences, and the ARP the
  cell answers itself. Coverage is measured (97.53 % of regions, 98.53 % of
  lines) and CI holds a FLOOR of 95 % of lines — which catches not "too few
  tests" but a module linked into `kernel-unit` with none at all.
- **`unsafe` with written justifications** in the eight files on the boundary with
  the workload (`syscall.rs`, `uaccess.rs`, `user.rs`, `proc.rs`, `sync.rs`,
  `sched.rs`, `fs.rs`, `ext2.rs`): every block and every `unsafe fn` justified
  against NAMED module invariants (W1-W4, C1-C4, D1-D4, E1-E7). The lint
  `unsafe_op_in_unsafe_fn` is on where the contract allows and off with a written
  reason where it does not.
- **Open-source mechanics**: Apache-2.0 (`LICENSE`, `NOTICE`), a CLA without a bot
  (`CLA.md`, `CLA-SIGNERS.md`), `CONTRIBUTING`/`SECURITY`/`SUPPORT`, issue and PR
  templates (which require the boot line, the stand, the serial log, a noise floor
  and a gate name), an export of the public tree with hard checks and a secret
  scan (`tools/export-public.sh`, `tools/export-check.py`, gitleaks), a
  DETERMINISTIC regression subset for CI (`TAL_CI=1`, 175 asserts, 6 min 35 s) and
  the knob `TAL_IMAGE=` — the stand's image pins the QEMU version every number in
  `docs/` was measured against.

### Fixed

- **Memory isolation.** In a process's address space the identity window of
  physical memory (up to 1 GB) was reachable from ring 3 — a workload could read
  page tables, kernel stacks and other processes. The window is closed; access to
  identity memory that was explicitly handed out (shm, `MAP_SHARED`, the
  zero-syscall region) is explicit. Gate `isolation`.
- **Thread death as on Linux.** A thread killed in the middle of a syscall
  finishes it and releases locks and the journal; waiters see the death as a
  signal; the dump is written after the sisters are stopped and shows where each
  of them stood in its own code; restarting a multithreaded cell on SMP (after a
  crash, by watchdog and by the operator) works. Gates `restart-mt`,
  `restart-wdog`.
- **Three races and a durability defect, found by WRITING the justifications and
  the tests rather than by running anything.** (1) The APPLY pass of journal
  replay was not bounded by the scan pass — after a power cut the kernel could
  write somebody else's earlier epoch over live metadata (fixed as in
  `jbd2_journal_recover`). (2) `null_alloc_kind` handed ONE pseudo-fd to two cores
  calling `eventfd`/`timerfd` at once (now a CAS). (3) Finding and claiming a free
  row in the shm tables were separate, and the gap between them cannot be closed
  by a CAS — `shm_lookup`/`shm_open_fd`/`sysv_shmget`, the shm branch of
  `vfs_open_at`, both `stat` paths, `mmap` over an shm fd and the SysV syscalls
  29/30/31 are now under `syscall::shm_lock()`, with gate `shmrace` (four threads
  on a spin barrier, three phases) and an A/B knob `talaria.shmlock=0`.
- `CLOCK_MONOTONIC` in ring 3 went BACKWARDS on a clock adjustment (by up to
  520 µs); `top`/`ps` attributed a thread's sleep to its CPU %; `perf record`
  printed something other than a snapshot; a cell thread crashing on an AP wrote
  no dump and left its sisters alive; thread numbers in a core dump; tokens in the
  log are printed as `***`; the gdb stub learned `break`, `hbreak` and `detach`.
- `proc`: a dead process's page tables are not freed while an idle AP still holds
  them.

### Changed

- **The gdb stub** switches threads (`Hg`, `qC`, `T`), takes up to three `hbreak`,
  and supports live debugging on SMP in all-stop mode (breakpoints on every core,
  the rest parked, stepping with IF=0); `talaria.gdb=break` stops before the
  workload's first instruction.
- **Code hygiene**: the kernel moved to edition 2024; the clippy allow-table went
  40 → 18 entries, each with a reason and a count of places, and clippy
  `-D warnings` is a gate (~120 places fixed); two kernel files stopped being
  "binary" for `grep` and `file` — a raw NUL byte was sitting inside a string
  literal.
- **The entry point and the guides are in English**: README,
  `CONTRIBUTING`/`SECURITY`/`SUPPORT`/`NOTICE`/`CLA`, everything under `docs/`,
  the subdirectory READMEs, the comments in `kernel/src` and the kernel's console
  messages.
- **Building from a clone was verified end to end.** Step 0 of the quick start was
  DEAD — the stand's images were built on Fedora 40, which went EOL together with
  its `updates` metadata; four separate causes were fixed, of which only the first
  was a defect of the tree. The kernel built IN A CLONE matched the private tree's
  kernel BYTE FOR BYTE, and the regression was re-run on the image from that clone
  (QEMU 8.2.2) with the same result — the version pin is confirmed by measurement,
  not assumed.

### Gate

`kernel/test.sh` **486/486 FAIL=0** plus the host's **22/22** with no SKIP;
`kernel-unit` **264/264**; clippy `-D warnings` clean in both kernel
configurations; `kernel/validate.sh` — 9 gates green plus PostgreSQL, MariaDB,
MongoDB, OpenJDK 21 (speedup4 = 4.006) and ClickHouse, with a power cut on
MongoDB reproducing the database. On a fresh clone `test.sh` gives 483: three
gates want a third-party loader, and the reasons are in `CONTRIBUTING.md`.

---

## v1.13 — the performance programme (2026-09-12 … 2026-09-21)

Fourteen sessions on a stand of four `c6i.xlarge` (a Talaria server and a Linux
server, each with its own generator, two cores per workload on both sides). The
regression went **438 → 458 FAIL=0** plus the host block;
`bench/suite/selfcheck.sh` 71/0; `kernel/validate.sh` 9/9 plus three databases
plus JVM plus ClickHouse. The public (exported) tree builds from source and
passes the regression: 455/455 with two honest SKIPs.

**The final scoreboard — 295 comparable points on six workloads** (mongodb,
mongoose, mysql, nginx, postgresql, redis): p50 66/7/1, p99 69/5/0,
p99.9 65/7/2, p99.99 63/2/8 (faster / parity / slower). All eleven lost points
are analysed by name and closed by an instrument rather than an explanation —
`docs/MEASUREMENTS.md` §2.

### Fixed

- **`fsync` spun waiting for EBS instead of giving the core up.** The device lock
  was held BY CORE, and yielding under it was forbidden. With the lock owned by
  the TASK, plus writing the descriptor without the FS lock, the `mongodb insert`
  p99.9 tail went **4.3 ms → 0.44**. FUA was not the culprit (checked with the
  `talaria.fua` knob) — our own spin was.
- **A doorbell to the NIC gave up silently.** `try_take` in `l1.rs` skipped the
  doorbell when a waiter on a neighbouring core held the card for a microsecond,
  and the answer lay in the ring until the next pass — a `mysql pk` p99 shelf of
  **+100 µs** exactly on the request grid.
- **An exact deadline muted the scheduler**: borrowing the LVT on a shared core
  stopped the tick for the whole timeout (a ready mongod thread waited 216 ms).
  Deadlines further out than one service period are served by the tick.
- **Connection-thread starvation 1167 ms → < 10 ms**: `fallocate` held the FS lock
  with `IF=0` for whole milliseconds; a preemption window now opens once per batch
  of blocks.
- **Four SMP races, each a rare crash under load**: a released task's stack still
  in use (`STACK_BUSY`), the BSP losing a live context in the migration window
  (`BSP_LIMBO`), an AP claiming a task on ownership checked BEFORE marking
  (`ap_claim`), and the pickup loop entering a task with `IF=1`.
- **Polling COM1 from the tick cost 2700 hypervisor exits per second** on core 0
  under postgresql; it is now time-based and `talaria.console=nopoll` applies from
  user-data too.
- Two instrument defects that faked findings: the age of a dirty block was
  computed as an UNSIGNED difference (a fresh block looked infinitely old; gate
  `agewrap`), and the "ns per call" column overflowed `u64` (gate `sysns`).

### Added

- **`talaria.tickmin=<ns>`, default 50 µs**: a tick rotation does not displace a
  task that has run for less than the threshold. `postgresql select1` p99
  **80 → 67 µs** (Linux 63); the class of 40-60 µs rotations 3.7 % → 0.07 %.
- **`talaria.iowin=`, default 2**: while waiting for the device the core is GIVEN
  to a ready thread (the "let the tick through" variant turned out 2.3-6.7× worse
  than the previous spin). `mongodb insert` p99.9 became a FLAT 536-745 µs against
  a 593-4793 spread on the baseline. Beside it, `talaria.wrwin=`.
- **`fdatasync` in two EBS round trips instead of three**: **6224 → 5018 µs**; the
  last chunk of data is submitted without waiting and is awaited by the descriptor
  batch of the same transaction. Verified by a power cut: 210332/210332.
- **`talaria.fscnt=0`**: `jrn_pending` and a second `writeback_locked` pass were
  walking the WHOLE cache three times per call — 320 µs of pure CPU. Cost of an
  `fsync` call **3010 → 2753 µs** at a higher call count.
- **`talaria.jrnfree=1`**: the journal is no longer held during the "data in
  place" phase — waiting for a foreign transaction disappeared ENTIRELY (185 932 µs
  in one episode → zero episodes), call cost **2801 → 2602 µs**.
- **Age-based pre-emptive flush of dirty blocks**: `talaria.dirtyage=`,
  `dirtykick=`, `dirtybatch=`, with a `DN_OLD` clock instead of a cache scan.
  **Default 0 — off, by measurement** (see below).
- **`talaria.reqfirst=<ns>`** (default 0, off): a thread with a request in flight
  does not yield the core to a network waiter until N ns have passed since the
  request started. The mechanism works (four times fewer stops mid-request) and
  buys no latency — see below.
- **The instruments that found all of the above**: a tick hole with its OWNER
  (how much of the hole the hypervisor took), `fsync` phases with a per-task
  remainder (gate `fsyncph`), the per-thread `app` stage split into "on the core"
  and "off it" (`talaria.appprof=1`), the logical-to-physical core layout
  (`talaria.smtpack=`), per-syscall call cost on request (`talaria.sysrow=`),
  batched slot eviction (`talaria.evictbatch=`), the latency band
  (`bench/suite/lib/bandscan.py`), and "who got the core from a thread with a
  request in flight" (gate `reqwho`).

### Rejected by measurement

So that the same ground is not walked twice. The full table with the numbers is
`docs/ENGINEERING-NOTES.md` §7: group commit and async commit (a ceiling of
exactly 0 µs — there is nothing to coalesce), `dirtyage=30` as a default (takes
138 µs off `fsync` and adds 501 µs to `write`), curing the 140-400 µs band with
the quantum length or with priority against the network waiter (both written, both
measured, both left off), `smtpack=1` as a default, and the two tail classes that
did not survive a second bring-up.

**The "foreign 8 ms" class is not our code**: the hole consists 97 % of vCPU theft
by the hypervisor (8092 µs of 8277) and arrives on both cores at once.

### Five INSTRUMENT defects, each of which faked a class

Recorded deliberately: half of what was "rejected by measurement" in the earlier
sessions had to be re-opened because of them. Our own generator did not sleep, so
the host's 8 ms quantum was written into the SERVER's tail (fixed by making
`talaria.netidle=1` the generator's default: `mongodb p99.99` 5/0/10 → 12/0/0). A
dead generator agent cost a whole workload's scoreboard. `talaria.stat=` measured
its own printing into the console. `run-remote.sh` selected a case by filter while
`netprof` is cumulative. The runner's VPN changes its address and the stand's
security group stops letting it in. The rules that came out of this are in
`docs/ENGINEERING-NOTES.md` §§1-2 and are worth more than any individual fix.

---

## v1.13 — observability (2026-08-31)

Regression **248 → 278 FAIL=0** plus 22 host checks; the build has no warnings.

- **`top` / `ps`** — processes and threads by name: core, `%CPU`, RSS (resident,
  shared, lazily reserved), allocation rate, context switches and syscalls per
  second, faults per second, L3 share, state, CPU time and lifetime, with a header
  of per-core occupancy, memory, hardware counters, FS and network. `%CPU` is
  computed from TSC cycles, not ticks: the main thread lives on a dedicated core
  where there is no timer at all, and a tick-based `top` would show `0 %` for
  exactly the thread everything is built for. RSS is computed by walking the page
  tables and only on request.
- **`netstat`** — interfaces (address, state, driver, MAC, card frames and bytes,
  transmit drops), routes, sockets (local and remote address, TCP state, traffic,
  interface and the OWNING THREAD), and the datapath.
- **`kstat <section>`** — kernel counters by section (sched/mem/paging/fs/net/
  sync/irq/cache/syscalls); counters whose correct value is zero are marked.
- **`perf`** — hardware counters per core: cycles, instructions, IPC, LLC misses
  and branch mispredictions per thousand instructions. `perf poke <core>` takes a
  snapshot of a DEDICATED core at the price of one IPI.
- **A sampling profiler with CALL STACKS**: eight frames unwound from RBP inside
  the tick, resolved to symbols on the host (`kernel/tools/talaria-prof` →
  `addr2line`, flamegraph). On a live ClickHouse under QEMU, 98 % of kernel time
  turned out to be in one stack (`wait_on_ex ← do_read ← dispatch`) — which only
  the stack shows: the leaf alone says "the thread is waiting", not "waiting in
  `read()`".
- **ELF core dumps** (`ET_CORE`) that `gdb <binary> <core>` opens and shows a `bt`
  with function names and line numbers; the same for the kernel, and a `core`
  command that dumps a live task without stopping the machine.
- Fixed while doing this: in `mode=proc` (the database and ClickHouse mode) the
  operator's console was not polled AT ALL; the kernel printed "HUNG" for every
  idle pool thread; and — while fixing the first of those — polling COM1 from the
  tick uncovered a REAL page-table race (a new table was installed into its parent
  entry by a plain write, two cores lost each other's work, and a thread lost the
  page it had just written to). Closed with a CAS; gate `memrace` on four cores,
  20 clean runs of 20.
- Also found: a latent "the failure depends on the image size" defect — the frame
  allocator handed out memory from the 2 MB supervisor leaf containing the
  kernel's tail, so a workload stack landing there died on a `#PF`; 70 KB of new
  statics was enough to expose it.
- The method is the new guide `docs/PERFORMANCE-OPTIMIZATION-GUIDE.md`.

---

## v1.13 — the pre-open-source review (2026-08-31)

Regression **248 → 253 FAIL=0**, build warnings **127 → 0**.

- **A boot mode was selected by SUBSTRING search, and that ran foreign modes.**
  `cmdline.contains("talaria.test=fs")` is true for `talaria.test=fsbig` as well —
  and with `fsbig` the `fs` block really did execute. The class touched 11 names
  out of 106. `kernel/src/cmdline.rs` now compares whole names, a collision is
  impossible by construction, and an unknown name is no longer silent.
- **Control-plane tokens were hard-coded in the sources.** `/ctl` and `filed` (a
  service that WRITES files onto the node) used a constant, and the same password
  was in the client. Tokens are now given at boot only
  (`talaria.ctltoken=`, `talaria.filedtoken=`), with no default — without a token
  the service is closed entirely — and comparison is constant-time.
- **Inspecting virtio-mmio windows turned somebody else's card off**:
  `MmioTransport` resets the device on `Drop` and `find_net_mmio` had no guard.
- **`rust_main` was a 1740-line function** — machine bring-up interleaved with
  ~90 built-in gates and production modes. `main.rs` 2257 → 539 lines (bring-up
  only), gates moved into `selftest/` by topic, production modes into `runmode.rs`.
- Disk images left the repository (256 MB of derived files) and are built in
  tmpfs: disk writes per run fell from ~2 GB to 0.24 GB.
- The target spec stopped contradicting itself (`x86_64-talaria.json` declared a
  softfloat ABI with `+sse,+sse2,+aes` enabled); the crate was renamed
  `talaria-poc` → `talaria`; the dead `net.rs` and `trade.rs` modules were removed.
- **The Intel 82599 VFIO driver was opened up**: `private/ixgbe` → `bench/ixgbe`.
  It is the second half of the proof — the same pipeline on a live card with no
  VMEXIT gives `rx→tx` of 14 ns (min) and 52 ns (avg) against ~4 µs under QEMU.

---

## v1.12 (2026-08-29) — two large third-party runtimes

- **Unmodified OpenJDK 21** (Temurin, musl) passes the whole multithreaded
  benchmark on four cores: JIT, GC, CDS, threads. Against Linux on the SAME
  hardware with the same JDK and the same class — thread wakeup p50 2.19 µs
  against 5.81, `parkNanos(1 ms)` overshoot 19.4 against 62.7 µs, monitor
  contention 23.7 against 15.5 M acquisitions/s, message queue 5.27 against
  2.77 M/s, `System.nanoTime()` 52 ns against 65 (`services/jvm/README.md`).
- **The official ClickHouse 25.8.33.6 binary** executes queries (`clickhouse
  local`: configuration, planner, vectorised engine, aggregation and GROUP BY over
  millions of rows) — `services/clickhouse/README.md`. To have anywhere to put an
  ET_EXEC binary whose segments occupy 2 MB..526 MB, the **kernel's link base
  became a build parameter** (`--defsym KBASE=`).
- What the kernel needed for them: **synchronous signals** (SIGSEGV from a #PF
  with `siginfo`/`ucontext` — HotSpot catches it on every implicit null check and
  every safepoint poll), **demand-zero memory** (`PTE_LAZY`: 60 829 frames instead
  of 874 308 at `-Xmx256m`), a real `PROT_NONE`, `MADV_DONTNEED`,
  `MAP_FIXED_NOREPLACE`, **cross-core TLB shootdown**, `execve` of a dynamic image
  with environment and auxv, **XSAVE instead of fxsave**, and the full FPU state in
  a signal frame.
- **Six real kernel bugs that only a runtime this size exposes**: a timed wait on
  an AP borrowed the LVT timer for a TSC deadline and never returned it; the
  user-rsp substitution for signal delivery lived in per-CPU area and was taken by
  a neighbouring task; a kernel stack and address space were reused while the task
  was still physically on a core; `FUTEX_WAKE` kept handing the wakeup to the same
  already-woken waiter; and two places lost the upper halves of YMM registers.
- Debts of the same session closed: **ext2 symbolic links** (fast and long,
  traversal in the middle and at the end of a path, `readlink`/`symlink`, ELOOP),
  **multithreaded `execve`**, **ELF segment loading in 2 MB batches** (ClickHouse
  start 4.5 s → ~1 s), **clustered memory materialisation** (64 KB per fault:
  ~500 faults instead of ~7400), and **a vDSO** — `clock_gettime` in user space at
  15 ns against 127 through a syscall.
- **Workload parallelism**: a four-thread ClickHouse query ran THREE TIMES slower
  than a single-threaded one. Three causes: `/proc/cpuinfo` returned ONE record
  (workloads sized their pools from it and thought the machine had one core); a
  woken thread waited for another core's TICK, up to 500 µs (now a resched IPI
  with a hint goes to the core running the sleeping task); and an idle core did
  not take work from an overloaded neighbour (work stealing added).
- **A workload's error no longer stops the machine**: if a signal has nowhere to
  be delivered, the process is killed, as Linux would do, and the kernel carries
  on.

---

## v1.11 (2026-08-29) — the operator's side

- The console stopped being a list of fourteen words: a line editor (history,
  arrows, Ctrl-A/E/U/K/W, **Tab completion of commands AND paths**), grouped help,
  a suggestion on a typo.
- **File work inside the kernel** — `ls`, `cat`, `less`, `head`/`tail`, `hd`,
  `cp`/`mv`/`rm -r`, `df`, and a full-screen **`edit` (a subset of vi)**. busybox
  is deliberately not used for this: it is a ring-3 workload, and the console is
  needed exactly when ring 3 is not there.
- **Files upload and download over TCP** (`talaria.mode=filed` plus
  `kernel/tools/talaria-file`) with an rsync-style delta: a matching file is not
  transferred at all, and a 10-byte edit in a 200 KB file travels as ONE 8 KB
  block (4 %).
- **Real networking**: a node brings up ALL the machine's cards as separate
  interfaces (`net0`, `net1`, …), each with its own address and DHCP, and routes
  are chosen from a TABLE by longest prefix (`ip`/`route`/`dhcp`/`dns` in the
  console, `talaria.net=`/`talaria.route=` in the cmdline).
- Along the way, a defect of the class "inspecting a device turns it off": scanning
  virtio-mmio windows killed an already working second card — and latently would
  have killed the network when mounting the FS on a machine whose disk comes after
  the cards.

---

## v1.10 (2026-08-28) — clocks, shared L3, a device model

- **Clocks are disciplined.** `CLOCK_REALTIME` no longer runs from a single
  calibration point but is pulled towards an external clock (kvmclock under KVM,
  the ENA device's PHC on hardware) by a servo that adjusts FREQUENCY, not value:
  time stays monotonic and backward steps are counted separately. The cost of the
  old behaviour was measured — the TSC calibration was off by **95 ppm, 8.2 seconds
  a day** — and the residual error against the host is now **−4.8 µs**.
- **Shared L3 is divided.** `llc=` in the manifest hangs a cache share on the same
  group of cores as the v1.9 placement (Intel CAT directly in the kernel — a mask
  in an MSR and a class per core), and the DMA rings' footprint against L3 is
  computed automatically so that leaky DMA cannot return silently.
- **A new network card is one file and one table line.** The card stopped being an
  enumeration with twelve `match` arms in the datapath: there is a `NetDev` trait
  and a driver table (`netdev.rs`), the driver keeps its own bookkeeping, and the
  datapath, the stack and the cell know nothing about cards. MSI is parsed
  alongside MSI-X (a card without MSI-X used to come up silently and silently not
  interrupt). Gate `talaria.test=nicdev`.

---

## v1.9 (2026-08-28) — the manifest places threads on cores

- A CPU core is now either **dedicated** (one thread, no LAPIC timer armed, run to
  completion — what Talaria exists for) or a **shared-pool core** (round-robin by
  tick), and the scheduler learned what it could not do before: hold SEVERAL
  threads on a secondary core.
- Placement is declared by RULES, not by numbers: `pin=md_*:dedicated` means "a
  dedicated core for each such thread" and the OS picks the number, gathering one
  process's threads onto one NUMA node (SRAT). Between "a specific core" and
  "anywhere" there are **named pools**: `pool=fast:1,2` names the cores once,
  `pin=md_*:fast` sends threads there by name pattern, and the layout within the
  pool is the OS's work. Pools do not overlap, their cores are not given to
  dedicated threads or to foreign ones, and a pool thread sees exactly its pool's
  cores in `sched_getaffinity`.
- Threads with no rule spread over the shared pool; if there are more of them than
  cores, the load is balanced **by demand** (a thread asleep on a socket does not
  load a core) and with hysteresis, so in the steady state there are zero
  migrations.
- **NUMA reaches memory**: the stack, heap, private mappings (malloc arenas), COW
  copies and the FS block cache all come from the node's region, and `node=<N>`
  names the node explicitly. Device interrupts stop arriving anywhere: `irq=` sets
  the core, and a dedicated core will not be given them.

---

## v1.8 (2026-08-27) — SMP and networking on real hardware

- **SMP works on a real machine**: on `c6i.metal` threads migrate to secondary
  cores and compute in parallel — speedup **2.00** on two cores and **4.00** on
  four (under a hypervisor the same tests give 1.86 and 3.59, and the difference
  is vCPU preemption). The cause of three sessions of silence was not the bring-up:
  a logical core number was being confused with an APIC id, and on bare metal
  those are sparse (0, 2, 4, 6 with the HT twins between them). The core ceiling
  went 4 → **64**, `getcpu` was added, and `sched_setaffinity` to a non-existent
  core stopped lying with success.
- **Networking works on hardware too**: ENA on `c6i.metal` comes up fully — the
  device offers 32 queue pairs and 132 MSI-X vectors, a 4 MB LLQ window with
  write-combining, a BAR doorbell of 35 ns, an address from the VPC by DHCP. The
  cause of the silence: in BIOS mode the firmware does not assign a BAR to
  something it does not boot from, and the kernel was handing out its own address
  outside the bridge's window, so every register read as `0xFFFFFFFF`.
- **The production mode runs on hardware whole** — the application in ring 3 in
  its own address space, **15 cores** under `talaria.smp=16`, networking. Three
  sessions were broken by one line: polling the serial console from the tick
  handler. On a port that is absent (metal has no COM1) every register reads
  `0xFF`, i.e. "a byte is ready" forever, and an unbounded polling loop never
  returned from the first tick.
- The machine **powers itself off** (ACPI S5) at the end of a run: on EC2 that is
  both money and the only unambiguous "it reached the end" where there is no
  console. A workload can be deployed on AWS with one command
  (`infra/aws/talaria-deploy.sh`), and the boot chain is the same for virtual
  instances and for `*.metal`.

---

## v1.7 (2026-08-27) — Talaria on real hardware

- On `c6i.metal` (128 cores, 256 GB, no hypervisor) the kernel goes through the
  whole bring-up via **our own BIOS chain** (`tools/tboot/`: stage1 of 72 bytes
  plus stage2 of ~1.7 KB reads the kernel off the disk itself and jumps to
  `mb2_start`; GRUB is not involved): a memory map of 22 records, ACPI (the RSDP
  found by scanning — on the BIOS path nobody hands it over, and without MCFG
  there is no ECAM window), MADT with 64 CPUs, PCIe through ECAM (52 bridges plus
  307 functions), an NVMe at `c2:00.0`, ext2 on a live EBS volume.
- Real `MONITOR/MWAIT` (`sync: mwait(ring0)`) — cross-core wakeup p50 **901 ns**,
  where the same detection on Nitro guests falls back to `hlt`+IPI. That is, the
  synchronisation tier works on hardware the way it was designed to.
- Diagnostics for a machine without a console: `talaria.trace=<LBA>` (the whole
  kernel log in raw sectors on the disk), `talaria.suite=<LBA>` (a set of modes in
  one image with a reboot between items), a boot counter in the loader's trace.
- Also measured there: `fsync` of 512 blocks in 7.4 ms (a repeat in 4 µs), PMU
  through `rdpmc` with IPC 1.93.

---

## v1.6 (2026-08-26) — our own bootloader instead of GRUB

EC2 `*.metal` does not accept an AMI with `boot-mode uefi` (verified on six
instance types), so the chain is ours and BIOS-based: `tools/tboot/` plus
`tools/mk-tboot.sh`. Along the way, the diagnostics that made it possible: an MBR
marker that prints and CHAINS on (`tools/mbr-marker.sh`), a VGA text console as a
second output channel (`talaria.console=vga`), `talaria.rebootat=` as a signal
from the zone before the disk comes up. Regression 131 → 139.

---

## v1.5 (2026-08-26) — receive hits the instance limit, htree is ours

- Receive tops out at the Nitro **shaper**, not in our code: 1 000 000 pkt/s with a
  spread of ±3 (the first measurement of 557 k was the GENERATOR's ceiling).
- **htree grows by itself**: the kernel adds to somebody else's directory index
  and the growth path is exercised (`e2fsck -fn` clean after 3000 insertions by
  the kernel); a journal transaction is cut by the journal's capacity; the file
  mapping no longer needs a contiguous cache window anywhere.
- Talaria works on **7th-generation EC2** (c7i: an ECAM window above 4 GB, a card
  on a bus outside the bridge tree, a hundred bridges in the device table).
- Regression 125 → 131.

---

## v1.4 (2026-08-26) — the ENA datapath gap closed: faster than Linux

- **The accelerated LLQ mode.** `SET_FEATURE(LLQ)` has an
  `accel_mode.enabled_flags` field; until the driver declares
  `DISABLE_META_CACHING | LIMIT_TX_BURST` in it, the device serves the queue on
  the NON-accelerated path. Ours was zero — and that was the WHOLE gap against
  Linux which three sessions of hypotheses (queues, RSS, offloads, moderation,
  ring depth, buffer addresses, instance generation, interrupts) had failed to
  find. It was found by byte-decoding an admin-command trace of the live Linux
  driver. **RTT 92 → 33.6 µs, against 43.0 for Linux on the same path.**
- **Write-combining on the LLQ window** (as `devm_ioremap_wc` does in the
  reference driver): the kernel programs `PA4 = WC` in the PAT on every processor
  and converts the window at 4K granularity (the same 2M page holds BAR0 with the
  registers, for which WC is contraindicated). No effect on latency, decisive on
  transmit: **1 266 664 against 188 235 pkt/s**.
- Regression 123 → 125 (two write-combining checks, `talaria.test=wc`).

---

## v1.3 (2026-08-25) — htree, c7i, the ENA gap localised

The internal turnaround inside Talaria is **4.3 µs out of 90.5 µs** of full RTT,
i.e. the gap lies outside our software path. Regression 106 → 115.

---

## v1.2 (2026-08-25) — a third boot path and the next FS layer

- **BIOS boot**: a second entry point in the kernel — **multiboot2**
  (`tools/mk-bios.sh`, GRUB i386-pc). The MBI is translated into the same
  `hvm_start_info` as PVH and UEFI use, so nothing below the entry distinguishes
  loaders. `HYBRID=1` builds ONE disk that boots both ways (GPT: bios-boot + ESP +
  /boot + `talaria-data`), and the kernel finds the data partition BY NAME, not by
  number.
- **The filesystem, next layer**: the block cache became 4-way with LRU and a
  per-way layout; the file mapping no longer needs a contiguous window of slots;
  the journal writes the descriptor and the copies in one submission; block
  allocation goes in runs of 16 (bitmap trips 33 per 512 blocks instead of 512+,
  and files land contiguously); the mode **`talaria.ext2=journal=data`** was added.
- **§8 was verified on a LIVE EBS/NVMe volume** and found two bugs invisible under
  QEMU: a write batch longer than the controller's MDTS was silently rejected (the
  data was not written), and a freshly allocated block was read from the disk
  before being zeroed (the `fsbench` write phase **290 200 → 817 µs**).
- **One AMI for every experiment**: `talaria.userdata=1` — the cmdline tail from
  EC2 user-data (IMDS). The hypothesis-check cycle on EC2 fell from ~15 minutes to
  ~2.
- Regression 94 → 106.

---

## v1.1 (2026-08-24 … 2026-08-25) — Talaria on EC2 without QEMU

- **Talaria boots as an AMI on `c6i.large` and lives in a VPC** — DHCP, IMDSv2,
  NVMe/EBS, a workload from a manifest. The key to getting the IO queues open: on
  Nitro the device only opens them for a driver that has INTRODUCED itself
  (`host_info` as the first command, driver version 2.x).
- The path to get there: `kernel/uefi-stub` (a PE with no dependencies, EFI memmap
  → PVH), ACPI (MADT topology, MCFG), PCIe enumeration over ECAM, an NVMe driver,
  `tools/mk-ami.sh`. A critical fix that only showed up after a UEFI boot: an `sti`
  race before `sched::start`.
- **LLQ for ENA** (`talaria.ena=llq`): transmit descriptors and the frame header in
  the device's memory (BAR2). RTT Linux→Talaria **126.6 → 84.7 µs**.
- **The ext2 journal in JBD2 format** (`mkfs.ext2 -O has_journal`): metadata
  transactions with barriers, replay at mount — and our journal is understood and
  replayed by a stock `e2fsck`.
- **`fsync` became cheap**: coalescing adjacent blocks into ranges (512 blocks →
  2 requests), a per-file flush, PRP lists and command pipelining for NVMe.
- **File `mmap(MAP_SHARED)`** instead of ENODEV — the pages of the block cache
  itself are mapped, with writeback by the page-table D bit.
- The repository was restructured: `poc/` dissolved into `kernel/ services/
  control-plane/ bench/ private/` at the top level.
- Regression 78 → 94. A platform limitation found: EC2 bare metal does not boot a
  UEFI AMI, so native metal needs a BIOS loader (became v1.6).

---

## v1.0-preview (2026-08-24) — the product finalised

- Memory across supervisor restarts: an instance's mmap regions and kernel stacks
  are returned (a drift of 0-128 KB per restart instead of leaking hundreds of MB);
  along the way, the disk-workload restart broken since v0.48 was fixed.
- **The application pipeline**: `talaria-pack` (TFS1/TFS2/ext2 plus manifest
  generation), `talaria-check` (an ABI compatibility verdict),
  `talaria.mode=cell` (the canonical manifest launch, works with no NIC),
  `examples/hello-talaria`.
- A soak gate (`soak.sh`: MariaDB for 30-60 minutes under client load),
  `docs/PRODUCT.md` (boundaries, trade-offs, limits), the open-source wrapping
  (Apache-2.0, NOTICE, CONTRIBUTING/SUPPORT/SECURITY, CI without KVM, all
  user-space binaries from source, `export-public.sh`) and a green run from a clean
  clone.
- Gate: `test.sh` 78/78; `validate.sh` 10/10 plus three databases; soak 30 min OK.

---

## v0.53 (2026-08-24) — actors and a sub-µs condvar

- An actor benchmark with SObjectizer 5.8.6, Talaria against a Linux guest
  (pingpong/ring/fanout/pipeline/flood): a tsync dispatcher on a new MPSC queue
  (`tsync_mpsc`, Vyukov bounded, sleeping on a per-slot seq word) — actor-to-actor
  RTT 653-667 ns (parity with Linux); on so_5's own STOCK primitives Talaria is
  **6.4× faster than the Linux guest** (2.1 µs against 13.5), flood 4.71 M
  elements/s. SMP up to four cores (`talaria.smp=N`).
- v0.53b: five roots of slow wakeups removed (a membarrier on every futex WAIT
  with a runnable BSP, an EOI VMEXIT on a software `int 0x41`, insurance IPIs on
  every wake, `sched_yield` as `sti;hlt`, and a lost wakeup of a preempted
  `kwait` sleeper) — musl condvar 44 µs → **1.3 µs**, ring 21.6 ms/lap →
  5.7 µs/hop, p99 futex cross-core 0.2-0.3 µs.

---

## v0.52 (2026-08-23) — SPSC v3 and batched virtio notify

- The SPSC queue v3: **9 → 140-168 Mops** (a predicate `upark` in the kernel,
  state feedback, doorbells on both sides, a layout by line ownership, an inline
  fast path).
- `virtio-drivers` vendored: batched TX notify (wrap-safe per VirtIO 2.6.6.4) —
  fastflood 230 k → 539-682 k pkt/s. MongoDB on the patched ld-musl.

---

## v0.51 (2026-08-23) — synchronisation without a busy spin

- A futex sleeps IN THE KERNEL with `mwait` on the user's own word, and the
  waker's store wakes it with no IPI and no syscall: futex ping-pong cross-core
  **165 ns p50** on unmodified musl.
- SMP threads across cores (percpu/GS, `sched_setaffinity` migration BSP→AP),
  libtsync (mutex 45-58 cycles, wait/wake 223 ns), fibers (an 11 ns switch, a 30 ns
  condvar handoff), a patched ld-musl, and a cross-VM wake through ivshmem at
  496 ns with no kick.

---

## v0.50 (2026-08-23) — the process model's debts

Syscalls on a kernel stack, a process's memory freed at exit, ext2 write-back (an
8 MB cache, `fsync`/`sync`), a procfs/sysfs minimum, signalfd, `talaria.stop=` (a
clean database shutdown), glibc's ld-linux verified.

---

## v0.48 / v0.49 (2026-08-23) — databases on Talaria

- **PostgreSQL 16** (postmaster plus forks, `psql` from the host, WAL,
  persistence) and **MariaDB 10.11** (InnoDB, the MySQL protocol) on a read-write
  ext2 with a full process model (fd tables, pipes, signals, SysV/POSIX shm, FPU
  per thread, CR0.WP).
- **A dynamic ELF loader in the kernel** (PT_INTERP, PIE, file `mmap`/MAP_FIXED, a
  full auxv) and **MongoDB 4.0.5** (mongod plus 18 shared objects, WiredTiger);
  eventfd/timerfd.

---

## v0.46 / v0.47 (2026-08-22) — outgoing networking and a transport benchmark

A multithreaded workload loaded from disk by manifest holds outgoing TLS and
WebSocket connections (outgoing TCP and DNS, TFS2, supervision with a manifest, an
operator's console on top of a workload). A "low-latency data transfer" harness in
two VMs: the transport's p50 ≈ 2× below a Linux guest's.

---

## v0.44 / v0.45 (2026-08-22) — open-source workloads and production-readiness

Unmodified mongoose, Lua, SQLite, busybox, QuickJS and Redis in ring 3; a full
fork+execve+wait4 cycle. The production gaps closed with automated tests: the
munmap UAF, the COW refcount, metrics, telemetry, soak, watchdog, `/ctl`
authentication, a syscall filter, network fuzzing.

---

## v0.40 – v0.43 (2026-08-21) — from a PoC to a working tool

A large static C++ binary (26 MB, AVX2) executes on the bootable kernel from
disk. Hardening (W^X/NX/guard pages/`mprotect`), a per-process address space with
fork/COW/exec, observability (a flight recorder, PMU, a gdb stub), supervision
with failure isolation.

---

## v0.20 – v0.39 (2026-08-20) — the OS foundation

Our own kernel: PVH boot, an IDT, our own page tables, the Linux syscall ABI,
musl/C++/pthreads, a smoltcp network stack in ring 3, epoll, virtio-blk plus a
filesystem, a zero-syscall datapath (28 ns per packet in the cell),
multithreading through clone and futex.

---

## v0.1 – v0.19 (2026-08-19) — the thesis as a PoC

A PVH unikernel (a busy-poll reactor, virtio and a VFIO 10G card): on an
equivalent transport, a p50 2.6× below a Linux guest's; a trading pipeline
feed→strategy→risk→crypto with AES-NI; an isolated stand on AWS `m5zn.metal`; a
control plane (`talaria`: manifests, core donation, a syscall proxy).
