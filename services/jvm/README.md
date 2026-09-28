# The JVM on Talaria (Eclipse Temurin 21, musl)

Unmodified **OpenJDK 21 (Temurin 21.0.12.1+1, the Alpine/musl build)** starts
on Talaria in `talaria.mode=proc` and passes a multi-threaded benchmark. One and
the same JDK directory and one and the same `Bench.class` run on Talaria and on
Linux (Alpine in docker) on **the same machine and the same number of cores** —
otherwise the comparison makes no sense.

```
openjdk version "21.0.12.1" 2026-08-18 LTS
OpenJDK 64-Bit Server VM Temurin-21.0.12.1+1 (build 21.0.12.1+1-LTS, mixed mode, sharing)
```

## How to run

```sh
cd services/jvm
# 1. the JDK: build/jdk-21.0.12.1+1 (Temurin, alpine-linux x64 musl)
curl -sL -o build/jdk21-musl.tar.gz \
  "https://api.adoptium.net/v3/binary/latest/21/ga/alpine-linux/x64/jdk/hotspot/normal/eclipse"
tar -C build -xzf build/jdk21-musl.tar.gz
# 2. the benchmarks (compiled with THE SAME JDK in an Alpine container)
./build-bench.sh
# 3. the disk image (JDK + musl + classes + manifest)
JVM_MAIN="Bench all" ./mkdisk.sh 900M
# 4. run on Talaria (SMP = number of cores, MEM = MB)
SMP=4 MEM=3072 ./run.sh 400
# 5. the Linux reference — the same JDK, the same class, as many cores
SMP=4 ./run-linux.sh all          # natively in docker
SMP=4 ./run-linux-vm.sh all       # in a Linux guest under the same QEMU/KVM
```

`JVM_MAIN` — what to run (`-version`, `Bench all`, `Bench scale pingpong`…),
`JVM_ARGS` — JVM flags. The defaults in `mkdisk.sh` are chosen for Talaria:
SerialGC (fewer service threads), `-Xmx256m`, no hsperfdata.

## What the kernel had to add (and why)

| What | Why the JVM needs it |
|---|---|
| **execve of a dynamic/PIE binary with an environment** (`user::execve_dyn`) | the `java` launcher RE-EXECUTES itself (`execve`) with `LD_LIBRARY_PATH` appended. The old execve loaded the ELF without a PIE base, without the interpreter and without envp |
| **Synchronous signals from CPU exceptions** (`syscall::deliver_exception_signal`) | HotSpot catches SIGSEGV on EVERY implicit null check, on the safepoint poll and on the stack-depth check, and continues after adjusting `ucontext`. The handler gets `siginfo` (si_addr = CR2) and `ucontext` (gregs, ERR/TRAPNO/CR2, FPU); the return is a normal `rt_sigreturn` |
| **`sigaltstack`** | the JVM sets an alternate stack for every thread and handles stack overflow on it |
| **PROT_NONE as a truly inaccessible page** (`paging::protect_none`, the PNONE bit) | the safepoint polling page is made PROT_NONE and a fault is expected from it; while PROT_NONE stayed readable a safepoint would never come |
| **Demand-zero anonymous memory** (`PTE_LAZY`, `paging::lazy_fault`) | the JVM reserves the heap, the code cache and the class space entirely. With `-Xmx256m`, eager allocation handed out **874,308** frames (~3.4 GB) and hit the memory ceiling; with lazy allocation — **60,829** (14 times fewer) |
| **`MAP_FIXED_NOREPLACE`** | HotSpot (seeing `uname` 5.15) tries heap addresses with exactly this flag and takes the next candidate on `EEXIST`; ignoring the flag meant SILENTLY overwriting an existing mapping — heap corruption |
| **A real `MADV_DONTNEED`** | runtimes "decommit" pages and expect ZEROS on the next access; an "ok" without doing the work is silent corruption |
| **TLB shootdown between cores** (`paging::shootdown`, vector 0x42) | when a mapping was removed or tightened, a neighbouring core kept writing through a stale TLB entry — into a frame already given to someone else |
| **The demand-zero race** (CAS on the PTE + "the page is already present ⇒ retry") | two cores fault on one page: the loser must return its frame, not replace the page the other one filled with zeros |
| **Read-only file `MAP_SHARED` as a private copy** | the JVM maps `lib/modules` (136 MB); for a mapping without `PROT_WRITE` a copy is indistinguishable from a shared one |
| **`F_GETPIPE_SZ`/`F_SETPIPE_SZ`** | a zero answer to "how big is the pipe" is an endless loop in code that grows it |
| **The vDSO** (`kernel/src/vdso.rs`, `user/vdso/vdso.S`) | `System.nanoTime()` is called for every message in a queue; a syscall costs 95 ns, reading the TSC in user space — 15 |
| **Clustered materialisation of memory** (64 KB per fault) and **2 MB large pages** | the first touch of a page costs ~2-3 µs, and the JVM touches tens of megabytes in a row; a 2 MB leaf takes one TLB entry instead of 512 |
| **The right number of cores in `/proc/cpuinfo`** | the JVM and ClickHouse size their thread pools by it; a single "processor: 0" entry turned the machine into a single-core one |
| **The scheduler gives the core to a woken thread** (a resched IPI to the core spinning a sleeping task) and **work stealing** by an idle core | otherwise a woken thread waits for the other core's tick — up to 500 µs |
| **Loading ELF segments in 2 MB batches** | the JDK image and `lib/modules` used to be read page by page |
| **XSAVE instead of fxsave on a context switch** | the kernel enables AVX for the workload, and `fxsave` knows only the low 128 bits of XMM: a thread preempted in the middle of AVX code got back its NEIGHBOUR's upper YMM halves — silent memory corruption in JIT code |
| **The full FPU state in the signal frame** | the same through the handler: the JVM catches SIGSEGV on the safepoint poll right in the middle of JIT code |
| **The user-rsp substitution of signals became per task** | the value lived in the per-CPU area (gs:16); a task preempted between "stored" and "the stub read it" handed it to the NEXT task of that core — which returned to ring 3 with SOMEONE ELSE's stack |
| **A dead task's stack and address space are not reused while it is still on a core** | a DEAD task stays on its kernel stack until the next switch, and every tick writes an interrupt frame there |
| **`FUTEX_WAKE` skips an already woken waiter** | without this check every next `WAKE(1)` went to the same first waiter, and the real addressee never woke up (ClickHouse's pool grew to the task limit, the JVM benchmark stalled) |
| **A timed wait gives the LVT timer back on preemption** | a timed wait on an AP borrows the LVT for TSC-deadline; a preempted waiter left the core without a periodic tick FOREVER |
| **The signal frame is not built "blindly"** | when the thread's stack has no room (overflow), the kernel used to fault in ring 0 on the guard page; now it checks, tries the alternate stack and declares the signal fatal honestly |
| **Task ceiling 64 → 192** | the thread pools of big runtimes hit it and abort |

## Results

The stand: a desktop, 4 physical cores (8 HT), the same hardware for both sides.
Talaria — QEMU microvm `-smp 4 -m 3072`, KVM. Linux — Alpine 3.20 in docker with
`--cpuset-cpus=0-3` (the same 4 physical cores). One and the same JDK directory,
`Bench.class` and JVM flags. The host ran nothing else during the measurements
(this matters: under a background build both sides drift by 1.5-2×).

| Test | Metric | **Talaria** | Linux |
|---|---|---|---|
| JVM start | ms to the first Java line | 56 | 36 |
| sieve | M cells/s | 228 | 241 |
| matmul | GFLOP/s | 2.3–5.3 (JIT variance) | 2.7 |
| alloc | M objects/s | 75 | 95 |
| alloc | Full GC, ms | 3.5 | 3.2 |
| scale | speedup on 2 threads | 1.99 | 1.98 |
| scale | speedup on 4 threads | 3.98 | 3.89 |
| **pingpong** (LockSupport park/unpark) | p50 RTT, µs | **2.19** | 5.81 |
| **pingpong** | p99 RTT, µs | **3.91** | 12.8 |
| **queue** (ArrayBlockingQueue) | M messages/s | **5.27** | 2.77 |
| queue | p50 latency, µs | **62** | 169 |
| queue | p99 latency, µs | **367** | 871 |
| **sync** | M monitor acquisitions/s | **23.7** | 15.5 |
| sync | M ReentrantLock acquisitions/s | 20.9 | 20.8 |
| cas | M AtomicLong increments/s | 56.9 | 49.8 |
| chm | M ConcurrentHashMap operations/s | 10.8 | 13.8 |
| **timer** | `parkNanos(1ms)` overshoot p50, µs | **19.4** | 62.7 |
| **timer** | `parkNanos(1ms)` overshoot p99, µs | **43.6** | 275.0 |
| **timer** | `System.nanoTime()`, ns | **52** | 65 |

The whole set (`Bench all`) passes on 4 cores in one go — the numbers above come
from ONE run, not stitched together from pieces.

How to read it:

- **Latency is Talaria's strong side, exactly where it is claimed.** Waking a
  thread (`LockSupport.park/unpark` → futex → an mwait sleep in the kernel on the
  user word) is 2.7× faster by the median; a 1 ms sleep wakes with a 19.4 µs
  overshoot against 62.7 — 3.2× more precise, and by p99 (44 vs 275 µs) six
  times. Monitor contention is ours too: 23.7 vs 15.5 M acquisitions/s
  (ReentrantLock — parity).
- **Scaling over cores — parity with a slight edge** (3.98 vs 3.89 on four
  threads).
- **Throughput is behind Linux where it hits memory** (sieve by 6 %, allocation
  by 21 %); the message queue is ours by a wide margin (5.27 vs 2.77 M/s, p99 367
  vs 871 µs) — the sum of the vDSO (time without a syscall) and of a woken thread
  getting the core at once instead of waiting for a tick.
- **`System.nanoTime()` is 52 ns against Linux's 65** thanks to the **vDSO**: two
  pages (code + data) mapped into the process, time computed in user space from
  the TSC without a syscall (`clock_gettime` 15 ns vs 127 as a syscall).
- **matmul is noisy** (2.3-5.3 GFLOPS between runs on one machine): the result
  depends on whether C2 recompiled the hot loop in time. Not usable as an OS
  difference; kept as a liveness indicator of the JIT.

## Boundaries

- **The full set passes twenty runs out of twenty.** It used to fail 1-2 times
  out of 10 for one reason: the return from a signal handler went through
  `sysret`, which PHYSICALLY cannot return a full context — it needs the return
  address in rcx and the flags in r11. HotSpot catches SIGSEGV on every safepoint
  poll and implicit null check, i.e. in the middle of JIT code where every
  register is live; after the return the code continued with two corrupted
  registers. Now the return goes through `iretq`, as on Linux; the gate
  `talaria.test=fputest` checks that rcx/r11 survive.
- **The 4-core speedup is bimodal: 3.6-4.0× or 2.4-2.6×** — about a third of
  the runs land in the lower mode (two threads start on one core and are not
  always spread in time). Placement at thread creation and work stealing
  narrowed the tail but did not remove it.
- SerialGC and the C1/C2 JITs are checked, CDS is on. Other collectors (G1,
  Parallel, ZGC) were not checked; ZGC certainly will not work — it needs
  coloured pointers and multi-mapping.
- 2 MB large pages exist in the kernel but are not used by default (a 32 MB
  per-mapping threshold, `talaria.huge=<MB>`, 0 turns them off). Measured with
  them: sieve 220 vs 216 M cells/s, matmul 2.58 vs 2.62 GFLOPS — the run-to-run
  spread is larger than the effect.
- JVM networking (sockets, HTTP) was not exercised by the benchmark.

## History: how the "lost wakeup" was closed

In the first version the full set on 4 cores never finished — it hung or the GC
crashed. That turned out to be not one disease but ten, each a real kernel bug
found by the JVM (the `[futex]` hang diagnostics and the task snapshot in
`sync.rs` were added for this and stayed in the kernel):

1. a timed wait on an AP borrowed the LVT timer and did not return it on
   preemption — the core was left without a tick forever;
2. the user-rsp substitution for signal delivery lived in a per-CPU area — the
   neighbouring task took it;
3. the kernel stack and address space of a dead task were reused while it was
   physically still on a core;
4. `FUTEX_WAKE` "woke" the same already-woken waiter again and again;
5. the context switch lost the upper YMM halves (fxsave with AVX on);
6. signal delivery lost the same — and the JVM catches SIGSEGV on every
   safepoint poll, in the middle of JIT code with live AVX registers;
7. **the TLB shootdown was asynchronous** — `mprotect`/`munmap` sent the IPI and
   returned at once, while the neighbour kept using the removed mapping through
   a stale TLB entry. Now the initiator waits for acknowledgements through a
   mailbox (request/done counters per core) rather than an interrupt: syscalls
   run with IF=0, and two cores unmapping at the same time would deadlock;
8. **a new thread was published half-built** — a slot is freed by clearing the
   "dead" flag, and the previous tenant's kernel stack had already gone back to
   the allocator; a tick on a neighbouring core in that window switched to the
   "ready" task and ran off a garbage frame. Now a task becomes visible to the
   scheduler only fully built;
9. **the file system was not protected between cores at all** — the block
   cache, bitmaps, the open-file table and the virtio queue had no lock, while
   the runtime's threads read files from four cores at once;
10. **the return from a signal handler lost rcx and r11** (`sysretq`, see
    Boundaries) — the cause of the last remaining failures.

Plus `membarrier` (syscall 324), which had silently returned "done" without doing
anything. Two gates close the suspected classes: `talaria.test=memrace`
(parallel memory operations, every byte checked) and `talaria.test=fputest`
(YMM survives a syscall, a signal handler and preemption).
