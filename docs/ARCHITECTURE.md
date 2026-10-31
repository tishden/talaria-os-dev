# Talaria OS — the architecture

The design of the system: why it is shaped the way it is, how the pieces fit,
and what each mechanism buys. This page is the *concepts*; the *module map* is
`docs/DEVELOPMENT-GUIDE.md` §4, the *conditions behind every number* are
`docs/MEASUREMENTS.md`, and the *machine differences* are
`docs/ARCH-X86-ARM64.md`. Numbers below are load-bearing figures, quoted with
their conditions when it matters; each one's full provenance is in
`MEASUREMENTS`.

---

## 1. The problem and the bet

A general-purpose kernel is code on the hot path. To be fair and general it
pre-empts a running thread for a neighbour, migrates threads between cores,
sends inter-core TLB flushes, schedules behind every I/O, and charges the
`syscall` entry's full price even when the caller only wants a timestamp. None
of that shows up in the mean; all of it shows up in the **tails** — the p99,
p99.9 and p99.99 that a trading system, an embedded bus or a dedicated
appliance actually lives or dies on.

Talaria's bet is that a **latency-critical workload is one trusted application
per machine**. Drop the defence against hostile code and the multi-user model,
and the operating system collapses from a middleman into a control plane: the
**application owns the CPU cores and the devices**, the OS keeps the few
services the application cannot be trusted with — memory safety at the
boundary, the console, the supervisor, the operator's side.

The bet is made concrete by what it lets the kernel *not* do:

- **No KPTI** — there is no kernel/application isolation to enforce; the Meltdown
  page-table split is priced and rejected.
- **No retpoline** — Spectre's indirect-branch fence is priced and rejected.
- **No seccomp, no audit** — there is one trusted workload; the syscall surface
  is the kernel's own allowlist, not a filter over a hostile one.
- **No fairness** — a dedicated core gets no timer tick and runs to completion.

The product consequence is a **fixed deployment unit** (64 TCP sockets, 192
scheduler tasks — sized by static tables for "one VM per service", not "a
thousand connections per node"). Scale-out is more VMs.

---

## 2. The runtime model

A **cell** (`talaria.mode=cell`) is one multithreaded process under a
supervisor, launched by a manifest (`/cell.manifest`) that names the binary,
its arguments and the **placement rules**. The kernel reads the manifest and
lays threads out:

- a **dedicated core** — no timer tick, run to completion;
- a **named pool** — a set of cores shared by the threads put there;
- the **shared pool** — the default.

`pin=`/`spread=`/`numa=` decide who sits where; Intel **CAT** `llc=` divides the
L3 between groups of cores (x86-64 only — Graviton has no cache allocation).
The scheduler migrates a thread only on its *own* wake or yield, BSP → AP
(`ROADMAP.md` §5).

The **control plane** is the OS itself: an in-kernel console
(`top`/`ps`/`netstat`/`perf`), `filed` (files over the network), a supervisor
with restart and watchdog, and a syscall allowlist.

Boot reaches this from three shapes — PVH (`-kernel`), multiboot2, or UEFI on
x86-64; a Linux `Image` header with a device tree *or* ACPI, UEFI-only, on
AArch64 (`ARCH-X86-ARM64.md` §1).

---

## 3. The syscall path

The entry is `syscall`/`sysret` (x86-64) or `svc` (AArch64); the cost of the
**empty syscall is 28 ns**. Two things keep the real calls cheap:

- **Pointer validation is declarative.** Each syscall declares its buffers, and
  the gate checks them in 7–14 ns — only for the calls that take pointers. A
  bad pointer is an `EFAULT`, never the kernel writing on the process's behalf.
- **The vDSO answers the clock without a syscall.** `clock_gettime` is 15 ns in
  user space (127 ns before the vDSO existed; the JVM's `System.nanoTime()` is
  52 ns).

The dispatcher matches on x86-64's syscall numbers as the **internal**
numbering. AArch64 renumbers at the door (`abi::to_internal`, 163 rows) and
translates the layouts a different `unistd` implies — `struct stat` 128 vs 144
bytes, `struct epoll_event` 16 vs 12, the `clone` TLS/`child_tid` swap, the
`O_*` flag pairs. A layout difference is a memory-safety bug at the process's
side, so the translation lives at the one door, not in the dozen places that
fill the structures (`ARCH-X86-ARM64.md` §4).

---

## 4. Synchronization

The core idea is **sleep in the kernel, wake by a store** — no IPI on the wake
path.

- **futex.** `FUTEX_WAIT` sleeps in the kernel *on the user's own word*; the
  waker's store to that word is what wakes the sleeper. A cross-core futex
  round trip is **165 ns at 0 % CPU** on the waiting core. The sleep itself is
  `mwait`/`monitor` on x86-64 (a `umwait` tier exists on WAITPKG parts), **WFE**
  on AArch64. Stock musl's `FUTEX_WAKE` still follows and finds the sleeper
  awake; `libtsync` skips even that syscall.
- **The fallback is honest.** Without KVM's `-overcommit cpu-pm=on`, the kernel
  falls back to `hlt` + IPI: 15.8 µs. The gate measures both.
- **libtsync** is the optional userspace layer: SPSC/MPSC rings and **fibers**
  that switch in 11 ns, for the workload that wants the futex syscall gone too.
- **Across VMs** the same mechanism scales: both guests sleep in `mwait` on an
  **ivshmem** word, and a store wakes the peer in **496 ns** (vs a ~10 µs
  VM-exit path).

---

## 5. The scheduler and the tick

Pre-emption is by a **variable-period tick**: 100 µs when a runnable thread
waits, 2000 µs when the core is busy. A tick costs **2.3 µs** (it was 35 µs).
Three rules do the work:

- **Hysteresis on stealing.** A steal that does not improve the balance is
  refused — the work-stealing does not churn.
- **"Do not deschedule a thread that worked less than 50 µs."** The rule took
  idle PostgreSQL requests from 3.7 % to 0.07 % of the run: a thread that just
  started is not a candidate for the tick's pre-emption.
- **A dedicated core gets no tick** (the `nohz_full` analogue): the core runs
  the pinned thread to completion.

The wake path is the latency-critical one: `reqfirst` (the request that has
waited longest gets the core), the **doorbell** (a self-move wakes the target
core's scheduler), and the rule that a woken thread waits at most the
pre-emption threshold, not a full slice (`ARM64-PLAN.md` §11 item 2b). The
AArch64 wake uses a store-release plus the `WAKE_VECTOR` SGI with a `dmb ish`
for `membarrier` — an exception orders nothing by itself on arm64
(`ARCH-X86-ARM64.md` §5, §9).

---

## 6. Memory

- **One page-table format.** The shared `paging.rs` computes in a canonical
  format; the arch boundary translates to the machine's descriptor. The dirty
  bit, the address mask, the large-page encoding and the TLB shootdown all
  differ — `ARCH-X86-ARM64.md` §3 is the full map (DBM + the `H_SWD` bit vs the
  D bit; ASIDs vs no PCID; `tlbi ...is` vs targeted IPIs; MAIR vs PAT).
- **A private address space per process**, `fork` with copy-on-write.
- **Lazy `mmap`** with a 64 KB touch window: the JVM with `-Xmx256m` takes 60
  thousand frames instead of 874 thousand, and 15× fewer page faults.
- **Huge pages:** 1 GB for the kernel's own memory, 2 MB for the application by
  threshold (ClickHouse `uniqExact` 253 ms → 197 ms; one page-table entry
  instead of 512).
- **`mremap` moves the page-table entries, not the data** — a re-map without a
  copy.
- **The frame allocator** is a bump plus a free-list reclaim, handing frames
  from the *current thread's* NUMA node.
- **No swap, no overcommit.** RAM is a hard budget; a page that is not there is
  not silently invented.

The arm64 port adds the **direct map** — the kernel reaches any RAM through a
fixed high window (`talaria.dmap=strict`), so a process's space carries no
kernel identity and a kernel write through the map never misses
(`ARM64-PLAN.md` §10.16).

---

## 7. The filesystem

Own ext2 with a **JBD2 journal in ordered mode**: data blocks go in place, the
metadata commits after — a crash replays to a consistent map with the data that
was promised. The structural choices:

- **Separate FS and device locks.** A long device wait does not hold the FS
  lock; the lock is released while the disk is awaited.
- **A dirty-block counter** bounds the sync work and tells the power-off path
  what is owed.
- **`fsync` promises durability through the barrier**, and the gate replays it
  after a hard power cut (`orderedcut`/`orderedv`).
- **NVMe with a real command depth** — the block layer flies commands in a
  batch, not one by one.

---

## 8. The network

The datapath is descriptor **rings** in DMA memory and a **doorbell** write to
tell the card; the choice of **interrupt-per-packet vs polling** is a per-card
decision (`pollring`). Three shapes matter:

- **ENA LLQ** writes descriptors and the frame header **directly into device
  memory** — 33.6 µs UDP RTT on EC2 vs Linux's 43.0 on the same path. On
  AArch64 the LLQ record goes as whole 64-byte blocks through a Normal-NC
  window, ordered by the doorbell's `dmb oshst` (`ARCH-X86-ARM64.md` §10).
- **The syscall-free mode** hands the rings to ring 3, so the application polls
  shared memory without entering the kernel on the hot path.
- **One lock over the network stack** is a deliberate trade-off: it keeps the
  TCP/ingress path simple and the state coherent, and the cost is paid where
  the tail work happens (the scoreboard, `ROADMAP.md` §4.7).

---

## 9. The process model

A private address space per process, `fork` (copy-on-write), `execve`, signals,
pipes, shared memory, and a dynamic ELF loader that runs ld-musl and glibc's
ld-linux unmodified. The vDSO supplies `__kernel_clock_gettime`,
`__kernel_gettimeofday`, `__kernel_clock_getres` and, on AArch64, the
`__kernel_rt_sigreturn` trampoline the kernel must provide because arm64 has no
`rt_sigaction` restorer (`ARCH-X86-ARM64.md` §4).

---

## 10. Protection, and what is rejected

For the trusted workload the kernel enforces the boundaries that catch *bugs*,
not the ones that resist an *adversary*:

- **W^X** per segment and **NX**;
- **guard pages** around stacks and the kernel's own;
- the **syscall allowlist** (a forbidden call is `EPERM`);
- the **supervisor**: a crash is caught, the image reloaded, the watchdog armed.

Rejected on purpose, priced and documented: **KPTI**, **retpoline**, **seccomp**,
**audit** — the patches that defend a threat model a single trusted workload
does not have.

---

## 11. Observability and the operator

Observability is built into the kernel, not bolted on:

- the **console**: `top`/`ps`/`netstat`/`perf` in the kernel, plus a line editor
  and a shell for the operator;
- a **sampling profiler** with stacks (unwound through the x29 chain on arm64,
  frame pointers on x86-64);
- **ELF core dumps** readable by a real `gdb`, and a **gdb stub** for live
  debugging over the console;
- a **flight recorder** in the crash dump, and the supervisor's status line as
  the telemetry (`/ctl`).

---

## 12. The two machines

Everything above is implemented once in the shared modules and reaches the
hardware through one of two `arch/` surfaces. The complete difference map —
boot, the exception/syscall entry, the MMU, the ABI, interrupts, time, the wait
tiers, FP/SIMD, the memory-ordering model, devices, crypto, cache allocation,
the instruction cache, the PMU — is `docs/ARCH-X86-ARM64.md`. In one line:
x86-64 is TSO, PCID-less, IDT + LAPIC + IOAPIC + MSI, a calibrated TSC, XSAVE,
real-mode SMP and Intel CAT; AArch64 is a weak memory model, ASIDs, a vector
table + GICv3 + ITS, a read-from-register generic timer, a 528-byte FP/SIMD
state, PSCI SMP and no cache allocation. The memory-ordering audit that the
weak model forced is `ENGINEERING-NOTES.md` §6.1.

---

## 13. The invariants

The rules every change must respect (the full list is `DEVELOPMENT-GUIDE.md` §12):

- **A number is a measurement, not a line of reasoning.** A claim without a
  stand, a gate or a bound does not get in.
- **The judging rules do not move.** A board scores a point at 0.95/1.05
  against Linux at the same rate, and "within a workload" — a board that moves
  its own goalposts proves nothing.
- **An exception on AArch64 orders nothing by itself.** A cross-core or
  cross-device ordering needs the barrier the site names (`dmb ish`/`osh`,
  acquire/release); the ten defects the weak model found are the evidence
  (`ENGINEERING-NOTES.md` §6.1).
- **The arch boundary is the only place the two machines differ.** A shared
  module must not learn which CPU it runs on; the translation lives in `arch/`.
