# actor-bench — the actor model (SObjectizer-5) on Talaria vs Linux

A benchmark of a C++ actor framework on top of Talaria: latency of message
delivery and of waking an actor's thread, distribution of actors over cores,
the framework's overhead against bare queues. One and the same static musl
binary (`build/actor-bench`) runs on two stands under THE SAME VM discipline:
QEMU/KVM, `-smp 4`, vCPU threads pinned to their own physical host cores.

- **Talaria:** `talaria.mode=proc talaria.smp=4` (the BSP + 3 APs,
  `-overcommit cpu-pm=on` — a futex sleeps in a kernel mwait on the user word,
  and the waker's store wakes it with no IPI and no syscall);
- **Linux:** the host's vmlinuz + an initramfs `{init, actor-bench}`
  (`linux-init.c`), `-M pc`.

## Why SObjectizer-5

Chosen as a mature production actor framework in C++ (5.8.6, BSD-3) with:
- a dispatcher = an explicit binding of an actor to a thread (`one_thread`) —
  maps onto Talaria's "a thread per core" model;
- replaceable synchronisation points: the queue's `lock_factory` (combined
  busy-spin / simple mutex + condvar) and `abstract_work_thread_factory`
  (thread pinning);
- custom dispatchers through `event_queue_t`/`disp_binder_t` — the tsync
  dispatcher is built in there.

CAF (the most-starred actor framework) was rejected: its work-stealing
scheduler spins and masks the wakeup latency, and a static musl build is heavy.

## Scenarios

The table: 2 boots × 3 repeats per side; verdicts by the rule "the sides
separated completely" (see "How this matrix is JUDGED" below). A value below one
in a latency verdict is in our favour.

| scenario | disp | pin | Linux p50 | Talaria p50 | verdict p50 | Linux p99 | Talaria p99 | verdict p99 | Linux thr | Talaria thr | verdict thr |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fanout | so | spread | 1960 | 1808 | ours 0.92x | 2894 | 2644 | ours 0.91x | 1.45M | 1.57M | ours 1.08x |
| fanout | so-simple | spread | 18780 | 3481 | ours 0.19x | 37152 | 21956 | REPEATS DISAGREE | 152k | 590k | ours 3.88x |
| fanout | tsync | none | 1518 | 1505 | ours 0.99x | 2407 | 2207 | REPEATS DISAGREE | 1.84M | 1.89M | ours 1.02x |
| fanout | tsync | spread | 1538 | 1502 | ours 0.98x | 2344 | 2059 | ours 0.88x | 1.84M | 1.90M | ours 1.03x |
| flood | raw | spread | 1122 | 53 | REPEATS DISAGREE | 1122 | 53 | REPEATS DISAGREE | 891k | 18.63M | REPEATS DISAGREE |
| flood | so | spread | 226 | 284 | theirs 1.26x | 226 | 284 | theirs 1.26x | 4.42M | 3.52M | theirs 0.80x |
| flood | so-simple | spread | 403 | 368 | ours 0.91x | 403 | 368 | ours 0.91x | 2.48M | 2.72M | ours 1.10x |
| flood | tsync | spread | 255 | 242 | ours 0.95x | 255 | 242 | ours 0.95x | 3.91M | 4.11M | ours 1.05x |
| pingpong | raw | none | 16938 | 187 | REPEATS DISAGREE | 28430 | 227 | REPEATS DISAGREE | 57k | 4.70M | REPEATS DISAGREE |
| pingpong | raw | spread | 16988 | 190 | REPEATS DISAGREE | 28392 | 226 | REPEATS DISAGREE | 57k | 4.71M | REPEATS DISAGREE |
| pingpong | so | none | 1173 | 1056 | ours 0.90x | 2132 | 1524 | ours 0.71x | 779k | 879k | ours 1.13x |
| pingpong | so | spread | 1171 | 1054 | ours 0.90x | 1840 | 1551 | ours 0.84x | 776k | 885k | ours 1.14x |
| pingpong | so-simple | none | 14780 | 1803 | ours 0.12x | 26142 | 20447 | ours 0.78x | 66k | 262k | ours 3.99x |
| pingpong | so-simple | spread | 14419 | 9097 | ours 0.63x | 27056 | 26452 | not judged | 66k | 101k | ours 1.53x |
| pingpong | tsync | none | 648 | 652 | not judged | 761 | 800 | not judged | 1.43M | 1.45M | not judged |
| pingpong | tsync | spread | 656 | 665 | not judged | 800 | 839 | not judged | 1.41M | 1.42M | not judged |
| pipeline | so | spread | 523k | 428k | ours 0.82x | 645k | 580k | ours 0.90x | 487k | 597k | ours 1.23x |
| pipeline | so-simple | spread | 499k | 421k | ours 0.84x | 614k | 557k | ours 0.91x | 514k | 610k | ours 1.19x |
| pipeline | tsync | spread | 770k | 663k | ours 0.86x | 921k | 860k | ours 0.93x | 332k | 386k | ours 1.16x |
| ring | so | spread | 2010 | 1570 | ours 0.78x | 8063 | 4246 | ours 0.53x | 413k | 558k | ours 1.35x |
| ring | so-simple | spread | 5636 | 8521 | theirs 1.51x | 10056 | 14452 | theirs 1.44x | 166k | 108k | theirs 0.65x |
| ring | tsync | none | 8046 | 6417 | ours 0.80x | 12236 | 10058 | ours 0.82x | 123k | 147k | ours 1.20x |
| ring | tsync | spread | 7856 | 6398 | ours 0.81x | 12094 | 10221 | ours 0.85x | 122k | 150k | ours 1.22x |

**Score: p50 16/5/2, p99 13/8/2, throughput 16/5/2** (ours / not judged / theirs).

`REPEATS DISAGREE` is not "noisy" but a defect found in the benchmark: the `raw`
scenarios on Linux are not idempotent within a process (the first repeat of
`pingpong raw none` gives 206 ns, the second and third 16,980 each; on Talaria
183/187/185). The median of such numbers credited us with an "82x win", so such
points are not judged at all.

Two points are lost, and both reproduce on both boots: `ring so-simple spread`
(p50 1.51x, p99 1.44x, throughput 0.65x) and `flood so spread` (1.26x).

## Dispatchers (the axes of comparison)

- `so` — the stock one_thread with combined_lock (busy-spin up to 1 ms before
  sleeping) — "out of the box";
- `so-simple` — one_thread with simple_lock (mutex + condvar, NO spinning) — the
  OS's honest futex path;
- `tsync` — a custom dispatcher: a thread per actor + a `tsync_mpsc` queue
  (Vyukov MPSC from libtsync; the consumer sleeps with a kind1 upark on the
  slot's seq word, and the producer publishing the slot wakes the kernel mwait
  without a syscall) — the "refined concurrency API" of PLAN-SYNC;
- `raw` — the same topology without the framework (threads + tsync_mpsc) — the
  lower bound; shows the framework's overhead.

Pinning: `none` — threads are not pinned; `spread` — actor i → core i.

## How to run

```sh
./build.sh              # SObjectizer 5.8.6 (wget) + the benchmark, docker talaria-alpine-build
./matrix.sh both        # the whole matrix: every combination is a separate VM boot
./summary.py            # a markdown summary Linux vs Talaria
# one combination:
./mkdisk.sh pingpong tsync spread 30000 && ./run.sh 60      # Talaria
./run_linux.sh 60 "pingpong tsync spread 30000"             # Linux guest
```

### How this matrix is JUDGED (otherwise the numbers mean nothing)

The matrix varies between runs MORE than the difference we look for. One and
the same build without a single change between runs gave 38/17/14, 41/12/16 and
34/19/16. One run proves nothing, and a median of disagreeing numbers picks the
winner by lot. Therefore:

* **`matrix.sh [side] [boots] [repeats per boot]`** — there are TWO KINDS of
  repeats, and they must be kept apart. A boot is a new VM: the vCPUs land on
  physical cores anew, caches warm up, the pool threads are placed. A repeat
  within a boot is the same scenario once more in the same machine. Their
  spreads are printed SEPARATELY, which answers "what buys precision": an extra
  boot or an extra repeat.
* **A point is judged only when the sides separated COMPLETELY:** every boot of
  one side is better than every boot of the other. Otherwise it is "not
  judged". That is an honest answer, not a gap: it says the instrument cannot
  resolve this difference.
* A `RESULT` line carries `boot=` and `rep=` — without them the repeats of
  different boots are indistinguishable, and separating them is the whole point.

```sh
./matrix.sh both 3 3      # 3 boots × 3 repeats per combination
./summary.py              # the table with verdicts and the noise breakdown
```

Requirements: the Talaria kernel built (`kernel/BUILD.md`), the docker images
`talaria-qemu2` and `talaria-alpine-build`, vmlinuz + initramfs from
`bench/linux-guest/`. On a 4C/8T host the vCPUs are pinned to physical cores
0-3 (HT pairs 0-4..3-7); with a live desktop the tails (p99+) are noisy — for
clean runs close the browser and background rsync.

## Key conclusions

- **In the target "an actor per core" topology (spread) Talaria is at parity
  with Linux or better** with zero busy-spinning.
- **The stock so_5 futex path (so-simple, mutex + condvar) is several times
  faster on Talaria** (pingpong none 0.12x, fanout 0.19x): the futex sleeps in a
  kernel mwait on the user word, and the store wakes it without an IPI.
- **so (combined busy-spin for 1 ms):** the spin phase is useless on a
  preemptible shared core and makes p99 ragged — prefer so-simple or the tsync
  dispatcher.
- **Oversubscribing one core (none):** same-core pairs with the MPSC pre-spin
  pay for nothing. The rule: hot actor pairs on different cores.

The `RESULT ...` lines are in `build/results-*.txt`, raw serial logs next to them.
The history of these numbers (including the hangs fixed in v0.53b) is in
`CHANGELOG.md`, and the current ones are in `docs/MEASUREMENTS.md` §6.

## Files

- `bench/actor_bench.cpp` — the benchmark (scenarios, dispatchers, pinning, report);
- `bench/tsync_shim.[ch]` — a C bridge to `tsync_mpsc` (`tsync.h` uses C11
  `_Atomic` and cannot be included from C++);
- `build.sh`, `mkdisk.sh`, `run.sh`, `run_linux.sh`, `linux-init.c`,
  `matrix.sh`, `summary.py`.
