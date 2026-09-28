# Talaria OS — the performance guide

How to measure, profile and tune a workload on Talaria. For two roles:

* the **developer** — writes an application that will live on Talaria and wants
  to understand what it runs into;
* the **administrator** — operates the machine and has to answer the question "what
  is happening right now" and "why is today worse than yesterday".

Adjacent documents: `docs/USER-GUIDE.md` (how to run things), `docs/DEVELOPMENT-GUIDE.md`
(how to change the kernel itself), `docs/POC-RESULTS.md` (the measured numbers and their analysis),
`docs/BARE-METAL.md` (what QEMU hides).

---

## 0. The main thought

**Talaria does not make your program faster. It stops making it slower.**

A general-purpose OS is optimised for fairness and throughput:
it pre-empts you for the sake of a neighbour, migrates your threads between cores, swaps
pages out, sends inter-core TLB flushes and inserts its own code between you and
the device. Talaria does not do that — but only if it has been asked not to.
A cell started without a manifest, on a shared core, with an automatic layout,
will get exactly what Linux would have given, and there will be nothing to be surprised at.

Hence the practical conclusion that determines the whole order of work below:
**first check that the system is configured the way you think it is, and only then
profile the code.** Three quarters of "it is slow on Talaria" is a thread that
arrived on the wrong core, or a card's interrupt that arrives on the datapath's
core.

---

## 1. The map of the tools

| The question | The tool | Where |
|---|---|---|
| What is happening now? | `top` | the operator's console |
| Who eats the processor, the memory, the syscalls? | `top`, `ps` | the console |
| Which cores the threads live on, what shares of the L3? | `cores`, `top` (a wide terminal) | the console |
| What is with the network: the cards, the addresses, the ports, who holds them? | `netstat` | the console |
| The hardware counters: the IPC, the misses of the cache and of the predictor | `perf` | the console |
| Where exactly the workload spends its time and **who brought it there**? | `perf on` → `perf top` / `perf record` | the console + `kernel/tools/talaria-prof` |
| The kernel's counters by section | `kstat <section>` | the console |
| The price of the syscalls by number | `talaria.sysprof=1`, then `stat` | the cmdline |
| The stages of the network datapath | `talaria.netprof=<N>` | the cmdline |
| A histogram of the datapath's latency | `telem` | the console |
| Step-by-step debugging, breakpoints | the gdb stub on COM2 | `talaria.mode=gdb` |
| It crashed — what was on the stack and in the variables? | an ELF core dump + `gdb` | `talaria.core=<path>` (§10.3) |
| It hung — where does it stand? | `core [tid]` in the console | §10.3a |
| A machine without a console: a snapshot into the log | `talaria.top=<ms>` | the cmdline |
| A machine without a console: the whole log onto the disk | `talaria.trace=<LBA>` | the cmdline |
| The workload "has stalled" | `talaria.stuck=N`, `talaria.heartbeat=N` | the cmdline |

Everything except the last two lines is available **on a live machine without a restart**.

**One caveat about the `proc` mode.** In `talaria.mode=proc` (PostgreSQL, MariaDB,
MongoDB and ClickHouse live in it) the supervisor does not come up, while the operator's
console is polled out of its housekeeping — which means that without the flag
`talaria.console=poll` it does not answer: a typed command accumulates in the port.
The flag has deliberately not been made the default, and the reason is named in `docs/TODO.md` (A24):
with that polling the gate of concurrent work with the memory starts failing, uncovering
somebody else's race. The periodic snapshot (`talaria.top=<ms>`) and the profile
(`talaria.prof=<hz>`) do not depend on the flag — they write into the log by themselves.

---

## 2. The price of observation

Observability that gets in the way of measuring is useless. So here it is explicitly, item by item.

| What | When it is paid | How much |
|---|---|---|
| the time on the processor per thread | at a context switch | one read of the TSC + one atomic `+` |
| the switches per thread and per core | in the same place | two atomic `+`s |
| the syscalls and the faults per thread | at a syscall / a fault | one atomic `+` (the thread's number is read next to it anyway) |
| the memory frames per thread | at a call of the frame allocator | one atomic `+` |
| the hardware counters (the PMU) | in the scheduler's tick | seven `rdpmc`s per tick (at a slice of 8 ms — fractions of a per mille) |
| the profile's samples | in the tick, **only at `perf on`** | an unwinding of up to eight frames: two reads per frame |
| the RSS (a walk over the page tables) | **only when asked** by `top`/`ps` | milliseconds per process; switched off by `-m` |
| a snapshot of a dedicated core's counters | **only `perf poke <N>`** | one IPI into a latency-critical core |

**On the zero-syscall fast path (`talaria.mode=fast`) the price of observation is zero.** There
are neither syscalls, nor ticks, nor context switches there, that is, not a single point
where anything is counted. It is exactly for that reason that the observability is built around those
three events rather than around a periodic poll.

To switch the taking of the PMU in the tick off: `talaria.pmutick=0`.

---

## 3. The process: how to figure things out

The order is not arbitrary. Every step cuts off a whole class of causes, and to swap
them round means to profile the code where the thread is simply sitting on the wrong
core.

### Step 0. Establish what exactly is slow

While there is no number that can be measured twice, there is nothing to optimise. The number
is the `p50`/`p99` of the latency or the throughput **on a reproducible
run**, not "it seems to have got worse". How to measure so as not to measure nonsense is
§9.

### Step 1. Is the system configured the way you think it is?

```
cores        # where the threads live, which cores are dedicated, what the L3 shares are
kstat sched  # the role of every core, whether the timer is armed, how many tasks are ready
top          # who is on which core and how much they eat
```

What to look for:

* **the main thread on a shared core.** If in the `CPU` column it has a core from the shared
  pool (`kstat sched` will show the role `shared`), it is being pre-empted. It is cured by a `pin=` in
  the manifest (§8.1).
* **two hot threads on one core.** The classic picture "the speedup is now 4x, now
  2.3x": the threads did not spread out the way it was intended. `top -t` will show both with a core of
  the same number.
* **a dedicated core with an armed timer** (`kstat sched`, the `timer` column).
  On a dedicated core there must be no timer — otherwise it is not dedicated.
* **the card's interrupts on the datapath's core.** `kstat irq` will give the interrupt core,
  `netstat -i` — which card is on which interface. It coincided with the core of the receiving thread —
  move the `irq=` (§8.1).
* **migrations and work steals** (`kstat sched`). Non-zero ones on a latency-critical
  workload mean that the layout is not set and the scheduler decides by itself.

### Step 2. Is the processor busy or waiting?

```
top          # the columns CPU% and WAIT% (ready but not running) per thread, the occupancy per core
perf         # the IPC, the LLC misses, the predictor's misses
```

Three outcomes, and they lead in different directions:

| What is visible | The diagnosis | Where to go next |
|---|---|---|
| the CPU% is low for everyone | the workload is **waiting** rather than computing | §5 (the waits), §6 (the network), §7 (the disk) |
| the CPU% is high, the IPC ≥ 2 | it computes and it computes well | §4 (the code's profile): it computes the wrong thing |
| the CPU% is high, the IPC < 1 | it computes but **idles on the memory** | §4.3 (the cache) |

The IPC ("instructions per cycle") is the first quantity worth looking at.
A modern x86 core retires up to 4-5 instructions per cycle; 2.0-3.0 is a healthy
computational workload, 0.3-0.8 is a workload that goes into memory.

### Step 3. Where exactly the time is

```
perf on              # switch the sampling profile on (200 samples/s)
<wait under load>
perf top             # the top of the leaves and of the most frequent STACKS, the kernel and ring 3 separately
perf record          # dump the samples with their stacks line by line
```

Then it is on the host, with the symbols (§4.1).

### Step 4. Change one thing and measure again

One. With the same input data, the same image, the same number of runs.
The analysis in §9 exists because at this step people make mistakes more often than at all
the previous ones together.

---

## 4. Profiling the code

### 4.1. The sampling profile with the call stack

The profile is collected by the **scheduler's tick**: on every tick the kernel records
the CALL STACK (up to eight frames), the thread's number, the core's number and the protection ring.

```
perf on          # switch it on, 200 samples/s (a ring of 2048 samples)
perf on 1000     # more often: 1000 samples/s (the ceiling is the tick's rate)
perf top         # the top of the leaves + the most frequent STACKS, the kernel and the workload separately
perf record      # the machine format: one line is one sample with its stack
perf record 500  # only the last 500 (the whole buffer takes ~20 s over the serial line)
perf off         # switch it off (the ring stays)
perf reset       # forget what has accumulated before a new measurement
```

**The rate is given in hertz rather than as a divisor of the tick.** The default slice is 0.5 ms,
that is, the tick comes 2000 times a second; the ring would fill up within a second and
the profile would cover one instant. `perf on 200` is two hundred samples a second,
that is, ten seconds of recording; how many ticks that is the kernel computes itself. The ceiling is
the tick's rate: one cannot ask for more samples than there are ticks, and the kernel will say so
rather than invent them.

### 4.1a. Where the stack comes from — and when there will be none

The stack is taken **by the chain of frame pointers** (the RBP): two reads per frame,
tens of nanoseconds, it fits into an interrupt handler. The right answer is
the unwinding tables `.eh_frame`, they work without frame pointers too, but
parsing DWARF from the timer's tick is impossible: one can neither allocate memory there nor go
to the disk.

Hence a direct consequence that has to be known in advance:

| the code | will there be a stack? |
|---|---|
| your workload, built with `-fno-omit-frame-pointer` | **yes**, up to eight frames |
| your workload without it (an ordinary `-O2`) | no, only the leaf |
| someone else's binary (ClickHouse, the JVM) | as a rule no — check the `average depth` in `perf top` |
| the Talaria kernel, an ordinary build | no, only the leaf |
| the Talaria kernel, `kernel/build-prof.sh` | **yes** (see §4.2) |

The absence of a stack is visible directly: `perf top` prints the average depth and how many
samples unwind deeper than the leaf. **A depth of 1.0 is an honest refusal rather than a
breakage**: to invent a call tree where there is no data would be worse.

To get the stacks of your own workload, build it with
`-fno-omit-frame-pointer` (and do not throw the symbols away — `addr2line` keeps silent without
them). The price of frame pointers in the workload itself is usually 1-2 %; for a profiling
build that is the right trade.

The dumped samples are parsed on the host — where the binaries and the symbols lie:

```sh
# save the console output into a file (by any means: tee, the terminal's log,
# talaria.trace= on a machine without a console) and feed it to the parser
kernel/tools/talaria-prof report \
    --kernel kernel/target/x86_64-talaria/release/talaria \
    --user   kernel/user/mycell  < serial.log
```

The output is the table "the function: the samples, the share", ring 3 and ring 0 separately, plus
a breakdown by thread and by core. The kernel's names are demangled from **Rust v0** (`addr2line -C`
can do C++ and the legacy Rust mangling, but not v0, and the kernel is built with exactly that), so in
the report there stands `talaria::sched::exit_current_code` rather than `_RNvNtCs…`. The file and
the line `addr2line` will give only if the binary is built with debug information; without
it the column is simply empty. The last point matters more than it seems: "the time has been eaten" and
"the time has been eaten by ONE thread on ONE core" are indistinguishable by a list of functions, while
they are cured by opposite things.

A flame graph (single-level, see §4.4):

```sh
kernel/tools/talaria-prof folded --user ./cell < serial.log | flamegraph.pl > prof.svg
```

The order of the arguments is free, the file can be given positionally or through stdin;
`raw` prints the addresses with the counters if there are no symbols at all.

### 4.2. The kernel's profile: the stack costs 4 % and is therefore built separately

The ring 0 samples are resolved by the symbols of the kernel itself. That is the answer to the question
"the workload stands in the kernel — but where exactly": in the FS's lock, in a walk over the page
tables, in the network stack. If in the ring 0 top one sees `ext2`, go to §7; if
`paging` — to §4.3 and §8.3; if `netsys`/`l1` — to §6.

But a leaf without a stack in the kernel answers by half. `sync::wait_on_ex` ("the thread is waiting
for something") is not a diagnosis; the diagnosis is `wait_on_ex <- syscall::do_read`, that is, it waits
in a `read()`. For the kernel's stack to unwind, the kernel has to be built with frame
pointers:

```sh
kernel/build-prof.sh          # -> kernel/target-prof/x86_64-talaria/release/talaria
qemu-system-x86_64 ... -kernel kernel/target-prof/x86_64-talaria/release/talaria
```

**Why as a separate build rather than always.** The price is measured rather than assumed:
`talaria.test=mbench` gives a syscall of **114-119 ns** without frame pointers and
**123-124 ns** with them — about 4 % on the kernel's most frequent path (the ALU and the memory
bandwidth do not change). A system that exists for the sake of latency does not pay
4 % constantly for diagnostics needed occasionally. The WORKLOAD's stack does not depend on that build
at all — it is read out of its own frames.

The kernel's stack, having reached `syscall_entry`, continues **into the workload**: the chain of frames
is not broken at the boundary of the rings, and one sees which user function made the syscall.
`talaria-prof` looks for such addresses in both binaries and marks them `[ring3]`.

A quick variant without a dump is `perf code`: a histogram by the pages of the code,
accumulated by the same tick.

### 4.3. The hardware counters

```
perf          # a table by core + a summary with recommendations
perf poke 3   # take the counters of the DEDICATED core 3 (it interrupts it with one IPI)
```

What is shown:

* the **IPC** — instructions per cycle;
* the **LLC misses** — how many times an access to memory was not found in the last-level
  cache, that is, a trip into the DRAM (hundreds of cycles);
* the **branch predictor's misses** — a branch the processor guessed
  wrongly (a dozen or two cycles for each).

Both kinds of miss are printed **per thousand instructions** (MPKI) and, if the machine
had enough counters for the denominators, as a share as well. The MPKI is not decoration: a share of
misses requires a counter of references, and there are not enough PMU slots for everything (§10.2), while
the quantity "per thousand instructions" is comparable between runs and machines, whereas
a share without a denominator is not computed at all.

The landmarks (rough but useful):

| The quantity | Healthy | Worth looking at |
|---|---|---|
| IPC | > 1.5 | < 1.0 |
| LLC misses | < 2 per 1000 instr. | > 10 per 1000 instr. |
| the predictor's misses | < 5 per 1000 instr. | > 10 per 1000 instr. |

A high LLC MPKI means that the working set does not fit into the cache. There are two
levers: reduce the working set (the data structures, the batch size) or **give the
workload more cache** — if the machine has Intel CAT, the L3 share is set by the
manifest (`llc=`, §8.1), and `cores` will show what really stands in the hardware.

**About the dedicated cores.** On a dedicated core there is no scheduler's tick — which means
there is nobody to take its counters. That is not a breakage but a consequence: `top` and `perf` honestly
write "no snapshot (a dedicated or an idle core)" and name the numbers of such cores. They can be taken
by force with `perf poke <core>`: one IPI, which **interrupts
a latency-critical thread**. That is exactly why it is a separate command rather than
automatics — to peep into a dedicated core is to introduce the very jitter
for the elimination of which the core was dedicated.

### 4.4. What the profiler cannot do — and why

* **The stack's depth is eight frames.** A long recursion cannot be seen with this; the ninth
  frame and further are simply not recorded.
* **The stack only by frame pointers** (§4.1a). Code without them the unwinding does not
  take — and it honestly shows a depth of 1 instead of an invented tree.
* **The unwinding may break off earlier.** The RBP in code without frame pointers is
  an ordinary register, and the chain breaks at the very first check (a loop, a step backwards
  along the stack, too large a frame, a foreign ring, an unmapped page).
  How many reads broke down on a #PF in the process `perf top` shows on a separate
  line: a large number means that there is no chain of frames and the profile's depth
  must not be trusted.
* **There is no PEBS and no binding of an event to an instruction.** We count with counters, not with
  samples by event: "this `mov` missed past the L3" cannot be asked here.
* **There is no `perf_event_open` and there will not be.** That is a subsystem with event descriptors,
  rings through an `mmap` and groups of counters — exactly the machinery Talaria
  gets rid of. What perf gives in the end is available more cheaply: `rdpmc` is
  read **from ring 3 too, without a syscall** (the CR4.PCE is set), so the workload
  can take its own counters itself without leaving the datapath.

---

## 5. The workload is waiting: where exactly

A low CPU% is not "everything is fine" but "we are waiting". One can wait in four places.

```
kstat sync    # the futexes: how many waits there were and how they ended
kstat sched   # how many tasks are ready on every core
top -t        # the state of every thread: RUN / READY / WAIT
```

* **`WAIT` at most of the threads, there are many futex waits** — the workload runs into
  its own synchronisation. Look at the share of "woken" against "the word itself"
  in `kstat sync`: a lot of "the word itself" means that an active wait is spinning idly.
* **`READY` at several threads on one core** — there are more of them than there are cores, or
  the layout has brought them together. `kstat sched` will show "ready N" per core.
* **`RUN`, but the CPU% is low** — the thread is in the kernel: a syscall that waits. The ring 0 profile
  (§4.2) will name the subsystem.
* **everyone waits and there is no work** — we are waiting for something external: the network (§6) or the disk (§7).

### 5.1. The cores are underloaded: THERE IS NOWHERE TO PUT or THERE IS NOTHING TO PUT

Two diagnoses look IDENTICAL by the underloaded cores while they are cured by
opposite things: "a thread is ready but there is nowhere to put it" (the layout) and "the threads simply
are not ready" (the wake-up path). Guessing here is useless — there is a knob that answers
in one line:

```
talaria.runhist=1        # on the tick: how many tasks are READY and how many CORES are under them
```

```
stat: ready tasks at a tick: 0:0 1:416  2:89   3:91   4:5432 5:202 6:0 7:0 8+:0
stat: CORES busy under them:    0:0 1:1113 2:1333 3:927  4:2045 5:0 6:0 7:0 8+:0
```

It is read like this: there are almost always four ready ones while there are three cores under them — so it is about
the LAYOUT, and the wake-up path has nothing to do with it. The opposite picture (one or two ready ones
with four free cores) would have meant exactly the opposite.

`runhist` counts on the tick, that is, by sampling. The exact answer to the same question —
"how long a task was READY and not running" — is given by a separate count over every
interval from readiness to landing on a core (a wake-up, a pre-emption, the publication of
a new thread — everything; a sleep closes the interval):

```
stat: ready->cpu, us: <5:0 <20:1 <50:1 <100:0 <200:0 <500:2 <1000:1083 1000+:69
stat:   1156 intervals in total, 1151 ms altogether, 996 us on average, the worst since the previous block 1497 us (…)
```

This is NOT the same as `woken->started running`: a wake-up sees only those who were
woken, while a thread whose core was taken away by the quantum was woken by nobody — it is in
the queue for a whole foreign quantum, and that is not in the wake-up's histogram. The live version of
the same quantity is the `WAIT%` column in `top` (the share of the interval during which the thread was
ready and standing): `CPU%` + `WAIT%` around 100 means the thread wants a core and is not getting
it, that is the layout; a low `CPU%` and a zero `WAIT%` means the thread is waiting for something other than the processor.
The gate `talaria.test=rqwait` holds the metric on a workload with a known answer
(three computing threads on one core: the total wait is exactly two walls).

Next to it there are three numbers that separate "the stealing did not work" from "the stealing was not called":

```
stat: stealing: asked 3642, did not find a candidate 2870, refused by the balance 0, cache-hot 745
```

"Asked" is a lot while "it took place" is little — look at which of the refusals prevails:
`refused by the balance` is the selection rule, `cache-hot` is the cooling threshold,
`did not find a candidate` means the neighbours really have no queue. The diagnoses are different, and by
the single number "work steals = 40" they cannot be told apart.

**The unit of a core's work is CYCLES, not ticks.** The tick's period is not one per machine (on
a core with a network waiter it is five times shorter), so `ticks per core` must not be compared
between cores — take `stat: core N: cycles ...`.

A separate case: **the workload has stalled completely**. `talaria.stuck=N` prints if
a task does not leave the core for N ticks in a row, together with the address — an eternal loop and
a lost wake-up differ by exactly the address. `talaria.heartbeat=N` gives
the scheduler's pulse: its absence means exactly one thing — there is no pre-emption any more.

---

## 6. The network

```
netstat        # everything at once: the interfaces, the routes, the sockets with their owners, the datapath
netstat -i     # only the interfaces and the card's counters
netstat -s     # only the sockets
telem          # a histogram of the datapath's latency: the p50/p99, the gaps
```

The order of the analysis:

1. **Is the card receiving?** `netstat -i`, the columns `rx frames`/`rx bytes`. Zeroes mean
   a problem lower than the stack: the wrong driver (`nics`), the wrong address, the interface is
   not up.
2. **Are the transmit drops non-zero?** The `drops` column is the frames thrown away
   because the transmit ring is not freeing up. The counter stands next to the transmit
   deliberately: "tx 8 million" without it looks like health at exactly the moment when
   the device has stopped giving out completions.
3. **Who holds the port?** `netstat -s`, the `owner` column — the number and the name of the thread. On
   a machine with one application the question "who listens" is the question "which of its
   threads", and without the owner it cannot be answered.
4. **Is the socket on that interface?** The `if` column. `listen(0.0.0.0)` in our case
   expands into a listener on every interface; knowing which one a connection arrived on
   is half the analysis of a multi-interface machine.
5. **Where the time inside a round trip is?** `talaria.netprof=<N>` splits the round trip into
   stages: `rx->app`, `app`, `app->tx`, the full `rx->tx` and the price of one pass over the
   stack. That is the only way to tell "it is slow at our end" from "it is slow in the
   network": if the full internal round trip is single microseconds while the RTT is tens of them,
   the gap lies outside our software path.

The levers (`docs/USER-GUIDE.md` §7): `talaria.netidle=1` (sleep on the ring through
MONITOR/MWAIT instead of polling), the LLQ and the ENA's accelerated mode (on by default since v1.4),
the `irq=` in the manifest (take the card's interrupts away from the datapath's core),
`talaria.pollhard=<ns>` (how often to drive the stack when the card is silent: an idle
pass is cancelled by the receive ring's word rather than by the clock).

**WHAT HAS ALREADY BEEN TRIED HERE AND REJECTED BY MEASUREMENT** (session 22, so as not to check it
again): replacing the polling of the stack with an INTERRUPT from the card. The mechanism is made entirely and
is closed by gates (`talaria.netirq=1` + the parking of the waiter, `talaria.netpark=`), but
under a microvm the card is a virtio-mmio with a line through the I/O APIC, and the handler is bound
to clear the status and to arm the doorbell: TWO VMEXITs per interrupt. Under load the
line WITHOUT any parking already gave a p50 one and a half times worse, and with the parking the
ceiling fell below the rate. The direction is not closed but conditioned: it is about MSI-X
without an MMIO acknowledgement (the ENA on a metal, a virtio-pci with MSI).

---

## 7. The disk and the file system

```
kstat fs      # the block cache, the blocks, the barriers, the readaheads, the FS lock
df            # the volume's occupancy and the journal's state
```

What to look at:

* **the share of the block cache's misses.** The cache's size is chosen by the machine's volume
  of memory; forcibly it is `talaria.fscache=<MB>`. A large scan that does not
  fit into the cache reads the disk on every pass.
* **the readaheads.** `talaria.readahead=<blocks>`; zero switches it off. On linear
  scans the difference is a multiple — and it does NOT depend on the pieces the
  workload reads in: the window grows by the descriptor's behaviour (the reads go end to end) rather than by
  the request's size. To check it on your own FS: `kernel/user/fsread` prints the speed of
  one and the same file in megabyte pieces and in 4 KB pieces — the numbers are bound to be
  of the same order.
* **the device's barriers.** They are counted separately from the blocks: a write's price is in blocks,
  a barrier's is in the disk's rotational delay, and one cannot see the one behind the other by the number of blocks.
  Many barriers mean the workload calls `fsync` often.
* **the FS lock.** The last two quantities in the lock's line are bound to be **zeroes**:
  "longer than a second" — the waits for the lock that went over a second (each with
  a diagnosis into the console: who holds it; there is no entry into the FS without the lock any more, apart from
  a panic), "foreign entries" — a recursion from another task. Non-zero ones mean that
  the critical section is being held through something long or that it has been pre-empted.
* **the journal.** `journal=data` is dearer in volume, but after a crash the file's contents
  are recovered rather than only the metadata's consistency.
* **the disk's price BY KIND** (`stat: the disk: reads N (X ms, Y us each), writes …,
  barriers …`). Counting the blocks is not enough: a read and a write cost tens of
  microseconds, while a barrier costs tens of MILLISECONDS, and one barrier outweighs a thousand
  writes. It is exactly by that number that "a slow write" is analysed: if the sum over the
  barriers is close to the time the FS lock is held, then the workload is waiting not for the file
  system but for durability.
* **the FS lock and the DEVICE's lock are different things** (since session 22). The first protects
  the state in memory, the second the device itself. For the time of the wait for the disk the first one is
  RELEASED (`talaria.fsdevlock=1` by default, `=0` is as it was), so
  hits in the block cache no longer wait for somebody else's `fsync`. The rule of order on
  which that rests: whoever holds the device never asks for the FS lock.
* **the queue's depth.** The block layer keeps several commands in flight
  (`stat: the disk: commands submitted N, the greatest queue depth D`). A one in
  the depth column means there is no parallelism: then a barrier occupies the queue
  entirely and the reads stand behind it. It is done both for virtio-blk and for NVMe (the
  EBS path): "submit" and "wait for ONE'S OWN command" are separated, in the NVMe the owner is found by
  the command's identifier itself (the CID = the slot's number), and anybody at all may take a completion
  off. The gate requires the depth to be greater than one, on both drivers.

---

## 8. Tuning

### 8.1. The layout over the cores is the main lever

The cell's manifest (`cell.manifest`) or the command line. The full analysis is in
`docs/USER-GUIDE.md` §6.6-6.7; here it is what exactly is cured by what.

| The symptom | The knob | The meaning |
|---|---|---|
| the main thread is being pre-empted | `pin=main:dedicated` | a core without a timer, run-to-completion |
| the receiving threads get in each other's way | `pool=fast:1,2` + `pin=rx*:fast` | a named group of cores |
| the threads have crowded onto one core | `spread=on` | allow the spreading over the common pool |
| a skew between the cores | `balance=on` | a periodic levelling by demand |
| the memory arrives from a foreign socket | `numa=on`, `node=N` | the whole process onto one node |
| the card's interrupts hit the datapath | `irq=N` | the routing of the MSI-X onto another core |
| the workload washes out a neighbour's cache | `llc=<to whom>:<mask>` | a share of the L3 through Intel CAT |

To check the result there are `cores` and `top`: there one sees **what really stands in the
hardware** rather than what is written in the manifest. An `llc=` rule for a non-existent group of
cores is rejected loudly — a share of the cache applied silently would be isolation on
paper.

**A thread pool with threads BY THE NUMBER OF CORES is the most fragile case.** While there are more threads
than cores, the extra ones plug any skew with themselves, and the layout is almost invisible. It is exactly
at equality that it is visible entirely: ClickHouse's `max_threads=4` on four cores
gave 83, 110 or 163 ms from boot to boot (four cores, three or two), while the same
query with `max_threads=6` gave a steady 86 against Linux's 84. If your workload
starts exactly one thread per core, one must not judge it by a single run; the knob
by which this is analysed is `talaria.runhist=1` (§5.1).

The stealing of work (`talaria.stealbal=`, by default 3) moves a ready thread from
an overloaded core onto an empty one. The values: `1` is the balance by the ASSIGNED tasks
(sleepers in the pool are counted as load — that is how it was and it is wrong), `2` is by the READY ones,
`3` is by the ready ones and without the cooling threshold. The cooling threshold protects a cache-hot
thread from a move, but two ready threads on one core replace each other every slice and
therefore NEVER pass it — it protected exactly those for whose sake the stealing is needed.
To switch the stealing off entirely there is `talaria.nosteal=1` (for a counter-measurement).

### 8.2. The scheduler's slice and THE TICK'S PRICE

`talaria.slice=N` is the initial counter of the LAPIC timer. A short slice: a faster
reaction to a wake-up, but more switches and a colder cache. A long one: the other way round.
When changing the slice, remember that you also change the **sampling rate of the profile** (§4.1).

On a dedicated core there is no slice at all — the thread owns the core until it leaves.

**The slice is the FREQUENCY at which the tick's price is paid; the price itself is a separate
quantity, and one has to look at both.** `kstat` prints it directly:

```text
stat: the tick's price: 2288 ns on average, at most 4530263 ns (1180 ticks)
```

Two thousand ticks a second at 2 us is 0.4 % of the processor; the same two thousand at
35 us is already 7 %, and it looks like "the system is simply slower on computation",
because what is lost is not the switches but the cycles between them. The rule: before
lengthening the slice for the sake of throughput (and losing on the wake-up's latency),
look at whether it is not cheaper to make the tick cheaper. That is how 22.5 us for taking the
PMU counters on every tick and 10.2 us for acknowledging an interrupt were found.

**Under a hypervisor what is dear is exactly what exits to the host.** On a machine without APICv
(`enable_apicv=N` in `kvm_intel` — that is how it is on a Sandy Bridge and on a c6i) an access to the
LAPIC window is an exit WITH AN EMULATION OF THE INSTRUCTION, about ten microseconds; `rdpmc` is
of the same order; a read of the legacy COM1 port is too. So the kernel goes round them
with KVM's paravirtual protocols, and `kstat` shows whether they work:

```text
stat: interrupts: LAPIC xAPIC (MMIO), EOI past PV 24, IPIs by a hypercall 5130 / through the ICR 0
```

The knobs for a bisect (all of them are "switch it off and measure again", §3 step 4):
`talaria.pmutick=<ms>` (the step of taking the counters; 0 — do not take them),
`talaria.pveoi=0` (the acknowledgement of an interrupt only through the LAPIC),
`talaria.pvipi=0` (a doorbell to a neighbouring core only through the ICR),
`talaria.x2apic=0` (the LAPIC through MMIO rather than through MSRs).

**THE TICK'S PERIOD IS NOT ONE PER MACHINE.** A tick is a pre-emption, and it costs the same
everywhere while it is needed differently. The kernel chooses the period itself (`stat: the tick's period
was reconfigured N times`):

| when | the period | why |
|---|---|---|
| somebody on the core is waiting for the NETWORK | `talaria.netslice=<us>`, 100 | there are no interrupts from the card: a frame is seen only by the thread that waits for it, and while a computing neighbour holds the core, NOBODY sees the frame |
| there is NOBODY to pre-empt on the core | `talaria.idleslice=<us>`, 2000 | a tick in solitude is a pure tail: `actors pingpong raw` p99.9 3767 ns against 473 |
| otherwise | `talaria.slice=<ns>`, 500000 | chosen by measurement by the throughput |

Both knobs at zero mean the period is common, as it was.

A long tick has already uncovered somebody else's defect once: the collapse of `actors fanout tsync
spread` (25 us per message instead of 1.5) turned out to be not a property of a rare tick but
THE STEALING OF WORK, which turned the skew over instead of smoothing it out.
That is a general property of thinning the tick out: it does not create new errors but makes
visible those that depended on the frequency of entering the scheduler. The analysis is in
`docs/TODO.md`, session 20; the instrument with which it was made is the journal of the first steals
in `stat:`.

**The analysis of the tick's price by phases** is `talaria.tickprof=1`: `stat:` prints seven
lines (the saving of the FPU, the EOI and the timer, the clock, the cleanup of the AS, the housekeeping, the accounting, the choice,
the switch). Without it "the tick is dear" and "there are many ticks" are indistinguishable, while they are
cured differently: the first by making a phase cheaper, the second by thinning the period out.

### 8.2a. WHOM the tick takes off a core and whom it does not

The tick's period (§8.2) answers the question "how often to pre-empt". There is a second one, and
under a "request — answer" workload it matters more: **whom exactly to take off.** The tick comes
at an arbitrary phase of a request, and a thread that has processed half of it leaves the core for
a full rotation of the scheduler. In the latency that is visible not as a tail but as a BAND:
a share of the answers between 140 and 400 us at a median of 90.

Two knobs, both about this, and both chosen by measurement rather than by taste:

- **`talaria.tickmin=<ns>` (the default 50 000)** — a rotation BY THE TICK does not take off
  a task that has run less than the threshold. On a core where a network waiter lives, the tick goes
  once every 100 us, while the waiters give the core up every 5 us — almost every tick found
  a task younger than the threshold and took it off for nothing. PostgreSQL's `select1`: the `p99`
  80 -> 67 us, the class of round trips of 40-60 us 3.7 % -> 0.07 %. The waker's hint
  (a directed switch) does not obey the threshold.
  **Turning it up is useless:** `tickmin=200000` gives a third fewer stops
  and WIDENS the band (0.49-0.97 % against 0.22-0.75) — the band is fed by the LENGTH of a
  stop rather than by their number.
- **`talaria.reqfirst=<ns>`** — a thread that has a REQUEST IN PROGRESS (a `read`/`recvmsg`
  has returned data, a `write`/`sendmsg` has not been called yet) does not give the core up to a NETWORK
  WAITER until N nanoseconds have passed since the start of the request. The knob is narrow on purpose:
  the instrument (`talaria.appprof=1`) showed that under load it is exactly a waiter that takes the core
  from such a thread — 94-100 % of the rotations — while another thread in a request does so in only
  0-6 %; to shield from that one too would mean going back to the long quantum, which is harmful.
  The ceiling is an anti-starvation measure: it has expired, and there is no protection.
  **The default is 0, and that is the result of a measurement rather than caution:** the knob removes three
  quarters of the stops in the middle of a request (17 555 rotations over the window against 3 916), while
  the band does not narrow either at a ceiling of 200 or of 100 us. The reason is the same as with
  `tickmin`: the core taken from a waiter (it gives it up every 5 us) goes to
  a thread that does work and holds it longer. The analysis is in `docs/SCHED-43.md`.

The instrument with which such things are analysed is `talaria.appprof=1`: it divides the stage
`read->write` per thread into "it ran on the core" and "it was taken off", and at every rotation
in the middle of a request it names the class of the thread the core went to, and the time until
the return of the one taken off.

### 8.3. The memory

* `talaria.huge=<MB>` — the threshold from which the allocations go in huge pages of
  2 MB. One entry in the table and one TLB line per 2 MB instead of 512; under
  a hypervisor it matters doubly (a TLB miss drags in a nested EPT walk). `kstat
  paging` will show how many huge pages have really been given out and how many times a contiguous
  piece was not found.
  **FIRST LOOK AT THE NUMBER GIVEN OUT, not at the time.** The default is 32 MB, that is
  in practice switched off, and twice in a row the conclusion "huge pages give no
  gain" was drawn from measurements where EXACTLY ZERO were given out at hundreds of reservations.
  Half a gigabyte of free memory is needed as well: on a guest with 512 MB the mechanism
  will not switch on at all.
  Whom to switch it on for: a workload that GOES RANDOMLY OVER MEGABYTES. In ClickHouse
  `talaria.huge=2` gives `uniqExact` 253 -> 197 ms and a `GROUP BY` of 100 thousand
  235 -> 197. Whom NOT to switch it on for: those who measure themselves in microseconds — on a `redis
  ping` it costs one and a half microseconds of the p50 (there become one and a half times more TLB flushes
  to the neighbours), while the frames given out are 42 % more, because
  a huge page materialises entirely.
* `talaria.fscache=<MB>` — the size of the FS's block cache (0 — a share of the machine's memory).
  The misses are visible in `kstat` (`the FS block cache: hits X, misses Y`); every
  miss is an access to the device, and that goes under the FS lock.
* `talaria.readahead=<blocks>` — the read-ahead window (0 switches it off). The window grows
  by the descriptor's BEHAVIOUR while the reads go end to end, and does not depend on the request's
  size: a workload reading a file in 4 KB pieces gets accesses to the device just as large
  as one reading in megabytes. How many times the window
  fired is in `kstat` (`readaheads N (+M blocks)`); to measure it there is
  `kernel/user/fsread` (the gate `talaria.test=fsread`).
* `kstat mem` — how much memory there is, how much is free and how much the kernel itself
  occupies. The `ALLOC/s` column in `top` is the speed at which the workload takes frames;
  a steadily non-zero one at a constant RSS means that memory is being allocated and
  freed round and round.

### 8.4. The waiting tier

`talaria.sync=hlt|mwait|mwaitx|waitpkg` — the waiting primitive forced instead of
the one chosen by the detection. It is needed in order to tell "it is about the tier"
from "it is about something else" in one run: under a hypervisor the tier is always `hlt+ipi`, on hardware it is `mwait`,
and the difference on wake-ups is hundreds of nanoseconds. What is chosen now is in `kstat sync`.

---

## 9. How to measure so as not to measure nonsense

This section is paid for by mistakes rather than by reasoning. Every item is a real
case where a measurement compared not what it meant to.

1. **Compare identical media.** A Linux guest with the data in tmpfs against
   Talaria with the data on the disk compares not the operating systems but RAM with a disk. That cost
   four false conclusions in one session.
2. **Warm up.** The first pass reads the disk and builds the block cache, the second one already does
   not. Measure the steady state, unless it is exactly the cold start that interests you.
3. **Repeat.** One run is not a number. The spread between the runs is bound to be
   smaller than the effect you are looking for; if it is not smaller, you have not measured the effect.
4. **Change one thing at a time.** Two knobs at once give four hypotheses and not a single
   answer.
5. **Look at the tails, not at the average.** Talaria exists for the sake of the `p99`/`p99.9`.
   The average hides exactly the quantity for whose sake everything is done: `telem` gives
   the percentiles rather than only the sum.
6. **Do not measure with the syscall profile switched on.** `talaria.sysprof=1` costs
   a pair of `rdtsc`s per call (~30 ns at a syscall price of ~100 ns). That is acceptable for
   diagnostics and unacceptable for a measurement.
7. **Check that the mode is the right one.** The mode's name is compared as a whole, and a typo in
   `talaria.test=`/`mode=` is printed into the log as "there is no such mode". Before, a typo
   silently took the run onto the default path — and the run measured the wrong thing.
8. **Under a hypervisor you measure the hypervisor too.** The difference between 2.00 and 1.86
   in the speedup on two cores is the pre-emption of a vCPU by the host rather than your code. On
   a `*.metal` that component is absent.
   a `*.metal` that component is absent.

---

## 10. Debugging

### 10.1. GDB

The kernel carries a gdb stub that speaks the GDB Remote Serial Protocol over **COM2**
(0x2F8). For the connection, `perf gdb` in the console will remind you of the order:

```sh
# 1) load the kernel in the debugging mode
qemu-system-x86_64 ... -append "console=ttyS0 talaria.mode=gdb" \
    -serial file:/tmp/o.log \
    -serial tcp::1234,server,nowait      # the SECOND port = COM2 = gdb

# 2) on the host
gdb kernel/target/x86_64-talaria/release/talaria
(gdb) target remote :1234
```

What is supported: the reading and the writing of the registers (`g`/`G`), the reading and the writing of memory
(`m`/`M`), a step (`s`), a continuation (`c`), software breakpoints (`Z0`/`z0`),
**hardware watchpoints** through the DR0-DR7 (`Z2`/`Z3`/`Z4`), the list of the threads.

To switch the stub on on an **already running** machine there is `gdb on` in the operator's console.
As a separate command rather than automatically: the stub intercepts the `#DB`/`#BP`, and on
a machine where the workload sets breakpoints itself that changes its behaviour.

The limitations that have to be known in advance:

* **stopping in the debugger stops the whole kernel**, including the datapath. This is not
  "one process has stopped", as under Linux: the connections will drop by a timeout,
  the devices' rings will overflow. Live trading must not be debugged this way.
* on bare metal COM2 is a **physical port**; connect through it as well.
* the ring 3 threads are visible to the stub as contexts, but there is no full-blown switching of threads
  by GDB's means (`Hg`) with the restoration of their frames.

### 10.2. When there are not enough counters

The PMU counters are a hardware resource, and their number is finite. Talaria does not trust the
CPUID's declaration but **checks the counters against the deed** at the bring-up of every core: it sets them up,
spins a short loop and looks at whether they have moved. The reason is concrete: KVM declares
three fixed counters and arch-perfmon version 2, while `rdpmc` on them returns
zero — they are not emulated. To trust the declaration means silently to lose the IPC, the very
first quantity that is asked for.

The layout is printed at the boot in one line (`talaria: the PMU: cycles=…
instructions=… LLC-miss=…`), where `f1` is a fixed counter, `2` is a general-purpose
counter, `-` means **no counter was got**. If there are only four slots,
the numerators (the misses) are taken while the denominators are sacrificed: the MPKI
is computed, the share is not. That is visible from the output: the share is printed only when there is
something to compute it from.

### 10.3. Core dumps: `gdb` over the crashed one

The textual crash dump is always printed and answers "what happened". "And what
was in the variable" and "show me the stack with names" are answered by a debugger, and a debugger
needs its own format — so Talaria writes an ordinary **ELF `ET_CORE`**, the very one
gdb expects from Linux.

```
talaria.core=/core.elf          # where to write it (an absolute path on a mounted FS)
talaria.core=/core.elf,full     # + all the process's private memory rather than only the stacks
```

What is dumped and when:

| the event | the dump |
|---|---|
| a fatal signal to the workload (a SIGSEGV without a handler) | the process: an `NT_PRSTATUS` per thread + the stacks |
| an exception in the kernel | the kernel: the registers of the exception's frame + the kernel stack |
| `core [tid]` in the console | a LIVE task: where it stands right now |

It is read in exactly the way a Linux one would be read:

```sh
kernel/tools/talaria-file 10.0.0.2 pull /core.elf ./core   # take it off the node
gdb ./myapp ./core
(gdb) bt
(gdb) thread apply all bt
(gdb) info registers
```

For the kernel it is the same way, only the binary is different:

```sh
gdb kernel/target/x86_64-talaria/release/talaria ./core
```

**The heap and the data are NOT dumped by default.** In ClickHouse that is 700 MB, and writing
them at the moment of the crash means with a high probability writing nothing. The stacks
of all the threads take a few megabytes, and they are enough for a `bt`, the arguments and
the local variables; the code and the symbols gdb takes out of the binary you gave it.
If the data is needed there is `,full`, and then think about the room on the volume.

**There is a stack in the log even without a dump.** The crash dump prints a chain of addresses
(`the stack workload (5 frame(s)): 0x... 0x...`) — on a machine where there is neither an FS nor
a debugger that is the only thing that will be left, and that is often enough:
`addr2line -f -C -e <binary> <address>`.

The chain's depth obeys the same rule as the profile (§4.1a): frame
pointers in the workload, `kernel/build-prof.sh` in the kernel. Without them the top frame
is visible — honestly, but not much.

### 10.3a. A hung application

A debugger is no help here: entering the gdb stub stops **the whole kernel**,
including the datapath. The right tool is to take a dump without stopping the machine:

```
core          # a dump of the main thread (and of the stacks of all the rest)
core 7        # a dump of a specific thread
```

The registers in this case come from the context saved by the scheduler, that is, they show
**where the thread stands** rather than where it crashed. For a hung application that is exactly the
question that is asked.

How that looks on a live ClickHouse (the dump was taken by the `core` command during a
query, and the machine kept working):

```
$ gdb ./clickhouse ./core
(gdb) info threads
  Id   Target Id   Frame
* 1    LWP 1       0x0000000021042a41 in ?? ()
  7    LWP 7       0x1512288d in DB::AggregateFunctionUniq<...>::addBatchSinglePlace(...)
  ...
```

Thread 7 is computing an aggregate — the function is visible. The rest stand IN A SYSCALL, and their address
belongs to the KERNEL (`0x21042a41` is `sync::wait_on_ex`), so with the workload's binary
gdb shows `??`. That is not a defect of the dump: the thread really is there. To
see the names of those frames, open the same file with the kernel:
`gdb kernel/target/.../talaria ./core`.

### 10.4. A machine without a console

A `*.metal` behind an external perimeter, at night, without an operator:

* `talaria.top=<ms>[,rss]` — a snapshot of `top` into the log every N milliseconds. The same
  trick as `vmstat 5 > file` on a machine on duty, only without a process and without a file.
  The walk over the tables for the RSS is off by default (it costs milliseconds).
* `talaria.trace=<LBA>` — the kernel's whole output as raw sectors onto the disk, read by
  `infra/aws/read-disk-trace.sh`. After a hang the log lies on the disk exactly up to
  the place of the stop.
* `talaria.heartbeat=N` — the scheduler's pulse.
* the crash dump at an exception prints the registers, the stack and the ring of events
  (the flight recorder) — a post-mortem without a debugger.

---

## 11. The checklist before a production launch

- [ ] The manifest sets the layout: the main thread on a dedicated core, pools for
      the groups of threads, an `irq=` takes the card's interrupts away from the datapath's core.
- [ ] `cores` confirms that the layout **has been applied** (rather than rejected).
- [ ] `kstat sched`: on the dedicated cores the timer is `IDLE`, there are no migrations and no steals.
- [ ] `perf`: the IPC is in the expected range, the LLC MPKI does not grow with time.
- [ ] `netstat -i`: there are no transmit drops; the addresses are on the right interfaces.
- [ ] `kstat fs`: at the FS lock "longer than a second" and "foreign entries" are zeroes.
- [ ] `kstat paging`: the inter-core TLB flushes "NOT ACKNOWLEDGED" are zero.
- [ ] `talaria.top=<ms>` is switched on if the machine will have no live operator.
- [ ] `talaria.sysprof`, `talaria.strace`, `talaria.netprof`, `perf on`
      are **switched off** — that is diagnostics rather than a working mode.
- [ ] The tokens are set (`talaria.ctltoken=`, `talaria.filedtoken=`), otherwise
      the control plane and the file service are closed — that is how it is meant to be.

---

## 12. The reference of the observability commands

| The command | What it does |
|---|---|
| `top [-t] [-m] [-1] [ms]` | a refreshing table; `-t` is per thread, `-m` is without the walk over the tables for the RSS, `-1` is one snapshot |
| `ps [-t] [-m]` | one snapshot of the same table |
| `netstat [-i\|-r\|-s\|-d]` | the interfaces / the routes / the sockets / the datapath |
| `kstat [section]` | `sched`, `mem`, `paging`, `fs`, `net`, `sync`, `irq`, `cache`, `syscalls` |
| `perf` | the hardware counters per core + a summary with recommendations |
| `perf poke <N>` | take the counters of the dedicated core N (one IPI into the thread) |
| `perf on [hz]` / `off` / `reset` | the sampling profile with a stack |
| `perf top` | the top of the leaves and of the most frequent stacks |
| `perf record [N]` | a dump of the samples (the last N) for `kernel/tools/talaria-prof` |
| `perf code` | a histogram by the pages of the code |
| `perf gdb` | how to attach the debugger |
| `gdb on` | switch the gdb stub on on a live machine |
| `core [tid]` | take a core dump of a live task (without stopping the machine) |
| `cores` | the layout of the threads over the cores and the L3 shares |
| `stat` | all the kernel's counters in one stream (a post-mortem) |
| `telem` | the datapath's telemetry: the percentiles of the latency |
| `log` | the kernel's ring of events (the flight recorder) |

Inside `top`: `q` is quit, `t` is threads/processes, `m` is the RSS on/off, `c` is sorting
by the CPU, `s` is by the memory, `+`/`-` is the interval.

| The cmdline knob | What it switches on |
|---|---|
| `talaria.top=<ms>[,rss]` | a periodic snapshot of `top` into the log |
| `talaria.prof=<hz>` | the sampling profile from the boot (`=1` is on every tick) |
| `talaria.pmutick=0` | do not take the PMU counters in the tick |
| `talaria.sysprof=1` | the price of the syscalls by number (`stat`) |
| `talaria.netprof=<N>` | the stages of the network datapath |
| `talaria.strace=N` | the workload's first N syscalls |
| `talaria.stuck=N` | a task does not leave the core for N ticks — print the address |
| `talaria.heartbeat=N` | the scheduler's pulse |
| `talaria.slice=N` | the slice's length (and the rate of the profile's samples) |
| `talaria.netslice=<us>` | the slice on a core where somebody waits for the network (0 — the common one) |
| `talaria.idleslice=<us>` | the tick's period on a core where there is nobody to pre-empt (0 — the common one) |
| `talaria.tickprof=1` | the tick's price by seven phases (`stat`) |
| `talaria.uacheck=0` | switch the check of the syscalls' pointers off (an A/B of its price: ~10 ns per syscall with a pointer, `mbench read64`) |
| `talaria.nobalance=1` | switch the periodic levelling off (its contribution is measured against it) |
| `talaria.trace=<LBA>` | the kernel's whole log as raw sectors onto the disk |
| `talaria.console=nopoll` | do NOT poll the console from the tick (a machine without an operator) |
| `talaria.core=<path>[,full]` | ELF core dumps of the workload and of the kernel |
| `talaria.sigtrace=N` | the first N synchronous signals with the state of the page tables |

### The tools on the host

The kernel does not invent formats of its own — everything it gives out is read by ordinary
tools. Two of them have to be at hand IN ADVANCE: without them the data collected
will stay a set of addresses.

| The tool | What it reads | How |
|---|---|---|
| `kernel/tools/talaria-prof` | the output of `perf record` | `talaria-prof report --kernel <the kernel> --user <the binary> < serial.log`; `folded` is the input for `flamegraph.pl`; `raw` is without symbols |
| `gdb` | a core dump (`talaria.core=`) | `gdb <the binary> <core>` -> `bt`, `thread apply all bt`, `info registers` |
| `addr2line` | a single address from the log | `addr2line -f -C -e <the binary> <the address>` — that is how a stack from a crash dump is resolved without any dump at all |
| `kernel/tools/talaria-file` | take files off a live node | `talaria-file <ip> pull /core.elf ./core` |
| `kernel/build-prof.sh` | — | build the kernel with frame pointers: the ring 0 stack in the profile and in the dumps (§4.2) |
| `readelf -h/-n` | check a dump | `Type: CORE` and the notes `NT_PRSTATUS` — a quick check that the file is intact |

`talaria-prof` demangles **Rust v0** itself (`addr2line -C` can do C++ and the legacy mangling,
but not v0, and the kernel is built with exactly that) and looks for the addresses in BOTH binaries: the ring 0 stack
flows over into ring 3, and a workload's frame in the kernel's stack is marked `[ring3]`.
| `talaria.futexhang=<s>` | "IT HUNG" on a long futex wait (it keeps silent by default) |

---

## 13. What there is not in Talaria

An honest list, so as not to search:

* `perf_event_open`, `/proc`, `/sys` — the observability does not go through the file system;
* PEBS and the binding of an event to a specific instruction ("this `mov` missed
  past the L3" cannot be asked here);
* a dump of the kernel's memory as a whole: into the kernel's core dump go the registers and the kernel stack rather than
  its whole image — for a `bt` that is enough, for "look at a global
  structure" it is not;
* the unwinding by `.eh_frame`: the stack is taken only by frame pointers, and for
  code without them one leaf is visible (§4.1a);
* hardware timestamps of the network packets (IEEE-1588 / HW timestamping);
* DDIO/SDCI — they are inapplicable in our configuration;
* dynamic tracing (eBPF, kprobes) — the instrumentation points are set in
  the kernel's sources and are switched on by the cmdline's knobs;
* profiling by event (sampling on the overflow of a PMU counter) —
  the samples are collected by the scheduler's tick, so the ceiling of the profile's rate equals
  the tick's rate.
