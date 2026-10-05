# Talaria OS — the open plan

What is **not** done, why, and what it would take. This is the working plan: the
product-level summary is `docs/PRODUCT.md` §8, the history of what has been done
is `CHANGELOG.md`, and the numbers behind the verdicts below are in
`docs/MEASUREMENTS.md`.

Every item says what it is blocked on. Three blockers recur: **hardware** (the
thing cannot be exercised under QEMU at all), **a measurement** (it was
measured and is deliberately not done), and **time**.

---

## 1. Where the project stands

There are no release blockers. The gate of the current tag is in `README.md` and
is re-run in full before each one: `kernel/test.sh` plus the host block,
`kernel-unit`, clippy `-D warnings` in both kernel configurations,
`kernel/validate.sh` with the databases, JVM and ClickHouse, a power-cut test,
and an export of the public tree with hard checks and a secret scan.

**Performance: there are no open fix candidates.** The score is scoreboard 41 —
295 comparable points on six real workloads — and all eleven lost points are
analysed by name and closed (`docs/MEASUREMENTS.md` §2.3): five are vCPU theft by
the hypervisor (cured by placement, not by code), two are a scheduler band for
which BOTH cures were written and rejected by measurement, two are a class that
did not survive a second bring-up, and two sit on the 5 % judging boundary.

---

## 2. Open now

### 2.1. CI on a GitHub runner — never run once

`.github/workflows/ci.yml` exists and the local gate is documented in
`CONTRIBUTING.md`, but two jobs have never been exercised on a hosted runner:
`kernel-full` (needs docker) and `regression-subset` (needs `/dev/kvm`; the
deterministic subset is `TAL_CI=1`: 181 asserts in QEMU plus 16 host checks, ~7
minutes). The triggers are present for `workflow_dispatch`. Until that run
happens, the CI state of the repository is unknown rather than green.

### 2.2. Multi-queue receive and real RSS — deferred BY MEASUREMENT

One queue pair today, with the RSS table pointing entirely at it. Measured to be
enough for the whole packet limit of a `c6i.large`. Worth doing when an instance
type appears where one queue does not fit the limit, and then together with a
cell per core — otherwise there is nobody to drain the queues.

The cost: `Ena` has to be rewritten from single fields to arrays of queues and
the queue index threaded through `NicQ`, `fastpath` and `l1` — with no local
regression, because there is no card under QEMU.

**On the checklist when this is opened:** the ring footprint against L3. The rule
"a ring fits in L3" is currently satisfied BY A CONSTANT (`RX_RING=256` — 520 KB);
32 queues of the same rings are 16.6 MB, which is larger than L3 on part of the
target hardware — exactly the leaky DMA measured at ×5.6 in LLC misses. The
kernel now computes the footprint itself (`cat::report_footprint`, printed when
ENA comes up), but the REAL number of queues has to be passed to it, or the check
will keep reporting about one queue and stay silent where it should shout.

### 2.3. Clock discipline on `*.metal` — there is no reference to lean on

Under KVM the reference is cheap (kvmclock: read a page, good enough for a tick).
On metal there is none, and the only available reference — the ENA device's PHC —
runs into two things. First, `phc_read` calls into the device and polls for the
answer with a 1 ms timeout, and an unbounded wait has no place inside an
interrupt handler: a two-phase non-blocking form is needed (`request` on one tick,
`poll` on the next — the device answers in microseconds). Second, the `Ena`
descriptor is owned by the datapath (`l1::Nic`) and there is no global access to
it: either introduce one or feed the discipline from the context that already
holds the NIC. Today `talaria.test=phc` feeds the reference ONCE on metal (the
plumbing is verified); there is no periodic adjustment. **The cost of doing
nothing is known: 95 ppm — 8.2 seconds a day.**

**A reference that does not need the device (2026-10-05):** `talaria.ntp=aws|<ip>` feeds the
same servo from SNTP in the network stack's pass (`kernel/src/ntp.rs`; the shortest round trip
of a four-query burst is the sample). On a `c7g.large` it holds the clock within ±21 us of the
Amazon Time Sync Service, judged from outside (`docs/MEASUREMENTS.md` §10.9). What is left
here is the PHC as a BETTER reference where an instance type has it, not the only one.

### 2.4. CAT on a live chip

The code is in the kernel (`cat.rs`, `llc=` in the manifest) and the logic is
gated locally — mask validation, footprint arithmetic, the whole path from
manifest to pool to a refusal without CAT — but the ACTUAL MSR write cannot be
checked on a local stand: CAT is a server-Intel feature (RDT-A) and KVM does not
pass it to a guest (`CPUID 0x10` is empty under QEMU). Needs a run on `*.metal`:
mask read-back, `IA32_PQR_ASSOC` on the pool's cores, and a repeat of the
synthetic + LLC-hog measurement with CAT and without.

### 2.5. The development machine's own hardware: 82599 and Alveo U50

The dev box has two Intel 82599ES 10G ports and a Xilinx Alveo U50. The device
model (v1.10) is ready to accept both: binding rules, MSI parsing, ring footprint
accounting, a BAR taken from the bridge window when firmware assigns none, a
write-combining window and contiguous DMA memory.

**It is not blocked on code.** The host has no IOMMU (`/sys/class/iommu` is
empty, the board has no VT-d), so QEMU cannot pass a card into a guest at all —
Talaria-in-QEMU will never see these devices, however much is written. Two roads:
(1) boot Talaria on that machine ON BARE METAL with our own `tools/tboot/` chain,
which already does this on `c6i.metal` — then both cards are directly visible with
no passthrough; (2) find a host with VT-d. The first is cheaper and closer:
everything needed is written, what is missing is a boot medium and a stand.

Order of work when it happens: `talaria.pci=1` plus the console's `nics` on bare
metal (see both cards and their interrupt type) → an ixgbe driver per
`docs/DEVELOPMENT-GUIDE.md` §10.6 (82599's rings are simple, there is no LLQ
analogue) → for the Alveo, the UIO path first (BAR plus a DMA arena in ring 3)
rather than an in-kernel driver, because the XDMA register map is defined by the
bitstream and keeping it in the kernel makes no sense.

### 2.6. `filed` on top of a RUNNING cell

Today the file service is served only where the kernel itself owns the polling
loop (`mode=console`, `mode=filed`), because `netsys` is not re-entrant: calling
it from the scheduler's tick means entering the stack on top of a workload that is
already inside it. Over a cell, file access goes through the serial console —
that is a declared boundary, not an omission, but it is worth removing.

What it needs: either a separate management interface with ITS OWN socket set and
its own loop on a control-plane core (the model already allows it — interfaces are
independent), or explicit serialisation of entry into `netsys` (a busy flag plus a
deferred queue). The first is more honest: the management network has its own card
anyway, and mixing its datapath with the working one buys nothing.

### 2.7. The operator's side on bare metal

Everything built for the console and the file service is verified under QEMU. On
`*.metal` there is no console at all (no COM1), so the operator's side there is
`filed` over the network plus `talaria.trace=` to disk. To check on a live
instance: bringing up several ENIs as separate interfaces (`ip`/`route` through
the disk trace), uploading a binary over `filed` into the management network, a
delta upload at real latency. Separately: `talaria.rows=`/`cols=` and the ANSI
editor in the EC2 console — Nitro's serial console is not a full ANSI terminal, so
`edit` may be inapplicable there, and that has to be either confirmed or written
down as a boundary.

### 2.8. In-kernel NIC drivers, PTP and a hardware tail SLA

The VFIO code for 82599 is ready in `bench/ixgbe` and the datapath numbers from it
are in `docs/MEASUREMENTS.md` §5. What is missing in the kernel: the driver
itself (see §2.5), hardware packet timestamps (ENA does not provide them; they
arrive with in-kernel ixgbe/mlx5), the IEEE 1588 protocol (not planned before a
stand with a grandmaster exists — on EC2 the time source arrives through the ENA
admin queue, not as 1588 packets), and a p99.9 tail SLA measured against a
hardware baseline rather than a virtualised one. Also open: x2APIC ids above 254.

### 2.9. Smaller open items

* **`talaria.test=connect` flakes and does not reproduce** — 40 consecutive runs
  without a failure. The serial diagnostics are already in place, so the next
  occurrence will name the cause. Not worth hunting deliberately. (The current
  flake list is in `CONTRIBUTING.md`.)
* **`wvsock` cannot be a gate**: both ends in one process need a connection to
  THEIR OWN address, and the stack has no loopback interface (a bind on 0.0.0.0
  goes through, a connect to the node's own address does not). To make a gate, the
  reader has to move out to the host. The behaviour is meanwhile covered by the
  suite: syscalls per request for nginx 3.0 → 2.0 and exactly one tx frame per
  request.
* **WAITPKG / Sapphire Rapids**: tier 1 (`umwait` in ring 3) is correct — all
  checks green, wakeup 201 ns — but in SPSC/MPSC burst phases it degrades to
  slice-granular wakeups (a ~120 s benchmark phase against 14 s on EPYC). Tuning
  for SPR is open. The kernel path (futex) on the same hardware is fast (212 ns)
  and is what applications get by default.
* **Adaptive same-core MPSC pre-spin.** An MPSC consumer on the SAME core as its
  single producer costs ~13 µs against 0.67 µs across cores, because the
  poll-then-sleep pre-spin delays the producer. Documented as a recommendation
  instead (`docs/PRODUCT.md` §6: put the consumer on its own core); skipping the
  pre-spin based on a same-core parking history is the actual fix.
* **Per-core network stacks.** One stack under a machine-wide lock today; many
  threads doing network syscalls at once contend for it.
* **A lost device barrier is not carried through to an `EIO` at `fsync`.** When the
  disk does not answer a flush within 10 s the durability of that `fsync` is not
  promised, but the call still returns 0: `fsync` answers `EIO` only when a WRITE
  failed (`fs::WRITE_FAILED`), and a lost barrier does not set it. It is counted
  (`fs::FLUSH_LOST`) and printed in `stat:`, which is how it is noticed today.
* **"No stall longer than 20 ms for the neighbour" holds on several cores, not on
  one.** The owner of the FS lock is recognised by the CORE'S NUMBER and so cannot
  yield to a neighbour sharing its core, while a barrier under that lock on a
  gp3-class disk lasts up to ~170 ms (measured: one run in thirty). The gate
  `talaria.test=fsyncconc talaria.smp=1` therefore judges the files' content rather
  than the neighbour's latency.
* **ext2**: the journal CHECKPOINT still puts copies in place one block at a
  time (a transaction already goes to the device as ONE batch), and the block
  cache is four-way (`WAYS` in `kernel/src/ext2.rs`).
* **TLS and authentication for the control plane** beyond demo level; `up` and
  `supervise` over TCP (needs privilege on the host side).
* **The cell's telemetry channel** (serial/shm) beyond what exists
  (`telemetry.rs` plus the supervisor's status line and `/ctl`).
* **Protecting the DMA memory from the workload's allocator.** A user-data token
  that touches the network makes the kernel reopen the NIC, and the cheap half of
  that is fixed (do not do what is unnecessary — the kernel prints the guilty
  token by name). Fencing off the DMA rings from the workload's own allocator is
  the other half and is not done.

---

## 3. Deliberately not done, with the reason

Re-opening one of these costs at least a day. The reason is written down so the
reasoning does not have to be repeated.

| Item | Why not |
|---|---|
| **Deferred block allocation (delalloc)** in ext2 | A cache slot is addressed BY BLOCK NUMBER, so a data page must have its number at write time. Deferring the decision means a second index (inode, block-in-file) and re-teaching mmap pins, journal tags and coalescing to use it. What delalloc is done for was obtained by run-based allocation: bitmap trips 33 → 7 per 512 data blocks. |
| **SMAP** | The kernel copies the process's memory with ordinary loads, and that is what makes a syscall cheap. SMEP is enabled where the CPU has it. |
| **DDIO / SDCI control** | DDIO on server Intel parts is always on and cannot be turned off by software; only the share of L3 ways (an undocumented MSR) and CAT masks are controllable, and the documented lever — CAT in the kernel — is done. SDCI/DCA is superseded by DDIO and ENA does not support it. |
| **An IOMMU** | DMA is not restricted at all: a conscious trade-off of the "one trusted workload" model (`docs/PRODUCT.md` §2). |
| **General-purpose GPU drivers** | A "GPU driver" is a contract with the vendor's userspace stack (CUDA, `nvidia.ko`, GSP firmware, a memory manager), not access to registers. |
| **Hot-plug**, INTx-only cards | MSI-X and MSI are handled; a card with only INTx is not routed and the kernel says so. |
| **A full coreutils** | Contradicts a single-application minimal OS and needs fork/exec/shell in the control path. The answer is the in-kernel console plus `filed` plus the gdb stub; management belongs to the control plane. |

**Deliberately not covered by tests** (documented, not forgotten):

* everything behind `talaria.ena=*` and `talaria.test=phc` — there is no card
  under QEMU, so the gate for them exists only on EC2. When `ena.rs` changes, the
  regression will not catch it: that is the price of working with somebody else's
  card. Partial compensation: `talaria.test=wc` checks the write-combining
  mechanics (PAT and the page-type change) without a card.
* continuing the chain on an htree hash COLLISION (`dx_next_leaf`): making a leaf
  split on a boundary of equal hashes needs hundreds of names with a colliding
  32-bit hash, and such a set cannot be built by brute force.
* `ext2::mmap_unprepare` — the mapping rollback path; it runs only when pages
  cannot be raised into the cache and pinned.
* `talaria.smp=N` is exercised by the `services/actors` benchmark, not by
  `test.sh`; the diagnostic modes `talaria.test=threads|ipi|sysbench` are legacy
  probes and are outside the gate on purpose.

---

## 4. The roadmap, in order of size

1. **Bare metal as a supported configuration.** Talaria boots on `c6i.metal`
   through our own BIOS chain and the production mode runs there whole (§2.7,
   §2.3, §2.4 are what is missing); making it a *product* configuration —
   with hardware NIC drivers (§2.8) and a tail SLA against a hardware baseline —
   is its own stage.
2. **Track A (host-VFIO) as a production tool.** The measurement instrument and
   the datapath reference (`bench/ixgbe`) are useful in themselves; what is
   missing is multi-core operation (a hot loop on a dedicated core, background
   threads on the BSP), persistence, an operator's side (manifest launch through
   the control plane plus supervision plus telemetry) and removing UDP
   fragmentation of large frames by segmenting them.
3. **The challenge harness** (`bench/task-harness`). Needs a stand with ≥ 6
   physical cores (or `isolcpus`) for `IO=talaria`: on a 4C/8T host the cells
   share HT threads with the vCPUs. Then: the shm doorbell instead of virtio data
   notifications (an mwait word in ivshmem, proved at 496 ns RTT) integrated into
   the backend, and guest-side `busy_poll`/`napi` sysctls on the Linux side for a
   fair comparison.
4. **Porting workloads.** A static Python 3 (no dlopen, so no `.so` modules); a
   Go binary (its own runtime model — clone/futex plus SIGURG for preemption;
   signals exist, it needs a soak); nginx in a production configuration; NATS and
   Kafka clients; a thread-pool/epoll server over many connections as a stress
   test for the scheduler.
5. **Memory work for a LARGE cell state.** Today's datapath is memory-light and
   the working set is tiny, so this only matters when the cell holds a big state
   (an order book over thousands of instruments, a tick store, features) and TLB,
   LLC and NUMA start to dominate. In order of expected return: dTLB-miss and
   memory-bandwidth counters in the telemetry (without measurement this is blind);
   prefetch of the next RX descriptor and the next buffer header, paired with
   adaptive batching; a synthetic large state to make memory the bottleneck at
   all; mapping the whole cell state with 1 GB pages (TLB reach from 6 MB to
   1.5 TB); NUMA locality (`node=` exists in the manifest; per-shard memory
   binding and NIC DMA into the local socket's buffers); MBA throttling next to
   the existing CAT (`mba_percent` in the manifest); a DDIO-aware RX ring size;
   cache-conscious layout (64-byte alignment on rings and per-shard counters,
   hot/cold split, a sliding order-book window); non-temporal stores for a tick
   store or append log.
6. **In-place TLS on the transmit path.** The trading pipeline
   (feed → strategy → risk → crypto+TX as a shard-per-core pipeline) exists and is
   measured; what remains is encrypting the order IN PLACE in the DMA buffer
   (AES-128-GCM with AES-NI, ~111 ns for a 96-byte record) instead of the usual
   application-TLS path of an OpenSSL record plus a copy plus a syscall per
   order. The verification is a stage-by-stage tick-to-trade breakdown showing
   that the hot price of TLS is the AES-GCM alone.
7. **The arm64 port** (EC2 Graviton, QEMU `virt`). Planned in full in
   `docs/ARM64-PLAN.md`: the inventory of every x86 mechanism the kernel leans
   on and its replacement, nine phases with named gates, the risks ranked.
   Phases 0-4 are done on the `arm64-port` branch (§10 of the plan): the kernel
   boots on QEMU `virt` and on a `c7g.metal` under KVM, and the regression that
   has an arm64 form passes — the ring-3 set (one source per test for both
   machines, the C++ set included), the process model, the signals, the core
   dumps of a workload and of the kernel, the supervisor, the network over
   virtio-net, PCIe through ECAM, NVMe, MSI-X through the GICv3 ITS, the
   ext2/jbd2 block on both disks; `kernel/test-aarch64.sh` is 232 checks. The
   product boot path runs under `edk2` and on EC2 itself: a `c7g.large` boots
   the AMI to ring 3, runs ext2 on the EBS NVMe, both vCPUs, and the ENA with
   LLQ, DHCP and a UDP echo (§10.8). What is left: the RAM above 4 GiB (the
   shared identity window), a clock reference on Graviton, the weak
   memory model (an audit of 1 444 `Relaxed` and 367 `volatile` sites that TSO
   made correct — phase 5; the device barriers are in, the soak that judges the
   rest is `kernel/soak-aarch64.sh`), and the six workloads (phase 7). The stand
   is `infra/aws/kvm-stand-aarch64.sh`: QEMU under TCG on the x86 box runs the
   whole gate but can show neither the timing gates nor a single ordering bug —
   though a LOADED TCG host found four races of the shared code that a quiet KVM
   one never lost (`docs/ENGINEERING-NOTES.md` §6.1).

---

## 5. Known limitations of the current SMP model

These are implementation constraints rather than plans, and they bite when
writing kernel code or placing threads. The mechanics are in
`docs/DEVELOPMENT-GUIDE.md` §6b.

* **Migration is of ONE'S OWN thread only, and BSP → AP only** (there is no way
  back). An AP is run-to-completion — no timer, no preemption, one task at a time.
* **Signals are not delivered to AP threads** (delivery is on BSP paths).
* **An AP thread's syscall surface**: futex, clock, `write` to serial, `mmap` and
  `exit` are verified; file and network syscalls from an AP are not serialised
  against the BSP — do not call them there.
* **`clone`/`fork` from an AP thread are untested** — create threads from the BSP.
* **futex is private-semantics within a process** (a cross-process futex over
  shared memory is not matched; the database workloads do not use one).
* **futex timeouts**: tick granularity on the BSP, an exact TSC-deadline on an AP;
  without the latter a timed wait on an AP degrades into pause-polling.
* **The fd table is not thread-safe** (it is designed for one datapath core).
* A hardware watchpoint is fixed at 4 bytes.
* Shared pages between two ELF segments are last-writer-wins for permissions
  (safe for page-aligned musl; arbitrary ELFs would want an OR of the rights).

---

Related: `docs/PRODUCT.md` §8 (the product-level roadmap and what is out of
scope), `docs/MEASUREMENTS.md` (the numbers behind the verdicts),
`docs/ENGINEERING-NOTES.md` §7 (hypotheses already closed by measurement),
`docs/DEVELOPMENT-GUIDE.md` §10 (step-by-step recipes for the work above),
`CONTRIBUTING.md` (the gate and the flake list).
