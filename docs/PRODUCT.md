# Talaria OS — the product: boundaries, mechanisms, trade-offs, limits

What Talaria OS is, what it guarantees, what makes it fast (§3a), what it pays
for its properties and where its boundaries lie. The numbers and statements here
are checked against the code and the measurements (`docs/MEASUREMENTS.md`); the
version and the regression count live in `README.md`. Guides:
`docs/USER-GUIDE.md` (running), `docs/DEVELOPMENT-GUIDE.md` (development).

## 1. What it is

**Talaria OS** is a latency-first single-application OS (unikernel-style),
Linux-ABI compatible. It is its own x86-64 kernel (Rust, `no_std`) that runs
**unmodified Linux applications** (static musl, dynamic ELF through ld-musl,
glibc's ld-linux tested) and gives the application the CPU cores and the
devices, keeping for itself the role of the control plane.

Who it is for: latency-critical workloads — trading, telemetry and industrial
buses, dedicated appliance servers behind an external security perimeter —
where one trusted workload should own the machine (a VM) and p99 tails matter
more than average throughput.

The size of a deployment unit is set by static kernel tables (§5), and it is
worth knowing BEFORE choosing: **64 TCP sockets and 192 scheduler tasks per
machine**. This is "one VM per service" (or per client), not "a thousand
connections per node"; scaling out is done with VM instances. More in "Where
the connection ceiling comes from" in `docs/POSITIONING.md`.

The key mechanism: **synchronisation without busy-spinning**. A futex sleeps IN
THE KERNEL with `mwait` on the user's own word; the waker's store wakes the
sleeper with no IPI and no syscall. Plain pthread mutexes and condition
variables in an ordinary Linux binary get sub-microsecond wakeups at 0 % CPU
while waiting (mechanics: DEVELOPMENT-GUIDE §6b, results: MEASUREMENTS §6). It
is the largest single mechanism, not the only one: the rest — the scheduler, the
memory, the file system, the datapath, the syscall path and the cost of watching
all of it — are enumerated in §3a.

Tested with real software: PostgreSQL 16, MariaDB 10.11, MongoDB 4.0
(persistent network servers), OpenJDK 21, ClickHouse 25.8, SObjectizer-5 (an
actor framework — several times faster than itself on a Linux guest), Redis,
nginx, mongoose, busybox, Lua, SQLite, QuickJS.

## 2. What it is NOT

- **Not a general-purpose OS:** one trusted workload (or its processes) per VM.
  No users, no sessions, no zoo of drivers.
- **Not multi-tenant and not a security perimeter:** the isolation (W^X,
  per-process address spaces, COW, guard pages, syscall pointer validation, the
  syscall allowlist) protects against workload bugs, not against hostile code.
  The `/ctl` and `filed` tokens and the syscall filter are operator tools, not a
  cryptographic perimeter: the passwords are set at boot
  (`talaria.ctltoken=`/`talaria.filedtoken=`; without them the services are
  closed) and compared in constant time, but there is no TLS and no rate
  limiting. Talaria is placed BEHIND an external perimeter. See `SECURITY.md`.
- **Not POSIX-complete:** a pragmatic subset of the Linux ABI is implemented
  (~185 syscalls, USER-GUIDE §11); `talaria-check` shows the gaps for your
  binary in one run.
- **Bare metal works, with a narrow driver set.** QEMU/KVM (PVH) is the
  standard mode. Since v1.1 the kernel boots on real EC2 without QEMU (its own
  UEFI stub, ACPI, PCIe, ENA/NVMe); since v1.2 there is a BIOS path (multiboot2
  and GRUB), since v1.6 its own BIOS boot loader; since v1.8 the full production
  configuration runs on `c6i.metal`: the application in ring 3 in its own
  address space, 15 cores (`talaria.smp=16`, speedup 2.00/4.00 on 2 and 4
  cores), ENA with DHCP and LLQ, NVMe. What is still missing on hardware:
  in-kernel NIC drivers other than ENA (ixgbe/mlx5 — the VFIO code exists in
  `bench/ixgbe`), PTP and hardware packet timestamps, CAT on a live chip, and
  machines with APIC ids above 254 (§8; the open items are in `docs/ROADMAP.md`).
- **Not realtime-certified:** there are no formal WCET guarantees; "low latency"
  here is measured behaviour, not a certificate.

## 3. Performance (measured)

The primary stand: an i7-3820 (Sandy Bridge-E, 4C/8T, no WAITPKG), a Linux
host, QEMU/KVM with `-overcommit cpu-pm=on`; "versus Linux" comparisons use a
Linux guest in the same VM topology on the same host. Details and methodology:
`docs/MEASUREMENTS.md`, `bench/suite/README.md`, `services/actors/README.md`.

**Validated on** (full `test.sh` from scratch):
- Intel Sandy Bridge-E (i7-3820, `mwait` in ring 0) — the primary stand; all
  numbers in the table below were taken on it;
- AMD EPYC 7R13 / Zen3 (AWS `c6a.metal`, `monitorx`) — green; futex across
  cores 350 ns, condvar 1.2 µs — the class of the primary stand;
- Intel Sapphire Rapids (AWS `m7i.metal-24xl`, **WAITPKG**: the `umwait` tier in
  ring 3 is active and works without `cpu-pm=on`) — functionally green; wakeup
  201 ns, futex across cores 212 ns; tier 1 in SPSC bursts needs performance
  tuning (§6). To run on your own hardware: `infra/aws/validate-remote.sh`
  (any host: build and `test.sh` from a clean tree).

| Metric | Talaria | Reference |
|---|---|---|
| futex ping-pong RTT across cores (unmodified musl) | **165 ns p50** | Linux futex: single to tens of µs |
| musl pthread condvar RTT across cores | **1.3 µs** | Linux guest: comparable |
| Wakeup by a store to the word (kernel, `mwait`) | **237 ns p50 / 338 ns p99** | `hlt` + IPI fallback: 15.8 µs |
| tsync SPSC throughput (2 cores, sleeping instead of spinning) | **140–168 Mops** | busy-spin of the same queue: 203–375 Mops |
| tsync SPSC push→pop with a full sleep cycle | **533 ns p50** | spinning ring: 52–64 ns (burns a core) |
| Fiber switch / condvar handoff (one core) | **11 ns / 30 ns** | coroutines: 10–25 ns |
| Actor→actor RTT (SObjectizer, tsync dispatcher, across cores) | **653–667 ns p50** | Linux guest: 649–667 ns (parity) |
| Actor→actor RTT (stock SObjectizer, musl mutex + cv) | **2.1 µs p50** | Linux guest: 13.5 µs (×6.4) |
| Actor flood (throughput, spread) | 3.97–4.71 M items/s | Linux guest: 2.57–3.78 M |
| Cross-VM wakeup over ivshmem (both VMs asleep in `mwait`) | **496 ns RTT** | virtio kick: ~10 µs VM-exit path |
| virtio-net with batched notify (fastflood, vhost busy-poll) | **682 k pkt/s** (1.5 µs per frame) | without batching: 230 k |
| UDP RTT over ENA on REAL EC2 (`c6i.large`, 64-byte frames) | **33.6 µs p50** (min 29.6, p99 40.7) | Linux → Linux on the same VPC path: 43.0 |
| Transmit over ENA on real EC2 (64-byte frames) | **1.28 M pkt/s** (780 ns per frame) | LLQ window without write-combining: 188 k |
| Trading pipeline tick-to-trade (RX → strategy → encryption → TX, 82599, `bench/ixgbe --trade`) | **620 ns avg** (min 472) | of which in-place AES-GCM of the order: ~200 ns |
| Transport UDP RTT vs Linux guest (same topology) | **7.8 µs p50 (×2.6 lower)** | Linux guest: 20.3 µs |
| Uncontended mutex lock + unlock | 45–58 cycles | pthread/musl: 58–68 cycles |
| Syscall with a pointer argument: cost of validating it | **+7–14 ns** (`read` of 64 bytes: 137 → 144–151 ns) | empty syscall: unchanged, ~120 ns |

What is behind this (all at 0 % CPU while waiting, without burning cores): the
waiter sleeps in the kernel with `mwait` right on the futex word; the waker's
store wakes it with no IPI or syscall; on the same core — a directed switch;
across VMs — the same words in ivshmem.

### Real workloads against Linux (EC2, board 41)

The stand: four `c6i.xlarge` in one zone `ap-northeast-1c` (a Talaria server and
a Linux server, each with its own generator), two cores per workload on both
sides, an open loop, a rate ladder from the shared ceiling, six workloads.
SERVICE latency is judged (the generator's schedule debt is not charged to the
server); a point counts only if it is comparable: no overload, no generator
debt, a difference larger than the stand's own jitter, and not decided by a
handful of requests. The instrument and the method: `bench/suite/`; the full
board: `docs/MEASUREMENTS.md` §2.

| workload | p50 | p99 | p99.9 | p99.99 | points |
|---|---|---|---|---|---:|
| mongodb | 12 / 3 / 0 | 15 / 0 / 0 | 11 / 2 / 2 | 13 / 0 / 1 | 59 |
| mongoose | 10 / 0 / 0 | 10 / 0 / 0 | 10 / 0 / 0 | 7 / 1 / 2 | 40 |
| mysql | 6 / 3 / 1 | 7 / 3 / 0 | 6 / 4 / 0 | 4 / 1 / 5 | 40 |
| nginx | 9 / 0 / 0 | 9 / 0 / 0 | 9 / 0 / 0 | 9 / 0 / 0 | 36 |
| postgresql | 9 / 1 / 0 | 8 / 2 / 0 | 9 / 1 / 0 | 10 / 0 / 0 | 40 |
| redis | 20 / 0 / 0 | 20 / 0 / 0 | 20 / 0 / 0 | 20 / 0 / 0 | 80 |
| **total** | **66 / 7 / 1** | **69 / 5 / 0** | **65 / 7 / 2** | **63 / 2 / 8** | **295** |

Each cell: faster / parity / slower. 11 points of 295 are lost, eight of them
at `p99.99`. The order of magnitude where the win is large: `postgresql select1`
at 36 k req/s — `p99.9` 72.8 µs against Linux's 11,206.7; `mysql pk` at 28.9 k —
`p99.99` 110 µs against 18,792.

**The remainder, honestly** (analysed by name in `docs/MEASUREMENTS.md` §§2.3-4):

- five lost `mysql` points at `p99.99` are not our code: 97 % of the gap is
  vCPU time stolen by the hypervisor (the "tick gap" instrument with `steal`
  attribution);
- two `mongodb insert` points at `p99.9` are a band of 140-400 µs: the thread is
  taken off the core by the tick in the middle of a request, and the core goes
  to a network waiter (94-99 % of rotations, instrument `talaria.appprof=1`).
  BOTH treatments were written and rejected by measurement — a long quantum
  (`talaria.tickmin=200000`) widens the band, and priority over the waiter
  (`talaria.reqfirst=`) removes three quarters of the stops without narrowing
  the band: it is fed by the length of a stop, not by their number;
- the rest are the judging boundary (1.05-1.06) and classes that did not
  reproduce on a second deployment.

## 3a. What makes it fast: the mechanisms, subsystem by subsystem

§3 is the result; this is what produces it. There is no single trick here — the
speed is the ordinary costs of a general-purpose kernel removed one at a time.
Every item below is in the code, most of them can be switched off by a knob for
an A/B, and each stayed because a measurement on ONE bring-up said so. What did
not survive that test is listed in §3a.9 rather than quietly dropped. The whole
knob registry with one-line descriptions is `kernel/src/cmdline.rs` (`KNOBS`,
validated at boot — an unknown knob is a boot refusal) and `docs/USER-GUIDE.md`
§7.1; the mechanics are `docs/DEVELOPMENT-GUIDE.md` §§5-8, the numbers
`docs/MEASUREMENTS.md`, the tuning `docs/PERFORMANCE-OPTIMIZATION-GUIDE.md`.

### 3a.1. Synchronisation and threads

- **The only sleep primitive is `kwait_once`**: MONITOR on the line → a recheck
  of the word → `sti; mwait`. The tier is chosen at boot by CPUID —
  WAITPKG/MONITORX in ring 3, else MONITOR/MWAIT in ring 0, else `hlt` + an IPI
  kick; `talaria.sync=` forces one for an A/B.
- **The waiter sleeps on the application's OWN futex word.** The waker's plain
  `mov` in user space wakes the core: no syscall, no IPI, nothing to deliver.
  That is why an unmodified musl `pthread` gets 165 ns RTT across cores at 0 %
  CPU while waiting.
- **A wake is a directed switch, not a rotation.** On the same core the waker
  leaves a hint and the scheduler takes exactly that task; a sleeper on another
  core gets a resched IPI with the hint (before that a woken thread waited for
  the other core's TICK — up to 500 µs).
- **The scheduler does not scan sleepers.** A registered waiter is skipped
  (`blocked_for_sched`) with a backstop: once every 64 skips it gets a quantum
  anyway, which the futex ABI permits as a spurious wake.
- **Predicate parking (`upark`, syscalls 4600-4602)** so that a waiting
  protocol does not pay a syscall per change: the kernel itself re-arms the
  monitor on each change of the word (`kind1`), or sleeps on a SEPARATE
  doorbell line so that the partner's progress does not ping the monitor
  (`kind2`), and writes back a state word that tells the waker whether a wake
  syscall is needed at all.
- **libtsync** (`kernel/user/tsync/`): the SPSC fast path is a `static inline`
  in the header — a call boundary in a hot loop costs about 4× of the
  throughput — fields laid out by line ownership, a producer hysteresis, an
  adaptive poll-then-sleep pre-spin, an MCS lock that sleeps on its own node,
  fibers that switch in 11 ns. musl's `__wait`/`__timedwait` are patched to the
  same tiers, so a stock binary gets them without being aware of them.

### 3a.2. The scheduler and the cores

- **A dedicated core has no timer armed at all**: one task, run-to-completion,
  zero preemptions and zero jitter from us. Queues are per core (a task belongs
  to `CPU_OF[t]`, foreign cores do not choose it), so a migration costs one
  write plus a doorbell rather than a lock on a shared runqueue.
- **The tick's PERIOD is not one per machine** — a tick costs the same
  everywhere and is needed differently: 100 µs on a core where somebody waits
  for the network (there are no card interrupts: a frame is seen only by the
  thread waiting for it), 2000 µs when there is nobody to preempt (a tick in
  solitude is pure tail: `actors pingpong raw` p99.9 3767 → 473 ns), 500 µs
  otherwise (`talaria.netslice=`/`idleslice=`/`slice=`; the slice is in
  nanoseconds and is measured by the TSC, so it means the same thing on every
  machine).
- **The tick's PRICE is a separate quantity and was attacked as one**: taking
  the PMU counters on every tick cost 22.5 µs, acknowledging the interrupt
  through the LAPIC 10.2 µs, polling COM1 from every tick 2700 hypervisor exits
  per second. With KVM's PV EOI and PV IPI, x2APIC, a TSC-deadline timer and a
  time-based console poll the price is ~2.3 µs — at 2000 ticks a second that is
  0.4 % of the core instead of 7 %. `talaria.tickprof=1` splits it into seven
  phases.
- **Whom the tick takes off**, which under request-response matters more than
  how often: `talaria.tickmin=` (default 50 µs) forbids a rotation from
  displacing a task younger than the threshold — `postgresql select1` p99 80 →
  67 µs, the class of 40-60 µs rotations 3.7 % → 0.07 %.
- **A network waiter yields the core every 5 µs (`talaria.netyield=`, the
  default) and yields it TO the thread whose socket has data, on its own
  core.** PostgreSQL: p50 1148 → 66-77 µs, the ceiling 8.9 k → 18-20 k req/s.
- **Placement is declared by rules, not by numbers** (`pin=`/`pool=`/`spread=`/
  `numa=`/`irq=` in the manifest or `talaria.pin=`/`talaria.pool=`): one
  process's threads are gathered onto one NUMA node, a card's interrupts are
  kept off the datapath's core, and unpinned threads are balanced BY DEMAND
  with hysteresis (a thread asleep on a socket does not load a core), so in the
  steady state there are zero migrations. Work stealing exists, and a steal
  must improve the balance or it is refused — the rule is there because
  stealing once turned a skew over instead of smoothing it (`actors fanout`: 25
  µs per message instead of 1.5).
- **The tuned quantities live in the workload's `cell.manifest`, not in a
  global default** — the slice, the idle and the network slice, the
  wake-preemption threshold, the read-ahead window, the network waiter's yield,
  the stealing push and the block cache's size — and that is a measured
  position rather than a taste: the pair `slice=150000` + `wakepreempt=20000`
  removed a mongodb tail by 20-70× and made mysql 16-22 % WORSE. The gate that
  reads them BACK out of the atomics the scheduler and the stack actually use
  is `talaria.test=tuning`, because parsing a key and reaching the machine are
  two different claims.

### 3a.3. Syscalls and the ABI path

- **One trust domain and a minimal dispatcher**: no KPTI, no retpoline, no
  seccomp, no audit. An empty syscall is 101 cycles (**28 ns**) against ~50-150
  ns on Linux with mitigations; SYSCALL/SYSRET do not VMEXIT, so the number
  holds on real hardware.
- **Pointer validation is a declarative table**, checked before dispatch and
  unit-tested on the host: the price is +7-14 ns on a syscall with a pointer
  argument (`read` of 64 bytes 137 → 144-151 ns) and nothing on the rest;
  `talaria.uacheck=0` exists to measure exactly that.
- **A vDSO** so the runtimes stop paying for the clock: `clock_gettime` at **15
  ns against 127** through a syscall, on a seqlock over a data page the kernel
  writes and the process reads. `System.nanoTime()` is called per message in a
  JVM queue, and ClickHouse times every pipeline stage.
- **The zero-syscall datapath** (`talaria.mode=fast`) removes even those 28 ns:
  a cell on an AP bridges the card to two SPSC rings in memory the application
  can reach, and ring 3 polls them without entering the kernel at all.

### 3a.4. Memory and the TLB

- **Identity mapping with the largest leaf that fits**: 1 G leaves for RAM and
  MMIO, the first gigabyte split into 2 M leaves for per-page protection, and
  the walker splits a leaf only where a finer grain is actually needed.
  `nx_range` keeps the 2 M leaves where the range is aligned — less TLB
  pressure on a large heap or on DMA arenas.
- **Demand-zero reservations (`PTE_LAZY`)**: an `mmap` hands out no frames at
  all, and the first touch materialises a cluster of 64 KB (~500 faults instead
  of ~7400). The JVM at `-Xmx256m` costs 60 829 frames instead of 874 308. An
  `mprotect` of a reservation that has not been touched corrects the FUTURE
  rights without materialising anything.
- **COW that does not copy when it does not have to**: `fork` marks the entries
  RO+COW and shares the frame's refcount; a write fault with `rc <= 1` writes
  in place instead of copying.
- **Huge pages by threshold** (`talaria.huge=<MB>`): one table entry and one
  TLB line per 2 MB, which under a hypervisor counts twice (a TLB miss drags in
  a nested EPT walk). ClickHouse `uniqExact` 253 → 197 ms, `GROUP BY` of 100 k
  235 → 197. It is NOT for microsecond workloads — `redis ping` pays 1.5 µs of
  p50 in extra flushes — hence the default of 32 MB, that is, in practice off.
- **TLB shootdowns are filtered, not broadcast.** A range above 4 GB is a
  process's private area, so only the cores actually running that CR3 are
  called: PostgreSQL is a process per connection, and every `madvise` of one
  backend used to stop three other cores on an IPI (an exit to the host). A
  neighbour asleep in `hlt` is not called at all — it flushes on wake-up
  (`talaria.lazytlb=`); a neighbour the host has taken off its vCPU has the
  flush delegated to the host and is not waited for, counted separately from a
  lost acknowledgement so that the norm of a noisy stand is not confused with a
  defect.
- **Loading and mapping in batches**: ELF segments load in 2 MB pieces
  (ClickHouse's start 4.5 s → ~1 s), and a file `mmap` maps the block cache's
  own pages — zero-copy and coherent with `read`/`write` rather than a second
  copy.
- **The allocator**: a bump above the kernel plus a free list with coalescing;
  frames come from the local NUMA node for the stack, the heap, private
  mappings, COW copies and the FS cache alike. DMA under a non-identity address
  space resolves through `virt_to_phys`, so there are no bounce buffers.
- **The shared L3 is divided rather than left to luck**: Intel CAT in the
  kernel (`llc=` per group of cores, with a read-back) plus an automatic check
  of the DMA rings' footprint against L3.

### 3a.5. The file system and the disk

- **A set-associative write-back block cache** (4 ways, LRU within a set, a
  sixteenth of free memory clamped to [2 MB, 1 GB], `talaria.fscache=`). The
  slots are laid out BY WAY, so blocks that are neighbours on the disk are
  neighbours in memory and glue into one device request; with a per-set layout
  the gluing would disappear.
- **The dirty set is a counter, not a walk.** Three full scans of the cache per
  `fsync` cost 320 µs of pure CPU (`talaria.fscnt=0` returns the walks), and
  the age of the oldest dirty datum is a clock (`DN_OLD`) rather than a pass
  over the cache.
- **Eviction is ranked by what a miss will cost**: a clean slot first, then
  dirty data, then dirty metadata (whose eviction is a transaction boundary: a
  commit has to happen before the slot can be re-tagged); a writer is held
  by a dirty-balance threshold instead of letting dirt pile up without limit,
  and `talaria.evictbatch=<blocks>` will send consecutive dirty blocks out in
  one request (the default is 1, one at a time — the batch is a lever for a
  write-heavy workload, not a win everywhere).
- **The FS lock and the DEVICE lock are different locks**, the device lock
  belongs to the TASK rather than to the core, and the FS lock is RELEASED
  across a device wait (`talaria.fsdevlock=1`, the default; `=0` is the
  rollback) — so a hit in the block cache no longer waits for somebody else's
  `fsync` (`mongodb insert` p99.9 **4325 → 1212 µs**), the descriptor batch
  flies with the FS lock released (**1212 → 439 µs**, Linux 326), and
  `fdatasync` goes in two device round trips instead of three (per call,
  **6224 → 5018 µs**; checked by a power cut: 210332/210332 acknowledged writes
  in place).
- **Waiting for the device gives the core away** (`talaria.iowin=2`, the
  default): `insert` p99.9 became a FLAT 536-745 µs against a 593-4793 spread
  on the baseline. A long `write` and a long `fallocate` open a preemption
  window between portions (`talaria.wrwin=`) — connection-thread starvation
  1167 ms → < 10 ms.
- **The journal costs what it must and no more.** JBD2 in ordered mode, and it
  is not held during the "data in place" phase (`talaria.jrnfree=1`): waiting
  for a foreign transaction disappeared ENTIRELY (185 932 µs in one episode →
  zero episodes). FUA follows the device's declared write cache, so on EBS
  (VWC=0) barriers cost 0 µs and a transaction's price is the number of
  SEQUENTIAL round trips to the device — which is what the work went into
  reducing.
- **Layout and batching on the way out**: blocks are allocated in runs (33
  bitmap trips per 512 blocks instead of 512+, and files land contiguously),
  adjacent blocks are coalesced into ranges (512 blocks → **2 requests**), a
  freshly allocated block is not READ before being zeroed (the write phase of
  `fsbench` 290 200 → **817 µs** on EBS — invisible under QEMU), and the
  controller's MDTS is respected on every path.
- **Read-ahead by the descriptor's BEHAVIOUR**, not by the request's size
  (`talaria.readahead=`): a workload reading a file in 4 KB pieces gets device
  requests as large as one reading in megabytes.
- **NVMe with a real queue depth**: PRP lists up to 128 KB in one command,
  `rw_many` submits up to QD-1 commands with ONE doorbell and finds the owner
  of a completion by the command id, so submitting and waiting are separated on
  both drivers — a barrier no longer holds the reads standing behind it.

### 3a.6. The network datapath

- **Polling-first, with an honest silence criterion.** There is no interrupt in
  the datapath by default, and "the card has brought nothing" is decided by the
  receive ring's own word rather than by the clock (`talaria.pollhard=`), so an
  idle pass over the stack is cancelled before it starts; a pass for the
  stack's own timers still happens by its deadline.
- **Doorbells are batched everywhere**: the TX tail once per pass, the receive
  buffers replenished once per 64, the CQ heads once per quarter of the ring;
  the virtio notify goes once per batch with a wrap-safe `vring_need_event`
  (the naive comparison loses the notify for good after 65536 descriptors) —
  **230 k → 682 k pkt/s**.
- **On EC2 the whole gap was the LLQ contract**: descriptors and the frame's
  header written straight into the device's BAR, the accelerated mode declared
  (`DISABLE_META_CACHING | LIMIT_TX_BURST`) and that window mapped
  write-combining. RTT 126.6 → 84.7 → **33.6 µs** (Linux on the same VPC path
  43.0); WC does not move the latency but moves the transmit rate from 188 235
  to **1 266 664 pkt/s**.
- **A socket has a named OWNER, and a pass over the stack rings exactly it**
  (`wake_owner`; the owner is a column in `netstat`, because on a
  one-application machine "who listens" means "which of its threads").
  Readiness is ONE mask serving both `epoll` and `poll`. On top of that, "a
  pass of the stack is one per machine" — the non-polling waiters sleep and are
  woken by the delivery — lives behind the EXPLICIT knob `talaria.netsolo=1`,
  and the outcome of every such sleep is counted separately (a delivery, or the
  5 ms backstop): switching it on by the number of waiters cost MariaDB 29×
  (`select1` 776 req/s against 22 741, p99 of exactly 6 ms — the backstop was
  the graph), and a mechanism that claims to be woken by delivery has to prove
  it with a counter. With the card's line laid (`talaria.netirq=1`) the line
  itself drives the stack — off by default, see §3a.9.
- **A machine with nothing to do need not spin**: `talaria.netidle=1` sleeps on
  the receive ring itself — the device's DMA write wakes an MWAIT with no
  interrupt and no vmexit (on virtio the passes over the stack per round trip
  went 8 000 000 → 4 551), and the insurance against a lost wake-up is a timer
  rather than a poll. It costs about +14 µs of RTT, so it is the default only
  where latency is not the product — on a LOADED server it was measured as a
  catastrophe (`find` p50 97 → 217 µs), and it stays off there.

### 3a.7. IPC, shared memory and across VMs

- Everything in §3a.1 is the IPC: a futex, an `upark` predicate, an SPSC ring,
  a Vyukov MPSC for the many-senders case, fibers at 11 ns, a directed switch
  (`swtchto`) past the rotation.
- **Across VMs the same words**: with ivshmem both guests sleep in `mwait` on
  one line and an RTT is **496 ns** against the ~10 µs of a virtio kick.
  `/dev/shm` objects follow musl's POSIX semantics, and the region is remapped
  cacheable — the MMIO gigabyte is uncached by default, and leaving it so would
  have made the fast path slow for an invisible reason.
- **`membarrier()` is an IPI acknowledgement over the online cores** (p50 of
  about 7.5 µs per ack) and it stands only on the sleeper's SLOW path, where
  correctness
  needs it: without it a sleeper can miss the partner's final store and both
  sides stop.
- The application-level result: actor→actor 653-667 ns across cores (parity
  with Linux), and 2.1 µs against 13.5 on STOCK SObjectizer over musl mutexes
  and condition variables — the same binary, the difference is whose wake-up it
  is.

### 3a.8. Observability and post-mortem that cost nothing until asked

- **The counters hang on three events that happen anyway** — a context switch,
  a syscall, a fault — and cost one atomic increment each; the PMU is seven
  `rdpmc`s per tick (`talaria.pmutick=0` removes them), and RSS walks the page
  tables only when somebody types `top`. On the zero-syscall path the price is
  exactly zero, because none of those three events exists there.
- **The sampling profile is taken by the tick along the frame-pointer chain** —
  two reads per frame, which fits inside an interrupt handler; DWARF unwinding
  does not (one may neither allocate nor go to the disk there), and that limit
  is stated rather than hidden.
- **Core dumps are ELF `ET_CORE` that a stock `gdb` opens**: every thread's
  registers plus the STACKS by default (ClickHouse's 700 MB heap is the last
  thing to write at the moment of a crash; `talaria.core=<path>,full` if it is
  really wanted), the same format for a fault in the KERNEL, and `core [tid]`
  in the console dumps a LIVE hung task without stopping the machine — the
  alternative, the gdb stub, stops the datapath with it. Even without a disk
  the crash print carries the frame chain for `addr2line`.
- **The instruments are part of the product, because a claim about latency is
  worth what it can be falsified with**: `talaria.appprof=1` (who took the core
  from a thread mid-request, and for how long), `netprof` (the datapath by
  stages), `sysprof`/`sysrow` (the price per syscall number), `tickprof`,
  `kstat <section>`, the `fsync` phases with a per-task remainder, and the tick
  hole with its OWNER — how much of it the hypervisor took, which is how the
  largest "our tail" class turned out not to be ours.

### 3a.9. What was written, measured and left switched OFF

The list matters as much as the one above: these are mechanisms that exist in
the code, are covered by gates, and did not pay. Full numbers in
`docs/ENGINEERING-NOTES.md` §7.

| mechanism | why it is off |
|---|---|
| Group commit / async commit of the journal | the ceiling is exactly 0 µs — the FS lock was taken 597 of 597 times in 0 µs, the journal was busy with a neighbour once: there is nothing to coalesce |
| Age-based pre-emptive flush (`talaria.dirtyage=`) | does what it promises (dirty blocks per `fsync` 47 → 9.4) and moves the price into `write`: 6.7 → 507.6 µs per call, ×76 |
| A longer quantum (`talaria.tickmin=200000`) | a third fewer stops mid-request and the 140-400 µs band WIDENED: the band is fed by the length of a stop, not by their number |
| Priority over a network waiter (`talaria.reqfirst=`) | four times fewer stops mid-request, no latency bought — the core taken from a waiter goes to a thread that holds it longer |
| Hyperthread packing (`talaria.smtpack=1`) | gives the host a whole physical core, and costs 5-8 % of the median plus the top of the rate ladder |
| The card's interrupt line (`talaria.netirq=1`) | under a microvm the handler pays two VMEXITs per interrupt; open only for MSI-X without an MMIO acknowledgement |
| Huge pages for microsecond workloads | see §3a.4: `redis ping` pays 1.5 µs of p50 |
| Sleeping on the receive ring on a loaded server (`talaria.netidle=1`) | `find` p50 97 → 217 µs and overload; it is the right default on a load GENERATOR and wrong on the server |
| An idle stack pass by an empty core (`talaria.idlepoll=`) | the mechanism never fired: a sleeping waiter goes into `hlt` inside its own context and never returns to where the hook was cut in |
| "More cores is always better" | the opposite, and per workload: the databases on 2 cores beat themselves on 4 (extra cores breed spinning waiters that steal the core from the computing thread), while ClickHouse wants 4 and is measurably worse on 2 — the number belongs in the workload's `meta.env`, not in a global default |

## 4. Trade-offs (the price of the properties)

| Decision | What it gives | What it costs |
|---|---|---|
| One workload owns the machine | no foreign noise, predictable tails | no multi-tenancy; one VM = one workload |
| Sleeping in `mwait` in the kernel instead of busy-spinning | 0 % CPU while waiting, sub-µs wakeups | needs KVM `-overcommit cpu-pm=on`; without it, `hlt` + IPI fallback (~16 µs) |
| Timer-driven scheduler (0.5 ms slice by default, tunable per workload), run to completion on dedicated cores | simplicity, cheap syscalls, no tick at all on a dedicated core | pure spin locks of the workload (busy-spin phases) degrade on shared cores — use futex paths |
| One network stack (smoltcp) for the machine, serialised by a lock | no per-core stack state to keep consistent | many threads doing network syscalls at once contend for the stack lock |
| ext2 write-back + JBD2 metadata journal (ordered) | after a successful `fsync` a crash does not leave metadata half-done; the journal is also replayed by stock `e2fsck` | data not synced is still lost; with `talaria.ext2=journal=data` data is journalled too; the image must be created with `-O has_journal` |
| TFS1/TFS2 read-only | trivial packing and loading | writes only on ext2 |
| Identity memory + bump/free-list allocator | cheap, transparent, zero-copy DMA | no swap, no overcommit; the VM's RAM is a hard budget |
| A subset of the Linux ABI | unmodified binaries | rare syscalls may be missing — run `talaria-check` |
| Supervision with setjmp/longjmp in the kernel | failure isolation + restart within µs | not full process isolation (for that — `mode=proc`) |

## 5. Hard limits (from the code)

| Resource | Limit |
|---|---|
| vCPUs | ≤ 64 (`talaria.smp=N`, the BSP + up to 63 APs; the ceiling is the width of the `sched_getaffinity` mask). Core numbers are LOGICAL (0..N-1), as on Linux: on bare metal APIC ids are sparse and do not match them |
| Scheduler tasks (threads + processes) | 192 (`sched::NTASKS`) |
| TCP / UDP sockets | 64 / 8 (`netsys::NS`/`NU`); a listener's backlog is 8 slots OUT OF THE SAME 64; 32 epoll instances; 4 interfaces |
| User fds per process | 256 |
| Open TFS files | 64; TFS1: names ≤ 24 bytes, flat; TFS2: paths ≤ 83 bytes |
| ext2 block cache | write-back, a sixteenth of free memory clamped to [2 MB, 1 GB]; `talaria.fscache=<MB>` (fsync/fdatasync/sync/exit write to disk) |
| Process stack | 8 MB in `mode=proc`, 32 MB in `mode=cell`, with a guard page |
| brk heap | a fixed region per process (64-128 MB depending on the mode); mmap comes from the general RAM budget |
| Clock | TSC disciplined by kvmclock under KVM or by the ENA PHC (§8) |

These are also what `getrlimit`/`prlimit64` answer: the limits reported to the workload are
the ceilings above, not round numbers, and a request to raise one above its ceiling is
refused (EPERM) instead of being granted silently. The gate is `talaria.test=schedapi`.

**Interface stability:** there are NO compatibility contracts before 1.0 — the
`cell.manifest` format, the `talaria.*` boot parameters, the Talaria-specific
syscalls (1000-1002, 4600-4602), the libtsync API (`tsync.h`/`fiber.h`) and the
TFS1/TFS2 formats may change between versions without migration. The Linux ABI
subset only grows (existing syscalls do not change semantics).

## 6. Known limitations

- **Tails depend on a quiet host:** on a live desktop, p99.9 of cross-core paths
  catches vCPU preemption by the host; for measurements and production, use
  `isolcpus` and vCPU pinning (see `services/actors/matrix.sh`).
- A rare ~0.9 ms outlier on musl condvar across cores was once observed; a
  targeted re-check (12 boots × 8000 waits, slow-wait tracing) did NOT reproduce
  it — it is attributed to host noise during the original measurement. p99 of
  cross-core cond on a live desktop floats at 14-74 µs (vCPU preemption).
- WAITPKG / Sapphire Rapids: tier 1 (`umwait` in ring 3) is correct (all checks
  green, wakeup 201 ns), but in SPSC/MPSC burst phases it degrades to
  slice-granular wakeups (~120 s benchmark phase against 14 s on EPYC); tuning
  for SPR is on the roadmap. The kernel path (futex) on the same hardware is
  fast (212 ns) and is the default for applications.
- MPSC on one core: the consumer's pre-spin delays a producer on the same core
  (~13 µs against 0.67 µs across cores) — put the MPSC consumer on its own core
  (`--core`/`sched_setaffinity`).
- File `mmap`: MAP_PRIVATE and MAP_SHARED map pages of the block cache, so
  `read`/`write` and the mapping are coherent; ENODEV remains only in the
  identity address space (`talaria.cell=identity`), where the virtual address
  equals the physical one by construction.
- `talaria.mode=cell` runs the workload in ITS OWN address space (like
  `mode=proc`); the old behaviour is `talaria.cell=identity`. The modes differ in
  the process model (fork, signals and fd tables in `proc`), not in memory
  isolation.
- Signals: delivery, masks and signalfd are implemented to the needs of the
  databases and the JVM; exotic parts (siginfo fields, deep rt queues) are not
  complete.
- One instance of the workload per VM (scale with VM instances).

## 7. Requirements

- A Linux host with KVM (`/dev/kvm`), QEMU ≥ 7 (PVH), docker (the stand and the
  toolchains — `services/docker/build-images.sh`).
- The QEMU flag **`-overcommit cpu-pm=on`** is required for sub-µs
  synchronisation (native `mwait` in the guest). Without it everything works,
  but wakeups take ~16 µs.
- For networking: `/dev/net/tun`, `/dev/vhost-net`, `CAP_NET_ADMIN` (tap + vhost).
- Rust nightly — only to build the kernel (pinned; `kernel/BUILD.md`).

## 8. Roadmap and out of scope

- **Bare metal:** the production configuration runs on `c6i.metal` (v1.8).
  Missing on hardware: in-kernel ixgbe/mlx5 NIC drivers (the VFIO code is ready
  in `bench/ixgbe`), PTP and hardware packet timestamps (below), CAT on a live
  chip, a p99.9 tail SLA against a hardware baseline, x2APIC ids above 254.
  Details: `docs/ROADMAP.md` §§2.2-2.8.
- **Clocks: what exists and what does not.** EXISTS (v1.10): `CLOCK_REALTIME`
  runs on the TSC from a base point that is **disciplined by an external clock** —
  the servo adjusts frequency, not value, time stays monotonic, steps are
  counted separately; the reference is kvmclock under KVM or the ENA device's
  PHC (the readless protocol, `talaria.ena=phc`). MISSING: the **IEEE 1588**
  protocol (no Sync/Delay_Req, no BMCA — on the target stand there is nobody to
  talk to: AWS Time Sync comes through the ENA admin queue, not as 1588 packets)
  and **hardware packet timestamps** (ENA does not provide them; they will come
  with in-kernel ixgbe/mlx5). Also open: `*.metal` has no kvmclock, and the PHC
  is read through a device descriptor owned by the datapath — there is no
  periodic PHC discipline there yet.
- **DDIO/SDCI — out of scope by the design of the target hardware.** DDIO on
  server Intel parts is always on and cannot be turned off by software; only the
  share of L3 ways (undocumented MSR `0xC8B`; `EIO` on the i7-3820) and CAT masks
  are controllable. The documented lever is done: **CAT in the kernel** (`llc=`
  in the manifest, a class per core, read-back) plus an automatic check of the
  DMA ring footprint against L3. SDCI/DCA is not considered: on server Intel it
  is superseded by DDIO, and ENA does not support it.
- **New PCIe devices.** Network cards are attached through a device model
  (v1.10): the `NetDev` trait plus a driver table; a new card is a driver file
  and one table line, and neither the datapath nor the stack knows about cards
  (recipe: `docs/DEVELOPMENT-GUIDE.md` §10.6). MSI-X and MSI are handled; a card
  with only INTx is not routed, and the kernel says so. Not there YET: "BAR and
  DMA arena in ring 3" (UIO — what the whole FPGA world lives on), an IOMMU (DMA
  is not restricted at all — a conscious trade-off of the "one trusted
  workload" model, §2) and hot-plug. General-purpose GPUs are out of scope by
  design: a "GPU driver" is a contract with the vendor's userspace stack (CUDA
  and `nvidia.ko`, GSP firmware, memory manager), not access to registers.
- Per-core network stacks (today one stack under a lock, §4).
- ABI growth on demand: Go runtime (signals exist, needs a soak), static
  Python, NATS/Kafka clients.
- ext2: a multi-way block cache beyond the current four ways (`WAYS` in
  `kernel/src/ext2.rs`). A transaction already goes to the device as ONE batch
  (the descriptor plus the copies of the blocks, §2.3/§8.12); what is left is the
  CHECKPOINT — the copies are put in place one block at a time.
- TLS and authentication of the control plane beyond demo level.
- SMAP/SMEP (`SECURITY.md`).

## 9. Naming

The official name is **Talaria OS**; the search token is **TalariaOS**; handles
are `talaria-os`; the prefix is **TAL** (TAL-RFC-NNN, `TAL_*` environment
variables); a workload is a **cell**. Tagline: "latency-first
single-application OS (unikernel-style), Linux-ABI compatible". Not used:
"microkernel" (technically wrong), "TalOS" (collides with Talos Linux),
"Talaria Linux" (it is not Linux; a third-party trademark).

---

Related: `README.md` (version and status), `docs/USER-GUIDE.md`,
`docs/DEVELOPMENT-GUIDE.md`, `docs/MEASUREMENTS.md` (all the numbers and their
analysis), `docs/ROADMAP.md` (what is open), `docs/ENGINEERING-NOTES.md` (the
rules and the traps).
