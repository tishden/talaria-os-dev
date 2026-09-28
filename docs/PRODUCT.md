# Talaria OS — the product: boundaries, trade-offs, performance, limits

What Talaria OS is, what it guarantees, what it pays for its properties and
where its boundaries lie. The numbers and statements here are checked against
the code and the measurements (`docs/MEASUREMENTS.md`); the version and the
regression count live in `README.md`. Guides: `docs/USER-GUIDE.md` (running),
`docs/DEVELOPMENT-GUIDE.md` (development).

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
while waiting (mechanics: DEVELOPMENT-GUIDE §6b, results: MEASUREMENTS §6).

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
