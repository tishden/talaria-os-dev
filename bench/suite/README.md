# bench/suite — the comparative suite: Talaria against Linux

One set of workloads, one measuring instrument, one result format. The goal is
numbers that can be shown and decided on: **latency at high percentiles
(p99.9, p99.99) and its spread**, not only the average.

The suite's rule: **unsuccessful and neutral results are published on equal terms
with successful ones**. A run where Talaria loses, or where there is no
difference, is as much a result as a win; hiding it makes the other numbers
worthless too.

## 1. What is measured

| Workload | Class | Protocol | What it loads |
|---|---|---|---|
| `redis` | in-memory KV | RESP | network + epoll, single-threaded |
| `nginx` | HTTP server | HTTP/1.1 | network + epoll (`/nop`), network + FS (`/`) |
| `mongoose` | HTTP server (lightweight) | HTTP/1.1 | the same on another implementation |
| `postgresql` | DBMS, a process per connection | PG v3 | fork, signals, shared memory, FS |
| `mysql` (MariaDB) | DBMS, a thread per connection | MySQL v10 | threads, InnoDB, FS |
| `mongodb` | document database, a dynamic ELF | OP_MSG | ld-musl, mmap, WiredTiger |
| `clickhouse` | analytical engine | queries from a file | compute, memory, FS (not the network) |
| `actors` | the actor model (SObjectizer-5) | inside the process | thread wakeup, the scheduler |
| `task` | a transport with FEC/NACK | UDP | the datapath, losses, recovery |

The first six are measured by one generator, `tlgen/`; the other three have their
own harnesses that bring the result to the common format (`lib/schema.md`).

## 2. The instrument: `tlgen`

`tlgen/` is a load generator built **once**, statically against musl, and run
**both on Linux and on Talaria** (linked at `0x10000000`, Talaria's user region;
on Linux it is an ordinary `ET_EXEC`). A difference in the numbers cannot be a
difference between clients, because the client is byte-for-byte the same.

Three things it does deliberately:

1. **An open loop.** The schedule of sends is fixed in advance by the target rate
   and does not depend on whether the server keeps up. Latency is counted from
   THE SCHEDULED MOMENT, not from the moment the request could be put into the
   socket. Otherwise a server's slowdown hides itself: a slow server receives
   fewer requests and the tail of the distribution looks healthy (coordinated
   omission).
2. **Busy waiting, called by its name.** A microsecond-scale schedule cannot be
   held while sleeping in `epoll_wait` with millisecond precision. So the
   `--spin` mode is the main one for latency measurements, and the number of idle
   polls (`recv_empty`) is printed separately from the useful syscalls and is not
   mixed into "syscalls per request".
3. **Overload is announced.** If the server does not hold the requested rate, the
   generator accumulates debt, and that is visible in the report (`overload`,
   `backlog_max`, `rate_achieved_pct`). The percentiles of such a run measure the
   generator's queue rather than the server — and the report says so.

Time comes from `rdtsc` calibrated against `CLOCK_MONOTONIC` at startup (200 ms of
busy waiting). Percentiles come from an HDR-like histogram with a relative error
below 0.1 %; the full histogram goes into the report, so runs can be merged and
percentiles recomputed after the fact.

```
tlgen --proto resp --host 10.0.0.2 --port 6379 --op mixed --read-pct 90 \
      --conns 8 --depth 1 --rate 100000 --duration 60 --warmup 10 --spin \
      --name redis-mixed-100k --stand talaria --out result.json
```

Building it: `make -C bench/suite/tlgen` (needs the docker image `talaria-musl`,
`services/docker/build-images.sh`).

## 3. The measurement discipline

- **The warm-up** (`--warmup`) does not enter the statistics: the first seconds
  are cache filling, page faults and frequency ramp-up.
- **A load ladder.** One run at one rate says nothing: a system with low latency
  at 10 % load may fall apart at 80 %. Every workload is walked as a ladder of
  rates (fractions of the measured ceiling), and the report carries a
  "rate → p99/p99.9" curve rather than a single point.
- **The ceiling** is found by a separate run in a closed loop (`--rate 0`) — that
  is "how much the system can take"; the ladder's steps come from it.
- **Repeats.** Every point is at least three runs; the median goes into the
  report and the spread between runs is printed next to it. A spread above 10 %
  is a reason to look for the cause rather than to average.
- **An identical VM/instance discipline** on both sides: the same number of
  cores, the same memory, the same storage for the data. Storage is a pitfall of
  its own: four ClickHouse test cases once compared tmpfs on Linux with a disk on
  Talaria.

### 3.1. WHAT MAY BE COMPARED WITH WHAT AT ALL

The rule is worth more than any individual fix, and it was bought with a lost
scoreboard.

**Only points taken on ONE stand bring-up may be compared.** Between bring-ups
not only p99 fails to reproduce (that was known — up to 1.6x), but **the median**
as well. A measurement: an A/B on one bring-up gave mysql **10/0/0** against
Linux by median, twice in a row; the final scoreboard on a fresh stand three
hours later gave **2/3/5**, with OUR numbers moved by 20 %
(`pk` p50 53.9-55.4 -> 65.4-88.3) and Linux's by 5 % the other way.

Three consequences follow, and all three are mandatory:

* **A hypothesis is checked by phases of one bring-up** — `bench/suite/ab-phase.sh`
  (the stand is brought up once, phase A is taken with both sides, and after that
  each hypothesis is one phase over ladder A). `run-remote-ab.sh` can do two
  phases and brings up its own stand; for the third and beyond use `ab-phase.sh`
  with `AB_KEEP=1` on the first.
* **A scoreboard judges the score WITHIN a workload** (with the Linux anchor of
  the same hour next to it) and **does not judge** a comparison with a previous
  scoreboard: `run-remote-all.sh` brings the stand up per workload, i.e. four
  workloads are four measurements that are not comparable with one another.
* **"It got better" from comparing two scoreboards is worth nothing.** A pair of
  phases is needed.

### 3.2. THE FIRST LADDER POINT PAYS FOR THE WARM-UP

The ladder goes from a low rate to a high one, so the BOTTOM point is taken FIRST
and carries a cold block cache, a cold database buffer pool and lazy page faults.
In one scoreboard that is visible in three places at once: `mongodb/insert` p99.9
of 3764 µs against 195 on Linux at the bottom rate while being 1.0-1.6 at the top
ones; `mysql/pk` p50 88.3 against 66 at the bottom while 65-70 further on; the
`max` of the first point an order of magnitude above the rest. And the `p99
spread` over three repeats is 1.02-1.07 — the three repeats AGREE, i.e. this is
not an outlier but a state of the machine, the same in every repeat of the first
point.

The discriminator is free: walk the ladder IN REVERSE ORDER (`RATES=` given
explicitly) on the same bring-up. If the "worst point" moved with the order, the
warm-up is to blame; if it stayed at the bottom rate, it is a property of the
rate.

### 3.3. THE GUEST AND THE GENERATOR ON DIFFERENT HOST CORES (the local suite)

A network waiter in Talaria SPINS through the stack, i.e. the guest occupies
exactly `smp` cores regardless of the rate, while `tlgen --spin` occupies one
more. Without a layout they fight each other and the desktop:
`VM_CPUSET=0-2 GEN_CPUSET=4-5` took 25 µs off the local median (107 -> 73). The
tail is still not judged locally — p99 wanders by up to six times — but the
median holds within 10 %.

## 4. Metrics besides latency

They are collected **non-invasively**, from what the system already counts:

| Metric | Linux | Talaria |
|---|---|---|
| latency, rate, errors | `tlgen` (identical) | `tlgen` (identical) |
| syscalls | `perf stat -e raw_syscalls:sys_enter` | `kstat` |
| context switches | `perf stat -e context-switches` | `kstat sched` |
| cache misses, IPC | `perf stat -e cache-misses,instructions,cycles` | `perf` (the console) |
| threads and their placement | `/proc/<pid>/status`, `ps -L` | `top`, `ps`, `kstat sched` |
| network | `ss -i`, `/proc/net/snmp` | `netstat` |

Talaria's tools are described in `docs/PERFORMANCE-OPTIMIZATION-GUIDE.md`; the
console works on top of a running workload, so counter snapshots are taken before
and after the measurement window rather than in the middle of it.

## 5. The stands

| Stand | What it is | What for |
|---|---|---|
| `local` | two QEMU/KVM VMs on one machine | debugging the suite, not numbers for publication |
| `ec2` | two EC2 instances in one cluster placement group | the main numbers |
| `metal` | two `*.metal` instances without a hypervisor | the final numbers |

On `ec2` and `metal` Talaria **is the instance itself** (an AMI, its own ENA
driver), and Linux is an ordinary instance of the same type tuned for low
latency. Deployment is `infra/terraform` + `infra/ansible`, a run is
`run-remote.sh`.

Locally the numbers are noisy by construction (a hypervisor, a shared host, the
generator and the server on the same physical cores) and are good only for
debugging the suite: "the workload came up, the protocol parsed, the report was
assembled".

## 6. The directory layout

```
tlgen/        the load generator (C, musl) — the shared instrument
lib/          the result format, metric collection, reduction, the report
workloads/    one script per workload: bring the server up, walk the ladder, take the metrics
infra/        terraform + ansible for EC2/metal
results/      run results (not committed to git)

run-local.sh      one workload locally (both sides in QEMU)
run-remote.sh     one workload on a stand that is already up
run-remote-all.sh the whole suite: stand -> run -> cleanup PER WORKLOAD
run-remote-ab.sh  TWO phases (A as is, B with a cmdline tail) on one stand
ab-phase.sh       ONE MORE phase on a stand that is already up, over ladder A (see 3.1)
preflight.sh      cut off a breakage before a long run
```

## 7. How to run it

Locally (debugging the suite):

```sh
make -C bench/suite/tlgen                      # the instrument
services/clickhouse/build-kernel.sh            # the high-base kernel (goes stale on ANY kernel change)
bench/suite/run-local.sh nginx both            # one workload, both stands
bench/suite/run-all.sh                         # six tlgen workloads
bench/suite/run-everything.sh 3                # EVERYTHING: tlgen + five harnesses + the summary, 3 repeats of the noisy ones
```

Survivability (not performance, but the same stand): `bench/suite/powercut.sh mysql 180` —
a fresh image, a warm-up, load, 180 s of idling, QEMU killed without warning, a
second boot of the same image; the probe passes if the database listens on the
port again. That is the scenario of an EC2 instance stop without a handled power
button (see `talaria.pwrbtn` in the guide) — which is exactly how mysql on EC2
once ended up without an InnoDB checkpoint.

**Repeats are not a luxury.** The actor, ClickHouse and JVM harnesses are noisier
between runs than the difference being looked for in them: three runs of the
actor matrix in a row on ONE AND THE SAME kernel gave 38/17/14, 41/12/16 and
34/19/16, and the JVM total changed from 9/7/9 to 7/13/5 without a single kernel
change. They are judged by concrete points and by no fewer than three runs. In
ClickHouse the time is additionally quantised BY THE MILLISECOND: a ratio of 1.09
there may mean 12 ms against 11, i.e. one measurement quantum rather than a
result.

On EC2 (the numbers for publication):

```sh
export AWS_DEFAULT_REGION=ap-northeast-1
bench/suite/infra/stand-up.sh nginx            # AMI + terraform + ansible
bench/suite/run-remote.sh nginx                # the run
bench/suite/infra/stand-down.sh                # TEAR IT DOWN (the stand costs money)
bench/suite/run-remote-all.sh                  # THE WHOLE suite: stand -> run -> cleanup per workload, one summary
```

A hypothesis is checked NOT with a separate stand but with phases of one bring-up
(see 3.1):

```sh
AB_KEEP=1 bench/suite/run-remote-ab.sh mysql "talaria.netscan=0"   # A and B, the stand is kept
R=bench/suite/results/ec2ab-mysql-<stamp>
bench/suite/ab-phase.sh $R C "talaria.netirq=1"       mysql        # the third phase
bench/suite/ab-phase.sh $R D "talaria.slice=150000"   mysql        # the fourth
python3 bench/suite/lib/compare.py $R/C $R/A                       # a ratio < 1 means C is faster
bench/suite/infra/stand-down.sh                                    # TEAR IT DOWN
```

Before the stand comes the smoke gate of the deployment path (eight minutes,
cents):

```sh
bench/suite/infra/smoke.sh redis               # AMI -> instance -> DHCP -> user-data -> the role -> one request -> cleanup
SMOKE_TYPE=c6i.2xlarge bench/suite/infra/smoke.sh redis   # the same gate on another instance type
```

It asserts the console (the DHCP lease, the user-data tail, the manifest knob
printed, no boot refusal, the manifest's process started, no panics), one live
request to the workload and — most importantly — that after the cleanup the
region's resource inventory (instances, AMIs, snapshots, volumes, groups) matches
the inventory from BEFORE the run. `SMOKE_KEEP=1` keeps the instance on failure
for analysis; the journal and the console go to `~/.talaria-lab/smoke-<time>/`.

### WHAT TO KNOW ABOUT THIS STAND BEFORE BELIEVING ITS NUMBERS

The stand counted as ready long before it first actually ran. The first run found
four blockers because of which the Linux side never came up; the next two runs
found as many again. What has been fixed and what remains is below, and all of it
affects how the numbers should be read:

* **The SERVICE latency is judged** (`latency.service`, from the actual send to
  the answer) — that is the default of `report.py`/`summary.py`/`promo.py`.
  The generator (which is Talaria too) does not hold the open loop's schedule:
  `backlog_max` grows into the thousands already at 2.8 thousand req/s, and in the
  open loop the Talaria side gave not a single judgeable point — every number
  spoke about the generator. The debt is not hidden: the "debt p99" column (the
  open loop minus the service latency) is printed in every table; it is a measure
  of the instrument. `TLJUDGE=open_loop` restores the old judging for
  cross-checking against old tables.
* **The Linux server must run on TWO cores, and that is checked.** For three runs
  in a row the Linux side was invalid, and none of them said so by itself: the
  server did not come up (a uid from the manifest without a user on the host, a
  chroot root of 0700) — "ceiling 0"; then it came up, but `isolcpus` in the
  cmdline removed balancing, and ALL of mariadbd's/mongod's threads stayed on one
  core — a tail of 30-60 ms and a collapse at 39 thousand req/s. Now `isolcpus`
  goes only to the generator, the playbook fails the bring-up if the server is not
  `is-active` and not listening on the port from `meta.env`, and `statd` prints
  the placement of threads on cores (`psr cpuN:count`) — look at it BEFORE
  believing a Linux loss.
* **The early kernel blockers are gone and are held by gates**: the receive break
  after ~4.6 thousand connections (a backlog slot closed by the peer before
  `accept` was not reissued — `reap_backlog`) and "`c6i.2xlarge` stops at the
  first frame" (the network was initialised twice) — the smoke gate
  `infra/smoke.sh` passes on `xlarge` and `2xlarge`. The main numbers are still
  taken on `c6i.xlarge`.

The reports are `report.md` and `report.json` in the result directory. The summary
over ALL workloads at once is one table, and decisions are made from it:

```sh
python3 bench/suite/lib/summary.py <directories...> --md SUMMARY.md
```

The promo page is `python3 bench/suite/lib/promo.py <directories> -o promo.html`.

### What does NOT enter the comparison (and why)

Three rejection rules; all three took more points from us than from Linux.

1. **Overload, a rate shortfall or errors** on at least one side — the whole run,
   with the reason printed next to the asterisk (`* (overload)`,
   `* (rate 96.8%)`, `* (errors 500 of 1801)`). Single errors (up to 0.1 % of the
   requests — one hung connection per half a million) do not reject a point but
   are marked: `talaria (errors 2)`.
2. **The generator's debt, per percentile** — only with `TLJUDGE=open_loop`. tlgen
   measures two latencies: `open_loop` (from the scheduled deadline) and `service`
   (from the send), and the difference between them is exactly the instrument's
   own debt. Where it is larger than the service latency and larger than a
   millisecond, the number speaks about the generator. That is how a redis/ping
   point with a "thousandfold win" was caught: Linux had `open_loop` p50 46.6 ms
   with `service` p50 0.13 ms. When judging by the service latency the debt does
   not enter the number and is printed as a column.
3. **A difference smaller than the stand's jitter.** The floor
   (`lib/noisefloor.sh`) is measured by the generator against nginx ON THE HOST,
   so ABSOLUTE guest numbers must not be compared with it — it includes that
   nginx's own time. The difference is compared instead: a point is considered
   unresolvable where the sides diverge by less than the floor's tail departs from
   its own median.

### The harness self-check — `selfcheck.sh`

The instrument broke more often than the systems it measured, and every time
silently. So the harness has a regression of its own —
`bench/suite/selfcheck.sh` (half a minute on the host, without guests and EC2;
needs `make -C tlgen host`). Every check is a former defect: the tlgen report
parses and contains `counts.sent` (on EC2 an old tlgen appended `{"error": 1}`
after the report and all the Talaria points dropped out of the table);
`service <= open_loop`; the `--serve` receiver survives a second job and gives the
same counters; a connection drop, a closed port and garbage instead of a report
mark the point with a stub (`lib/job.py`) rather than bringing the ladder down; a
silent server hits `--timeout` rather than hanging the generator for seconds;
`report.py` shouts "NOT PARSED" on a broken file, tolerates a tail and does not
take `report.json`/`floor.json` for points; a rejection is printed with its
reason; the summary does not depend on the order of the directories and is
idempotent; the actor matrix's accumulator is not the file the runner writes; the
ClickHouse Linux guest's initramfs is rebuilt when `q.sql` changes (the stamp of
`services/clickhouse/ir-stamp.sh`).

## 8. What this suite has already found

The suite is not only a showcase but also a way to find in the kernel what the
regression does not find. In its very first session:

| finding | how it showed up |
|---|---|
| a closed fd stayed in the epoll sets | nginx with keep-alive broke a fresh connection without answering (fd numbers are reused) |
| fork did not give the child the vDSO | every PostgreSQL backend died with signal 11 while the postmaster lived |
| the network stack had no mutual exclusion | two backends on different cores brought THE KERNEL down with a "RefCell already borrowed" panic |

All three are about SMP and the process model, i.e. exactly what Talaria trades
in. None of them is visible on a single-threaded workload.
