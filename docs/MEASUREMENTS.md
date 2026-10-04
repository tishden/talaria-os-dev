# Talaria OS — the measured record

Every load-bearing number this project claims, with the conditions it was taken
under and what it does and does not prove. It replaces the per-session
measurement journals: the *analysis* of each figure lives here, the *history* of
when it appeared lives in `CHANGELOG.md`, and the *method* for taking a new one
lives in `bench/suite/README.md` and `docs/PERFORMANCE-OPTIMIZATION-GUIDE.md`.

Nothing here is an estimate. Where a claim is bounded ("this stand does not
resolve less than X"), the bound was measured too.

---

## 1. How these numbers are judged

The comparative suite (`bench/suite`) drives both systems with **one
instrument** — `tlgen`, an open loop that counts latency from the SCHEDULE, not
from the send. On both sides the workload is the same binary, the stand is the
same, and each server has its own generator node.

* **A ladder of rates**, not a single point: the ceiling is found first in a
  closed loop, and the steps are fractions of the ceiling that BOTH sides share.
* **Three repeats per point**; the median goes into the table and the spread is
  printed beside it.
* **The judging threshold is 5 %**: a point counts for Talaria at a ratio below
  0.95, against at above 1.05, parity in between. The ratio is Talaria over
  Linux at the same rate, so **below one means Talaria is faster**.
* **Four rejection rules** (all four have taken more points from Talaria than
  from Linux): overload or errors on either side; the generator's own schedule
  debt exceeding the service latency; a difference smaller than the stand's
  measured noise floor; and a percentile backed by fewer than ten requests.
* Beside every ratio the tables print **">1/>4 ms"** — how many requests across
  all repeats took longer than a millisecond and longer than four, on each side.
  That separates a tail of hundreds of events (a property of the system) from a
  tail of one outlier, and immediately shows whether Linux has the same thing.

**What may be compared with what** is the rule that cost the most: only points
from ONE stand bring-up. See `docs/ENGINEERING-NOTES.md` §1.

---

## 2. Against Linux on real workloads — scoreboard 41

The current score. Rise `ec2all-20260919-152818`, kernel `2940c86b0b25a61d`,
write-path defaults unchanged (`talaria.dirtyage=0`). Six servers —
PostgreSQL 16, MySQL/MariaDB 10.11, MongoDB 4.0, Redis, nginx, mongoose — on
four `c6i.xlarge` in one availability zone, two cores per workload on both sides.

| 295 comparable points | faster | parity | slower |
|---|---:|---:|---:|
| p50 | 66 | 7 | 1 |
| p99 | 69 | 5 | 0 |
| p99.9 | 65 | 7 | 2 |
| p99.99 | 63 | 2 | 8 |

Per workload (faster / parity / slower):

| workload | p50 | p99 | p99.9 | p99.99 | points |
|---|---|---|---|---|---:|
| mongodb | 12 / 3 / 0 | 15 / 0 / 0 | 11 / 2 / 2 | 13 / 0 / 1 | 59 |
| mongoose | 10 / 0 / 0 | 10 / 0 / 0 | 10 / 0 / 0 | 7 / 1 / 2 | 40 |
| mysql | 6 / 3 / 1 | 7 / 3 / 0 | 6 / 4 / 0 | 4 / 1 / 5 | 40 |
| nginx | 9 / 0 / 0 | 9 / 0 / 0 | 9 / 0 / 0 | 9 / 0 / 0 | 36 |
| postgresql | 9 / 1 / 0 | 8 / 2 / 0 | 9 / 1 / 0 | 10 / 0 / 0 | 40 |
| redis | 20 / 0 / 0 | 20 / 0 / 0 | 20 / 0 / 0 | 20 / 0 / 0 | 80 |
| **total** | **66 / 7 / 1** | **69 / 5 / 0** | **65 / 7 / 2** | **63 / 2 / 8** | **295** |

One run was not compared (overload or errors on one side) and one percentile was
backed by too few requests.

### 2.1. Where Talaria loses

| workload | case | rate | pct | Talaria, µs | Linux, µs | ratio | >1/>4 ms us | >1/>4 ms Linux |
|---|---|---:|---|---:|---:|---:|---:|---:|
| mysql | pk | 3214 | p99.99 | 2725.9 | 124.2 | 21.95 | 190 / 75~ | 0 / 0 |
| mysql | pk | 8036 | p99.99 | 414.7 | 98.4 | 4.21 | 872~ / 320~ | 0 / 0 |
| mysql | pk | 16072 | p99.99 | 246.5 | 118.2 | 2.09 | 1771~ / 753~ | 3 / 0 |
| mysql | select1 | 11711 | p99.99 | 120.4 | 72.6 | 1.66 | 216~ / 111~ | 0 / 0 |
| mongodb | insert | 2168 | p99.9 | 240.8 | 163.3 | 1.47 | 241 / 148 | 308 / 218~ |
| mongodb | insert | 2168 | p99.99 | 4870.1 | 3346.4 | 1.46 | 241 / 148 | 308 / 218~ |
| mysql | select1 | 4684 | p99.99 | 107.2 | 79.3 | 1.35 | 251~ / 99~ | 1 / 0 |
| mongoose | dyn | 29589 | p99.99 | 142.5 | 112.4 | 1.27 | 0 / 0 | 0 / 0 |
| mongoose | dyn | 11835 | p99.99 | 112.8 | 90.1 | 1.25 | 0 / 0 | 1 / 0 |
| mysql | select1 | 42159 | p50 | 43.4 | 41.1 | 1.06 | 5016~ / 1323~ | 68125 / 29399 |
| mongodb | insert | 16262 | p99.9 | 415.7 | 395.0 | 1.05 | 48615~ / 37022~ | 142720~ / 120111~ |

### 2.2. Where Talaria wins most

| workload | case | rate | pct | Talaria, µs | Linux, µs | ratio |
|---|---|---:|---|---:|---:|---:|
| postgresql | select1 | 36081 | p99.99 | 88.6 | 19873.8 | 0.004 |
| mysql | pk | 28930 | p99.99 | 110.0 | 18792.4 | 0.006 |
| postgresql | select1 | 36081 | p99.9 | 72.8 | 11206.7 | 0.006 |
| mysql | select1 | 42159 | p99.9 | 60.7 | 9003.0 | 0.007 |
| mysql | pk | 28930 | p99.9 | 82.2 | 10076.2 | 0.008 |
| postgresql | pk | 21553 | p99.99 | 174.7 | 20578.3 | 0.008 |
| postgresql | pk | 21553 | p99 | 93.1 | 3416.1 | 0.027 |

The shape is consistent: at the top of the ladder Linux collapses into
millisecond tails (tens of thousands of requests over 1 ms) while Talaria stays
in the tens of microseconds. That, not the median, is what the model buys.

### 2.3. The eleven lost points, each closed by name

| class | points | verdict |
|---|---:|---|
| "foreign 8 ms" (mysql pk / select1) | 5 | **NOT our code.** The tick hole of 8277 µs arrives on both cores at once and consists 97 % of vCPU theft by the hypervisor (8092 µs of 8277; 44 holes ≥ 1 ms where the host took more than half, 246 ms taken in total). The class MIGRATED from mongodb to mysql between two boards without a line of code changing — a defect of ours does not do that. The cure is placement (a dedicated host, `isolcpus`, vCPU pinning), not code. |
| the 140-400 µs band (`mongodb insert` p99.9) | 2 | **Closed by measurement.** The payer is named (§4) and BOTH candidate cures were written and rejected. |
| class C (`mongoose dyn` p99.99) | 2 | **Not a class.** 22-30 µs of difference with ZERO events over a millisecond on either side, and our worst answer better than Linux's (305-389 µs against 152-1061). Re-measured on two consecutive rises with no losses at all: mongoose 40/40, nginx 20/20. |
| the judging boundary | 2 | 1.05-1.06 against a 5 % threshold. `mysql select1` r42159 p50 43.4 against 41.1 µs at the TOP step, where Linux has 68125 events over a millisecond against our 5016. |

**There are no open fix candidates left from this board.**

### 2.4. The boards before it, and what the stand resolves

| board | p50 | p99 | p99.9 | p99.99 | points |
|---|---|---|---|---|---:|
| 35 | 60 / 11 / 1 | 67 / 3 / 2 | 62 / 5 / 5 | 53 / 3 / 16 | 288 |
| 37 | 67 / 3 / 3 | 69 / 4 / 0 | 66 / 2 / 5 | 60 / 5 / 7 | 291 |
| **41 (current)** | **66 / 7 / 1** | **69 / 5 / 0** | **65 / 7 / 2** | **63 / 2 / 8** | **295** |

Boards 35 → 37 are a real improvement with one large caveat: the biggest tail
class on board 35 (ten of its twenty-three lost points) turned out to live in our
own GENERATOR node, not in the server (`docs/ENGINEERING-NOTES.md` §2). Board 37
is the first one taken with the corrected instrument.

**How much this stand resolves at all** was measured rather than assumed: session
36 re-took mongodb with the SAME kernel binary as board 37.

| | p50 | p99 | p99.9 | p99.99 |
|---|---|---|---|---|
| first rise | 11 / 2 / 0 | 10 / 2 / 1 | 8 / 0 / 5 | 12 / 0 / 0 |
| second rise | 11 / 1 / 3 | 14 / 1 / 0 | 10 / 0 / 5 | 11 / 1 / 2 |

Three median losses and two in p99.99 appeared with no change in the code. Hence
the judging rule used ever since: a class exists only if it reproduced on TWO
rises.

---

## 3. What `fsync` is actually doing (sessions 37-42)

The `mongodb insert` tail was the last large gap, and it was taken apart with an
instrument that splits the call into phases and prints the remainder per task
(gate `fsyncph`). Point: `insert` at the bottom ladder step, where the ratio to
Linux is worst.

**Where a call goes** (597 calls, 3109 µs per call, difference between the first
and last snapshot of the run):

| phase | total, µs | share | times | per call, µs | worst, µs |
|---|---:|---:|---:|---:|---:|
| commit | 536476 | 28.9 % | 594 | 903 | 1521 |
| descriptor + journal copies | 518918 | 28.0 % | 594 | 874 | 7927 |
| data in place | 501529 | 27.0 % | 597 | 840 | 420630 |
| waiting for a foreign transaction | 193532 | 10.4 % | **1** | 193532 | 351456 |
| not covered by a phase | 101977 | 5.5 % | 597 | 171 | 305 |
| barrier before commit | **0** | 0.0 % | 594 | 0 | 0 |
| FS lock on entry | **0** | 0.0 % | 597 | 0 | 189 |
| **sum of phases** | | **99.9 %** | | | |

Three findings, each of which killed a planned edit:

1. **There is nothing to coalesce.** The FS lock was taken 597 times out of 597
   in 0 µs (if two `fsync`s overlapped, the second would stand there for
   milliseconds); the journal was busy with a neighbour once in 597; and there
   were 1235 transactions per 1247 `fdatasync` calls. Group commit and async
   commit both have a ceiling of exactly 0 µs.
2. **The barrier costs nothing.** 594 barriers at 0 µs, because the device has no
   volatile write cache (`VWC=0`): durable writes go as ORDINARY writes and FUA is
   redundant. The cost of a transaction is not barriers and not the number of
   transactions — it is **the number of sequential round trips to the device**.
3. **The critical path is the DATA, not the copies** (1289 µs against 926) — so
   "take X off the chain" must first show that X *is* the chain.

**What was then actually won** (each by a knob A/B within one rise):

| edit | effect |
|---|---|
| device lock owned by the TASK, not the core; `nvme_batch` waits through `io_wait_step` | `insert` p99.9 **4325 → 1212 µs** |
| §8.12 — the descriptor batch flies with the FS lock released | p99.9 **1212 → 439 µs** (Linux 326; was 11×) |
| §8.13 — `fdatasync` in two device round trips instead of three | **6224 → 5018 µs** per call |
| `talaria.fscnt=1` — a dirty counter instead of three full cache scans | 320 µs of pure CPU per call removed; call **3010 → 2753 µs** |
| `talaria.jrnfree=1` — the journal is not held during a foreign data flush | waiting for a foreign transaction gone ENTIRELY (185 932 µs in one episode → zero episodes); call **2801 → 2602 µs** |

And the one that was written, measured and left OFF: age-based pre-emptive flush
(`talaria.dirtyage=`) does exactly what it promises — dirty blocks per `fsync`
47 → 9.4, dirty age 2.13 s → 29.7 ms — and moves the price into `write`:
6.7 µs → 507.6 µs per call, a factor of 76, which over a 240 s window is
+5490 ms against −330 ms won. **Default 0.**

Durability of the write path is verified by pulling the plug, not by reasoning:
`bench/suite/powercut.sh mongodb` (pymongo `j:true`, the number of acknowledged
inserts written to the generator's disk after every ack, then `stop --force` and
a re-boot). Result: the generator died mid-insert after 12295 acknowledged,
the mount replayed 1498 journal transactions, and 12295/12295 were in place plus
one unacknowledged — exactly `j:true` semantics.

---

## 4. Who takes the core away from a thread mid-request (sessions 42-43)

The remaining `mongodb insert` p99.9 loss is **not a tail but a BAND**. The share
of requests above a threshold, as a ratio of the two sides' medians:

| threshold, µs | 110 | 120 | 130 | **140** | 150 | **160** | 180 | 200 | 250 | 300 | 400 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ratio | 0.92 | 1.28 | 1.38 | **1.61** | 1.75 | **1.82** | 1.78 | 1.75 | 1.60 | 1.39 | 1.04 |

Up to 110 µs we are no worse, the ratio grows to 1.8 at 160-200 µs and returns to
parity beyond 400. p99.9 merely happens to stand inside that band.

**The payer**, from `talaria.appprof=1`, which splits the per-thread `read→write`
stage into "ran on the core" and "taken off it": at p99.9 the stage is 316-362 µs,
of which **226-316 µs the thread is NOT on the core**, and what takes it off is
the timer tick. The request's own work at p99.9 is 113 µs, while the slice on a
core shared with a network waiter is 100 µs — so one request in a thousand does
not fit in the quantum and pays a full scheduler rotation. Hence the shape: a
plateau of 150-350 µs, exactly the length of a rotation.

**Who gets the core** (gate `reqwho`, under load, import windows discarded):

| the core went to | share of rotations |
|---|---|
| **a network waiter** | **94.2-100 %** |
| another thread inside a request | 0-5.8 % |
| ring 3 not in a request | 0 |
| a kernel thread | 0 |
| nobody (idle) | 0 |

Time until the displaced thread returns: p50 8.5 µs, p90 90.4, p99 361.7,
p99.9 542.6, worst 7742.8 — which is precisely the band seen in the request
latency.

**Both cures were written and both were rejected by measurement:**

* Longer quantum (`talaria.tickmin=200000`): a third FEWER stops (42 225 → 29 184
  episodes) and the band WIDENED (0.49-0.97 % against 0.22-0.75).
* Priority against the network waiter (`talaria.reqfirst=<ns>`): four times fewer
  stops mid-request (17 555 → 3 916 rotations, the share going to the waiter
  94.5 % → 57.8 %) and the band did not narrow — p50 1.00, p99 0.99, p99.9 0.91,
  p99.99 1.05 against the baseline, inside the baseline's spread with itself.

**The explanation both agree on:** the band is fed by the LENGTH of a stop, not
by their number. Taking the core from a waiter (which yields every 5 µs) gives it
to a thread that does WORK and holds it longer. Both knobs default to 0.

---

## 5. Microbenchmarks: what the model costs

From inside the guest via `rdtsc`, without host or network noise. TSC 3.601 GHz,
KVM/QEMU microvm, `-cpu host -smp 2`.

| metric | cycles | ns | how clean |
|---|---:|---:|---|
| **syscall cost** (ring3→ring0→ring3, nop) | 101 | **28** | clean — SYSCALL/SYSRET do not VMEXIT, so it holds on real hardware too |
| reflect-compute (parse + swap MAC/IP/ports of a UDP frame) | ~216 | **60** | clean |
| AES-128-GCM seal in place (96 B) | 401 | **111** | clean (AES-NI + PCLMULQDQ) |
| empty loop reference (subtracted) | 7 | 2 | — |

On Linux a nop syscall costs ~50-150 ns with mitigations (KPTI, retpoline,
seccomp, audit). Talaria has one trust domain and a minimal dispatcher, so the
path is shorter — but the zero-syscall datapath removes even those 28 ns: a
socket UDP echo pays `recvfrom` + `sendto` = ~56 ns of pure overhead per packet
plus a full stack traversal, and the fast path pays nothing.

**End-to-end RTT** (host ping-pong, 30-50 k packets). All three paths share the
same QEMU virtio floor of ~9.5 µs (a VMEXIT on the virtio kick plus
tap/vhost/host scheduler); the delta above the floor is the price of each layer.

| guest path | min | p50 | p99 | delta over floor |
|---|---:|---:|---:|---|
| in-kernel inline reflect (reactor, 1 core) | 9.5 µs | 14.9 µs | 24.8 µs | — (the floor) |
| zero-syscall rings (BSP ring 3 + AP cell) | 14.4 µs | 18.7 µs | 33.0 µs | +3.7 µs (cross-core hop) |
| full UDP stack (smoltcp in the kernel) | 14.8 µs | 20.0 µs | 28.4 µs | +5.1 µs (the stack) |

**The virtio kick is the whole difference.** `rx→tx` inside the reactor is
~3-4 µs while reflect-compute is 60 ns; the gap is the VMEXIT on the notify
register. On real hardware through VFIO the same `rx→tx` is:

| packets | min | avg | max | < 100 ns |
|---:|---:|---:|---:|---:|
| 23 652 | **14 ns** | 56 ns | 4102 ns | 92 % |
| 107 319 | 14 ns | **52 ns** | 4001 ns | 94 % |
| 52 877 | 14 ns | 57 ns | 3357 ns | 91 % |

Intel 82599, 10G, VFIO, two ports over an AOC cable, TSC 3.606 GHz. A packet is
reflected in 14 ns (min) / ~52 ns (avg) with 92-94 % under 100 ns and **not a
single VMEXIT** — so the microseconds under QEMU were entirely the virtio kick,
exactly as predicted. End-to-end with the Linux side as generator: min 16.1 µs,
p50 18.3, p99 26.8, p999 41.1 over 200 000 ping-pongs — and those ~18 µs are the
LINUX stack on the peer port, which is the very overhead Talaria removes.

---

## 6. Synchronisation without a busy spin

Stand: i7-3820 (no WAITPKG/MONITORX), so the working tier is `mwait` in the
kernel (KVM `-overcommit cpu-pm=on`); the ring-3 tiers (`umwait`/`mwaitx`) are
written and enable themselves on newer hardware.

| metric | busy-spin / Linux baseline | Talaria (asleep, 0 % CPU while waiting) |
|---|---|---|
| cross-core wakeup by a store (ring 0) | IPI path 15.8 µs | **mwait p50 237 ns / p99 338 ns** |
| futex ping-pong RTT cross-core, unmodified musl | Linux futex: units to tens of µs | **p50 165 ns** |
| tsync wait/wake RTT cross-core | — | **p50 223 ns** |
| SPSC push→pop (empty queue → full sleep cycle) | spin ring 52-64 ns | **533 ns p50**, p99 582-852 ns |
| SPSC throughput (burst, 2 cores) | spin 315-375 Mops | **140-168 Mops** (busy mode of the same queue 203-226; v1 was 9) |
| uncontended mutex lock+unlock | pthread/musl 58-68 cycles | **tsync 45-58 cycles** |
| green-thread switch (one core) | coroutine ≈ 10-25 ns | **11 ns** |
| condvar handoff RTT (one core, fibers) | — | **30 ns** |
| SPSC mutex+cv on one core | — | **41 ns per element** |
| futex same-core RTT (directed switch) | 2 ticks (16-96 ms) | **1.3 µs** |
| **cross-VM** wakeup through ivshmem | virtio kick ~10 µs VMEXIT | **RTT p50 496 ns**, both VMs asleep in mwait |

Actor-to-actor RTT cross-core with SObjectizer 5.8.6 on the tsync dispatcher:
**p50 0.67 µs**; on so_5's own stock primitives Talaria is **6.4× faster than the
Linux guest** (2.1 µs against 13.5), flood 4.71 M elements/s.

What produced the SPSC jump from 9 to 140-168 Mops: a predicate `upark` syscall
(the kernel re-arms the monitor itself), state feedback from the kernel (1 = it
sleeps in mwait on the word, so a store suffices and no wake syscall is needed;
2 = parked, a futex wake is needed), doorbell lines on both sides, a field layout
by line ownership, the fast path as a `static inline` in `tsync.h` (crossing a
call boundary in a hot loop cost ~4× of throughput), and an adaptive
poll-then-sleep pre-spin.

---

## 7. Large third-party runtimes

**OpenJDK 21** (Temurin, musl, unmodified) against Linux on the SAME hardware,
the same JDK and the same class:

| metric | Talaria | Linux |
|---|---|---|
| thread wakeup (LockSupport) p50 | **2.19 µs** | 5.81 µs |
| `parkNanos(1 ms)` overshoot p50 / p99 | **19.4 / 44 µs** | 62.7 / 275 µs |
| monitor contention | 23.7 M acquisitions/s | 15.5 M |
| message queue | **5.27 M msg/s** | 2.77 M |
| `System.nanoTime()` (after vDSO) | **52 ns** | 65 ns |
| scaling across 4 cores | parity | parity |
| throughput where it hits memory (sieve, allocation) | 10 % / 26 % behind | — |

The 4-core speedup is **bimodal between runs**: 3.6-4.0x, with about a third of
the runs on a lower mode of 2.4-2.6x, on the same kernel and the same binary — it
is the thread layout that is drawn, not the parallelism that is missing. The gate
in `kernel/validate.sh` therefore asserts a floor of 2.2x: it is there to catch the
ABSENCE of scaling, not to win a lottery.

**ClickHouse 25.8.33.6** (the official binary, `clickhouse local`) executes
queries — configuration, planner, vectorised engine, aggregation and GROUP BY
over millions of rows. On four cores it is mostly parity (7 of 12 cases; losses
1.07-1.17 on ddl, agg300m, groupby7); on two cores it is noticeably worse. **The
right number of cores depends on the workload**: compute-bound analytics wants
more, databases want fewer (extra cores breed spinning waiters that steal a core
from the computing thread). With `talaria.huge=2`: `uniqExact` 253 → 197 ms,
GROUP BY over 100 k 235 → 197 (Linux 188 and 236).

Full harnesses and tables: `services/jvm/README.md`,
`services/clickhouse/README.md`, `services/actors/README.md`.

---

## 8. The ENA datapath and bare metal

The datapath gap against Linux was closed over four sessions; the number that
mattered each time was UDP RTT on the same VPC path, Linux→Linux as the anchor.

| state | RTT | note |
|---|---|---|
| first working ENA driver | 126.6 µs | Linux→Linux on the same path: 41.9 |
| LLQ (descriptors and header in device memory, BAR2) | 84.7 µs | plus kick batching, L3/L4 metadata in the descriptor |
| **accelerated LLQ mode (`accel_mode`)** | **33.6 µs** | **Linux on the same path: 43.0** — i.e. faster than the reference |
| write-combining on the LLQ window | unchanged | transmit **1 266 664 against 188 235 pkt/s** |

The whole three-session gap was one unset field: until the driver declares
`DISABLE_META_CACHING | LIMIT_TX_BURST` in `SET_FEATURE(LLQ)`, the device serves
the queue on the NON-accelerated path. It was found by byte-decoding a captured
admin-command trace of the live Linux driver. The mode is a CONTRACT, not a
performance flag — see `docs/DEVELOPMENT-GUIDE.md` §12.21.

**Receive** tops out at the instance's shaper, not in our code: 1 000 000 pkt/s
with a spread of ±3 on `c6i.large` (the first measurement of 557 k was the
GENERATOR's ceiling).

**Bare metal** (`c6i.metal`, 128 cores, 256 GB, no hypervisor, our own BIOS chain
in `tools/tboot/`): the full bring-up works — ACPI (RSDP found by scanning, MADT
with 64 CPUs, MCFG), PCIe through ECAM (52 bridges, 307 functions), NVMe on bus
`c2`, ext2 on a live EBS volume. Measured there:

* `talaria.test=smp2` speedup **2.00**, `smp4` **4.00** (under a hypervisor the
  same tests give 1.86 and 3.59 — the difference is vCPU preemption);
* real `MONITOR/MWAIT` in ring 0: cross-core wakeup **p50 901 ns** (on Nitro
  guests the same detection falls back to `hlt`+IPI);
* PMU through `rdpmc`: IPC 1.93, a real counter;
* `fsync` of 512 blocks on a real NVMe: **7.4 ms** (a repeat 4 µs) against
  108-300 ms under QEMU;
* ENA comes up fully: the device offers 32 queue pairs and 132 MSI-X vectors (the
  driver raises ONE pair — see `docs/ROADMAP.md`), a 4 MB LLQ window with
  write-combining, a BAR doorbell of 35 ns (a Nitro guest: 2 queues, 6 vectors,
  256 KB, 79 ns);
* the production mode runs there whole: the application in ring 3 in its own
  address space, **15 cores** under `talaria.smp=16`, networking.

**Clock discipline** (v1.10): the TSC calibration was off by **95 ppm — 8.2
seconds a day**; after the servo (kvmclock under KVM, the ENA PHC on hardware)
the residual error against the host is **−4.8 µs** with zero backward steps.

---

## 9. The filesystem, measured on a live volume

Numbers that only appear on real storage, and the defects they exposed:

* `fsync` of 512 blocks on a live NVMe: **7 ms**, against 108-300 ms under QEMU.
* Coalescing adjacent blocks into ranges: 512 blocks became **2 requests**.
* Run-based block allocation: bitmap trips **33 instead of 512+** per 512 blocks,
  and files land contiguously.
* A freshly allocated block was being READ from the disk before being zeroed —
  invisible under QEMU, 512 round trips on EBS: the write phase of `fsbench`
  **290 200 → 817 µs**.
* A write batch longer than the controller's MDTS was silently rejected — the
  first `fsync` of 512 contiguous blocks went out as ONE 2 MB command and the
  data was not written. The device limit was enforced on one path and not on
  another.

---

## 10. The arm64 KVM stand: the first facts and numbers (2026-09-30)

Stand: `c7g.metal` (Graviton3, Neoverse V1, 64 cores, no SMT), ap-northeast-1d,
AL2023 arm64, QEMU 8.2.2 built natively in the stand image, `-M virt,gic-version=3,its=on
-enable-kvm -cpu host -smp 4 -m 1024`. The kernel is the `arm64-port` branch at
`0bda853` plus the `talaria.poweroff=1` end (`kernel/build-aarch64.sh` on the stand,
`AARCH64_OBJCOPY=objcopy VDSO_CC_AARCH64=gcc`). One bring-up, `infra/aws/kvm-stand-aarch64.sh`;
the pitfalls are in `docs/ENGINEERING-NOTES.md` §5.1.

**The facts the plan asked for (§9.2 A)**, from the banner under KVM:

| fact | value |
|---|---|
| MIDR | implementer 0x41, part 0xd40 r1p1 — Neoverse V1 |
| `CNTFRQ_EL0` | **1 050 000 000 Hz** (952 ps per tick; QEMU's TCG gives 62.5 MHz) |
| hardware dirty bit | FEAT_HAFDBS DBM works: the probe reads true / false / true |
| PSCI | 1.1 through `hvc` |
| GICv3 | 256 SPIs, the ITS at 0x8080000, the redistributor of every core found |
| the tick | PPI 27 at 1000 µs: **49 ticks in 50 ms** (the gate's KVM window is 49-51; TCG gave 48-49) |
| the cores | 4 of 4 up through PSCI `CPU_ON`, each took its SGI and 21 ticks of its own |
| EL0 | three `svc` round trips and a controlled `brk`; the vDSO `__kernel_clock_gettime` read from EL0 lands inside the kernel's bracket |
| the gate | `TAL_KVM=1 kernel/test-aarch64.sh`: **36/36**, 5.1 s for five boots |

**The four probes TCG could not judge (§9.2 B-E):**

| probe | what it tells | result |
|---|---|---|
| `ring` — the `fastring` SPSC protocol with the §4.13 fences, core 1 produces, core 0 consumes, 32-word slots with a checksum | are the fences enough on a weak machine? | 200 M slots at `-smp 2`, 4 and 8: **0 corrupted, 0 with a wrong length** in every run; 9.6 / 9.8 / 8.9 M slots/s (20.8 / 20.5 / 22.4 s). A first run of 20 M at `-smp 2`: 0 / 0, 9.9 M slots/s |
| `bbm` — 48 regions of 2 MiB, 2000 rights flips each on core 0 while core 1 reads through a guarded load | break-before-make: a stale translation or a TLB conflict abort? | 49 227 562 reads, 776 201 faults (the PROT_NONE phases), **0 wrong values, 0 conflict aborts**, 167 ms |
| `icache` — a frame executed at EL0, rewritten WITH `icache::sync`, then rewritten WITHOUT | how does a stale instruction line look? | A 0xa, B (with sync) **0xb**; C (without sync) answered 0xc — coherent, in this run and the ten repeats below |
| `svccost` — 50 000 `svc` round trips from EL0, timed by the counter | ns per syscall entry, with and without the 528-byte FP/SIMD save | **55 ns with the save, 41 ns without — the save costs 14 ns**; four runs, the same three numbers each time |
| the gate in a loop — 50 runs of `test-aarch64.sh`, 250 boots of 4 cores | the SMP bring-up paths (PSCI `CPU_ON`, the redistributor wake, SGIs) under a real weak machine | **0 of 50 runs failed**, 36/36 each, about 5 s a run |

The `icache` probe's unsynchronised rewrite (C) answered the NEW code in all ten
repeats under KVM as well: on one core, one fresh page, the stale line the plan's
§4.12 warned about did not appear. The sync stays (the architecture does not
promise what this core did); a gate that can show the failure needs a harder shape.

The x86 references for the last line: 28 ns for a nop syscall on the i7-3820
microvm stand (§5), 95 ns as `mbench` measures the full path on the EC2 stands.
The plan's §4.5 asked whether the eager FP/SIMD save is visible: it is a quarter of
the entry, 14 ns — enough to keep the lazy-save candidate on the list for phase 9,
not enough to do it before the workloads run.

### 10.1. The set under KVM: the syscall, the wakeup, the speedups, the soak (2026-09-30, night)

The same stand, brought up again the same night (`c7g.metal`, ap-northeast-1d, QEMU 8.2.2,
`-enable-kvm -cpu host -smp 4 -m 1024`), the kernel at `c1ea6e6` on `arm64-port` — the
whole ring-3 set of `docs/ARM64-PLAN.md` §10.4 with the four KVM-only fixes that the first
boot of the night forced (a poisoned `TPIDR_EL1`, the SMCCC clobbers, the instruction cache
after loaded code, the TLB on both sides of a root switch — ENGINEERING-NOTES §5.1).
`TAL_KVM=1 kernel/test-aarch64.sh`: **120/0** (the speedups judged too).

**The primitives (`mbench`, the binary that goes to Linux; three boots, the spread in
parentheses):**

| primitive | Talaria arm64, Graviton3 under KVM | the x86 reference (§5, EC2 stands) |
|---|---|---|
| ALU | 0.771 ns/step (1.30 G steps/s), the same to the last digit each boot | — |
| L1 32k / L2 256k / L3 8M / RAM 64M | 17.4–17.8 / 16.7 / 15.8–15.9 / 16.6 GB/s | — |
| pointer chase, 64 MB | 96–107 ns/step | — |
| `syscall` (the full path: the `svc`, the dispatcher, the fd table, the gate) | **151–152 ns** | 95 ns |
| `syscall-ptr16` (one 16-byte pointer argument through `uaccess`) | 196 ns | — |
| `read64` (64 bytes from /dev/zero) | 172–173 ns | — |

**The bare entry (`svccost`, an `svc` from EL0 through the dispatcher to ENOSYS and back,
50 000 round trips; the first pass is a warm-up and is not judged — it carried the
first-call print and was 313 ns):** **130 ns with the 528-byte FP/SIMD save, 112 ns
without — the save costs 18 ns** (three boots: 129/111, 130/112, 130/112). The morning's
55/41 (§10) was the bring-up's stub answering `x0 = x8 + 1` in the vector; the 75 ns
between are the dispatcher — `abi::to_internal`, the two counter reads, the per-task
records, the pointer gate. The save's price did not move (14 → 18 ns).

**The wakeup (`futexlat`, musl's own futex and condvar ping-pong, N = 2000; three boots):**

| pair | p50 | p99 | min |
|---|---|---|---|
| futex, the two threads on one core | 518–565 ns | 528 ns – 3.7 µs | 483–553 ns |
| futex, across two cores | **509–559 ns** | 519–1262 ns | 502–553 ns |
| condvar, one core | 2.4–4.1 µs | 6.4–6.9 µs | 1.1–1.9 µs |
| condvar, across cores | 2.4–3.8 µs | 6.4–6.9 µs | 1.3–2.1 µs |

The x86 reference for the cross-core futex RTT is §4's 4-8 µs on the microvm stands and
the `wakelat` numbers of §7 on the EC2 metal; the arm64 number is with the wait tier at
its default (`wfe` on the line, §4.9 of the plan), unpinned, no knobs.

**The parallel speedups (`smp2`, `smp4`, the same binaries as x86's gate):** **2.00** on two
cores and **4.00** on four, three boots each (x86 under KVM on the EC2 stands: 1.9-2.0 and
3.5-4.0).

**The soak (`kernel/soak-aarch64.sh 50`: 50 boots of each mode, 4 cores):** `ring`
(2 M slots a boot), `shmrace`, `memrace`, `fputest`, `futexlat`, `pthreads`, `sigtest`,
`forkap`, `schedapi` — **0 failed boots of 450** in the clean runs (a first attempt had
two soaks running at once on one instance over the same disk image and reported
consoles that never booted; those are not counted and the lesson is in §5.1 of the
notes). The whole gate in a loop, 120 checks a run: **0 of 10 runs failed**. The logs:
`infra/aws/logs/aarch64-20260930-213404/` (the gate, the twelve number consoles, the
probe, the soak's first consoles and the verdicts).

### 10.2. The stand on the 229-check gate, and the first EC2 facts (2026-10-03/04)

The same `c7g.metal` brought up once more (`infra/aws/kvm-stand-aarch64.sh`, ~52 min, the
instance self-destructed), the kernel at `f3f06bc` on `arm64-port`. The gate under KVM:
**229/1** — the one FAIL was the gate's own host check (`htree (host)`: e2fsck refused a
tree whose `dx_root` carried the effective hash version instead of the raw one, a defect
that only an arm64 host's unsigned-hash `mkfs` exposes; ENGINEERING-NOTES §6.1 item 26,
fixed the same session). Everything the stand alone judges was clean:

| what | number |
| --- | --- |
| `mbench` | alu 0.77 ns/step (1.30 G steps/s), syscall 153 ns, syscall-ptr16 197 ns, read64 177 ns, chase-64m 94 ns, L1/L2/L3/RAM 15–18 GB/s |
| `futexlat` | futex same/cross-core p50 ~512 ns; cond same/cross-core p50 ~1900 ns |
| `smp2` / `smp4` | speedup 2.00 / 4.00 |
| the probes TCG cannot judge | `ring` 2 M slots in 216 ms, 0 corrupted, 9.25 M slots/s; `icache`, `bbm`, `svccost` all "as expected" |
| the soak (`soak-aarch64.sh 50`, 9 modes) | **0 of 50** each |
| the parallel race detector (`PAR=24 … 96`, forkap/shmrace/memrace, 24-at-once on 64 cores) | **0 of 96** each — the three shared-code races of §10.5 hold under a real weak machine |
| the network, polling | udp RTT p50 11.2 us, p99 11.8; tcp 75 142 req/s (13.3 us/req) |
| the network, `netirq=1` | udp RTT p50 22.7 us (the line + parking); **tcp `diskserver` FAULTS** — a kstack page unmapped across the park/wake address-space switch (ARM64-PLAN §10.7, still open) |

The EC2 facts off a live Nitro guest (`c7g.large`, `edk2`) and the bare metal: the console
is a 16550 (`c7g.large`: MMIO `0x8000_0000`, `mmio32`; `c7g.metal`: `0xE2F0_0000`), the map
starts at `0x4000_0000` (the `0x4008_0000` load address is RAM), PSCI is SMC, the counter
runs at 1.05 GHz. Logs: `infra/aws/logs/aarch64-20261003-231145/` and
`infra/aws/logs/aarch64-{graviton,nitro}-acpi-c7g.*.log`.

### 10.3. Graviton like for like: the UDP RTT against a Linux `c7g`, and no PHC (2026-10-04)

The first arm64 RTT (§10.8 of the plan, p50 40.3 us) was taken from an x86 neighbour.
This one is like for like: the neighbour is a `c7g.large` with AL2023 arm64 (kernel 6.18,
`bench/udp_ping`, 20 000 × 64 B, pinned), the echo is either a second AL2023 `c7g.large`
running `bench/udp_echo` (L) or Talaria on a `c7g.large` with `talaria.mode=userver` — the
musl echo through the socket syscalls, ENA LLQ, polling (T). All three in one cluster
placement group; `lab-up.sh` now gives the neighbour the arm64 AMI when its type is arm64.

| 8 alternating pairs | p50 | p99 | p99.9 |
| --- | --- | --- | --- |
| Linux ↔ Linux (L) | 33.6–34.9 us (median 34.1) | 39.8–49.4 us | 52–71 us |
| Linux ↔ Talaria (T) | 33.7–35.0 us (median 34.5) | 39.9–42.1 us | 52–60 us |

The median is a tie (within 1 %; one side of each path is the same Linux), the tail is
narrower on Talaria's side. Before the alternation both sides showed a second mode at ~49 us
p50 in some runs (L 49.1/49.0/35.8, T 33.7/34.1/49.1) — the path's, not the stack's.

The ENA's PHC (`supported` bit 29) is **not declared** on any Graviton type tried —
`c7g.large`, `c7gn.large`, `c8g.large`, `m7g.large` (`supported` = 0x14205df6 / 0x54205df6 /
0xd4205df6 / 0x94205df6). The wall clock's reference on Graviton has to be something else
(the Amazon Time Sync service over the network is the candidate); the x86 types that declare
it (`m7i`) stay the PHC's ground. Log (local): `infra/aws/logs/aarch64-ec2-c7g.large-rtt-linux-vs-talaria.log`.

### 10.4. The stand on the ordering audit (2026-10-04)

`c7g.metal` once more (`infra/aws/kvm-stand-aarch64.sh LOOPN=1`, the kernel at `bd0e701`, the
instance destroyed). The gate under KVM **268/0**.

| what | number |
| --- | --- |
| `mbench` | alu 0.77 ns/step, syscall 157–162 ns, syscall-ptr16 194–198 ns, read64 176 ns, chase-64m 92–93 ns |
| `futexlat` | futex same/cross-core p50 559–599 ns (§10.2: ~512); cond same/cross-core p50 2449–2590 ns (§10.2: ~1900) |
| `smp2` / `smp4` | speedup 2.00 / 4.00 |
| the probes | `ring` 2 M slots in 226 ms, 0 corrupted, 8.84 M slots/s; `svccost` 134 ns with the FP/SIMD save, 115 without |
| the soak (`soak-aarch64.sh 50`, 10 modes with `piperace`) | **0 of 50** each |
| the parallel race detector (`PAR=24 … 96`, forkap/shmrace/memrace/piperace) | **0 of 96** each |
| the network, polling | udp RTT p50 11.5 us, p99 12.0; tcp 72 700 req/s (13.8 us/req) |
| the network, `netirq=1` | udp RTT p50 22.7 us; **tcp `diskserver` 39 509 req/s, 25.3 us/req — no fault** (the fix of the plan's §10.8, confirmed) |

The wake path is the one regression: the futex RTT +11 %, the condvar RTT +32 % against the
previous stand, the audit's new barriers on it (the plan's §10.10). The stand ran before
the generic timer's interrupt-storm fix (the plan's §10.10), which touches every timed wait.
Log (local): `infra/aws/logs/aarch64-20261004-150746/`.

### 10.5. The wake path's step, bisected (2026-10-04)

`c7g.metal` (`kvm-stand-aarch64.sh` at `e64d4bb`, `BISECT=… VARIANTS=…`, the instance
destroyed). The gate under KVM **270/0** (`tsync` among the checks — the timer-storm fix
holds on hardware); the soak 0 of 50 on ten modes, the parallel detector 0 of 96, one gate
loop clean; the network as in §10.4 (udp p50 11.6 us, tcp 72 327 req/s, `netirq=1` tcp
39 803 req/s).

The A/B: every variant built in its own tree on the stand, `futexlat` booted in rounds with
the variants in turn (a drift of the stand lands on all alike). The cross-core p50, the
median of the rounds (5 or 6), ns:

| variant | futex | condvar | note |
| --- | --- | --- | --- |
| `f3f06bc` (the §10.2 stand) | 536 | 2176 | |
| `bc96dd6` (IRQs masked before the restore) | 535 | 1949 | |
| `04be5c4` (the RAM above 4 GiB) | 526 | 1937 | |
| `ea24e3f` (the commit before the audit) | 518 / 511 / 541 | 2008 / 1956 / 2121 | three passes |
| `a843638` (the audit) | 563 | 2596 | **the step** |
| `88951b0` (the timer fix) | 577 | 2515 | |
| HEAD `e64d4bb` | 572 / 569 / 572 | 2437 / 2465 / 2467 | three passes |
| HEAD − the SGI's `dsb` / − the membarrier ack / − the dead-space Acquire | 573 / 604 / 571 | 2396 / 2595 / 2452 | |
| HEAD − all three | 577 | 2486 | not them |
| HEAD + `rcpc` (Acquire as `ldapr`) | 566 | 2458 | no effect |
| HEAD − the audit's `proc.rs` / `sched.rs` / `sync.rs` / `trap.rs` | 570 / 569 / 583 / 576 | 2484 / 2458 / 2534 / 2356 | |
| HEAD − the audit's `syscall.rs` | 572 | 2180 | the condvar's spread 1983–2482 |
| HEAD − the audit's memory files (`mem`, `paging`, `mmu`, …) | 541 | 2501 | |
| `ea24e3f` / HEAD, all functions and loops 64-byte aligned | 530 / 580 | 2102 / 2438 | not alignment |
| `svccost` (the bare entry), every variant | 130–134 | — | flat |

The step is real and is the audit commit's: +50–60 ns on the futex RTT, +400–500 ns on the
condvar's, reproduced in three passes. The barriers it was blamed on are not it, nor the
`ldar`/`ldapr` choice, nor code alignment; no single file's revert takes it back beyond the
spread. The combined reverts (memory + `syscall.rs`, + `trap.rs` + `sched.rs`, the whole
commit as a control) were cut off by the stand's own teardown — the next pass is
`ABONLY=1` with those and a hunk-level split. Logs (local):
`infra/aws/logs/aarch64-20261004-163328/` (`ab.log`, `ab2-perfile-rcpc.log`, `ab3-align.log`,
the patches).

### 10.6. Phase 8 on hardware, and the step is not the audit's code (2026-10-04)

`c7g.metal` (`kvm-stand-aarch64.sh` at `be5de19`, `LOOPN=0 SOAKN=20 PARN=48`, the A/B with
`BISECT="ea24e3f be5de19"` and three revert patches, the instance destroyed after 28 minutes).
The gate under KVM **305/0** — the new PMUv3, profile and gdb-stub checks among them: the
counters with retired instructions and the 32-bit widening across a wrap on a Neoverse V1
through KVM, the stub's steps, `hbreak` on a secondary and the watchpoint's silent
step-over with KVM's emulated debug registers, a host gdb attaching over the console's
line. The soak 0 of 200 (20 boots × ten modes), the parallel detector 0 of 192. The network
as before (udp p50 11.3 us, tcp 75 004 req/s; `netirq=1` tcp 39 209 req/s); `mbench` syscall
156 ns, `svccost` 131 ns with the FP/SIMD save, 114 without.

The A/B of §10.5's step, six interleaved rounds, the median p50 in ns:

| variant | futex cross | condvar cross |
| --- | --- | --- |
| `ea24e3f` (before the audit) | 546 | 2243 |
| HEAD `be5de19` | 574 | 2400 |
| HEAD − the audit commit's WHOLE `kernel/src` (`rfull`) | 588 | 2432 |
| HEAD − its wake-path files (`sched`, `sync`, `proc`, `syscall`, `trap`, `smp`, `gic`) | 560 | 2349 |
| HEAD − its other files (the network, PCI, ext2, the ITS, the barriers, the wall clock) | 588 | 2429 |

The control decides it: taking back every line the audit commit changed in `kernel/src`
leaves the step where it is. The price is not in the audit's code. What the commit also
changed is outside `kernel/src` — `build.rs` began embedding the `tsync` set, the image
grew, everything after the embedded data moved — so the next A/B is the embedding alone
(and an image padded to the same size without it). The barriers and the atomics stay.

## 11. Reproducing any of this

```sh
# the local comparative suite (one workload, both sides, on this machine)
bench/suite/run-local.sh postgresql both
# the harness's own regression — run it before believing a stand
bench/suite/selfcheck.sh
# the EC2 stand: one rise, phase A on both sides, then one phase per hypothesis
bench/suite/run-remote-all.sh
bench/suite/ab-phase.sh   # further phases on the rise already up
# the summary table over result directories
python3 bench/suite/lib/summary.py <dirs...> --md SUMMARY.md
# the latency band (what a percentile hides)
python3 bench/suite/lib/bandscan.py <dir>
# durability: pull the plug and count the acknowledged writes
bench/suite/powercut.sh mongodb 180
# microbenchmarks from inside the guest
kernel/test.sh                      # the regression, including sync/futexlat/fiber/fsbench
bench/ixgbe/hw-bench.sh             # real 82599 through VFIO (needs root and two ports)
```

The knobs used to take the measurements above (`talaria.appprof`,
`talaria.sysrow`, `talaria.netprof`, `talaria.stat`, `talaria.prof`, …) are
documented in `docs/PERFORMANCE-OPTIMIZATION-GUIDE.md`, and every one of them is
in the registry in `kernel/src/cmdline.rs`.

---

Related: `docs/PERFORMANCE-OPTIMIZATION-GUIDE.md` (how to measure and tune a
workload), `bench/suite/README.md` (the suite and its discipline),
`docs/ENGINEERING-NOTES.md` (the rules and the traps), `docs/PRODUCT.md`
(boundaries and limits), `CHANGELOG.md` (when each number appeared).
