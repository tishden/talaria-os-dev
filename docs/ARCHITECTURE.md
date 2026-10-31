# Talaria OS — the architecture

The one-page map of the system: the bet, the runtime model, and the mechanisms,
with the concepts the public article marks as important
(<https://habr.com/ru/articles/1099054/>) called out. Depth lives in the linked
docs; this page is the index.

The public article is the x86-64 story. The arm64 port (`docs/ARM64-PLAN.md`)
reaches the same mechanisms on a second machine; where the two implementations
differ, `docs/ARCH-X86-ARM64.md` is the reference.

---

## 1. The bet

Talaria is a **latency-first single-application OS** in the unikernel family:
it runs *unmodified Linux binaries* in ring 3 (EL0 on arm64), with the
**workload owning the CPU cores** and the OS reduced to a **control plane**.
The contract, and what it deliberately gives up:

- **What it keeps:** the Linux ABI — `syscall`, the memory model, `fork`/`execve`,
  signals, pipes, shared memory, a dynamic ELF loader, `futex`, a vDSO — so that
  PostgreSQL, MariaDB, MongoDB, Redis, nginx, mongoose, OpenJDK 21 and ClickHouse
  run without a rebuild.
- **What it drops:** protection against a *hostile* workload (there is one trusted
  workload per VM), the full syscall surface (~185 calls, an allowlist on top), and
  the multi-user model. KPTI/retpoline/seccomp/audit are priced and rejected — they
  defend a threat model the product does not have.
- **The goal:** the p99/p99.9/p99.99 **tail**, not the mean. Every mechanism below
  exists to move a tail.

This is the "one VM per service, the application owns the machine" model the
article opens with. `docs/PRODUCT.md` §1-§2 states the boundaries formally.

---

## 2. The runtime model

- **A cell** is one instance of the OS: a manifest (`/cell.manifest`) names the
  binary, its arguments and the **placement rules** — `pin=`, `spread=`, `numa=`,
  the pools, and the Intel CAT `llc=` shares (x86 only).
- **The cores:** a thread may get a **dedicated core** (no timer tick,
  run-to-completion), a named pool, or the shared pool. The manifest, not the
  application, decides who sits where. The scheduler migrates one's *own* thread
  only, BSP → AP (`ROADMAP.md` §5).
- **The control plane:** the OS itself is the operator's side — an in-kernel
  console (`top`/`ps`/`netstat`/`perf`), `filed` (files over the network), a
  supervisor with restart/watchdog, and an allowlist.

---

## 3. The mechanisms

### 3.1. The syscall path (article: *syscall/sysret, pointer validation*)

A cheap `syscall`/`sysret` (x86) or `svc` (arm64) into ring 0, a **declarative
pointer-validation table** (7–14 ns, only for the calls that take pointers — an
`EFAULT` instead of the kernel writing on the process's behalf), and a
dispatcher that matches on x86-64's syscall numbers as the internal numbering
(arm64 translates at the door, `docs/ARCH-X86-ARM64.md` §4). Empty syscall: 28 ns.

### 3.2. Synchronization (article: *futex, mwait/monitor, libtsync*)

- **futex:** `FUTEX_WAIT` sleeps *in the kernel* on the user's own word, so the
  waker's store itself wakes the sleeper — **no IPI**. Round trip 165 ns across
  cores, 0 % CPU while waiting. The sleep is `mwait`/`monitor` on x86, **WFE** on
  arm64 (one tier on both ring 0 and ring 3).
- **libtsync:** the optional userspace library — SPSC/MPSC rings and **fibers**
  that switch in 11 ns, for the workload that wants even the futex syscall gone.

### 3.3. The scheduler and the tick (article: *планировщик, тик, nohz_full*)

Pre-emption by a **variable-period tick** (100 µs / 2000 µs; a tick costs 2.3 µs),
work stealing with **hysteresis** (a steal that does not improve balance is
refused), and the rule "do not deschedule a thread that worked < 50 µs" (which
took idle PostgreSQL requests from 3.7 % to 0.07 %). A dedicated core gets **no
tick** (Linux's `nohz_full` analogue). Placement, affinity and the wake path
(`reqfirst`, the doorbell) are `docs/DEVELOPMENT-GUIDE.md` §6.

### 3.4. Memory (article: *виртуальная память, TLB shootdown, huge pages, mremap*)

- **Page tables** in one canonical format, translated at the arch boundary; a
  **per-process address space**, `fork` with COW, lazy `mmap` (a 64 KB touch
  window: the JVM's 874 k frames became 60 k).
- **TLB shootdown** reaches only the cores *holding* an address space, and sleeps
  the sleeping ones (x86); arm64 does it with a broadcast `tlbi ...is`
  (`docs/ARCH-X86-ARM64.md` §3).
- **Huge pages:** 1 GB for the kernel's own memory, 2 MB for the application by
  threshold (ClickHouse `uniqExact` 253 ms → 197 ms).
- **The frame allocator:** a bump + a free-list reclaim, handing frames from the
  *current thread's* NUMA node. No swap, no overcommit — RAM is a hard budget.

### 3.5. The filesystem (article: *ext2 + JBD2 ordered, fsync, NVMe*)

Own ext2 with a **JBD2 journal in ordered mode** — data blocks go in place, the
metadata commits after. Separate FS and device locks, a dirty-block counter, and
the lock released while the device is awaited. `fsync` promises durability
through the barrier. NVMe with a real command depth.

### 3.6. The network (article: *кольца/DMA/doorbell, ENA LLQ, без сисколов*)

Descriptor **rings** in DMA memory, a **doorbell** write to tell the card, and a
choice of interrupt-per-packet vs **polling**. The **ENA LLQ** mode writes
descriptors and the frame header directly into device memory (33.6 µs UDP RTT on
EC2 vs Linux's 43.0). The **syscall-free network mode** hands the rings to ring 3
so the application polls shared memory without entering the kernel.

### 3.7. The process model (article: *модель процессов*)

A private address space per process, `fork` (COW), `execve`, signals, pipes,
shared memory, a dynamic ELF loader (ld-musl, glibc), and a vDSO that answers
`clock_gettime` in 15 ns without a syscall.

### 3.8. Protection, and what is rejected (article: *W^X, guard pages, митигации*)

W^X per segment, NX, **guard pages**, a **syscall allowlist**, and the supervisor
(restart + watchdog). Rejected on purpose: KPTI, retpoline, seccomp, audit — the
Meltdown/Spectre patches that defend a threat model the product does not have.

### 3.9. Observability and the operator (article: *top/ps/netstat/perf, профайлер, кор-дампы, gdb*)

`top`/`ps`/`netstat`/`perf` **in the kernel console**, a sampling profiler with
stacks, ELF core dumps readable by a real `gdb`, a gdb stub, and a flight recorder
for the crash dump. The supervisor's status line and `/ctl` are the telemetry.

### 3.10. Deployment (article: *QEMU/KVM, EC2, bare metal, VM exit, steal time*)

Boots under **QEMU/KVM**, as an **AMI on EC2** (through ACPI/PCIe/UEFI), and on
**bare metal** (`c6i.metal` x86-64 through our own BIOS chain; `c7g.*` arm64).
The hypervisor is part of the latency budget — VM exits and steal time are
measured and reported, not ignored.

---

## 4. Two machines

The mechanisms above are implemented once in the shared modules and reach the
hardware through one of two `arch/` surfaces. The full difference map —
boot, exceptions, MMU, ABI, interrupts, time, wait tiers, FP/SIMD, memory
ordering, devices, crypto, cache allocation, the I-cache, the PMU — is
`docs/ARCH-X86-ARM64.md`. In one line: x86-64 is TSO, PCID-less, IDT + LAPIC +
IOAPIC + MSI, calibrated TSC, XSAVE, real-mode SMP and CAT; AArch64 is a weak
model, ASIDs, a vector table + GICv3 + ITS, a read-from-register generic timer, a
528-byte FP/SIMD state, PSCI SMP and no cache allocation.

---

## 5. Where the concepts are proven

- The numbers behind each mechanism: `docs/MEASUREMENTS.md` (x86 §1-§9, the arm64
  scoreboard §10).
- The rules for judging a measurement and the closed hypotheses:
  `docs/ENGINEERING-NOTES.md`.
- The invariants a kernel change must respect: `docs/DEVELOPMENT-GUIDE.md` §12.
- The public article these concepts are marked in:
  <https://habr.com/ru/articles/1099054/>.
