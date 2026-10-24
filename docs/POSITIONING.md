# Talaria OS — positioning: value, for whom, boundaries

This document answers three questions that are not about the code: **what is
being offered**, **who needs it** and **where the line between proven and
promised lies**. Technical boundaries and limits are in `docs/PRODUCT.md`,
numbers in `docs/MEASUREMENTS.md`, status and version in `README.md`.

The rule of this document is the rule of the measurements: **a claim without a
number or a reference to a gate does not get in here**.

## 1. In one paragraph

Talaria is its own x86-64 kernel (Rust, `no_std`) that runs **unmodified Linux
binaries** — static musl, dynamic ELF through ld-musl, glibc tested — and gives
one application the whole machine: CPU cores, network card, disk. Not a
container, not a distribution, not a hypervisor: a unikernel approach with the
Linux ABI. `mongod`, `postgres`, `mariadbd`, `redis`, `nginx`, JVM 21 and
ClickHouse run as the same binary as on Linux, with no rebuild and no kernel
changes.

The main internal mechanism is **synchronisation without busy-spinning**: a
futex sleeps in the kernel with `mwait` right on the user's word, and the
waker's store itself wakes the sleeper, with no IPI (stock musl's
`FUTEX_WAKE` that follows finds it awake; libtsync skips that syscall too).
Ordinary pthread code gets sub-microsecond wakeups at 0 % CPU while waiting.

## 2. The headline number

A comparison with Linux on six real workloads (PostgreSQL, MySQL/MariaDB,
MongoDB, Redis, nginx, mongoose). The stand: four `c6i.xlarge` in one zone (a
Talaria server and a Linux server, each with its own generator), two cores per
workload on both sides, an open loop, a rate ladder from the shared ceiling,
three repeats per point. The full board: `docs/MEASUREMENTS.md` §2.

| percentile | faster | parity | slower |
|---|---:|---:|---:|
| p50 | 66 | 7 | 1 |
| p99 | 69 | 5 | 0 |
| p99.9 | 65 | 7 | 2 |
| p99.99 | 63 | 2 | 8 |

**295 comparable points, 11 lost — and each one analysed by name and closed**
(five are vCPU time stolen by the hypervisor, i.e. not our code; two are the
scheduler's 140-400 µs band, for which BOTH treatments were written and
rejected by measurement; two are a class that did not reproduce on a second
deployment; two are the judging boundary of 1.05-1.06).

The order of magnitude where the win is large: `postgresql select1` at 36 k
req/s — `p99.9` **72.8 µs against Linux's 11,206**; `mysql pk` at 28.9 k —
`p99.99` **110 µs against 18,792**.

A point does not count if it is not comparable: overload, schedule debt of the
generator, a difference smaller than the stand's own jitter, a handful of
requests behind the percentile. These rules are written in
`bench/suite/README.md` and checked by `bench/suite/selfcheck.sh` (71 checks).

## 3. What people pay for here (proven)

1. **Predictability under load, not "faster on average".** See §2: in the tails
   the gap reaches two orders of magnitude, and this is a real database under an
   open loop, not a microbenchmark.
2. **Zero porting cost.** The difference from Unikraft, MirageOS and other
   unikernels: the application is not rewritten or relinked. The path "binary ->
   image -> VM" takes minutes (`kernel/tools/talaria-pack`, `cell.manifest`),
   and a foreign binary's compatibility is checked in one run
   (`kernel/tools/talaria-check`).
3. **Synchronisation as an asset of its own.** futex ping-pong 165 ns across
   cores, condvar 1.3 µs, wakeup by a store to the word 237 ns, SPSC 140-168
   Mops, fiber switch 11 ns, cross-VM wakeup over ivshmem 496 ns, trading
   pipeline tick-to-trade 620 ns. All at 0 % CPU while waiting.
4. **Observability that unikernels usually lack:** `top`/`ps`/`netstat`/
   `kstat`/`perf` inside the kernel, a sampling profiler with stacks, core dumps
   in ELF `ET_CORE` format that a real `gdb` opens.
5. **Measurement discipline.** Several hundred QEMU regression gates, host unit
   tests, 71 self-checks of the benchmark harness, and the rules "a class exists
   if it reproduced on two deployments" and "write no fix until the instrument
   has named the share with a number". Over 26 sessions this discipline killed
   five of our own "discoveries" that turned out to be instrument defects. It
   cannot be sold, but it is what makes every number above checkable.

## 4. What is not here (and is not planned)

- **Not multi-tenant and not a security perimeter.** The isolation (W^X,
  per-process address spaces, COW, guard pages, syscall pointer validation, the
  syscall allowlist) protects against application bugs, not against hostile
  code. Talaria is placed BEHIND an external perimeter.
- **Not a general-purpose OS:** one trusted workload per VM, no users, sessions
  or zoo of drivers.
- **Not POSIX-complete:** a pragmatic subset of the Linux ABI (~185 syscalls).
- **Not realtime-certified:** no formal WCET guarantees; "low latency" is
  measured behaviour, not a certificate (§7).
- **Interfaces are not stable before 1.0:** `cell.manifest`, the `talaria.*`
  parameters, the Talaria syscalls and the libtsync API may change between
  versions.
- **As-is support:** commercial support by separate agreement.

## 5. Where the connection ceiling comes from (and how to raise it)

This question comes first, so the answer is here and not in the limits.

**64 TCP sockets and 8 UDP sockets per machine** is neither a protocol limit
nor laziness, but the consequence of a deliberate decision: **there is no
dynamic memory allocation on the datapath**. Socket buffers live in `.bss` as
static arrays (`kernel/src/netsys.rs`): 128 KB receive + 64 KB transmit per TCP
slot, so the 64-slot table costs 12 MB, and a UDP slot with packet metadata
another megabyte or so. Three consequences:

- the cost of a connection is known in advance and does not depend on the load —
  the allocator takes no part in receiving a frame at all;
- the ceiling is a compile-time constant, and exceeding it is now REPORTED
  (earlier a listener silently stopped answering SYNs: `warn_no_slots`);
- a listener takes one slot for itself and up to eight for its backlog, so fewer
  are left for accepted connections than it seems.

The history of the ceiling is telling: 16 -> 32 -> 64. Thirty-two were enough
while connections were short; PostgreSQL starts a process per connection, the
table filled up completely in validation, and the client HUNG. Six megabytes of
`.bss` were a price worth paying.

**How to raise it further when needed.** The same way the file-system block
cache already works: not a static array but a pool taken from the frame
allocator at boot and sized by a parameter (for the file system it is
`talaria.fscache=<MB>` — a share of free memory). For sockets that means buffers
from a pool instead of `.bss` and a parameter like `talaria.sockets=<N>`; the
datapath stays allocation-free (memory is taken once at boot). Estimate: hours
of work, not sessions — but nobody has asked for it yet: the target scenarios
live in the "one VM per service" model.

## 6. Who needs it: categories by decreasing fit

| # | Who | Why | What stands in the way today |
|---|---|---|---|
| 1 | **HFT and low-latency trading**: in-house C++/Rust engines, exchange gateways, strategies | tails and jitter are money; tick-to-trade 620 ns in the reference pipeline, sub-µs synchronisation; the application is alone on the machine and behind a firewall anyway | needs a dedicated host and vCPU pinning; on bare metal the network is ENA only; no 24/7 support |
| 2 | **Market data and telemetry**: stream distribution, normalisation, industrial buses | predictable `p99.9` at a constant rate; 1.28 M packets/s over ENA, 682 k pps over virtio | fan-out hits the socket ceiling (§5) — done with VM instances |
| 3 | **Appliance vendors**: a database/cache/proxy "in a box" on the customer's hardware | the same `postgres`/`redis`/`nginx` with better tails and no distribution to maintain (Redis won 80 points of 80, nginx 36 of 36) | the usual operational tooling is missing: packages, monitoring agents, updates, live migration |
| 4 | **Industrial and embedded gateways**: robotics, test benches, protocol converters | a Linux application + determinism without rewriting it for an RTOS | the driver set is narrow by design; no certification (§7) |
| 5 | **Network functions and edge**: packet processing, UPF-like | latency and the simple "the application owns the card" model | competitors are DPDK/VPP on Linux; needs SR-IOV, many queues, mature operations |
| 6 | **OS researchers and teachers, benchmark authors** | a live Rust kernel with an honest regression and a reproducible method of comparing with Linux | brings no money; but the cheapest path to external verification of the numbers and to contributors |
| 7 | **General-purpose web/microservices, multi-tenant, public hosting** | — | **not our case**: no tenant isolation and no ecosystem; do not go there |

## 7. Realtime: what exists, what does not, and what "certify" means

Talaria provides **measured determinism**, not guaranteed determinism. The
difference is fundamental: there are tail numbers on a specific stand and
specific workloads (§2), but no proof of an upper bound on response time for
arbitrary input (WCET) and no development process recognised by a
certification body.

What already works in that direction: static tables instead of dynamic memory
on the datapath (§5), priority of return to the core and bounded preemption
windows, no busy-spinning, counters and instruments that show every stop of a
thread (`talaria.appprof=1`, the "tick gap" with attribution of stolen vCPU
time), and a regression where every feature has a gate.

What certification is missing and in what order it is obtained — see §7.1.
The short answer: certification under IEC 61508 / ISO 26262 / DO-178C is not a
property of the code but a **process and a body of evidence** taking years and
a budget that is a multiple of the development itself; the current target
categories (1-3) do not need it, and for category 4 it is bought together with
a partner who already has such a process.

### 7.1. If certification is needed after all

1. **Choose the standard and the level.** Industry — IEC 61508 (SIL 1-4),
   automotive — ISO 26262 (ASIL A-D), aviation — DO-178C (DAL A-E), medical —
   IEC 62304, railway — EN 50128. The level determines not "code quality" but
   the amount of evidence: from "there are tests" to "there are formal methods
   and an independent audit of every line".
2. **Narrow the object of certification.** Not "the OS in general" is
   certified, but a configuration with a fixed set of functions. For Talaria
   that would be a separate build: no dynamic workloads, a fixed manifest, no
   file system or a read-only one, one NIC driver.
3. **Build the body of evidence:** requirements traced "requirement -> code ->
   test", hazard analysis (HAZOP/FMEA), MC/DC coverage for high levels, **WCET
   analysis** (static path analysis plus measurements on the target hardware),
   configuration management and process documentation. The regression gates
   are a good start, but "the feature works" is not "the requirement is covered
   with traceability".
4. **Qualify the tools.** The compiler for certified software must be qualified
   itself: for Rust that is **Ferrocene** (ISO 26262 ASIL D, IEC 61508 SIL 4) —
   so the Rust path exists and others have walked it.
5. **Budget time and money.** The industry reference: 1.5-3 years and a budget
   of "several person-years plus the certification body's work" for a middle
   level (SIL 2 / ASIL B); high levels cost several times more.
6. **The realistic alternative in 90 % of cases:** not a certificate but
   **certification-ready artefacts** — documented requirements, traceability,
   WCET analysis of the target configuration and regression reports that the
   customer includes in THEIR certification as a purchased component (SEooC in
   ISO 26262 terms). This takes months, not years, and it is exactly what
   integrators pay for.

## 8. Where the value is proven and where it is still a promise

**Proven** (there is a gate, a number or a deployed stand): latency and tails on
six workloads; running unmodified binaries, including dynamic ELF and the JVM;
a file system with a JBD2 journal on live EBS, checked by power cuts; SMP and
ENA on Nitro and on `c6i.metal`; syscall pointer validation with an A/B gate.

**Promised** (the code does not exist or is not finished): bare metal as a
product (drivers beyond ENA/NVMe, PTP and hardware timestamps); production
operations (updates, monitoring, on-call); scaling beyond one VM; any
multi-tenant story; certification (§7).

## 9. Conclusion

This is not "yet another OS" but a narrow tool with one clear advantage —
predictability under load at zero porting cost — and with honestly drawn
boundaries. The shortest road is categories 1-2 (trading, market data): there
the buyer already keeps a dedicated machine behind a perimeter and knows how to
read `p99.9`. Category 3 is the next market by size but needs operational
tooling that does not exist yet. Category 6 is the way to get contributors and
external verification of the numbers, and for it everything is ready today:
Apache-2.0, and the public tree builds from source and passes the regression.
