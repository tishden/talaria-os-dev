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

### 10.7. Phase 7 on the metal: the five servers under KVM; the step is the layout's (2026-10-05)

`c7g.metal` (`kvm-stand-aarch64.sh` at `192ff25`, the services' volumes uploaded as sparse
tarballs, `BISECT=ea24e3f VARIANTS=infra/aws/ab-layout ABROUNDS=6`; provisioned in 98 s, the
kernel built in 439 s). `kernel/test-aarch64-db.sh` under KVM, Neoverse V1, four vCPUs:

| server | the product's wait (spinning) | `talaria.netmix=1000000` |
| --- | --- | --- |
| PostgreSQL 16.14 (Alpine aarch64) | 15/15 checks of the set; answers in 3 s | answers in 1 s |
| MariaDB 10.11.18 | answers in 1 s | 2 s |
| MongoDB 4.0.5 (Alpine 3.9, dynamic) | 3 s | 2 s |
| Temurin 21 aarch64, `Bench all` | speedup4 **3.98**, park/unpark p50 **0.40 us** | 3.99, 0.42 us |
| ClickHouse 25.8 (the official arm64 ET_EXEC) | B12's set in **5 s**, exit 0 | 5 s |

Both rows 15/0: validate.sh B10-B12's arm64 twin on hardware. The x86 B11 thresholds
(speedup4 > 2.2, park p50 < 10 us) are passed with margin; the JVM's park p50 on x86 under
KVM is 4.1 us (`services/jvm`, this session).

The PMU on a Neoverse V1 through KVM: **PMUv3p4**, 6 event counters + the cycle counter, the
width CHAIN pairs (32-bit counters, no PMUv3p5 `LP`); the events found by `PMCEID`:
`INST_RETIRED` 0x08, `LL_CACHE_MISS_RD` 0x37, `BR_MIS_PRED_RETIRED` 0x22 — no LLC-reference
and no branch-retired counter is exposed. The gate's loop: 60 000 660 cycles, 120 000 057
instructions, IPC 1.99.

The A/B of §10.6's open question, six interleaved rounds, the median p50 in ns (the variants
are patches over `192ff25`):

| variant | futex cross | futex same | condvar cross |
| --- | --- | --- | --- |
| `ea24e3f` | 539 | 540 | 1984 |
| `notsync` (no `tsync/` embedded: the image 143 KB smaller) | 534 | 533 | 2064 |
| `tsyncpad` (a blob of tsync's 143 424 bytes in its place) | 532 | 533 | 2033 |

Both of today's variants are at `ea24e3f`'s level, with and without the embedded set: the
+30-50 ns "step" of §10.5-§10.6 does not belong to any code or to the embedding — today's
tree moved everything again (the kernel's `.bss` grew by 4 MB this session) and the step is
gone. It was the layout's lottery; nothing to chase. (HEAD's own `futexlat` runs, not
interleaved: 567 and 546.)

The numbers otherwise as before: udp p50 11.3 us (polling), tcp 71 882 req/s; `mbench`
syscall 155 ns, `svccost` 131 ns with the FP/SIMD save and 113 without; the ring, icache and
bbm probes clean (0 wrong values, 0 TLB conflict aborts over 51 M reads).

Two failures, both one class: the gate's `tsync` and one of three `futexlat` boots died on
their FIRST instruction — a level-3 permission fault ("not executable") on the static
image's first page at 0x5000_0000, the task on the kernel's own tables (`run_threaded`). The
soak: `syncbench` 7 of 50, the rest 0; the parallel detector (24 boots at once):
`futexlat` 10 of 96, `syncbench` 13 of 96, `forkap`/`shmrace`/`memrace`/`piperace` 0. Never
under TCG, in any CPU model (`-cpu max`, `neoverse-v1`). Found and closed in §10.8.

### 10.8. The first-instruction faults: four targeted runs (2026-10-05)

`kvm-stand-aarch64.sh` with `CUSTOM="PAR=24 MODES='tsync futexlat syncbench'
kernel/soak-aarch64.sh 96 /tmp/custom"` (the build, the 288 boots, the instance down; ~15
minutes of a `c7g.metal` each), boots that fail out of 96:

| kernel | instruments | futexlat | syncbench | tsync |
| --- | --- | --- | --- | --- |
| `192ff25` (§10.7's detector) | — | 10 | 13 | — |
| `d9f7d04` (the image's range reserved) | `talaria.ptlog=1` | 0 | 0 | 0 |
| `d9f7d04` | — | **17** | **13** | **1** |
| `579113e` (the secondaries arm their entry only) | — | **0** | **0** | **0** |

The second row is the trap: the page-table journal's writes moved the timing and the race
hid; the same kernel without it failed 31 of 288. The failures' signatures named the cause
once read together — the code page NX, `udf` where the code was (a SIGILL, EC 0), and a
SECONDARY core's write fault on the read-only code page from `syscall::init`: the secondaries
allocated and re-zeroed the workload's brk heap (`docs/ENGINEERING-NOTES.md` item 41). One
targeted run failed for capacity: no `c7g.metal` in any of the three zones for 18 minutes; the
next one, minutes later, got one in the first.

### 10.9. A Graviton's own boot: ClickHouse, the clock by SNTP, the pulled plug (2026-10-05)

Three runs on `c7g.large` (Nitro, edk2, one core by default) through
`infra/aws/ec2-boot-talaria.sh` with `ARCH=arm64`, the neighbour an AL2023 `c7g.large` in
the same cluster placement group (`lab-up.sh c7g.large`).

**ClickHouse, the official arm64 binary** (25.8.33.6, an ET_EXEC at 0x200000..0x1CA9_7310,
from `services/build-aarch64-db.sh`'s ext2 volume; `talaria.pci=1 talaria.mode=proc`):
validate.sh B12's set — 10 million rows to 49999995000000, MergeTree written, merged and read
back (19999900000, 28572 marks aggregated), `exit_group(0)`; the queries' own log spans
0.72 s. No knob: the firmware's map puts the GIC (0x1000_0000) and the ECAM (0x2000_0000)
in GB0 and RAM from 1 GiB, so GB0 is devices on a Graviton as on `virt`, and the devices'
alias of §10.13 already frees it for the binary.

**The wall clock by SNTP** (`talaria.ntp=aws,4`: a burst of four queries to
169.254.169.123 every 4 s, the shortest round trip fed to the servo):

| | |
| --- | --- |
| the base's error at the first sample (UEFI `GetTime`) | −209 ms; −1.03 s on another boot |
| the best round trip of a burst | 49–55 us (111–124 us in the first burst: ARP) |
| the servo's correction, settled | +14…+17 ppm (the neighbour's chrony: its crystal +13.6 ppm) |
| the residual the kernel reports | −63 → −38 us over the first five samples |
| **the guest − the reference, judged from the neighbour** | **+18, +3, −1, −10, +21, −3 us** |

The judge (`kernel/tools/ntpcmp.py` on the neighbour) asks the kernel echo
(`talaria.kecho=7777 talaria.kts=1`, the kernel's wall clock in the answer) and
169.254.169.123 in turn, each answer referred to the neighbour's clock at its round trip's
midpoint; the median of 15 rounds, six times 20 s apart. Without the reference the same
machine counts from the firmware's whole seconds at the crystal's +15 ppm: 1.3 s a day. On the
dev box the gates' server is the host's clock +3 s running 150 ppm fast: the guest is −64 us
from it under KVM (x86) and −54 us under TCG (arm64), whose servo settles at −150…−190 ppm —
the reference's rate, exactly.

**The pulled plug on EBS** (`infra/aws/ec2-crash-test.sh`: `stop-instances --force` after a
committed, un-checkpointed transaction, then the same volume with phase 2's user-data): ordered
mode — 6 blocks replayed, `journal2: RECOVERED OK`; `journal=data` — 14 blocks (the data in the
transaction) replayed, `RECOVERED OK`. Phase 7's second exit gate, on the target hardware.

### 10.10. The whole stand on `e95abf4`, and the guide's workload on a Graviton (2026-10-05)

**The KVM stand** (`kvm-stand-aarch64.sh`, `LOOPN=2`, `c7g.metal`, 59 minutes, destroyed by
itself; logs `infra/aws/logs/aarch64-20261005-104403/`) on the commit with §10.8's fix:

| step | result |
| --- | --- |
| the gate under KVM | **313/0** |
| the five servers, default wait / `netmix` | 15/0 / 15/0 (the JVM's speedup4 3.975 / 3.996, park p50 0.43 / 0.44 us; ClickHouse's B12 in 5 s) |
| the soak, 13 modes × 50 boots one at a time | **0 of 650** (tsync, futexlat, syncbench among them) |
| the parallel detector, 6 modes × 96 boots, 24 at once | **0 of 576** |
| the gate twice more in a loop | 0 of 2 failed |
| UDP RTT p50, polling / `netirq=1` / `netirq=1 netpark=0` | 11.7 / 24.0 / 11.5 us |
| TCP keep-alive, polling / `netirq=1` | 14.1 / 26.2 us per request |
| futex RTT p50 same-core / cross-core | 540 / 540 ns |
| `mbench` syscall / svc round trip with and without the FP save | 155 ns / 133 and 114 ns |

`netmix` costs nothing under KVM (the servers' numbers are the same in both modes) and stays
off by default: on hardware the product's waiter is a dedicated core's spinner.

**The guide's workload on a `c7g.large`** (`examples/mdpipe`, `make a64`, `talaria-deploy.sh`
with the binary's architecture; 2 cores, 20000 ticks/s, 30 s; HANDS-ON-GUIDE §11.1):
p50 1.6 us, p90 5.6, p99 34.8, p99.9 50, the clock never backwards — against p50 2.5 / p99 6.2
on a `c6i.large` (x86's §11). The Graviton's p99 is its own and not yet explained.

**The PMU on a Graviton VM**: PMUv3p4, two event counters plus the cycle counter, the
instructions on a chained pair — which does not chain: `top` showed an IPC of 6.8e7 now and
then, and a raw read of the pair at every wrap showed the low half wrapping five times in ten
seconds with the high half at 0 (`docs/ENGINEERING-NOTES.md` item 44); CHAIN is opt-in now
(`talaria.pmuchain=1`) and the reader widens 32-bit counters. The busy loop's IPC 1.99 (two instructions a cycle, a mul-add chain), a halted core's
cycles do not count (0.15-1.5 M in 100 ms of `wfi`).

### 10.11. The Graviton's mdpipe tail: the wait tier, not the wake; the cache counter (2026-10-05)

`examples/mdpipe` (`mdpipe-a64`, 2 workers, 20 000 ticks/s, 30 s) on `c7g.large`: one AMI per
batch with the tail of the command line from user-data, each variant its own instance, and in
every batch a Linux AL2023 arm64 `c7g.large` running the same static binary three times in the
same hour (the stage latency, us):

| | runs | p50 | p90 | p99 | p99.9 |
| --- | --- | --- | --- | --- | --- |
| Talaria, the default tier `wfe(monitor)` | 10 | 1.59-1.75 | 5.5-6.0 | **33.5-36.1** | 43-46 (96-110 with `talaria.stat=`) |
| Talaria, `talaria.sync=hlt` (`wfi+ipi`) | 2 | 2.6-2.7 | 6.3-6.4 | **6.8** | 8.6-8.9 |
| Talaria, `talaria.sysprof=1` (the profile perturbs the timing) | 1 | 1.89 | 5.6 | 8.2 | 36.9 |
| Talaria, wfe + `talaria.wakepreempt=0` (a woken thread preempts at once) | 1 | 2.41 | 6.4 | **8.2** | 36.7 |
| Talaria, wfe + `talaria.nowakebal=1` | 1 | 1.16 | 10.2 | 12.0 | 37.4 |
| Talaria, wfe + `talaria.nosteal=1` / `talaria.slice=150000` | 1 + 1 | 1.58 / 1.72 | 5.5 / 5.7 | 32.9 / 34.9 | 37.3 / 38.9 |
| Linux 6.18 (AL2023) | 18 | 3.5-4.2 | 4.1-4.5 | **4.6-5.7** | 5.5-15.5 (one run 4881: a host hiccup) |

The `stat:` block over the same 30 s, wfe / hlt: futex waits 591 k / 246 k (ended by "the
word itself" 316 k / 123 k), FUTEX_WAKE 374 k / 124 k, IPI reschedules of a core whose current
task sleeps 139 k / 61 k, kicks 129 k / 61 k.

What each wake costs on the same machine (`talaria.test=wakeprobe`: the first secondary sleeps
as a futex waiter does, the boot core wakes it after 30 us / 100 us / 300 us / 1 ms; the
medians of four, ns, two instances):

| | 30 us | 100 us | 300 us | 1 ms |
| --- | --- | --- | --- | --- |
| a store, under `wfe` | 137-150 | 138-147 | 138-147 | 138-148 |
| an SGI, under `wfe` | 2287-2319 | 2351-2402 | 2435-2494 | 2532-2612 |
| an SGI, under `wfi` | 2209-2289 | 2249-2316 | 2254-2325 | 2494-2591 |

The Nitro host wakes a `wfe` sleeper as fast as a `wfi` one, by either means, at every length:
the 34 us is the wfe tier's policy, not its wake — and a knob moves it: with
`talaria.wakepreempt=0` (a woken thread preempts at once) the p99 is 8.2 us
(`docs/ENGINEERING-NOTES.md` item 47). `talaria.sync=hlt` (p99 6.8) and
`talaria.wakepreempt=0` (8.2) are the measured levers until that placement is understood.
The `stat:` block's new kick line names it (one more run, wfe): of 129 188 kicks, 2 746 went to
the woken task's own core (it sleeps in place — the kick ends its wait) and **126 442 to a core
running ANOTHER task** (md_work0 101 093, md_work1 25 337, each having run ~1 us; asleep at that
moment: 29). A kick only ends a `wfe`; a running core does not notice it. But the same block's
"wakeup -> cpu" histogram says the woken thread mostly does not wait long: under 5 us 194-213 k,
5-20 us 50-81 k, 20-50 us 1.2-2.0 k, beyond 50 us under 1 k, of 251-290 k wakeups — while a
p99 of 34 us needs ~6 k of the 600 k ticks beyond it. So the tail is not the wake's latency
either; what `wakepreempt=0` changes to remove it is open (in `wfi+ipi` the same histogram is
5.6-5.9 k under 5 us and 108-117 k at 5-20 us: slower wakes, no tail). Under TCG, for comparison: 0.6-1.6 / 4.3-6.2 / 16-51 us.

**The tail is the oversubscription's (2026-10-06).** mdpipe runs five threads (the generator,
two workers, the sink, main) and the variants above gave them two cores. One batch on
`c7g.xlarge` (4 vCPUs) with `infra/aws/variants.sh` — one AMI built with no `talaria.smp` and
the core count in each variant's user-data (a user-data tail is appended and the FIRST
occurrence of a key wins, so a baked `smp` cannot be overridden), the console confirming the
online mask each time:

| | p50 | p90 | p99 | p99.9 | wakeups | transplants at a wakeup |
| --- | --- | --- | --- | --- | --- | --- |
| Talaria `smp=2`, wfe | 1.61 | 4.1 | **32.9** | 37.4 | 289 k | 12 |
| Talaria `smp=4`, wfe | 1.30 | 1.7 | **2.6** | 12.3 | 48 k | 117 k |
| Talaria `smp=2`, `sync=hlt` | 2.59 | 6.7 | 7.0 | 9.1 | 123 k | 3 |
| Talaria `smp=2`, `wakepreempt=0` | 2.55 | 6.7 | 8.1 | 37.0 | 264 k | — |
| Linux 6.18, 4 vCPUs, 3 runs | 3.5-3.8 | 4.0-4.1 | 4.6-4.7 | 4.8-7.0 | | |

With a core for every thread that is not asleep the default tier is Talaria's again (p99 2.6
against Linux's 4.6 on the same type), and the woken thread mostly goes to an idle core (117 k
transplants) instead of to a core whose futex waiter sleeps in `wfe` as its current task and
must be kicked off it. The 33 us is what that eviction path costs when there is no idle core;
which step of it (the kick that a running core ignores, the threshold of `wakepreempt`, the
yield of the evicted waiter) is still to be traced per tick.

The threshold's value is not it. A second batch, `smp=2`: the default (150 us) 34.0,
`wakepreempt=5000` 34.2, `20000` 34.1, `60000` 32.6 — only 0 helps (8.1), and 0 is not "a small
threshold" but another branch: `busy_long` is then true for any current task, so every wake to
a core running something sends a resched IPI instead of a kick that a running core ignores
(`sync::wake_addr`). The kicked core's current task has run ~1 us when the kick lands (the
kick line above), under any of these thresholds. Talaria's "queue max" is 13-25 in every
two-core variant (5 on four cores, 2 on Linux) — the generator's `nanosleep` sleeps in 200 us
coarse windows and catches up in bursts — but `sync=hlt` has 21 with a p99 of 7, so the bursts
are not the tail by themselves. (The first batch's first variant lost its console to the tool's
per-variant window; it was fetched by hand, and `variants.sh` now polls every variant until one
deadline.)

**The cache counter on a `c7g.large`** (`talaria.test=pmu`, `pmu-ev:` — each event on the last
counter across a random walk of 64 MB, 1 M line loads): L2D_CACHE_REFILL (0x17) 0.99 per
line, L2D_CACHE (0x16) 2.00, L1D_CACHE_REFILL 0.99, L1D_CACHE 1.00, MEM_ACCESS 1.00,
L1D_TLB_REFILL ~0; LL_CACHE_RD/MISS_RD, L3D_CACHE(_REFILL) and BUS_ACCESS are not declared
and count nothing. PMCEID0 = 0x0000027079ff0f3f, PMCEID1 = 0x000000001e30a07f. With item 46's
fix the boot line is `LLC-miss=1:0x17` and `top` shows 13-17 M misses per 10 s (7.1-7.6 per
thousand instructions) over mdpipe, where it showed 0.

**The default's maximum (2026-10-21, ARM64-PLAN §11 item 2b, `variants-rqwait-c7g` /
`variants-wakedef-c7g`).** After the exact `nanosleep` the default tier's p99 was 5.1 us but its
maximum 0.5-7 ms. The slow-interval instrument, made to keep the LONGEST marked stages with each
core's decisions and sleeps, showed every one of the six longest (425-491 us) as a worker ready
behind md_sink, which had just got the core and ran to the END OF ITS SLICE: the wakeup found it
younger than the 150-us preemption threshold, kicked the core and nobody re-checked. With the
tick pulled to the threshold on such a wakeup (`stay:young`), three 30-s runs of the default
kernel on a `c7g.large`: max 297 / 155 / 121 us (492 and 7070 before), p99 4.84-5.00, p99.9
7.04-7.24 us; Linux the day before 3.7 / 4.9 / 5.4, max 400. `wakedef` under KVM (a futex waiter
behind a spinning hog on one core, 1000 wakes): p99 414 → 87-99 us, max 499 → 201-472.

### 10.12. The KVM stand on the session's branch (2026-10-05)

`kvm-stand-aarch64.sh` on `c7g.metal` from `4a69b13` (`arm64-s64`: the cache-counter fix and
`wakeprobe` on top of `bb198d8`), 63 minutes, destroyed by itself; logs
`infra/aws/logs/aarch64-20261005-175710/`:

| step | result |
| --- | --- |
| the gate under KVM, once and three times in a loop | 323 PASS, 1 FAIL each time — the new "`-` (no counter)" assertion, true only under TCG: KVM's PMU counts the cache events (`LLC-miss=1:0x37 LLC-ref=3:0x36 branches=4:0x21 predictor-miss=2:0x22`); split by machine in `3bd2ef4` |
| the five servers, default wait / `netmix` | 15/0 / 15/0 (the JVM's speedup4 3.997 / 3.996, park p50 0.444 us) |
| the soak, 13 modes × 50 boots | **0 of 650** |
| the parallel detector, 6 modes × 96 boots, 24 at once | **0 of 576** |
| UDP RTT p50, polling / `netirq=1` / `netirq=1 netpark=0` | 11.5 / 23.8 / 11.4 us |
| futex RTT p50 same-core / cross-core; condvar same-core | 500 / 500 / 2641 ns |
| `mbench` syscall; svc round trip with the FP save | 156 ns; 130 ns |

### 10.13. The first arm64 scoreboard: Talaria against Linux on Graviton3 (2026-10-05/06)

`ARCH=arm64 bench/suite/run-remote-all.sh` (ARM64-PLAN phase 9): four `c7g.xlarge` (4 vCPU =
four physical cores, 8 GB) per workload in one cluster placement group — a Talaria server and a
Talaria generator from one AMI (the aarch64 stub), a Linux server and a Linux generator on AL2023
arm64 (kernel 6.18, the playbook's tuning) — the same workload tree and the same `tlgen` binary on
both sides, the ladder at 10/25/50/75/90 % of the lower of the two ceilings, three repeats of 60 s,
the SERVICE latency judged (README §3.1: a point counts at a ratio below 0.95 or above 1.05; a
scoreboard judges WITHIN a workload, each workload is its own bring-up). Two runs: redis, mongoose
and mysql on `bb198d8` (`results/ec2all-a64-20261005-153336`); nginx, PostgreSQL and mongodb on
`e8981e5` (`results/ec2all-a64-20261005-235147`) — their first bring-ups found three defects
(below) and were re-taken with the fixes.

| workload | p50 | p99 | p99.9 | p99.99 | points | ceilings, Talaria / Linux, k req/s |
| --- | --- | --- | --- | --- | --- | --- |
| redis | 0 / 4 / 16 | **19 / 1 / 0** | 17 / 1 / 2 | 8 / 2 / 10 | 80 | ping 98.9 / 141.5, get 95.0 / 131.4 |
| mongoose | 0 / 9 / 1 | **10 / 0 / 0** | 9 / 1 / 0 | 9 / 1 / 0 | 40 | — |
| nginx | 3 / 4 / 2 | **8 / 1 / 0** | 7 / 2 / 0 | 9 / 0 / 0 | 36 | nop 98.2 / 146.1, file 79.4 / 94.3 |
| mysql | 1 / 6 / 3 | **8 / 2 / 0** | 8 / 2 / 0 | 9 / 1 / 0 | 40 | select1 61.2 / 43.3, pk 47.6 / 33.0 |
| postgresql | 4 / 3 / 2 | 3 / 2 / 4 | 3 / 1 / 5 | 4 / 1 / 3 | 35 | select1 42.1 / 43.8, pk 15.5 / 25.2 |
| mongodb | **9 / 4 / 2** | 9 / 3 / 3 | 10 / 3 / 2 | **13 / 1 / 1** | 60 | ping 53.5 / 61.7, find 23.9 / 23.5, insert 20.2 / 20.7 |

In a cell: Talaria faster / parity / slower. What the rows say:

* **The tail is Talaria's on four of five workloads.** p99 at the middle rates: redis 57-59 us
  against 87-95 (Linux's p99/p50 is 2.2-2.4, ours 1.2-1.3), mongoose 63-66 against 93-97, nginx
  57-63 against 67-90, mysql 76-80 against 86-124 and at the top rates 62-81 us against 0.4-3.0 ms
  where Linux's MariaDB falls over before its ceiling (`select1` p99.9 7.9 ms at 39 k req/s).
* **The median is Linux's on redis** (1-12 us at every rate; 54-59 against 44-52 at the top) and
  equal elsewhere; **the ceilings are Linux's on the network-only servers** (redis -30 %, nginx
  /nop -33 %, PostgreSQL pk -38 %) and Talaria's on MariaDB (+41-44 %). The closed-loop ceiling
  is 8 connections per generator, so it carries the generator's own system too (the Talaria pair
  is Talaria on both ends); the generator's open-loop debt at the top rate is up to 25 us on the
  Talaria side against 2.5 (printed in every report) — part of the median gap is the instrument's
  node, to be split off by an A/B with a Linux generator (ab-phase.sh).
* **The rare outliers are ours at low rates:** redis p99.99 285-558 us at 10-25 k req/s against
  116-130, max 0.5-0.9 ms against 0.2-0.4.
* **mongodb is Talaria's on `find`** (p50 84-91 against 100-117 us, p99 102-115 against 125-276)
  and on `insert`'s median and far tail (p50 87-90 against 89-115; p99.99 2.3-3.7 ms against
  0.8-8.6), **Linux's on `insert`'s p99 at the middle and top rates** (235-350 against 184-293 us
  — the write path's tail, ROADMAP's old item on EBS) and on `ping`'s median at the top two rates.
* **PostgreSQL is mixed:** `pk` p50 100-109 against 115-123 us (ours), p99 143-149 against
  131-144 (theirs), the top `pk` point is our overload (the lower ceiling); `select1` p99 80-92
  against 72-104 and 1.1 ms where Linux collapses at 31.6 k req/s.

**What the first bring-ups found** (the scoreboard as a test, ENGINEERING-NOTES 48-49): nginx's
Linux side refused to start (Alpine's arm64 nginx is built `--user=nginx`, the tree had no such
user — the TCG smoke runs only the Talaria side); PostgreSQL's Talaria side died at start
(`could not map anonymous shared memory`: the FS cache took half of the 844 MB below 4 GB) and
mongodb's panicked at launch (a refused 512 MB cache resize took the rest of the bump allocator).
All three are fixed and gated; the TCG reproduction in the Graviton's memory shape is
`virt` 10 GB with `talaria.lowmem=850`.

### 10.14. Whose is the network gap on a Graviton: the generator's (2026-10-06)

§10.13 gave the network-only servers' median and closed-loop ceiling to Linux (redis p50 0/4/16,
ceilings -30..-38 %), with a caveat: each side is measured by its own generator, and the Talaria
generator's open-loop debt reached 25 us at the top rate. One bring-up of the redis stand
(`c7g.xlarge` ×4, `results/ec2genab-a64-redis-20261006-093445`) separated the two: phase A as the
scoreboard takes it (each side its own generator), phase L the Talaria server under the LINUX
generator over A's ladder (`ab-phase.sh GEN=linux`), phase R phase A's Talaria side again (the
drift check: every p50 within 1.00-1.05 of A), then the closed-loop ceiling of the Talaria server
probed by the Linux generator (`run-remote.sh` `capacity()`, three probes, spread 0-1 %).

The median, us (ping; get/set/mixed alike):

| rate, req/s | Talaria server, Linux generator | Talaria server, Talaria generator | Linux server, Linux generator |
| --- | --- | --- | --- |
| 10 167 | 38.3 | 39.9 | ~43 |
| 50 837 | 38.4 | 45.2 | ~46 |
| 91 506 | **38.0** | 55.1 | ~44 |

The ceiling, req/s:

| case | Talaria server, Linux generator | Linux server, Linux generator | Talaria server, Talaria generator |
| --- | --- | --- | --- |
| ping | **165 164** | 144 535 | 101 674 |
| get | **152 309** | 132 753 | 97 218 |
| set | **144 759** | 124 125 | 95 073 |

(The Linux server's medians are phase A's through the scoreboard's ratio; its ceilings phase A's.)
Measured by the same generator, the Talaria redis server holds a flat 38-40 us median up to
91.5 k req/s and a ceiling 14-17 % above Linux's. **Every median and ceiling point redis lost on the
scoreboard is the Talaria GENERATOR's** — the client side of the network path (many connections,
a send and a receive per request on the load node), which the scoreboard's rule "each side its
own generator" charged to the server. That client path is the next thing to measure; the
scoreboard's verdicts on the servers stand as they were taken.

Named and fixed 2026-10-21 (ARM64-PLAN §11 item 2a, `results/ec2ab-redis-20261021-143739`): the
generator's ceiling was the PRICE of its empty `recvfrom` — the syscall gate validated tlgen's
whole 1 MiB receive buffer page by page, four table levels per page (256 walks, ~3 us), before
the handler could answer EAGAIN. The check now descends once per 4K table and is capped at what
the descriptor can deliver (`Len::ArgCap`): `recvfrom` on the Graviton 4274 → 455 ns per call, the
Talaria generator's closed-loop ceiling 85-89 k → 103-116 k req/s (Linux's 125-141), every
ladder median and p99 up to 77-86 k req/s under Linux's, the top rung 92-104 k against 77-80 k.
Gate `gatecost` (an empty read with a 4 KiB and a 1 MiB buffer must cost the same): `c7g.large`
Talaria 207 / 207 ns, Linux 217 / 216.

### 10.15. The generator's one-second stall was the TCP retransmission floor (2026-10-22)

The redis ladder of 2026-10-21 (`ec2ab-redis-20261021-143739`, `c7g.xlarge`) carried one
number no instrument of the scheduler could explain: the Talaria generator's **max** at every
rung from 77 k req/s up was 1 000 067..1 000 124 us — one second plus one RTT, the same at every
rate — while the Linux generator's max in the same hour was 280-380 us. A maximum that exact is
a protocol timer, not a scheduler. smoltcp 0.14 keeps RFC 6298's floor, `RTTE_MIN_RTO = 1000`
ms; a closed-loop request/response connection has no second segment in flight to earn a duplicate
ACK, so one lost segment is recovered by that timer alone, and one drop at the ceiling cost the
generator 1.000 s and 100-350 k requests of debt behind it. Linux's floor is 200 ms
(`TCP_RTO_MIN`).

The fix is one constant in a vendored smoltcp (`kernel/vendor/smoltcp/TALARIA.md`), and the
gate is a loss gate: `talaria.txdrop=N` drops every N-th TRANSMITTED frame on the floor (a drop
on the receive side would measure the peer's timer — its reply carries the ACK), and `txloss`
runs 60 echo rounds against the harness's echo server with every 7th frame lost. Under KVM on the
dev box:

| | slow rounds (>100 ms) of 60 | each | over 600 ms |
|---|---:|---:|---:|
| before | 10 | 1 000 ms | 10 |
| after | 10 | 200-201 ms | 0 |

What the ladder still owes an answer: why a frame is lost at all once per rung at 77 k+ req/s
where Linux loses none (the ENA receive ring under a burst is the first suspect) — to be measured
on EC2 with the drop counters; a drop now costs what it costs Linux.

### 10.16. The re-take with both generators, and what the instruments named (2026-10-22/23)

**The board** (ARM64-PLAN §12 item 0): HEAD 14b6720's kernel (`e1682979`), `c7g.xlarge` pairs,
three repeats, each workload's ladder taken twice on one bring-up — with its own generator and
with the Linux generator against the same Talaria server (`GENLINUX=1`,
`bench/suite/lib/genboard.sh`). Two stands in two regions (`ec2all-a64-20261022-200847`
ap-northeast-1, `ec2all-a64-20261022-193452` us-west-2). Faster / parity / slower:

| workload | own generators p50 / p99 / p99.9 / p99.99 | Linux generator p50 / p99 / p99.9 / p99.99 | server ceilings under the Linux generator |
|---|---|---|---|
| redis | 7/13/0, 18/2/0, 19/1/0, 18/2/0 | 15/5/0, 20/0/0, 20/0/0, 20/0/0 | 139-164 k (Linux pair 125-145 k) |
| mongoose | 2/5/3, 6/3/1, 9/1/0, 9/0/1 | 3/4/2, 7/2/0, 7/1/1, 5/1/3 | dyn 136 k, file 89 k |
| mysql | 2/7/1, 8/0/2, 6/4/0, 7/3/0 | 2/6/2, 8/0/2, 6/3/1, 8/1/1 | pk 49.5 k, select1 74.6 k |
| nginx | 3/6/0, 7/2/0, 8/0/1, 7/0/2 | 5/5/0, 10/0/0, 8/2/0, 8/1/1 | nop 126.8 k (Linux 147.2), file 81.9 k (105.3) |
| mongodb | 5/5/5, 5/2/8, 4/0/11, 8/1/6 | 4/6/5, 4/3/8, 3/2/10, 6/1/8 | — |
| postgresql | 0/0/10, 1/0/9, 1/0/9, 1/1/7 | (phase cancelled: the probe's role) | select1 24.8 k (Linux 43.6), pk 10.3 k (25.5) |

The losses of mongoose `dyn` and redis's top rungs are the generator's debt (a 200-ms maximum
drags a block of the open loop behind it — below); mysql `select1`'s p50 at 40 k (47 against
42.5 us) and mongodb's tails are the server's (items 4 and 6); PostgreSQL is a defect of its
own (below).

**The lost frame** (item 1). The 200-ms maximum of §10.15 was taken apart with the vendored
smoltcp counting every debug-level event by site and keeping the last eight with their text,
`talaria.netport=6379` counting the segments in and out on the workload's port, the device's
own frame counters (`GET_STATS`) beside the driver's, a classifier of frames at the driver, and
a report printed three seconds after the port falls silent — all on the `arm64-inst` branch
(`bench/suite/lossprobe-ec2.sh`, probes 2-8 in us-east-1). What is now known:

- it is not one frame per run: a lossy run (every rung from 62 k req/s up, none at 36 k) loses
  170-260 request frames in 70 s, ~3 per second, each one a 200-ms RTO on its connection and a
  retransmission of the server's unacknowledged reply (the generator's challenge ACKs equal
  the server's retransmissions to the frame);
- the direction is one per stand: the generator's requests on three stands, the server's
  replies on one;
- the device's transmit counter equals the driver's on the sender and the device's receive
  counter equals the driver's on the receiver, to the frame, at every quiet point; nothing
  is dropped by the stack above the driver (no malformed, no held card, no ring stall), the
  ENA `rx_drops`/`tx_drops` and the five Nitro allowances are zero;
- the Talaria generator against the Linux server loses nothing at 129 k req/s (max 1-2 ms,
  zero requests over 100 ms), and the Linux generator against the Talaria server loses nothing
  on every board — the loss needs Talaria at BOTH ends;
- `dma_rmb` after the completion's phase bit (kept: it is the spec), the receive-completion
  head doorbell off (`talaria.ena=nocqdb`), and the burst-budgeted LLQ doorbell were each
  excluded by a probe; `talaria.ena=nollq` and `nowc` are not usable as A/Bs (the first does
  not hold 35 k req/s, the second does not boot the server).

So a frame that one Talaria ENA counted as transmitted is not counted by the other Talaria ENA
as received, only when both ends are ours. The next instrument is outside the kernel: a VPC
traffic mirror of both ENIs, to see whether the frame leaves the sender's ENI and whether it
reaches the receiver's. Until then the loss is what it costs Linux at 200 ms (§10.15), and the
board's "debt" column is where it shows.

**A sendto's price** (item 1b, `talaria.sysprof=1` with the five-leg split): at 105 k req/s the
entry to the dispatcher 153 ns, to `netsys::write` 56, the write itself 1 710 (of which before
the lock 73, the queueing 53, the pass to the card 580-1 710 ns growing with the rate: the pass
also drains the receive ring), the return 67, the exit 20. The pointer gate is 65 ns of it.
There is no unexplained microsecond; the price is the pass.

**PostgreSQL** (item 3). Three defects, two fixed:

1. The ENA admin queue's tail and the AENQ head wrapped to zero and were written into the
   doorbell (ena_com keeps them monotonic, the index is the low bits). The AENQ is 16 deep and
   the device posts an event about every second, so every board kernel told the device a
   wrapped head within a minute of boot. Found by reading `GET_STATS` every 10 s: the 33rd
   admin command put the device into FATAL_ERROR. Fixed (`35b2bcd`).
2. `munmap` of 14 pages per `pk` query cost 12.5 us: the arm64 range flush paid a `dsb ish`
   per page, and the "shootdown" after it was the same broadcast flush once more. One pair of
   barriers around back-to-back `tlbi` (Linux's `__flush_tlb_range`): 2.8 us on the stand
   (`5e97998`).
3. The ceiling is bimodal within one boot — select1 45 052 / 25 689 / 45 179 req/s in three
   consecutive ten-second probes, pk 10 353 / 10 597 / 21 715 — and the slow state is the
   usual one (three stands of four; the 44.9 k / 15.9 k run of us-east-1 was the exception).
   In the slow state the syscalls are no slower than in the fast one (sendto 3.6 us, recvfrom
   1.1), the clock is the same 2.55 GHz, the IPC is 0.62/0.53 against 0.75/0.52 with more LLC
   misses per instruction, and the per-task accounting of a 10-s window reads: core 1 runs four
   backends at 2.44 s each (saturated), core 0 three at 2.2 s and one at 0.4-1.2 s — 82 us of
   "running" per `SELECT 1`, which is the spin in `epoll_wait` under the manifest's
   `netyield=5`. Parking the waiters under interrupts (`netirq=1 netpark=1000000`), `netyield`
   2, 20 and 0 do not change the ceiling (0 collapses it). The mechanism of the ×2 is not
   named; the facts are here for the next session.

**nginx** (item 5): the server is `smp=1`; per request `recvfrom` 591 ns, `writev` 3.1 us,
`epoll_pwait` 19.6 us with its wait. One core serves 125.8 k `nop` req/s at 8.0 us each against
Linux's 6.6; the gap is about `writev`'s price (the assembly under the stack's lock, then the
pass to the card that also drains the receive ring).

### 10.17. The lost frame's last suspects, ASIDs, and PostgreSQL's spinning waiters (2026-10-24/25)

**The lost frame** (ARM64-PLAN §12 item 1), probes 9-12 on `arm64-inst` in us-east-1:

- Nitro's connection tracking is not it: with the security group opened to all TCP (untracked
  flows, no reboot) the "with data" loss is the same — ~250 per rung above 62 k req/s, none at
  36 k. Both directions lose in one run (on earlier stands it was one direction per stand).
- The MTU is not it: ENA `SET_FEATURE(MTU)` = 1500, smoltcp's 1514.
- A VPC traffic mirror of both ENIs (`mirrorprobe-ec2.sh`, `lib/mirrorscan.py`) is unusable on
  `c7g.xlarge` at this rate: it delivers ~10 % of the segments in bursts about every second
  second, identically on both ENIs, and in probe 11 it caught no retransmission at all. Probe
  12 (the first rung after boot, 14.7 k req/s, 60 s, the mirror on from boot) lost nothing —
  "the first rung always loses" was wrong.

The loss stays bounded to "between the two Talaria ENAs"; the next stand is a `c7gn`/`.metal`
pair, not another counter.

**ASIDs on arm64** (`5efa961`). Every process space got its own 16-bit ASID (TTBR0[63:48]); a
switch between two spaces is the register write alone, where every root change used to be a
full `tlbi vmalle1`. The hypothesis was PostgreSQL's ×2 (four to five processes switching on
two cores, IPC 0.53-0.62). Measured on one stand (`kernprobe-a64-postgresql-20261025-000912`,
us-west-2, `talaria.asid=0` against `=1`): 0.94-1.04 on every point — the TLB is not the ×2.
The ASIDs stay: correct under TCG (which flushes on an ASID change itself and cannot see a
stale-ASID class) and on a `c7g.metal` under KVM — the gate, the databases, 650 soak boots and
576 parallel race boots with no failure (ARM64-PLAN §12.2).

**PostgreSQL's ×2 named** (`8610cda`). In session 73's accounting the heavy long-lived tasks
were the postmaster and PostgreSQL's background processes (65-79 s of "running" in a window),
not the backends. Every one of them waits in `epoll_wait` on a MIXED set — its socket(s) next
to the latch's signalfd or self-pipe and the postmaster's death pipe — and a mixed set was
never allowed to park (nothing moved a socket's word when a pipe became ready): each spun its
whole timeout in `yield_window` and shared the two cores with the backends. `talaria.netmix=<ns>`
lets a non-polling waiter of a mixed set park on its word for at most that long; pipes,
eventfds and pending signals kick it (`mix_kick`). Two stands, us-west-2:

| point | spin (A) | `netmix=1ms` | Linux |
|---|---|---|---|
| pk ceiling | 9.9-10.0 k | 18.8-18.9 k | 25.1-25.5 k |
| select1 ceiling | 26.6-26.8 k | 31.5-35.4 k | 43.0-44.1 k |
| select1 p50 | — | 61-65 us | ~62 |
| pk p50 | — | 118-135 us | ~120 |

p50 −5..−13 %, p99 −10..−16 %, pk@8882 p99 273 → 147 us. `netirq=1 netpark=1ms` on top is not
better (p50 +2..4 %, pk 17.2/21.3 k). The knob is declared by PostgreSQL's manifest
(`netmix=1000000`), not by default: it touches every server with a mixed set.

**Who else spins a mixed set** (`444cd35`): the stat block now counts the mixed-set waits that
spun, split by whether another task was inside a network wait at the same time — only those
are what `netmix` parks (one waiter of the set keeps the polling slot and spins under the knob
too). A local census, every network workload under x86 KVM with `netmix=0`:

| workload | spun alone | spun beside another waiter |
|---|---:|---:|
| postgresql | 79 | 206 M |
| mongodb | 14 M | 4.0 M |
| redis | 326 M | 0 |
| nginx | 180 M | 0 |
| mysql, mongoose | — | — |

redis's and nginx's sets hold a non-socket too, but each server has one waiter: the knob does
not touch them. MariaDB polls sockets only. So the default is a question about MongoDB.

MongoDB with `netmix=1ms` on one stand (`kernprobe-a64-mongodb-20261025-111746`, us-west-2,
A both sides, P1 the knob, P2 the knob off again as the drift control; p50/p99, us):

| point | A | netmix | off again |
|---|---|---|---|
| insert 10.3 k | 83.7 / 202 | 72.2 / 277 | 78.0 / 213 |
| insert 18.6 k | 86.9 / 390 | 72.6 / 343 | 80.7 / 347 |
| find 16.2 k | 83.3 / 100 | 100.2 / 254 | 87.8 / 118 |
| find 19.5 k | 82.5 / 108 | 159.7 / 421 | 93.6 / 207 |
| ping 51.8 k | 55.8 / 77 | 59.4 / 110 | 50.0 / 71 |

Against its own control the knob buys `insert` 7-9 % of the median and costs `find` and `ping`
their top rungs (find at 19.5 k: ×1.7 the median, ×2 the p99). `ping`'s low-rate "gain" in P1
is the stand's drift (P2 shows it too). Why the top rungs lose is not measured; the likely
reading is that a parked waiter's wake-up lands on the request's path once the rate keeps every
waiter busy. So `netmix` stays a manifest key, declared by PostgreSQL alone; MongoDB's manifest
does not take it.

### 10.18. PostgreSQL's polling slot: the postmaster held it (2026-10-25)

Under `netmix` (§10.17) one waiter of a mixed set keeps the polling slot and spins — the
stack's driver while the rest park — and the slot was first come, first kept. The stat block
now names the holder (`stat: net: the slot t<N> proc <P>: held <ms>, mixed spins`): locally
t1, proc 1 — the postmaster, whose set is the listener and the signal pipe — held it for the
whole run and spun 320 M passes, while every backend, the ones the requests come for,
parked and waited for the postmaster's pass to wake it. `talaria.netmixsteal=<ns>`
(`netsys::slot_by_traffic`): a mixed-set waiter whose last `epoll_wait`/`poll` returned
events less than `ns` ago takes the slot (one CAS) from a holder whose last events are
older; the old holder sees the slot is not its own at its next pass and parks like any
non-polling waiter. Two waiters with traffic never trade it. One stand, us-west-2,
`kernprobe-a64-postgresql-20261025-194000`, phase A = PostgreSQL's manifest (`netmix=1ms`),
P3 = the knob off again (the drift control):

| point | A | steal 1 ms | steal 200 us | off again | Linux |
|---|---|---|---|---|---|
| select1 ceiling | 36.0 k | 58.0 k | 57.8 k | 33.8 k | 44.3 k |
| pk ceiling | 18.4 k | 21.2 k | 21.0 k | 18.4 k | 24.6 k |
| select1@32.4k p50 / p99 | 66.0 / 507 | 53.9 / 78 | 54.7 / 80 | 65.7 / 537 | |
| pk@16.6k p50 / p99 | 121 / 921 | 128 / 307 | 134 / 267 | 120 / 820 | |
| pk@9.2k p99.9 | 795 | 262 | — | — | |

The low rungs move by −4..−10 % of the median or stay; the control repeats A within
0.89-1.07. The manifest takes `netmixsteal=1000000`. With it PostgreSQL's select1 ceiling is
past Linux's; pk's is 21.2 against 24.6 k — what is left of item 3. The gate is `mixwait`'s
second phase (a holder with no events, a waiter fed every 5 ms takes the slot, the old
holder's pipe still wakes it at once) on both machines.

### 10.19. PostgreSQL's pk ceiling: what it is not, and a profile that lied (2026-10-26)

After §10.18 pk's ceiling was 21.2 k against Linux's 24.6 k. Six stands in us-west-2
(`kernprobe-a64-postgresql-2026102{5,6}-*`), each phase A on both sides and then one phase
per hypothesis on the same rise; the ceilings below are Talaria's pk unless said.

**The double TLB invalidation (fixed, 4da5e4c).** On arm64 `flush_range` is already the
broadcast `TLBI VAAE1IS`; munmap/mprotect/madvise/relazy/unmap_ident then shot the same
range down a second time with IPIs. `paging::flush_range_all` does one or the other: the
per-request munmap 3.3 → 2.0 us, the stat block's acknowledgements 899 → 0 ns. KVM
c7g.metal gate (`infra/aws/logs/aarch64-20261025-230038`): 394/0, the databases 15/0 twice,
soak 13×50 and the races 6×96 without a failure. The ceiling did not move with it.

**Rejected by measurement** (each against phase A of its own rise):

| knob | pk ceiling | A | Linux |
|---|---|---|---|
| `netmix` 20 ms / 100 ms (`-222113`) | = the control | | |
| `netirq=1 netpark=1ms` / `netyield=2` (`-232442`) | ~21.9 k | | 25.3 k |
| `netirq=1 netpark=1ms` / `netirq=1` / stat only (`-012354`) | 21.2 / 21.9 / 21.0 k | | |
| `wakeidle=1` / `wakepull=1` / `wakepreempt=20us` / both pulls (`-035450`) | 20.2 / 20.7 / 21.9 / 20.9 k | 21.8 k | 25.2 k |

Linux's generator against Talaria's server gives 20.9 k (G1): the gap is the server's.
The card line does what it is for — the ring→pass of the 60-99 us class 19 → 6-8 us, of
the slow class 30 → 13-14, the top rung's p99 495 → 273 us — and the ceiling stays.

**The profile booked the kernel to ring 3.** Syscalls run with the interrupts masked;
a tick that comes meanwhile is taken at the first instruction the mask lets through, the
one after the `svc`. `talaria.prof` put 60 % of PostgreSQL's ring-3 samples on the libc
page of `epoll_pwait`'s `svc`. The stat block now counts a ring-3 sample on the task's last
syscall's return address separately (`sched::PROF_SYSTAIL`): 48-51 % of the "ring 3"
samples, 98 % of them within 5 us of the syscall's exit (`-023124`). The top pk rung with
that taken out (`bench/suite/lib/profdelta.py`, the difference of consecutive blocks):

| ring 3 | the kernel under the mask | the waiter's spin (`yield_window`) | cores asleep | the stat block's own print |
|---|---|---|---|---|
| 28-29 % | 29-30 % (`epoll_pwait` 14, `sendto` 7, `recvfrom` 3, `munmap` 3) | 16 % | 15 % | 10 % |

Ring 3 is ~29 us of a core per request; Linux's `/proc/stat` at 19.6 k gives 37 us of user
time per request (user 33 %, sys 58 %, idle 8 %) — PostgreSQL itself is not slower here.
`talaria.sysprof` per request at the top rung (`-031707`): sendto 5.2 us, munmap 2.0, mmap
1.5, recvfrom 2 × 1.0, lseek 2 × 0.5, one page fault. And a quarter of the cores sleep and
a sixth spin: the ceiling is not the processor's. With the generator's 4 connections it is
the round trip — 4 / 21.8 k = 183 us against Linux's 159 — and the 24 us are not named yet.
The turnaround histogram puts the server's read→write at 70 us on average for the
60-99 us class while the processor work between them is about 35: the next instrument is
the backend's runnable-but-not-running time inside that interval.

### 10.20. The read->write that was a cold TLB: every mmap emptied it (2026-10-26)

§10.19 left 24 us of pk's round trip unnamed. The server's turnaround now splits its
read->write by the reader's own counters (`sched::run_cycles`, `rq_cycles`,
`syscall::SYS_CYC`, the page faults): over the 60-99 us turnarounds — pk's — the backend was
**on a core 67-69 us**, ready off a core 1, asleep 2-3, in syscalls 4, with one page fault
(`kernprobe-a64-postgresql-20261026-080640`, `-084949`). No queue: the time was ring 3's.
The tick profile had said ~29 us because the tick's period follows the network waiters
(`retune_timer`) — its samples are not evenly spaced in time. §10.19's two conclusions drawn
from it — PostgreSQL's ring 3 not slower than Linux's, the ceiling not the processor's — are
withdrawn.

`talaria.pmuel0=1` counts the cycles and events at EL0 alone, `talaria.pmuev=<event>` puts
another event in the LLC-miss slot (arm64; both arrive with user-data, each core applies
them at its next sample). pk per request in ring 3 (`-093454`): 100 k instructions, 160 k
cycles — IPC 0.6 — with ~800 L2-TLB refills (0x2D) and ~930 L1I-TLB refills (0x02). A
count of the full TLB flushes by the caller's line in `paging.rs` named it: 2.2 M in a run,
all from `map_lazy_pages`, one per query. An mmap reservation writes non-present entries and
ended with an unconditional `flush_tlb`, which on arm64 with ASIDs (§10.17) is the broadcast
`tlbi vmalle1is` — every core walked its whole working set again after each query's mmap.
A TLB holds no non-present entry and `unmap_proc` has already flushed whatever the range
held, so the reservation now flushes only when it overwrites a present entry (a block
`pt_for` splits, a directory entry it replaces).

One rise, us-west-2 (`-111215`; this rise ran Linux faster than the earlier ones —
select1 50.0 k against 42.7-44.3):

| | before (`-093454`) | after | Linux (same rise) |
|---|---|---|---|
| full TLB flushes in a run | 2.2 M | 443 | |
| pk, ring 3 per request: cycles / instructions / L2-TLB refills | 160 k / 100 k / ~800 | 120 k / 100 k / 117-230 | |
| pk read->write on a core (60-99 us class) | 67-69 us | 56 us | |
| pk ceiling | 20.7 k (Linux 25.2) | 25.6 k / 26.2 k (A / P1) | 26.8 k |
| pk p50 at the low rungs | 117-122 us | 106-113 | |

select1 runs 34.6 k instructions per request in ring 3 at IPC 1.0 and kept its ceiling
(56.9 k against Linux's 50.0 k). Gates: arm64 TCG 413/0, x86 573/0, host 22/0; KVM c7g.metal
(`infra/aws/logs/aarch64-20261026-115259`): 394/0, the databases 15/0 twice, soak 13×50 and
the races 6×96 without a failure, the loop 0/1.

### 10.21. The PostgreSQL board after the TLB fix: both ceilings ours (2026-10-26)

The whole PostgreSQL board on `731883c` (kernel `17a6a5f2`, the image of the KVM gate in
§10.20), us-west-2, three repeats a point, the Talaria generator and then the Linux
generator over the same ladder on the same rise (`ec2all-a64-20261026-145401`). The ceilings,
req/s, the median of three probes:

| case | Talaria server, Talaria generator | Talaria server, Linux generator | Linux server, Linux generator | on 2026-10-23 (`-070520`), Talaria / Linux |
|---|---:|---:|---:|---:|
| pk | 25 546 | 25 386 | 24 903 | 9.9 k / 27.3 k |
| select1 | 54 963 | 55 040 | 42 963 | 24.6 k / 46.7 k |

The points (faster / parity / slower), Talaria generator, then the Linux generator:

| | p50 | p99 | p99.9 | p99.99 |
|---|---|---|---|---|
| 2026-10-23, 31 points | 0 / 2 / 6 | 1 / 0 / 7 | 1 / 0 / 7 | 1 / 0 / 6 |
| now, 40 points | 2 / 4 / 4 | 4 / 1 / 5 | 6 / 0 / 4 | 7 / 2 / 1 |
| now, Linux generator | 1 / 4 / 5 | 4 / 1 / 5 | 6 / 0 / 4 | 7 / 1 / 2 |

The two generators agree point for point, so what is left is the server's. It is two classes:

- **The top two rungs' median** (pk 18 677 and 22 412: 114-121 against 80-93 us; select1
  32 222 and 38 666: 57-60 against 46). Linux's median falls as the rate rises while its tail
  explodes — at pk 22 412 its p99 is 4.4-5.5 ms with 50 946 requests over 1 ms (ours 467 us,
  1 957) — its cores stop sleeping and the requests that are served at once are served
  without a wakeup. Ours stays flat (pk 110-121 us at every rung) — whether that is a wakeup
  ours pays at every rate is not measured. The same rungs' p99-p99.99 are ours by 1.7-70x, except select1 38 666's p99
  (104 against 85 us; Linux's p99.9 there is 10 ms).
- **The low rungs' p99-p99.9** (pk 2 490: p99 151 against 135, p99.9 170 against 147;
  select1 4 296 and 10 740: p99 81-84 against 73-74, p99.9 97 against 83-86) at an equal
  median (pk 121 against 117, select1 64-66 against 62-65). A request that arrives at an
  idle server pays for waking it, and our tenth slowest per thousand pays ~15 us more (a
  suspect, not a measurement).


**The low rungs, followed** (`kernprobe-a64-postgresql-20261026-171413`, `-180520`, `-191127`).
The first suspect — under `netmixsteal`'s 1 ms a connection's requests come 1.6 ms apart at
2.5 k req/s, so no backend counts as having traffic and every request wakes a parked one — is
dead: `talaria.netmixsteal=5000000` moved pk's lowest rung by as much as a phase with nothing
changed (`=`: p50/p99 124.7/154.2 → 117.1/139.3; the knob 115.9/143.9). Five boots of one kernel
on one rise agree to a microsecond (pk 2 428: p50 117.9-119.0, p99 141.8-143.5) — a boot is not
the variable, the rise is. `talaria.netmix=0` (the backends spin instead of parking) makes every
rung worse, the low ones included (select1 4 266: p99 79 → 91 us); `talaria.idlepoll=50000` is
the control's twin. Across the three rises the low points change hands: select1 10 666 is ours
on `-191127` (73.9 against 79.7), pk 6 304 is Linux's there (138 against 125) and parity on
`-180520`; select1's lowest rung is the steady one (p99 79-84 against 74-75, p50 64-65 against
62). What remains below 6 k req/s is the size of the rise-to-rise spread, and the two waits the
kernel can change — the park and the idle core's sleep — are not it.


### 10.22. Linux's side counted: its text is 64 KB TLB entries, ours was 4 KB (2026-10-26/27)

§10.21 left select1's lowest rung to an instrument on Linux's side. The harness had one and it
measured nothing: statd ran `perf stat -p <postmaster>` — PostgreSQL's backends are its
children, so the line read 21 k instructions a second at IPC 0.26 — and only before and after a
point, on an idle server. statd now adds `perf stat -a` in sets of three (a `c7g` guest has two
event counters and the cycle counter) at EL0 and EL1 apart; `run-remote.sh DURING=1` takes a
snapshot in the middle of each point, `lib/pmureq.py` divides it by the rung's rate, and
`lib/talpmu.py` gives ours from the stat blocks (`kernprobe-a64-postgresql-20261026-213154`).

Per request in ring 3 at the same instruction count (select1 34.6 k, pk 100 k), Linux took
42.8 k / 123 k cycles at the lowest rungs and **11 / 67 L2-TLB refills**; ours 47.6 k / 139 k
cycles and ~100 / ~233. Its kernel side is not comparable — `net.core.busy_poll=50` on the
stand spins 115-270 k cycles a request in EL1 — but ring 3 is the same program. The difference
was in the map: `/proc/<pid>/pagemap` + `kpageflags` on the Linux server show the postgres
text, ld-musl and libicu in large page-cache folios (75 heads, 1 105 tails over 9 MB), which
Linux 6.18 maps with the Contiguous bit — 64 KB a TLB entry. Anonymous THP is off on both
sides, the shared buffers are 4 KB pages on both.

The kernel now does the same (`a13d069`, `ac80bba`, on by default since `c5aade8`,
`talaria.cont=0` — off): the ELF loader takes a read-only segment's pieces 64 KB-aligned and
folds every whole group of sixteen; a read-only file `mmap` (a `.so`'s text) gets aligned
frames and is folded after its content; fork copies the groups whole. A write that changes a
folded slot unfolds the whole group first (`cont_guard`; the software bits — fork's COW mark —
keep it), and a TLB conflict abort in between is a local flush and a retry. On one rise
(`-230304`, us-west-2), two phases without the hint and three with it, each pair agreeing:

| ring 3 per request | without | with | Linux |
|---|---|---|---|
| select1 4 350: cycles / L2-TLB refills | 46.8 k / 97-101 | 38.8 k / 9 | 42.8 k / 11 |
| select1 39 156 | 35.4 k / 50 | 31.3 k / 6 | 18.1 k / 1 |
| pk 2 501 | 146 k / 233 | 128 k / 38 | 123 k / 67 |
| pk 12 509 | 125.5 k / 176 | 105.5 k / 27 | 129 k / 37 |

| latency, us (p50 / p99) | Linux (phase A) | without | with |
|---|---|---|---|
| select1 4 350 | 62.7 / 72.3 | 63.2 / 78.1 | 58.8 / 74.4 |
| select1 10 876 | 60.8 / 73.1 | 61.2 / 76.5 | 57.0 / 73.7 |
| select1 39 154 | 43.7 / 81.8 | 60.0 / 111.2 | 56.3 / 89.1 |
| pk 2 501 | 115.3 / 131.5 | 114.2 / 140.0 | 107.7 / 134.0 |
| pk 6 254 | 116.7 / 131.8 | 107.1 / 136.1 | 99.1 / 122.8 |
| pk 22 515 | 78.9 / 3 338 | 119.0 / 460.8 | 110.9 / 323.1 |

Every median moved 5-8 %, every p99 down or level; select1's steady loss of §10.21 (p99 79-84
against 74-75) is 74.4 against 72.3 — parity by the board's 1.05 on this rise. A second rise
with the hint by default (`-001538`: phase A and P1 with it, P2/P3 `talaria.cont=0`) moved the
same way (select1 4 503 p50/p99 60.8-62.6/76.0-77.2 against 64.9-65.2/80.9-81.5 without; pk
2 528 109.9-111.5/131.0-132.6 against 118.0-119.4/142.7-143.5), but its Linux was faster at the
bottom (select1 4 503: 61.4/70.3) — select1's lowest p99 is Linux's there by 8-10 % while our
ring 3 is 38.4 k cycles against its 42.8 k: what is left of that point is not ring 3. The top
rungs' medians stay Linux's (its cores stop sleeping there), and so does select1's top-rung
ring 3 (IPC 1.1 against 1.9).

The stat block names the writes that unfold a group: of 422 in a run, 414 were a new address
space's or a split's writes into a FRESH table frame, whose old bytes the guard read as
Contiguous entries — they are written with `pte_w_raw` now; 8 were real (ld-musl's MAP_FIXED
over its folded reservation of a library). The KVM `c7g.metal` gate on `c5aade8`, the hint on
(`infra/aws/logs/aarch64-20261027-001522`): 397/0, the databases 15/0 twice, the soak 13 modes
x 50 boots and the race detector 6 x 96 with no failure, the loop 0 of 1. Gates: `conttext` (256 KB of counted increments run, forked, COW-rewritten and restored
under two runner cores) and `dyn` with and without the knob; arm64 TCG 418/0, x86 573/0. Under
QEMU 8.2's multi-threaded TCG `conttext` kept the old code of the rewritten page on every vCPU
in ~1 of 7 boots without the hint too (0 of 15 with one thread): the gate runs it with one.

### 10.23. A parked waiter driving the stack does not pay; four cores against four (2026-10-27)

**A parked waiter driving the stack (`talaria.parkpoll`, not kept).** The stat block of
`kernprobe-a64-postgresql-20261027-001538` split select1's lowest rung (4.5 k req/s) by
delivery: a quarter of the requests reached the backend that holds the polling slot (read in
under 5 us); three quarters reached a backend parked on the other core, woken by its doorbell
(5-10 us); and 3.4 % of the frames waited 10-50 us in the ring. That last band is the p99. The
holder had left for ring 3 with its own request, and nobody drove the stack while the other core
slept. With the knob, the leaving holder woke one parked waiter on an idle core, and that waiter
polled until the holder came back (the slot stayed with the holder). It worked as built: 5.4 M
drives, 51 s of polling a run, 1.36 M frames picked up. The A/B
(`kernprobe-a64-postgresql-20261027-075723`, phases 1/0/1/0) found no effect above the drift
between phases: the pair 1/0 said it helps and the pair 1/0 after it said the opposite, and the
best phase of the four (select1 4.5 k p50/p99 54.8/69.3 us against Linux's 60.3/71.0) had the
knob off. The code is reverted.

**`talaria.smp=4` for PostgreSQL, and what it really compared.** The passport's `SMP=2` dates from
session 30 on x86 (the comment above `WL_SMP` in `bench/suite/infra/stand-up.sh`). On the arm64
stand (`-20261027-153147`, phases 4/2/4/2, the two 4s within a microsecond of each other), four
cores took every pk point from Linux (the top rung 22.2 k: p50/p99 72.6/113 us against
77.4/4 473) and select1's middle and tails. Then the stand's playbook was read: the Linux server
runs PostgreSQL under `CPUAffinity=2-3`, with the NIC's interrupts on 0-1. Four of our cores
against Linux's two for the backends is not the board's comparison, so the stand got
`LINUX_SERVER_CORES` (the Linux server on the cores named; recorded as
`STAND_LINUX_SERVER_CORES` in stand.env). With `LINUX_SERVER_CORES=0-3` and `TF_VAR_talaria_smp=4`
(`-20261027-172637`), Linux's ceilings doubled. select1 went from 43 k to 91 k req/s and pk from
25.7 k to 50.6 k; ours are 75.7 k and 48.5 k on four cores:

| case, rung | Linux 0-3 p50/p99 | Talaria smp=4 (two phases) | Talaria smp=2 |
|---|---|---|---|
| pk 4.9 k | 115.3/133.4 | 100.7/119.1, 105.3/129.1 | 104.3/128.8 |
| pk 12.1 k | 92.6/143.4 | 70.5/102.7, 73.9/110.4 | 101.8/129.5 |
| pk 24.3 k | 82.7/107.8 | 68.7/107.9, 75.8/119.4 | 121.7/493.8 |
| pk 43.7 k | 76.2/105.9 | 68.7/139.0, 74.9/145.9 | 138.5/454.7 |
| select1 7.6 k | 55.0/70.6 | 53.8/65.8, 58.5/73.8 | 58.7/71.2 |
| select1 18.9 k | 46.2/79.4 | 45.7/59.6, 51.0/69.2 | 57.4/71.3 |
| select1 37.8 k | 45.7/100.0 | 43.4/57.9, 47.5/64.0 | 55.1/74.0 |
| select1 68.1 k | 41.0/63.4 | 48.0/63.6, 51.5/70.2 | 60.0/152.8 |

So the low and middle rungs stay ours on a whole machine against a whole machine, and the top
of both ladders is Linux's: select1's ceiling is 20 % higher there, and its top-rung median is
17-25 % lower. The earlier "the top rungs' medians are Linux's because its cores stop sleeping"
was, in part, two of our cores against Linux's two plus two for its interrupts. The owner chose
the whole instance for both: the PostgreSQL passport now carries `SMP_ARM64=4` and
`LINUX_CORES_ARM64=0-3` (the Graviton stand only; x86 keeps `SMP=2` until its board is re-taken),
and the work it points at is the four-core ceiling (75.7 k against 91 k).

The stand also learned two things on the way. `stand-up.sh` names the Linux hosts' interpreter
(`/usr/bin/python3`): discovery on a host still booting answered "Platform unknown" and took
`/usr/bin/python`, and a stand died after three attempts. And a bring-up that loses the
network halfway leaves instances behind (terraform's `data "http" "myip"` cannot be read, so
the destroy fails). `stand-down.sh` run by hand afterwards cleared them, but check the regions
after every failed rise.

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
