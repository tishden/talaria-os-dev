# Talaria OS — a hands-on guide: a multithreaded C++ application from the build to the post-mortem

One application goes the whole way: the build, the check of the ABI, the disk image, the launch in
QEMU, the monitoring, the layout over the cores, a profile, live debugging, a crash and
a core dump, the transfer of files, a launch on EC2. All the output below was taken from real
runs. The numbers on your machine will be different, the arrangement of the output will be the same.

Where it was taken: a host Intel i7-3820 (4 cores, 8 threads), QEMU/KVM in the docker image
`talaria-qemu2`; EC2 — a `c6i.large`, `ap-northeast-1`. The 21st of September 2026.

The references the text points to: `docs/USER-GUIDE.md` (all the knobs and
the commands), `docs/PERFORMANCE-OPTIMIZATION-GUIDE.md` (the measurement's methodology).

---

## Contents

| § | What we do | The tool |
|---|---|---|
| 1 | the stand | the docker images, the build of the kernel |
| 2 | the application `mdpipe` | — |
| 3 | the build and the check of the ABI | `g++` (Alpine, musl), `talaria-check` |
| 4 | the disk image and the first launch | `talaria-pack`, `qrun.sh` |
| 5 | the monitoring of a live workload | the console: `ps`, `kstat`, `cores`, `perf` |
| 6 | the layout over the cores | `talaria.smp=`, the `pin=` rules |
| 7 | the profiling | `talaria.prof=`, `perf record`, `talaria-prof` |
| 8 | live debugging | `gdb break`, `hbreak` |
| 9 | a crash: the stack, a core dump, gdb | `talaria.core=`, `[fault]`, `gdb` |
| 10 | files onto the node and off it | `filed`, `talaria-file`, `debugfs` |
| 11 | the same on AWS | `talaria-deploy.sh`, the EC2 console |
| 12 | the cheat sheet | |

---

## 1. The stand

What is needed: a Linux with `/dev/kvm`, docker, `rustup`, `python3`, `e2fsprogs`, `gdb` on
the host. For chapter 11 there is the `aws` CLI with EC2 rights.

```sh
services/docker/build-images.sh                  # talaria-qemu2, talaria-musl, talaria-alpine-build
cd kernel && cargo -Z json-target-spec build --release
# -> kernel/target/x86_64-talaria/release/talaria
```

QEMU is always launched in a container. The network is a `tap0` inside the container: the host is
`10.0.0.1`, the node is `10.0.0.2`.

The example's files are in `examples/mdpipe/`:

| The file | The purpose |
|---|---|
| `mdpipe.cpp` | the workload |
| `Makefile` | the build, the disk images, a `make check` |
| `qrun.sh` | QEMU `microvm`; the serial line into the terminal — the kernel's log and the operator's console |
| `qgdb.sh` | QEMU `-M pc`; COM2 at `127.0.0.1:1234` for gdb |
| `qfiled.sh` | the node in the `filed` mode + the `talaria-file` commands |

---

## 2. The application

`mdpipe` is arranged as a datapath: the reception, the processing by a pool of threads, a write to the disk.

```
md_gen ──► a queue (a mutex + a condvar) ──► md_work0..N ──► every 4096th tick ──► md_sink ──► /mdpipe.log
                                                                                         (write + fsync)
md_main: prints a report once a second, and at the end the latency's percentiles
```

What is measured is the **stage's latency**: from the putting of a tick into the queue to the end of its
processing, by the `CLOCK_MONOTONIC`. The result is the `p50`/`p90`/`p99`/`p99.9`/`max`.

What is put into the code on purpose:

| The device | Where | For the chapter |
|---|---|---|
| the threads are named by a `prctl(PR_SET_NAME)`: `md_gen`, `md_work0`, `md_sink`, `md_main` | `worker()`, `sinker()`, `generator()`, `main()` | 5, 6 — the names in `ps` and in the `pin=` rules |
| `hot_mix()` — the most expensive function; its price is set by the `--rounds` | `mdpipe.cpp:75` | 7 |
| `book[5] == nullptr`; a `--crash-at N` slips instrument 5 in at the tick N | `book_init()`, `worker()` | 9 |
| the negative intervals are counted separately and do not go into the percentiles | `worker()` | 4 |

The arguments: `--secs`, `--rate` (ticks/s), `--workers`, `--rounds`, `--out`,
`--crash-at`.

---

## 3. The build and the check of the ABI

```sh
cd examples/mdpipe
make mdpipe
```

The flags from the `Makefile` and what they are for:

- `-static` — a static binary against musl (Alpine's `g++`): there is no need to put the loader and
  the libraries onto the disk.
- `-no-pie -Wl,-Ttext-segment=0x10000000` — the workload's code is above 256 MB
  (`USER-GUIDE` §6.1).
- `-g -fno-omit-frame-pointer` — the stacks in the profile, in the crash line, in the dump and in
  the debugger. Without frame pointers there will be one frame everywhere.

The compatibility check:

```
$ make check
talaria-check: mdpipe (25 s run, 17 distinct syscalls)
  top syscalls: futex(202)x1081, rt_sigprocmask(14)x17, mmap(9)x11, munmap(11)x9, prctl(157)x5, writev(20)x5, mprotect(10)x4, clone(56)x4, nanosleep(35)x3, brk(12)x2, fsync(74)x2, arch_prctl(158)x1
VERDICT: COMPATIBLE — no unsupported syscalls were hit
```

- `talaria-check` boots the kernel with a `talaria.strace=` and counts the syscalls.
  An unsupported syscall would have returned an `ENOSYS` and would have got into a separate list.
- `clock_gettime` is not in the list: the clock is read in user space through the vDSO, without
  a syscall.
- `clone` x4 — the four threads besides the main one, `prctl` x5 — their names, `futex` —
  the queue on the mutex/condvar, `writev` — that is how musl's stdio prints.
- `nanosleep` x3 — that is `md_main`'s sleep. The generator did not fall asleep even once: `strace`
  prints every syscall into the serial line, the workload falls behind the schedule and has no time
  to sleep. Under `talaria-check` one looks at the list of the syscalls, not at the rate.

---

## 4. The disk image and the first launch

```
$ make disk.img ARGS="--secs 40 --rate 20000 --workers 2 --out /mdpipe.log"
disk.img: ext2, 134217728 bytes, files=2
  /mdpipe                                 1209904 bytes
  /cell.manifest                          90 bytes
manifest: talaria.mode=cell (multi-threaded, supervised) or talaria.mode=proc (own address space, fork)
```

`talaria-pack` assembles an ext2 volume and writes the workload's manifest into it:

```
binary=/mdpipe
args=mdpipe --secs 40 --rate 20000 --workers 2 --out /mdpipe.log
restart=0
```

The `Makefile` passes a `--journal`: a volume with a JBD2 journal, and after a successful `fsync`
the metadata will not be left in an intermediate state. Without the flag the kernel at the
mount writes `ext2: there is no journal`.

The launch:

```sh
./qrun.sh "talaria.mode=cell talaria.smp=4" 60 4
```

There are two parameters for the cores here, and they are different: the third argument (QEMU's `-smp 4`) gives
the machine four vCPUs, while the `talaria.smp=4` is how many of them the scheduler will take. Without
the second one the workload lives on one core, and the log says so with the line
`sched-plan: ... the scheduler's cores 0x1`.

The boot log (abridged):

```
talaria: cmdline = "console=ttyS0 talaria.mode=cell talaria.smp=4"
talaria: the knob talaria.mode = cell — the production mode
talaria: the knob talaria.smp = 4 — the number of cores
smp: the KVM PV IPI is on — a call to a neighbouring core goes in one hypercall
talaria: tsc = 3598290357 Hz
sync: the wait mode is hlt+ipi (monitor=0 waitpkg=0 mwaitx=0)
vdso: the clock in user space is up (2 pages, the data at 0x7ffd00001000)
smp: the topology: 4 logical cores (the BSP's apic 0) out of 4 by the MADT, apic_of[0..4]=[0, 1, 2, 3]
talaria: the SMP scheduler: the online cores mask=0xf
netsys: ip=10.0.0.2/24 gw=10.0.0.1 dns=10.0.0.1 tcp=64 udp=8
ext2: it is mounted — the block is 4096 B, 32768 blocks (128 MB), 1 groups, an inode is 256 B, free 26312 blocks / 32755 inodes, the cache is 52 MB write-back
ext2: a JBD2 journal — the inode 8, 4096 blocks, seq 1, start 0, the mode ordered (the data go in place)
talaria: a workload by the manifest /cell.manifest (restart=0, argc=9, envc=0)
sched-plan: rules 0, spreading=on balancing=on numa=on, the interrupts per core 0, the scheduler's cores 0xf
supervise: starting the workload (attempt 1/1, multithreaded)
cell: building my own address space
cell: the ELF is loaded (entry=0x1000308f) — mapping a 32 MB stack
cell: the stack is mapped — allocating a 64 MB heap
talaria: a multi-threaded workload from the disk in ring 3 (its own AS), entry=0x1000308f, argc=9, envc=0
syscall: the workload's first syscall: 158 (ring 3 is alive)
mdpipe: start — 2 threads, rate 20000 ticks/s, 40 s, file /mdpipe.log
```

- `the knob … = …` — the audit of the command line. Every key is parsed and named;
  a typo in a key the kernel rejects with an explanation rather than letting it through silently.
- `sync: the wait mode is hlt+ipi` — under KVM the waiting in the kernel goes through a `hlt` and
  inter-core interrupts; on hardware it will be an `mwait`.
- `cell: … my own address space` — the workload is in ring 3 with page tables of its
  own; the 32 MB stack and the 64 MB heap are mapped at once.
- `the first syscall … 158` — an `arch_prctl`: musl sets the TLS up. From that line on
  the workload works.

The end of the run:

```
cell: uptime=40s conns=0 rx=0 tx=0 syscalls=499560 faults=161 free=686MB restarts=0 threads=5
mdpipe: total — ticks 799999, samples 799999, clock went backwards 0 times (max 0 ns)
mdpipe: stage latency, ns: p50=7517 p90=21933 p99=42300 p99.9=100343 max=1240492
mdpipe: OK
[syscall] exit_group(the code=0) — the process has finished
supervise: the workload finished normally (code 0)
supervise: code 0 — no restart is needed, halting
```

- `cell: uptime=…` — the supervisor's status line, once every 10 seconds.
- `restart=0`: a code of 0 means a halt. A non-zero code or a crash means a restart, until
  the `restart=` limit runs out.

> **An instrument is bound to check itself.** The counter "the clock went backwards" appeared
> when the first runs gave a `max=4294925997`: a negative interval
> written into a `uint32`. The cause was in the kernel — the `CLOCK_MONOTONIC` in ring 3
> jumped backwards at every adjustment of the clock by kvmclock (by up to 520 us, in 3 runs
> out of 5; with a `talaria.clocksync=0` — not once). The defect has been fixed, the gate is
> `talaria.test=clock`. The rule has stayed: an interval that may turn out to be
> negative is counted separately and does not go into the percentiles.

---

## 5. The monitoring of a live workload

The operator's console lives in the kernel and works on top of the workload: the commands are typed
straight into the `qrun.sh` terminal, and the workload's output goes mixed in with the answers.
The prompt `talaria:/>` is printed after an answer.

The commands that wait for a key do not block on top of a workload: `top` prints
one snapshot and returns (a `q` pressed after it will become a new command:
`unknown command: 'q'`), while `less` and `edit` refuse. For a series of snapshots there is
`ps -t` several times.

### 5.1. `ps -t` — the threads, the cores, the processor

```
talaria:/> ps -t
ps: the first snapshot — the rates are averaged OVER THE WHOLE UPTIME (00:00:11); a repeated call gives an interval
…
talaria:/> ps -t
Talaria  uptime 00:00:14  TSC 3598 MHz  slice 500us  tasks 5 (running 4, ready 0, waiting 1, dead 0), processes 1
CPU   [0]9.6% [1]90.6% [2]0.6% [3]0.6%
     25.4% in total over 4 cores (4 shared, 0 dedicated); migrations 0, steals 0
MEMORY 1023M in total, free 688M (bump 638M + returned 50M), the kernel 51M; handed out 423K/s
FLOW   syscalls 12k/s, switches 3.9k/s, faults in total 84 (COW 0), steals 0
PMU    over the interval, over 4 cores: IPC 0.43, LLC misses 129711 (0.02/1000i), branch misses 2476594 (0.56/1000i)
FS     ext2 RW 128M in total, free 102M, the block cache 52 MB (bypassed 1.2%), the journal is on
NET    interfaces 1, connections 0, listeners 0 (+0 UDP), rx 0 tx 0

  PID  TID NAME            CPU CPU% WAIT%    RSS  SHR LAZY ALLOC/s SWITCH/s SYSC/s STATE SYSCALL
    0    1 md_sink           1 92.0%  0.0%    97M    0  1.4M       0        0     11 RUN   74
    0    4 md_gen            0  6.7%  9.2%    97M    0  1.4M    4.0K     1.9k   7.0k WAIT  35
    0    0 md_main           0  2.9% 14.8%    97M    0  1.4M       0     1.9k      2 RUN   35
    0    2 md_work0          2  0.6%  0.0%    97M    0  1.4M    221K        0   2.5k RUN   202
    0    3 md_work1          3  0.6%  0.0%    97M    0  1.4M    198K        0   2.6k RUN   202
```

- The first call averages over the whole uptime, the second over the interval between the calls.
  Look at the second one.
- The `CPU%` is counted in the core's TSC cycles rather than in the timer's ticks: a thread on a
  dedicated core has no ticks at all. The time in a `hlt`/`mwait` belongs to nobody.
- The `WAIT%` is that the thread was ready while there was no core. `md_main` waits 14.8 % at 2.9 %
  of work: it shares core 0 with `md_gen`.
- The `SYSCALL` is where the thread stands now: 202 is a `futex` (the queue), 35 is a `nanosleep`,
  74 `fsync`.
- `md_sink` occupies 92 % of a core while making five writes a second, and stands in an
  `fsync`. Where the time goes is in 5.2.

### 5.2. `kstat syscalls` and `kstat fs` — a syscall's price

The syscalls' price is counted if one boots with a `talaria.sysprof=1` (a pair of `rdtsc`s
per call — that is diagnostics, and it is not kept on during a measurement):

```
talaria:/> kstat syscalls
stat: syscalls by the time in the kernel (the number: calls, total ms, ns per call):
stat:   #202: 137216 times, 34926 ms, 254536 ns
stat:   #35: 28129 times, 26396 ms, 938399 ns
stat:   #74: 69 times, 7581 ms, 109872613 ns
stat:   #20: 14 times, 60 ms, 4320463 ns
stat:   #1: 69 times, 0 ms, 5345 ns
…
talaria:/> kstat fs
kstat fs: ext2 RW, the block is 4096 B, 128M in total (32768 blocks), free 102M, the journal is on
kstat fs: the block cache 52 MB: hits 7912, misses 98 (bypassed 1.2%)
kstat fs: 97 blocks read, 54 written (54 ranges), device barriers 110, read-aheads 5 (+77 blocks)
kstat fs: the lock: takes 339, with a wait 0 (0.0%), waited 0ns, held 194ms
kstat fs: the journal: transactions 54, checkpoints 0 (copies from the journal 0); the barriers: data 0, the transaction 108, the checkpoint 0, …
```

- `#74` (`fsync`) — 110 ms per call. The disk's image is a file on the host, and a guest's barrier
  becomes an `fdatasync` of that file at QEMU's end, and the host's disk pays for it; at
  that hour it was loaded by other processes (in the host's `top` there was an `rsync` in the state
  `D`). On EC2 (an NVMe EBS) a write costs less than a millisecond (§11).
- The waiting for the device in Talaria's kernel is active — a poll with windows for
  pre-emption. So in `ps` it looks like the thread's `CPU%`, and in the profile (§7) —
  like the function `fs::io_window`.
- The `#202` and the `#35` are expensive because their time includes the sleep: for a `futex` and a
  `nanosleep` "the time in the kernel" is the time of the waiting.
- The `transactions 54` and the `barriers: … the transaction 108` are two device barriers per
  every `fsync` with a journal.

### 5.3. `cores`, `kstat sched`, `kstat sync`

```
talaria:/> cores
cores: the scheduler's cores 0xf, migrations 0 (of them by the balancing 0), refusals of a dedicated core 0
cores:   the core 0 the node 0 shared (common): t0(md_main) t4(md_gen)
cores:   the core 1 the node 0 shared (common): t1(md_sink)
cores:   the core 2 the node 0 shared (common): t2(md_work0)
cores:   the core 3 the node 0 shared (common): t3(md_work1)
cat: L3 CAT is unavailable; the L3 16384 KB
talaria:/> kstat sched
kstat sched: tasks 5 (the ceiling 192), scheduler cores 0xf, the slice 500us
kstat sched: switches 93714, work steals 0, migrations 0, transplants on wakeup 35282
kstat sched: scheduler ticks 0, hardware ticks 46905, the pulse is going
kstat sched: the cores (the role, the timer, ready tasks, occupancy, switches):
kstat sched:   core 0  shared timer armed   ready 2   busy 00:00:02 (ticks 46904/46904) switches 93714 node 0 pool -
kstat sched:   core 1  shared timer armed   ready 1   busy 00:00:20 (ticks 11912/11912) switches 0 node 0 pool -
kstat sched:   core 2  shared timer armed   ready 0   busy 162ms (ticks 11913/11913) switches 0 node 0 pool -
kstat sched:   core 3  shared timer armed   ready 0   busy 161ms (ticks 11917/11917) switches 0 node 0 pool -
talaria:/> kstat sync
kstat sync: the wait primitive is hlt+ipi
kstat sync: futex waits 232308: missed at once 2081, woken 1, the word itself 128798, a timeout 101424, a signal 0
kstat sync: exits 230223, of them after sleeping 229363, on "the task is dead" 0
kstat sync: by operations: WAIT=130882 WAKE=132618
```

- `cores` shows who is on which core. The balancer itself laid the five threads out over
  the four cores: the generator and the main thread together, the rest one each.
- `kstat sched`: all the context switches are on core 0, where two threads live.
  On the cores 2 and 3 the switches are zero: a worker thread falls asleep on a futex without leaving the
  core, and wakes up there. The occupancy of core 1 (20 s out of 20) is the `fsync` of the
  thread `md_sink`.
- `kstat sync` shows how the futex waits ended: `the word itself` means the word
  changed and a write was enough for the waker; `woken` means by an explicit `FUTEX_WAKE`;
  `a timeout` means the deadline expired. `on "the task is dead"` counts the waits interrupted by
  the process's death; in a live workload there is a zero there.

### 5.4. `perf` — the hardware counters

```
talaria:/> perf
perf: counters accumulated SINCE BOOT; stale snapshots taken now (IPI).
  core  role           cycles     instructions    IPC  LLC misses                  branch misses
  0     shared     3264980064     1072485620   0.32  423216 (0.39/1000i)          8215835 (7.66/1000i)
  1     shared    87975767185    38387174149   0.43  290976 (0.00/1000i)          8745636 (0.22/1000i)
  2     shared     1022027037      237059133   0.23  219125 (0.92/1000i)          3102166 (13.08/1000i)
  3     shared      999395921      228802357   0.22  196442 (0.85/1000i)          2993564 (13.08/1000i)

perf: SUMMARY  IPC 0.42, LLC misses 1129870 (0.02/1000i), branch misses 23066015 (0.57/1000i)
perf:   IPC below 1.0 — the CPU is WAITING. Look at the cache misses below and `kstat paging`.
```

- Core 1 has 88 billion cycles: that is the polling of the device in the `fsync` from 5.2.
- The cores 2 and 3 almost do not work: a worker thread has 24 rounds of `hot_mix` per tick, and
  its cycles go mostly into the path of falling asleep and waking up — hence the 13
  predictor misses per thousand instructions. In this configuration the latency is
  determined by the wake-up rather than by the computation.
- `perf poke <core>` takes the counters of a dedicated core: there is no tick there, and there is
  nobody else to take them.

### 5.5. The files and the summary on top of a workload

```
talaria:/> status
Talaria: uptime 31 s (0 h), TSC 3598 MHz, tasks 5
memory: free 638 MB (bump) + 49 MB (reclaim)
FS: ext2 RW, 128 MB in total, 102 MB free, the journal is on
network: 10.0.0.2 gateway 10.0.0.1, connections 0, rx 0 B, tx 0 B
perf: PMU present, profile off, samples 0, per-core snapshots 0xf
talaria:/> ls -l /
-rw-r--r--         90 2026-09-21 21:39 cell.manifest
drwx------      16384 2026-09-21 21:39 lost+found/
-rw-r--r--    1209904 2026-09-21 21:39 mdpipe
-rw-r--r--       4129 2026-09-21 21:39 mdpipe.log
entries in total: 4
talaria:/> tail -n 3 /mdpipe.log
seq=675840 sym=0 px=10460
seq=679936 sym=0 px=10184
seq=684032 sym=0 px=10408
talaria:/> df
the ext2 FS: the block 4096 B, in total 128 MB, occupied 25 MB (19%), free 102 MB
the inodes: in total 32768, free 32754; the journal is on (JBD2)
```

`help` prints all the commands by group, and `help <command>` prints the detail on one of them.

### 5.6. A machine without a console

A `talaria.top=10000` on the command line prints a snapshot of `top` into the log every
10 seconds — without an operator and without a process in ring 3. On EC2 that is exactly what is done (§11):
everything that happened to the machine stays in its console.

---

## 6. The layout over the cores: a measurement instead of reasoning

How many cores to give the workload and how to lay the threads out is a question for a measurement on the machine
the workload will live on.

A ladder by the number of cores:

```sh
make disk.img                                   # by default --secs 15
for n in 1 2 4; do ./qrun.sh "talaria.mode=cell talaria.smp=$n" 30 $n | grep 'latency'; done
```

Two layouts are set in the manifest, that is, at the packing:

```sh
PACK="python3 ../../kernel/tools/talaria-pack --fs ext2 --journal --size 128M --app mdpipe --restart 0"
ARGS="--secs 15 --rate 20000 --workers 2 --out /mdpipe.log"
# every thread on its own core
$PACK -o disk-pin.img --args="$ARGS" --pin md_gen:1 --pin md_work0:2 --pin md_work1:3 \
      --pin md_sink:shared --pin md_main:shared
# the generator and the workers go on dedicated cores (without a tick and without neighbours)
$PACK -o disk-ded.img --args="$ARGS" --pin md_gen:dedicated --pin 'md_work*:dedicated' --pin '*:shared'
IMG=disk-ded.img ./qrun.sh "talaria.mode=cell talaria.smp=4" 30 4
```

How the rules were understood the log prints at the launch (here for the `disk-pin.img`):

```
sched-plan: rules 5, spreading=on balancing=on numa=on, the interrupts per core 0, the scheduler's cores 0xf
sched-plan:   md_gen -> the core 1
sched-plan:   md_work0 -> the core 2
sched-plan:   md_work1 -> the core 3
sched-plan:   md_sink -> the common pool
sched-plan:   md_main -> the common pool
```

The result, two runs per line (20 000 ticks/s, 15 s), microseconds:

| The configuration | p50 | p99 | p99.9 |
|---|---|---|---|
| 1 core | 16.2 / 16.4 | 36 / 41 | 57 / 65 |
| 2 cores | 7.1 / 10.3 | 70 / 72 | **2 873 / 3 349** |
| 4 cores, the balancer | 5.5 / 4.7 | 38 / 34 | 85 / 71 |
| 4 cores, a thread per core | 6.1 / 5.6 | 48 / 42 | 98 / 88 |
| 4 cores, `dedicated` | 5.8 / 7.8 | 36 / 49 | 87 / **1 016** |

- One core: the wake-up of a worker thread is a context switch on the same core,
  hence the median of 16 us. But the tail is even.
- Two cores: the median is twice as good, the `p99.9` is fifty times worse — in both
  runs. Five threads on two cores on this stand are worse than on one.
- Four cores with the balancer is the best line. An explicit layout did not
  beat it.
- `dedicated` in the second run gave a `p99.9` of 1 ms. A tail is judged by two runs
  in a row, not by one.

Why there are two dedicated cores rather than three is visible in `cores` (a `*` means the thread was placed
by a rule):

```
cores: the scheduler's cores 0xf, migrations 2 (of them by the balancing 0), refusals of a dedicated core 1
cores:   the core 0 the node 0 shared (common): t0(md_main)* t3(md_work1)*
cores:   the core 1 the node 0 shared (common): t1(md_sink)*
cores:   the core 2 the node 0 dedicated: t2(md_work0)*
cores:   the core 3 the node 0 dedicated: t4(md_gen)*
```

Core 0 is never dedicated: the OS lives on it (the console, the interrupts
of the devices). By the time `md_work1` named itself, the core 1 was already
taken by `md_sink`, put there by the rule `*:shared`, and a thread placed by a
rule is never evicted from its core. `md_work1` was refused and went into the
common pool to `md_main` — the line `refusals of a dedicated core 1`. The order
matters: a thread gets its core at the moment of `prctl(PR_SET_NAME)`, and
whoever named itself first took it.

The host of this stand has four physical cores, and the four vCPUs share them
with the host's processes. On EC2 (§11) two cores gave a `p99` of 6.2 us:
another machine — another answer.

---

## 7. Profiling

A profile is needed when there is something to compute in the workload. An image
with a heavy compute part (`--rounds 8000`, about 50 us per tick):

```sh
make prof
IMG=disk-prof.img ./qrun.sh "talaria.mode=cell talaria.smp=4 talaria.prof=400" 60 4 | tee serial.log
```

`talaria.prof=400` turns the sampling profile on from the boot: 400 samples per
second per core, each one with a stack of up to eight frames, the thread, the
core, the protection ring. In the console `perf on 400` does the same.

### 7.1. `perf top` — right on the node

```
talaria:/> perf top
perf: samples in the ring 2048 of 16298 total (the ring wrapped — old samples lost), rate 400 Hz, stack depth up to 8
perf: WORKLOAD (ring 3) — 504 samples, average depth 1.9, with a stack 500 of 504:
perf:   0x0000000010003c20     333 (66.0%)
perf:   0x0000000010003c10     117 (23.2%)
perf:   0x0000000010003c30      49 (9.7%)
perf:   0x0000000010029d40       2 (0.3%)
…
perf:   symbols: addr2line -f -e <workload binary> <address>
perf: KERNEL (ring 0) — 1549 samples, average depth 1.0, with a stack 0 of 1549:
perf:   0x000000000025c340    1506 (97.2%)
perf:   0x00000000002200c0      37 (2.3%)
…
```

The kernel does not resolve symbols — the symbol table lives on the host next to
the binary. Addresses are turned into names on the host.

### 7.2. `perf record` and `talaria-prof` — on the host

```
talaria:/> perf record 1500
#talaria-prof v2 samples=1500 total=14791 tsc_hz=3600207406 slice_ns=500000 hz=400 depth=8
#format: SAMPLE <ring> <tid> <cpu> <thread-name> <address> [<return-address> ...]
SAMPLE 3 2 2 md_work0 0x10003c13 0x1000a61f
SAMPLE 0 3 3 md_work1 0x25c34d
…
#talaria-prof end
```

`perf record N` dumps the last N samples into the serial, and they land in
`serial.log`. The profile is stopped for the duration of the dump: two thousand
lines over the serial take seconds, and without the pause the ring would have
been overwritten.

```
$ kernel/tools/talaria-prof report --kernel kernel/target/x86_64-talaria/release/talaria \
      --user examples/mdpipe/mdpipe < serial.log
# WARNING: the ring wrapped — 13291 samples lost, the profile covers only the last 1500

== WORKLOAD (ring 3): 361 samples of 1500 (24%) ==
 samples  share  function
     327  90.6%  hot_mix   /w/mdpipe.cpp:76 (discriminator 1)
      31   8.6%  worker(int)   /w/mdpipe.cpp:76 (discriminator 3)
       2   0.6%  unsigned int& std::vector<unsigned int, std::allocator<unsigned int> >::emplace_back<unsigned int>(unsigned int&&)   /usr/include/c++/13.2.1/bits/vector.tcc:119
       1   0.3%  __libc_free   /home/buildozer/aports/main/musl/src/musl-1.2.5/src/malloc/mallocng/free.c:151
  by thread: 2/md_work0=359, 3/md_work1=2
  by core: cpu2=359, cpu3=2
  average depth 2.0, with a stack 360 of 361
 samples  share  stack (leaf <- callers)
     327  90.8%  hot_mix <- execute_native_thread_routine
      31   8.6%  worker(int) <- execute_native_thread_routine

== KERNEL (ring 0): 1139 samples of 1500 (75%) ==
 samples  share  function
    1095  96.1%  talaria::sync::kwait_once_wchk
      44   3.9%  talaria::fs::io_window
  by thread: 1/md_sink=375, 3/md_work1=374, 0/md_main=370, 2/md_work0=20
  by core: cpu1=375, cpu3=374, cpu0=370, cpu2=20
  average depth 1.0, with a stack 0 of 1139
  no stacks: the code is built without frame pointers (-fno-omit-frame-pointer) — only the leaf is visible
```

- In ring 3 — 24 % of the samples, 90 % of them in `hot_mix`: the function that
  was planted has been found, with its source line.
- In ring 0 — 75 %, almost all of it `sync::kwait_once_wchk`: the threads sleep
  on a futex. A sample is taken by the kernel's tick, and a sleeping thread lands
  in the profile as "in the kernel". For a workload where the latency matters
  more than the throughput this is the main line of the profile: it says that the
  time goes into waiting, not into computing.
- `fs::io_window` in `md_sink` — the polling of the device inside `fsync` (§5.2).
- In the window of 1500 samples only `md_work0` was working: the window is less
  than a second on four cores. The distribution by thread is compared over
  several windows.
- The kernel's stacks are of depth 1: the kernel is built without frame pointers
  (they cost about 4 % on a syscall). A kernel with stacks is built by `kernel/build-prof.sh`.

For a flamegraph:

```
$ kernel/tools/talaria-prof folded --kernel … --user examples/mdpipe/mdpipe < serial.log | sort -k2 -nr | head -5
kernel;md_work1(3);talaria::sync::kwait_once_wchk 374
kernel;md_main(0);talaria::sync::kwait_once_wchk 370
kernel;md_sink(1);talaria::sync::kwait_once_wchk 331
workload;md_work0(2);execute_native_thread_routine;hot_mix 325
kernel;md_sink(1);talaria::fs::io_window 44
$ kernel/tools/talaria-prof folded … < serial.log | flamegraph.pl > prof.svg
```

---

## 8. Live debugging

The kernel's gdb stub speaks over COM2. `microvm` has no second port, so for
debugging there is `qgdb.sh`: the machine `-M pc`, COM2 on `127.0.0.1:1234`, an
NVMe disk (virtio on `-M pc` is mmio-only in Talaria), four cores (`SMP=`).

```sh
make disk.img ARGS="--secs 120 --rate 2000 --workers 2 --out /mdpipe.log"
./qgdb.sh
```

In the node's console — stop the machine:

```
talaria:/> gdb break
gdb: the machine is stopped in the debugger — `target remote :1234` from the host, `continue` releases it
```

On the host:

```
$ gdb -q examples/mdpipe/mdpipe
(gdb) set architecture i386:x86-64
(gdb) target remote 127.0.0.1:1234
0x00000000002c257d in ?? ()
(gdb) info registers rip cs
rip            0x2c257d            0x2c257d
cs             0x8                 8
(gdb) hbreak hot_mix
Hardware assisted breakpoint 1 at 0x10003c02: file mdpipe.cpp, line 76.
(gdb) continue

Thread 1 hit Breakpoint 1, hot_mix (rounds=24, x=2555637911364730948) at mdpipe.cpp:76
76	    for (int i = 0; i < rounds; i++) {
(gdb) bt
#0  hot_mix (rounds=24, x=2555637911364730948) at mdpipe.cpp:76
#1  worker (id=<optimized out>) at mdpipe.cpp:162
#2  0x000000001000a61f in execute_native_thread_routine ()
#3  0x0000000010028db5 in start (p=0x60000005eaf0) at src/thread/pthread_create.c:207
#4  0x000000001002a129 in __clone () at src/thread/x86_64/clone.s:22
(gdb) info registers rip rsp cs
rip            0x10003c02          0x10003c02 <worker(int)+274>
rsp            0x60000005ea10      0x60000005ea10
cs             0x2b                43
(gdb) p/x x
$1 = 0x23777323d96a8044
(gdb) delete
(gdb) detach
[Inferior 1 (Remote target) detached]
```

- The stop from `gdb break` lands in the kernel: `cs 0x8`, the address is in the
  kernel's text (`add-symbol-file kernel/target/x86_64-talaria/release/talaria -o 0`).
- `hbreak`, not `break`: the workload's code is mapped R-X (W^X), and an `int3`
  cannot be written there. Hardware breakpoints: up to three at a time.
- After `continue` the machine stopped already in a worker thread: `cs 0x2b` —
  ring 3, the stack unwinds down to `__clone`, the variables are read by name.

A breakpoint can be hit many times — handy for watching the data in motion:

```
(gdb) hbreak apply_trade
(gdb) continue
Thread 1 hit Breakpoint 1, apply_trade (mix=10208923068595446737, px=10442, sym=6) at mdpipe.cpp:88
88	    Instrument *ins = book[sym];
(gdb) p *book[sym]
$2 = {name = "SYM6\000\000\000", vol = 1514, notional = 15711350}
(gdb) continue
Thread 1 hit Breakpoint 1, apply_trade (mix=210975000264573163, px=10010, sym=2) at mdpipe.cpp:88
```

Threads work as in an ordinary gdb: `info threads`, `thread N`. The thread
number is the workload's tid (the task + 1, as in `ps` and in the dump); the
registers of the stopped one come from the exception frame, those of the rest
from the context where the scheduler took them off the core.

```
(gdb) add-symbol-file kernel/target/x86_64-talaria/release/talaria -o 0
(gdb) hbreak apply_trade
Hardware assisted breakpoint 1 at 0x10003c51: file mdpipe.cpp, line 88.
(gdb) hbreak hot_mix
Hardware assisted breakpoint 2 at 0x10003c02: file mdpipe.cpp, line 76.
(gdb) continue
[Switching to Thread 4]

Thread 4 hit Breakpoint 2, hot_mix (rounds=24, x=10077446034900276812) at mdpipe.cpp:76
76	    for (int i = 0; i < rounds; i++) {
(gdb) info threads
  Id   Target Id         Frame
  1    Thread 1          sccp (nr=<optimized out>, u=140737219919184, v=0, w=0, x=0, y=0, z=0) at ./arch/x86_64/syscall_arch.h:61
  2    Thread 2          0x000000000025b1d6 in talaria::sync::wait_on_ex ()
  3    Thread 3          0x000000000025b1d6 in talaria::sync::wait_on_ex ()
* 4    Thread 4          hot_mix (rounds=24, x=10077446034900276812) at mdpipe.cpp:76
  5    Thread 5          0x000000000027ac2c in talaria::sched::yield_to ()
(gdb) thread 1
[Switching to thread 1 (Thread 1)]
(gdb) bt 4
#0  sccp (nr=<optimized out>, u=140737219919184, v=0, w=0, x=0, y=0, z=0) at ./arch/x86_64/syscall_arch.h:61
#1  0x000000001002bc1c in __clock_nanosleep (clk=0, flags=0, rem=0x0, req=0x0) at src/time/clock_nanosleep.c:33
#2  __clock_nanosleep (clk=clk@entry=0, flags=flags@entry=0, req=<optimized out>, rem=<optimized out>) at src/time/clock_nanosleep.c:8
#3  0x000000001002a20e in nanosleep (req=<optimized out>, rem=<optimized out>) at src/time/nanosleep.c:6
```

- `Thread 4` is `md_work1`, stopped at the breakpoint.
- `Thread 1` is `md_main` in `nanosleep` (it prints a report once a second).
- `Thread 2` and `3` stand in the kernel, waiting on a futex (`wait_on_ex`):
  they were taken off the core in the middle of a syscall, and their symbols come from the kernel — hence `add-symbol-file`.

The price of a stop is the whole machine. The workload's report after `detach`:

```
mdpipe: 6 s — processed 11608 (+1567/s), queue max 1, records 3
mdpipe: 7 s — processed 20693 (+9085/s), queue max 2155, records 6
```

While the machine stood, the generator fell behind its schedule and after the
release produced a batch of 2155 ticks — the queue grew from 1 to 2155. A
debugger on the datapath changes what you are looking at; for latency tails use
the profile (§7), for crashes the dump (§9).

On several cores the debugger works in all-stop mode, like gdbserver on Linux:
while the stub talks to gdb the other cores are parked (IPI), and the hardware
breakpoints are armed on every core — a thread on any AP stops at them just as
on the BSP. A step (`stepi`, and the step over a breakpoint on `continue`)
releases only the stepping core. For a thread on a parked core `info threads`
shows its real registers, and the stop reply names the core (`core:`).

To stop before the workload's first instruction — to put breakpoints on its
start — use `talaria.gdb=break` on the command line
(`GDBARGS=talaria.gdb=break ./qgdb.sh`): the kernel enters the debugger before
the first entry into the workload, and no console is needed for `gdb break`.

Registers (`set $rax=…`) can be written only for the stopped thread.

---

## 9. A crash: the stack, the core dump, gdb

```sh
make crash                                            # --crash-at 150000
IMG=disk-crash.img ./qrun.sh "talaria.mode=cell talaria.smp=4 talaria.core=/core.mdpipe" 40 4
```

`talaria.core=<path>` turns on the writing of ELF dumps onto the node's FS.

```
mdpipe: at tick 150000 an instrument outside the book will be sent
mdpipe: 7 s — processed 140500 (+20101/s), queue max 99, records 35
[fault] the signal 11 to the thread t2: the rip 0x10003c7d, the address 0x8, the err 0x5 (a violation of the rights, a read) -> to the workload's handler
[fault] the core 2, the task was running on c2

[fault] ring 3: the exception #14 err=0x5 rip=0x10003c7d cr2=0x8 rsp=0x60000005ea10 fs=0x60000005eb38 [fs:0]=0x60000005eb38
║ the stack workload (2 frame(s)): 0x10003c7d 0x1000a61f
║   the symbols: addr2line -f -C -e <binary> <address>
[core] 24576 B into /core.mdpipe: threads 5, memory segments 5 (20 KB)
[core] to read it: gdb <binary> <file>; to take it off the node — kernel/tools/talaria-file <ip> pull /core.mdpipe ./core
sched: all the 5 task(s) have finished (COW copies: 0)
supervise: the workload's threads were killed by signal 11 (one thread crashing = the process crashing)
supervise: THE WORKLOAD CRASHED (exception #14 rip=0x10003c7d) — the kernel is intact
supervise: the restart limit (0) is exhausted — halting
```

The lines in order:

- `the signal 11 to the thread t2 … the core 2` — the thread `t2` (`md_work0`) crashed, on the core 2.
- `the address 0x8, … a read` — a read at the address 8: the field at the offset 8
  of a null pointer. In `Instrument` the offset 8 is the field `vol`.
- `fs=… [fs:0]=…` match — the thread's TLS is intact. Had they differed, the line
  would have ended with the mark `THE TLS IS BROKEN`: for workloads on musl a
  corrupted `%fs` is a frequent cause of crashes in ring 3, and by the `rip` it is not told apart from an application's bug.
- `the stack workload (2 frame(s))` — the stack is printed always, with no FS and
  no debugger. The addresses are resolved on the host:

  ```
  $ addr2line -f -C -i -e mdpipe 0x10003c7d 0x1000a61f
  worker(int)
  /w/mdpipe.cpp:89
  worker
  /w/mdpipe.cpp:166
  execute_native_thread_routine
  thread.cc:?
  ```

  `apply_trade` is inlined into `worker` (`-O2`), so `-i` shows both lines: 89
  is the dereference, 166 is the call.
- `[core] … threads 5` — a dump with all the process's threads. It is written
  when the other four threads have already stopped: one thread crashing kills the
  process (as on Linux), the threads finish what they had started in the kernel
  and stop, and only then comes the dump. That is why there is not a single line
  of the workload's report between `[fault]` and `[core]`, and in the dump every
  thread is at the place where it stood.
- `20 KB` — the threads' stacks from `rsp` up to the top. The threads stand
  shallow, so the stacks are small.

### 9.1. Taking the dump off the node

Over the network — the node boots in the `filed` mode, the commands come from
the "host" `10.0.0.1` (in detail in §10):

```
$ IMG=disk-crash.img ./qfiled.sh "ls /" "pull /core.mdpipe out/core.mdpipe"
$ talaria-file 10.0.0.2 ls /
-        108  cell.manifest
-      24576  core.mdpipe
d      16384  lost+found/
-    1209904  mdpipe
-        931  mdpipe.log

$ talaria-file 10.0.0.2 pull /core.mdpipe out/core.mdpipe
/core.mdpipe -> out/core.mdpipe: received 24576 B
```

Without a network — straight out of the image: `debugfs -R "dump /core.mdpipe out/core.mdpipe" disk-crash.img`.

### 9.2. Opening it in gdb

```
$ gdb -q mdpipe out/core.mdpipe
warning: core file may not match specified executable file.
[New LWP 3]
[New LWP 1]
[New LWP 2]
[New LWP 4]
[New LWP 5]
Core was generated by `md_work0'.
#0  0x0000000010003c7d in apply_trade (mix=<optimized out>, px=10000, sym=<optimized out>) at mdpipe.cpp:89
89	    ins->vol      += 1;
[Current thread is 1 (LWP 3)]
(gdb) bt
#0  0x0000000010003c7d in apply_trade (mix=<optimized out>, px=10000, sym=<optimized out>) at mdpipe.cpp:89
#1  worker (id=<optimized out>) at mdpipe.cpp:166
#2  0x000000001000a61f in execute_native_thread_routine ()
#3  0x0000000010028db5 in start (p=0x60000005eaf0) at src/thread/pthread_create.c:207
#4  0x000000001002a129 in __clone () at src/thread/x86_64/clone.s:22
Backtrace stopped: frame did not save the PC
(gdb) frame 1
#1  worker (id=<optimized out>) at mdpipe.cpp:166
166	        apply_trade(sym, t.px, mix);
(gdb) p t
$1 = {seq = 150000, t_enq = 7955506016, sym = 0, px = 10000}
```

- `LWP` — the thread numbers are the same the kernel prints: `LWP 3` = `t2` from
  the `[fault]` line (the thread number = the task number + 1).
- `warning: core file may not match` — the process in the dump is named after the
  crashed thread (`md_work0`), while the binary is `mdpipe`. It does not affect the analysis.
- The analysis: the tick came with `sym = 0` (a regular instrument), while into
  `apply_trade` went the instrument 5 — the substitution is in `worker()`, the
  line 165. The bug is not in the frame where it crashed but one frame above.

Where the other threads stood:

```
(gdb) info threads
  Id   Target Id         Frame
* 1    LWP 3             0x0000000010003c7d in apply_trade (mix=<optimized out>, px=10000, sym=<optimized out>) at mdpipe.cpp:89
  2    LWP 1             sccp (nr=<optimized out>, u=0, v=0, w=0, x=0, y=0, z=0) at ./arch/x86_64/syscall_arch.h:61
  3    LWP 2             sccp (nr=<optimized out>, u=0, v=0, w=0, x=0, y=0, z=0) at ./arch/x86_64/syscall_arch.h:61
  4    LWP 4             sccp (nr=<optimized out>, u=0, v=0, w=0, x=0, y=0, z=0) at ./arch/x86_64/syscall_arch.h:61
  5    LWP 5             sccp (nr=<optimized out>, u=0, v=0, w=0, x=0, y=0, z=0) at ./arch/x86_64/syscall_arch.h:61
(gdb) thread 3
(gdb) bt 6
#0  sccp (nr=<optimized out>, u=0, v=0, w=0, x=0, y=0, z=0) at ./arch/x86_64/syscall_arch.h:61
#1  0x000000001002b97e in __futex4_cp (to=<optimized out>, val=2, op=128, addr=0x60000003a974) at src/thread/__timedwait.c:24
#2  __timedwait_cp (addr=addr@entry=0x60000003a974, val=val@entry=2, clk=clk@entry=0, at=at@entry=0x0, priv=128, priv@entry=1) at src/thread/__timedwait.c:52
#3  0x0000000010028461 in __pthread_cond_timedwait (c=0x1003b328 <sink+40>, m=0x1003b300 <sink>, ts=0x0) at src/thread/pthread_cond_timedwait.c:100
#4  0x000000001000610d in std::condition_variable::wait(std::unique_lock<std::mutex>&) ()
#5  0x0000000010003940 in std::condition_variable::wait<Stage::pop(Tick&)::{lambda()#1}>(std::unique_lock<std::mutex>&, Stage::pop(Tick&)::{lambda()#1}) (__p=..., __lock=..., this=0x1003b328 <sink+40>) at /usr/include/c++/13.2.1/condition_variable:105
```

- All four sisters stood in syscalls (`sccp` is musl's entry into the kernel).
  `LWP 3` is `md_sink`, it waits for the write queue `sink` on a condvar; `LWP 1`
  and `LWP 5` are in `nanosleep` (`md_main` and `md_gen`).
- This is the place in the thread's code, not in the kernel's: a sister killed in
  a syscall stops on the way out of it, and the kernel remembers her user registers for the dump.

### 9.3. What is in the dump and what is not

By default the dump carries the threads' registers and **stacks**. There is no
global data (`.bss`, the heap) in it, and gdb reads it out of the binary file, that is, as zeroes:

```
(gdb) p book[t.sym]
$2 = (Instrument *) 0x0
```

This is not true: `book[0]` on the node is not null. A full dump is
`talaria.core=/core.mdpipe,full`: the whole memory of the process, 97 MB for
`mdpipe` (the same as the RSS — count the room on the volume by it):

```
[core] 99074048 B into /core.mdpipe: threads 5, memory segments 11 (96748 KB)
$ gdb -q -batch -ex 'frame 1' -ex 'p book[t.sym]' -ex 'p *book[0]' -ex 'p book[5]' -ex 'p processed' mdpipe out/core.full
$2 = (Instrument *) 0x1003b4a0 <book_init()::store>
$3 = {name = "SYM0\000\000\000", vol = 37604, notional = 390216888}
$4 = (Instrument *) 0x0
$5 = {<std::__atomic_base<unsigned long>> = {static _S_alignment = 8, _M_i = 150421}, static is_always_lock_free = true}
```

`book[5]` is null — here it is, the instrument that does not exist; `processed`
is how many ticks had been processed before the crash.

A dump of a live task, without a crash: `core <tid>` in the console (the same
`talaria.core=` is needed). The workload is not stopped for it, so the registers
of the threads running at that moment on other cores are not trustworthy.

The same scenario in the process model (`talaria.mode=proc`, as for the DBMSs)
gives the same dump and the line `[signal] the process pid=1 was killed by the signal 11`.

### 9.4. The restart

`restart=N` in the manifest is how many times the supervisor will bring the
workload up after a crash (`talaria-pack --restart N`). An image with `--restart 1 --crash-at 60000`:

```
supervise: starting the workload (attempt 1/2, multithreaded)
mdpipe: start — 2 threads, rate 20000 ticks/s, 20 s, file /mdpipe.log
[core] 24576 B into /core.mdpipe: threads 5, memory segments 5 (20 KB)
supervise: the workload's threads were killed by signal 11 (one thread crashing = the process crashing)
supervise: THE WORKLOAD CRASHED (exception #14 rip=0x10003c7d) — the kernel is intact
supervise: memory: free=689MB (the baseline after the first restart)
supervise: restarting...
supervise: starting the workload (attempt 2/2, multithreaded)
mdpipe: start — 2 threads, rate 20000 ticks/s, 20 s, file /mdpipe.log
[core] 24576 B into /core.mdpipe: threads 5, memory segments 5 (20 KB)
supervise: the workload's threads were killed by signal 11 (one thread crashing = the process crashing)
supervise: THE WORKLOAD CRASHED (exception #14 rip=0x10003c7d) — the kernel is intact
supervise: memory: free=689MB drift=-64KB
supervise: the restart limit (1) is exhausted — halting
```

`drift=-64KB` — after the second instance there is as much free memory as after
the first, to within 64 KB (a minus means more was freed than taken). The dump
is overwritten by every crash: to analyse the first one take it off before the
restart or keep `restart=0`. `restart` in the console and `watchdog=N` in the
manifest (no incoming traffic for N seconds) restart the workload the same way.

---

## 10. Files to the node and from the node

The `filed` service puts files onto the node and takes them off without
rebuilding the image. A token is mandatory and is given only from the outside
(`talaria.filedtoken=`); there is no default value.

```
$ IMG=disk-crash.img ./qfiled.sh "pull /mdpipe.log out/mdpipe.log" "push mdpipe /mdpipe" \
      "push out/mdpipe.v2 /mdpipe" "--token wrong ls /"
$ talaria-file 10.0.0.2 pull /mdpipe.log out/mdpipe.log
/mdpipe.log -> out/mdpipe.log: received 931 B

$ talaria-file 10.0.0.2 push mdpipe /mdpipe
mdpipe -> /mdpipe: identical, not sent (1209904 B)

$ talaria-file 10.0.0.2 push out/mdpipe.v2 /mdpipe
out/mdpipe.v2 -> /mdpipe: sent 8192 of 1209904 B (0%), block 8192

$ talaria-file 10.0.0.2 --token wrong ls /
filed: authentication rejected (ERR bad token)
```

Outside `qfiled.sh` the same is done directly, from a host that sees the node's
address: `TALARIA_TOKEN=<secret> kernel/tools/talaria-file <ip> pull /path ./file`.

- A file that matches is not sent: the client first asks for the size and the CRC32.
- `mdpipe.v2` is the same binary with five bytes changed at the offset 4096
  (`printf patch | dd of=out/mdpipe.v2 bs=1 seek=4096 conv=notrunc`).
  One block of 8 KB out of 1.2 MB went over: the client compares the CRC32 block
  by block and sends only those that differ.
- The other commands: `sync <directory> <path>` (the whole directory), `rm <path>`.

The node's log does not print the token:

```
talaria: cmdline = "console=ttyS0 talaria.mode=filed talaria.filedtoken=***"
talaria: the knob talaria.filedtoken = *** (6 bytes) — the filed token
filed: the receiving of files on tcp://10.0.0.2:7373 (the token is mandatory, demo-level — behind an external perimeter)
```

On EC2 the instance's serial is read through `aws ec2 get-console-output`, so
the secrets (`*token=`) are replaced with asterisks in the log.

The boundaries:

- `filed` works in the modes `talaria.mode=filed` and `talaria.mode=console` (or
  `filed on` in the console). On top of a running cell the network stack is busy
  with the datapath — the files are then read by the console (`cat`, `tail`, `hd`).
- There is no TLS. The service writes into the node's FS and is meant for a
  network behind an external perimeter; in an untrusted one — a tunnel.
- Offline from the image: `debugfs -R "ls -l /" disk.img`, `debugfs -R "dump /path file" disk.img`.

---

## 11. The same on AWS

`infra/aws/talaria-deploy.sh` does the path "the binary -> the image -> an EBS
volume -> a snapshot -> an AMI -> an instance" in one command. It needs the
`aws` CLI with EC2 rights and a default VPC; the key and the security group are
created by themselves. The output is abridged, the addresses and the paths are replaced with `<…>`/`~`:

```
$ infra/aws/talaria-deploy.sh --app examples/mdpipe/mdpipe \
      --args "--secs 30 --rate 20000 --workers 2 --out /mdpipe.log" \
      --type c6i.large --smp 2 --restart 0 --stop-after 150 --extra "talaria.top=10000" \
      --name mdpipe-guide-0001
== data image: mdpipe -> ~/.talaria-lab/mdpipe-guide-0001-data.img
   ~/.talaria-lab/mdpipe-guide-0001-data.img: ext2, 67108864 bytes, files=2
== boot image: ~/.talaria-lab/mdpipe-guide-0001.img
   cmdline: console=ttyS0 talaria.pci=1 talaria.trace=2048 talaria.mode=cell talaria.ip=dhcp talaria.smp=2 talaria.stop=150 talaria.poweroff=1 talaria.top=10000
   mk-tboot: stage1 72 B, stage2 1756 B (4 sect.), kernel 54493792 B -> LBA 8192 (106434 sect.), data LBA 114688 (131072 sect.), image 121 MB
== group sg-… has no ssh access from <your IP> — adding it
== volume 1 GiB in ap-northeast-1c
   the image has been uploaded onto /dev/nvme1n1
== snapshot snap-0123456789abcdef0
== AMI ami-0123456789abcdef0 (the volume with the instance: DeleteOnTermination=true)

== deployed
   instance : i-0123456789abcdef0 (c6i.large)
   address  : <public IP>
   AMI      : ami-0123456789abcdef0 (snapshot snap-0123456789abcdef0)
```

- `--smp 2` — both vCPUs of a `c6i.large`; `--stop-after 150` — after 150 s a
  `SIGTERM` to the workload, then a shutdown (ACPI S5): the instance goes into `stopped` by itself.
- `talaria.trace=2048` is always added by the script: the whole kernel log is
  written onto the disk as well (on a `*.metal` the instance's console is empty, this is the only channel).
- The data image is built here without a journal (the line
  `ext2: there is no journal` in the log below).
- The volume is filled by a disposable `t3.small` helper: it comes up, pours the
  image in and goes down. The whole path to a running instance takes a few minutes.

The instance's console. It appears with a delay: right after the instance was
stopped the answer was empty, and the text came a couple of minutes of retries later.

```
$ aws ec2 get-console-output --instance-id i-0123456789abcdef0 --latest --output text
…
nvme: 00:04.0 BAR0=0xfebf0000 nsze=2097152 blocks (1024 MB), an IO pair qid=1 of depth 64, a transfer of up to 128 KB, polling; the volatile write cache is ABSENT (a write is durable on completion — a barrier is not needed)
smp: the topology: 2 logical cores (the BSP's apic 0) out of 2 by the MADT, apic_of[0..2]=[0, 1]
smp: the layout by the physical cores (CPUID 0x0B, the SMT shift 1): the logical ones 0..2 -> the cores [0, 0] the threads [0, 1]; DIFFERENT physical cores 1
ena: the LLQ is configured (a record of 256 B, an inline header of up to 224 B, stride=0x2, accel: sup=0x3 enabled=0x3, meta=true burst=2 records)
netsys: net0 dhcp lease 172.31.13.252/20 gw=172.31.0.1 dns=172.31.0.2
ext2: it is mounted — the block is 4096 B, 16384 blocks (64 MB), 1 groups, an inode is 256 B, free 15053 blocks / 16371 inodes, the cache is 180 MB write-back
ext2: there is no journal (the volume was created without -O has_journal) — the mode without transactions
supervise: starting the workload (attempt 1/1, multithreaded)
mdpipe: start — 2 threads, rate 20000 ticks/s, 30 s, file /mdpipe.log
mdpipe: 1 s — processed 20009 (+20009/s), queue max 13, records 5
…
CPU   [0]3.8% [1]0.7%
  PID  TID NAME            CPU CPU% WAIT%    RSS  SHR LAZY ALLOC/s SWITCH/s SYSC/s STATE SYSCALL
    0    4 md_gen            0  2.7% 22.7%      0    0     0       0     2.0k   6.0k WAIT  35
    0    1 md_sink           0  0.8%  0.0%      0    0     0       0       60     14 WAIT  202
    0    3 md_work1          1  0.5%  0.9%      0    0     0    186K     2.0k   2.0k RUN   202
    0    0 md_main           0  0.2%  0.6%      0    0     0     36K     2.0k      5 RUN   35
    0    2 md_work0          1  0.1%  0.5%      0    0     0     17K     2.0k   2.0k WAIT  202
…
mdpipe: total — ticks 599998, samples 599998, clock went backwards 0 times (max 0 ns)
mdpipe: stage latency, ns: p50=2492 p90=6042 p99=6238 p99.9=108627 max=375187
mdpipe: OK
supervise: the workload finished normally (code 0)
acpi: the FS has been synchronised (there were 0 dirty blocks, 0 ms; writes 294 — 256455 us, barriers 0 — 0 us, the worst 0 us)
suite: the list has been gone through in full — powering the machine off (ACPI S5)
```

- The two vCPUs of a `c6i.large` are two threads of one physical core
  (`DIFFERENT physical cores 1`).
- `the volatile write cache is ABSENT` — on EBS a write is durable on completion,
  the barriers are not needed: `barriers 0` in the synchronisation line.
- A snapshot of `top` every 10 s (`talaria.top=10000`) goes into the serial in
  portions and mixes with the workload's output — that is how it looks in the log.
- `md_sink` stands here in `futex` (202) rather than in `fsync`: a write onto the
  NVMe of EBS is short, unlike in §5.2.
- The stage latency on EC2: a `p50` of 2.5 us, a `p99` of 6.2 us against 7.5 and
  42 us in QEMU on four cores (§4).

The cleanup — the instance, the AMI, the snapshot (the volume is deleted together with the instance):

```sh
aws ec2 terminate-instances --instance-ids i-0123456789abcdef0
aws ec2 deregister-image --image-id ami-0123456789abcdef0
aws ec2 wait instance-terminated --instance-ids i-0123456789abcdef0
aws ec2 delete-snapshot --snapshot-id snap-0123456789abcdef0
```

One AMI for several runs with different parameters — `--userdata`: the tail of
the command line is read from the instance's user-data (`USER-GUIDE` §7.1,
`talaria.userdata=1`). The files off the volume after a run — `infra/aws/read-disk-trace.sh`
(with `--keep-disk`).

### 11.1. The same on a Graviton (arm64)

The workload is rebuilt for aarch64 (`make a64` in `examples/mdpipe`: the
`aarch64-linux-musl-cross` toolchain, the same flags) and `talaria-deploy.sh` takes it as it
is — the architecture comes from the binary's ELF header, the type defaults to `c7g.large`,
and the image is the UEFI one (EC2's arm64 boots nothing else):

```
$ infra/aws/talaria-deploy.sh --app examples/mdpipe/mdpipe-a64 \
      --args "--secs 30 --rate 20000 --workers 2 --out /mdpipe.log" \
      --type c7g.large --smp 2 --restart 0 --stop-after 150 \
      --extra "talaria.top=10000 talaria.ntp=aws" --name mdpipe-guide-a64b
== data image: mdpipe-a64 -> ~/.talaria-lab/mdpipe-guide-a64b-data.img
== boot image: ~/.talaria-lab/mdpipe-guide-a64b.img
   cmdline: talaria.pci=1 talaria.mode=cell talaria.ip=dhcp talaria.smp=2 talaria.stop=150 talaria.poweroff=1 talaria.top=10000 talaria.ntp=aws
   mk-ami: ~/.talaria-lab/mdpipe-guide-a64b.img (89MB): p1 ESP@2048 (23MB, BOOTAA64.EFI + talaria.cfg), p2 data@49152 (64MB <- …-data.img)
== volume 1 GiB in ap-northeast-1c
== AMI ami-0123456789abcdef0 (the volume with the instance: DeleteOnTermination=true)
== deployed
   instance : i-0123456789abcdef0 (c7g.large)
```

The console works on a Graviton (the 16550 behind PCI), so the same `get-console-output`:

```
ena: the PHC (the device's clock): it is NOT declared on this instance type
netsys: net0 dhcp lease 172.31.1.81/20 gw=172.31.0.1 dns=172.31.0.2
ntp: sample 1 from 169.254.169.123: the error -121022 us, the round trip 124 us (the best of 3/4) — the boot's base is set onto it, not stepped
nvme: 00:04.0 BAR0=0x80040000 nsze=2097152 blocks (1024 MB), an IO pair qid=1 of depth 64, …
mdpipe: start — 2 threads, rate 20000 ticks/s, 30 s, file /mdpipe.log
ntp: sample 2 from 169.254.169.123: the error -209 us, the round trip 53 us (the best of 4/4), the servo +14 ppm, steps 0 back 0
CPU   [0]10.3% [1]4.0%
MEMORY 3.9G in total, free 3.2G (bump 3.1G + returned 141M), the kernel 55M; handed out 237K/s
FLOW   syscalls 27k/s, switches 14k/s, faults in total 60 (COW 0), steals 0
PMU    over the interval, over 2 cores: IPC 0.77, LLC misses 0 (0.00/1000i), branch misses 0 (0.00/1000i)
…
mdpipe: total — ticks 599996, samples 599996, clock went backwards 0 times (max 0 ns)
mdpipe: stage latency, ns: p50=1610 p90=5606 p99=34837 p99.9=50018 max=1017629
mdpipe: OK
supervise: the workload finished normally (code 0)
suite: the whole list is done — powering the machine off (ACPI S5, …)
```

- `talaria.ntp=aws` — a Graviton has no PTP clock on its ENA and no paravirtual clock: the
  firmware's clock was 121 ms off, the first SNTP answer set the base before the workload,
  and the second, 16 s later, found the clock 0.2 ms off with the crystal's +14 ppm taken up.
- `PMU` on a `c7g.large` has two event counters: the instructions and one more event. Its
  CHAIN (two counters as one 64-bit) is declared but does not carry, so the kernel widens the
  32-bit counters itself (`talaria.pmuchain=1` would use it).
- The stage latency: a `p50` of 1.6 us against 2.5 on the `c6i.large` above; the `p99` is
  34.8 us against 6.2 — the tail is the Graviton run's own and not yet explained.
- `talaria.trace=` is not laid on arm64: the image is an ESP plus the data, and the console
  carries the log.

---

## 12. The cheat sheet

The build and the run:

| The task | The command |
|---|---|
| the binary | `g++ -O2 -g -fno-omit-frame-pointer -pthread -static -no-pie -Wl,-Ttext-segment=0x10000000` |
| the compatibility | `kernel/tools/talaria-check ./app --secs 25` |
| the image | `talaria-pack -o disk.img --fs ext2 --journal --app app --args="…" --restart N` |
| the layout | `--pin 'name*:core\|dedicated\|shared'` at the packing |
| the run | `./qrun.sh "talaria.mode=cell talaria.smp=4" 60 4` |

The console on top of the workload:

| The question | The command |
|---|---|
| who eats the CPU, who waits for a core | `ps -t` (twice, look at the second one) |
| where each thread is | `cores` |
| what a syscall costs | `kstat syscalls` (boot with `talaria.sysprof=1`) |
| the FS, the journal, the barriers | `kstat fs`, `df` |
| the scheduler, the futexes | `kstat sched`, `kstat sync` |
| the IPC, the cache misses | `perf`, `perf poke <core>` |
| where it is hot | `perf on 400`, `perf top`, `perf record N` |
| to stop in the debugger | `gdb break` |
| a dump of a live task | `core <tid>` |
| the history of the kernel's events | `log` |

The command-line knobs from this practical guide:

| The knob | What it does |
|---|---|
| `talaria.smp=N` | the cores for the scheduler (plus `-smp N` for QEMU) |
| `talaria.prof=<hz>` | the profile with stacks from the boot |
| `talaria.sysprof=1` | the cost of the syscalls in `kstat syscalls` |
| `talaria.core=<path>[,full]` | the ELF dumps onto the node's FS |
| `talaria.top=<ms>` | a snapshot of `top` into the log without an operator |
| `talaria.filedtoken=<secret>` | the token of the `filed` service |
| `talaria.stop=<s>`, `talaria.poweroff=1` | stop the workload and power the machine off |

On the host:

| The task | The command |
|---|---|
| the profile into names | `talaria-prof report --kernel <kernel> --user <binary> < serial.log` |
| flamegraph | `talaria-prof folded … \| flamegraph.pl > prof.svg` |
| an address into a line | `addr2line -f -C -e <binary> <address>` |
| a dump | `gdb <binary> <dump>`, then `add-symbol-file <kernel> -o 0` |
| the files over the network | `talaria-file <ip> ls\|pull\|push\|sync\|rm`, `TALARIA_TOKEN=` |
| the files out of the image | `debugfs -R "dump /path file" disk.img` |
| on EC2 | `infra/aws/talaria-deploy.sh --app … --type … --smp … --stop-after …` |
