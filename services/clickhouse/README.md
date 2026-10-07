# ClickHouse on Talaria

**The official ClickHouse 25.8.33.6 LTS binary (not rebuilt) executes queries
on Talaria.** Verified with `clickhouse local`: configuration parsing, the
planner, the vectorised engine, aggregation and GROUP BY over millions of rows,
and MergeTree tables on disk.

```
$ SMP=2 ./run.sh
25.8.33.6	2
10000000	49999995000000	4999999.5
0	285715
1	285715
...
4773166019248396768
[syscall] exit_group(the code=0) — the process has finished
```

## The main obstacle and how it was removed

The official `clickhouse` (package `clickhouse-common-static`) is **an ET_EXEC,
i.e. a NON-RELOCATABLE image** (not PIE), and it is dynamically linked against glibc:

```
LOAD 0x000000 vaddr 0x00200000 memsz 0x0768b2a4 R      (headers, .rodata)
LOAD 0x768b2c0 vaddr 0x0788c2c0 memsz 0x18c014e0 R E   (.text — 415 MB)
LOAD ...       vaddr 0x2048e7a0 ...                    (.data/.bss)
INTERP /lib64/ld-linux-x86-64.so.2
```

Its segments must land exactly at **0x200000..0x20e8b1e8 (2 MB … 526 MB)** —
exactly where the Talaria image is LOADED (2 MB, ~60 MB). The binary cannot be
moved (an ET_EXEC has no relocations). It used to need a kernel with a high link
base (528 MB, `build-kernel.sh`); since 2026-10-07 the kernel's image runs at its
high alias (x86 −2 GiB) and a process space under `talaria.dmap=strict` (the
default) keeps none of its identity, so the normal kernel runs it
(docs/ARM64-PLAN.md §10.16 stage 7). The identity modes (`talaria.cell=identity`,
`run_disk`) still share the kernel's space and cannot.

## How to build and run

```sh
cd services/clickhouse
curl -sL -o build/ch-common.tgz \
  https://github.com/ClickHouse/ClickHouse/releases/download/v25.8.33.6-lts/clickhouse-common-static-25.8.33.6-amd64.tgz
tar -C build -xzf build/ch-common.tgz
./mkdisk.sh 1500M                 # the disk: binary + glibc runtime (Ubuntu 22.04) + config
SMP=2 MEM=3072 ./run.sh 300
./run-linux.sh                    # the reference: the same binary on Linux, as many cores
```

Queries are given with `CH_QUERY="SELECT ..."` or `CH_QUERIES_FILE=file`, the
arguments with `CH_ARGS`. By default a set runs that exercises the ENGINE, not
the start-up: aggregation over 10 M rows, GROUP BY over 2 M, arithmetic over 5 M,
and a MergeTree table.

## What it needed from the kernel

| What | Why it was ClickHouse that caught it |
|---|---|
| **A movable kernel link base** | otherwise there is simply nowhere to put a non-relocatable image |
| **`fcntl(F_GETPIPE_SZ/F_SETPIPE_SZ)` returns the SIZE** | ClickHouse grows a pipe with `for (; size < wanted; size *= 2)`; a zero answer gave an endless loop — the workload did not move at all |
| **An empty path = ENOENT** (`stat("")`, `access("")`, `open("")`) | ClickHouse writes `fs::exists(path.value_or(""))` and on "the file exists" goes to open the empty string — Poco kills the process with an assert. Here an empty path used to resolve to the cwd and "exist" |
| **`capget`** | without it ClickHouse prints an error on EVERY query (it checks CAP_NET_ADMIN for task statistics); an honest "no capabilities" sends it down the normal path silently |
| **Task ceiling 64 → 192** | the thread pool hit the kernel's limit and the process aborted |
| **Symbolic links in ext2** (`talaria.test=symlink`) | the image is built as is; the kernel follows `/lib64/ld-linux-x86-64.so.2 -> /lib/x86_64-linux-gnu/ld-linux-x86-64.so.2` itself |
| Plus everything fixed for the JVM (see `services/jvm/README.md`): reuse of a dead task's stack, per-task rsp substitution for signals, XSAVE, `FUTEX_WAKE` skipping an already woken waiter | without these ClickHouse crashed with `#GP` in the kernel after a few seconds |

## How to re-measure: `./compare.sh [runs]`

One query set (`bench.sql`, twelve test cases: aggregations, GROUP BY on a
small and a large dictionary, arithmetic, uniqExact, strings, sorting,
MergeTree — DDL, insert, read, filter), one machine, the same number of cores
and the same `max_threads`. Every query is repeated three times in the file,
there are several runs, and the MEDIAN is taken: the spread between single
measurements reaches 2× on BOTH sides, so a single number proves nothing.

A caveat about the config: Talaria runs with trimmed thread pools (the kernel
has 192 tasks per machine), Linux with the defaults. Our config does not run on
Linux at all: the MergeTree part loader gets no threads left and hangs
(checked). The queries run with the same `max_threads=4` on both sides.

## Numbers: Talaria against Linux (the same binary, 4 cores, the same queries)

Protocol: every query is repeated five times in the file, the run three times,
and the MEDIAN of fifteen measurements is taken (`--time`, logs off with
`--send_logs_level=none` — otherwise the speed of the console is measured: a
serial write costs 1.7 ms and adds up to 0.9 s per run).

The full set, both sides in THE SAME kind of QEMU/KVM, 4 cores, median 5×3
(`./compare.sh 5`). A value below one in the ratio columns is ours. The two
ratio columns are two independent runs in a row: that shows the real
precision of the measurement, not one pretty number.

| test case | Talaria, ms | Linux VM, ms | ratio | (repeat) |
|---|---|---|---|---|
| aggregation count+sum, 10 M | 11 | 12 | **0.92** | 1.00 |
| GROUP BY 7 keys, 2 M | 8.5 | 9 | **0.94** | 1.00 |
| GROUP BY 100 k keys, 2 M | 42 | 53 | **0.79** | **0.96** |
| arithmetic sum(x*x), 5 M | 12 | 13 | **0.92** | **0.92** |
| uniqExact 500 k, 2 M | 27.5 | 37 | **0.74** | **0.80** |
| strings concat+length, 1 M | 26 | 27 | **0.96** | 1.00 |
| sorting ORDER BY hash, 2 M | 12 | 12 | 1.00 | **0.92** |
| DROP/CREATE MergeTree | 91.5 | 89 | 1.03 | 1.03 |
| INSERT 500 k into MergeTree | 38 | 69 | **0.55** | **0.60** |
| reading the table count+sum | 7 | 6 | 1.17 | 1.33 |
| GROUP BY on the table | 4 | 5 | **0.80** | 1.25 |
| LIKE filter on the table | 9 | 8 | 1.13 | 1.25 |
| start of `clickhouse local` to the first query | ~1000 (4500 before batched image loading) | ~400 | | |

**Six to eight test cases are ours, one to three tie, three are theirs**; the
set total is 288 against 400. All three remaining losses are within a third
(1.03, 1.17-1.33, 1.13-1.25).

**The set does not see the main thing.** Its table (500 k rows) fits into the
block cache entirely, so it exercises the device read path only idly. On a
table of 10 M rows that does NOT fit into the cache: a four-thread scan went
**0.5 s → 0.09 s**, the insert 3.2 → 0.7-1.4 s, and four threads became FASTER
than one (0.09 vs 0.24) — the old pathology "four executor threads are slower
than one" is closed. The lever was the block cache size: a build constant (8 MB
on any machine) became a share of free memory (`talaria.fscache=<MB>`).

The remainder on the filter is known to be bounded by the wait for a CPU, not
by computation: single-threaded the filter takes 13 ms here against 14 on Linux
— we are faster; with four threads and a 500 µs slice 9-10, with 150 µs 8. So it
hit the length of the quantum. Preemption on wakeup with a 150 µs threshold took
most of the gap; the rest is there.

### Why this table is fairer than the previous one

Four test cases of twelve used to compare not systems but STORAGE: in the Linux
guest `/var/lib/clickhouse` was a tmpfs, i.e. the table lived in RAM and `fsync`
did nothing there, while Talaria wrote to virtio-blk with real device barriers.
The bias went BOTH ways: our DDL looked like a failure (94 ms vs 1.5) and our
insert like a bigger win than it was. Now the guest has its own virtio-blk with
ext4 and a journal (our ext2 has one; comparing a journalled file system with a
non-journalled one would swap one unfairness for another), and Linux's DDL is
81-87 ms instead of 1.5 — exactly our order.

The second bias was ours: the Talaria image is persistent, and the table
accumulated parts between runs. Now `bench.sql` ends with `DROP TABLE`, and
both sides start a run with an empty table.

What closed the gap — and all of it is about who HOLDS a core, not who eats cycles:
0. above all, not performance but correctness: the buffer of the normalised path
   was ONE PER MACHINE, and two cores opening files at once operated on each
   other's paths. On a 10 M-row table ClickHouse got "Cannot open file" and
   "Cannot read all data" in about half the runs;
1. the scheduler slice 8 ms → 0.5 ms (sweep: 308 / 246 / **216** / 227 ms per set
   at 8 / 2 / 0.5 / 0.2 ms);
2. a polling syscall sat in `hlt` without releasing the core — one watchdog
   thread on an eventfd held a quarter of the machine;
3. no stealing of a cache-hot task;
4. a timed wait on a secondary core quietly cancelled preemption there: giving
   back the borrowed LVT timer restarted the slice from zero, and a core sleeping
   on time more often than once per slice never reached its tick;
5. `sched_yield` on the control plane entered the scheduler unconditionally —
   5.8 µs where Linux takes a hundred nanoseconds, 4282 calls per run;
6. `mmap` sent the neighbours a TLB flush that was not needed (157 µs per call on
   four cores against 69 on one).

### The scaling ladder: `./scaling.sh [runs] ["param|param"]`

**An instrument for ONE question — parallel reading of a table.** The set
(`compare.sh`, `sweep.sh`) does not suit it, and two attempts in a row failed
exactly here: the change was smaller than the noise of the set. Three reasons at
once — the set measures ABSOLUTE milliseconds (which on this machine differ
between runs by up to 2×), its table lives ON DISK (the read path, the block
cache and a persistent image with the `NOT_ENOUGH_SPACE` trap enter the
measurement), and it runs ONE configuration — four cores — while the disease is
exactly in the DEPENDENCE on their number.

What this one measures: one and the same query over a table **in memory**
(`ENGINE = Memory` — no files, no decompression, no marks) on a ladder of 1..4
cores, with equal numbers of cores and threads. The main number is not
milliseconds but the **speedup within one side** (T1/TN): it does not depend on
whether the machine is fast or slow today, and so it is comparable between
sessions. The spread is next to it, and a point where the difference of the
sides is smaller than the sum of the spreads is NOT JUDGED.

The MECHANISM is printed next to the outcome (cycles per core, migrations on
wakeup, large pages): a gate on a ratio of two numbers cannot tell "the change
did not help" from "the change did not engage".

```sh
./scaling.sh 3                                  # both sides, three runs per step
SIDES=talaria ./scaling.sh 3 "|talaria.wakebal=2|talaria.wakebal=3"   # variants against each other
CORES="1 4" REP=20 ./scaling.sh 5               # only the ends of the ladder, more measurements
```

The control in the same run is pure multi-threaded counting with `numbers_mt`:
no files and almost no memory. It separates "the scheduler does not scale" from
"table reading does not scale".

### A sweep over a kernel parameter: `./sweep.sh "<param>|<param>|…" [runs]`

The same set and the same median protocol, but instead of Linux a second
Talaria configuration. An empty variant means the defaults:

```
./sweep.sh "talaria.slice=8000000|talaria.slice=500000" 3   # the scheduler slice
./sweep.sh "|talaria.huge=2" 5                              # large pages
./sweep.sh "|talaria.nosteal=1" 4                           # work stealing
```

The large-page counters are printed next to the numbers: "no gain" and "never
tried" must look different.

**Parallelism over cores is fixed.** The measurement it started from: a query
with four threads ran THREE TIMES slower than with one. Now four cores against
one: `sum(x*x)` over 5 M rows 304 vs 125 M rows/s, `GROUP BY` over 2 M 111 vs
87. Three things fixed it: an honest core count in `/proc/cpuinfo`, a resched
IPI for a woken thread, and work stealing by an idle core (plus placing a new
thread on the least loaded core).

### How the gap was hunted (and what turned out to be false)

First two instruments, because testing hypotheses costs evenings and measuring
costs an hour:

- `kernel/user/mbench.c` (gate `talaria.test=mbench`) — hot-loop primitives in
  ONE AND THE SAME static binary on Talaria and on Linux. The answer came at
  once: ALU 1.100 vs 1.094 ns/step, memory 11.5 vs 11.4 GB/s, L1/L2/L3 sums
  within noise, an empty syscall 101 vs 405 ns IN OUR FAVOUR. A TLB miss
  (pointer chasing over 64 MB) 79.5 vs 66 ns — the price of the nested EPT
  tables, but 20 %, not multiples. **The gap is neither in the hardware nor in
  computation.**
- the syscall time histogram in the kernel (`talaria.sysprof=1`). It named the
  culprits: `mmap` cost 1.69 ms PER CALL, `mprotect` 2.02, `munmap` 1.74,
  `madvise` 0.81. The kernel walked a range page by page with a full descent
  through four table levels for each. After the fixes (one descent per 2 MB, a
  reservation as one directory entry, a targeted `invlpg` instead of a full TLB
  flush on every core, a cheap `munmap` refusal before the file-system lock):
  0.16 / 0.026 / 0.056 / 0.041 ms, syscalls per run 51,000 → 5,500.

Checked and **rejected** by measurement:

- **the instruction set** — the host (Sandy Bridge) has no AVX2 at all, both
  sides take the same path;
- **large pages** — the hypothesis: memory is mapped in 4 KB pages, and under a
  hypervisor a TLB miss costs double. The counters showed that ZERO large pages
  were handed out per run: the ">= 32 MB per mapping" threshold killed them,
  because the runtime reserves address space in a big chunk and commits memory
  in small ones. The threshold was lowered, the pages came — the times did not
  change. The parameter `talaria.huge=<MB>` stayed, off by default (later the
  manifest of this image asks for them itself, see `mkdisk.sh`: uniqExact
  253 → 197 ms);
- **work stealing** — with `talaria.nosteal=1` the queries run TWICE as slow,
  so stealing helps rather than hurts.

## Boundaries

- **As many threads as cores is the most fragile case, and it has been analysed
  to the bottom.** Symptom: `max_threads=4` on four cores gave 83, 110 or 163 ms
  from boot to boot, while `max_threads=6` and `8` gave a stable 86 against
  Linux's 84. So neither the work nor the hardware was to blame. Three
  instruments gave the diagnosis without a single hypothesis: the hardware
  counters showed the WORK is equal (0.335 core-seconds per query here against
  0.325 on Linux — which reports it itself, `UserTimeMicroseconds`), but with
  three cores under it instead of almost four; `talaria.runhist=1` showed FOUR
  ready tasks in 87 % of the ticks — so the problem was placement, not wakeup;
  the steal counters named the culprit: 745 "cache-hot" refusals for 40
  successful steals. Two ready threads on one core alternate every slice and
  NEVER pass the cool-down threshold. The fix — `talaria.stealbal=3` (default),
  the gate — `talaria.test=stealbal`.
- `clickhouse local` and **MergeTree** are verified: CREATE TABLE, INSERT of
  200,000 rows, count/sum/uniqExact, GROUP BY, `OPTIMIZE TABLE ... FINAL`, exit 0.
  MergeTree used to hang on "Loading data parts", and that was blamed on the
  kernel. There were two causes: our own `max_thread_pool_size=16` (the part
  loader hands work to the global pool, which is taken by background executors —
  the same config hangs on Linux, checked) and `statfs` returning zeros, which
  made the database consider the volume full. The pool is raised to 32, `statfs`
  returns real numbers.
- ClickHouse's thread pools are trimmed by the config (`mkdisk.sh` writes
  `/config.xml`): by default it creates hundreds of threads, and the kernel has
  192 per machine. But NOT below 32 in the global pool — otherwise the part
  loader gets no thread and everything hangs (see above).
- Server mode (`clickhouse server`, ports 8123/9000) was not checked.
