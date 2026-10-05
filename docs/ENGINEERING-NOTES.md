# Talaria OS — engineering notes: the pitfalls and the rules

This file is the part of forty-eight working sessions that outlives them: the
traps that were stepped into, the rules that were paid for, and the hypotheses
that are already closed. It exists so that the same day is not paid for twice.

**The audience is whoever changes this system — a human or an AI agent.** Three
neighbouring documents cover different ground and are not repeated here:

| Document | What it holds |
|---|---|
| `docs/DEVELOPMENT-GUIDE.md` §12 | the **kernel's invariants**: what breaks if you edit paging/sched/syscall (CR3 on a shared stack, identity DMA, the ext2 preallocation window, PAT per core, …) |
| `docs/BARE-METAL.md` | what **QEMU hides**: zeroed memory, dense APIC ids, a live COM1, assigned BARs — every item paid for by a run on a `c6i.metal` |
| `bench/suite/README.md` §3 | the **measurement discipline of the comparative suite**: the ladder, the ceiling, the repeats, the rejection rules |

What follows is method rather than code: how to judge a number, how to write a
gate that cannot lie, and which specific tools betray you.

---

## 1. The rules of performance work

These nine rules were bought one session at a time, mostly by losing a day. They
are not up for revision; a proposal that contradicts one of them is a proposal
to repeat a measurement that has already been made.

1. **A class exists only if it reproduced on TWO stand bring-ups.** The same
   kernel binary, measured twice, gave `mongodb` p50 11/2/0 against 11/1/3 and
   p99.99 12/0/0 against 11/1/2 — three median losses appeared without a single
   line of code changing. On one bring-up you have a *candidate*, not a class.
2. **The instrument comes before the edit.** Two sessions in a row an edit died
   from a measurement made before it was written: group commit (nothing to
   coalesce — the FS lock was taken 597 times out of 597 in 0 µs) and async
   commit (the critical path is the DATA, not the journal copies). The price was
   one stand phase each against days of work.
3. **No edit until the instrument has named the share as a NUMBER.** "It is
   probably the tick" is not a diagnosis. Sessions 42 and 43 both killed a
   plausible fix this way: the instrument said *how much* of the request stage
   was spent off-core (226 µs out of 316), and that number, not the intuition,
   decided what to write.
4. **A mechanism named by the instrument must survive a knob pushed the SAME
   way.** `tickmin=200000` removed a third of the stops in the middle of a
   request and WIDENED the band it was supposed to narrow. That did not kill the
   instrument — it killed the proposed cure: stops must be counted by LENGTH,
   not by number.
5. **Look for the candidate where the edit PAYS, not only where it helps.** The
   `dirtyage` knob was measured in the cost of `fsync`, where it wins 5 %, and
   closed in the cost of `write`, where it loses 76× (+501 µs per call). The
   nine-hour scoreboard planned for it was replaced by two three-minute phases.
6. **"The knob made the tail worse" does not exist without a POSITIVE CONTROL** —
   a phase in the SAME bring-up that reproduces the old behaviour by knob.
   Without it, "no worse" and "the instrument resolves nothing" look identical;
   session 41 spent a whole step repairing a regression that did not exist.
7. **An instrument is judged by its own reproducibility, not by the importance
   of the question.** On one bring-up the cost of an `fsync` call repeated to
   0.2 %, while the p99.9 of the very same point moved by a factor of 6.85. The
   conclusion is drawn with the instrument that reproduces, and a **duplicate
   baseline phase is part of the plan, not a luxury**.
8. **A counter instead of a scan is a LYING edit by default.** When four values
   depend on four arrays at once, the price of one missed wrapper is a lost dirty
   block behind a successful `fsync`. Such an edit needs a gate that compares the
   counter against a FULL RESCAN (`dcnt`); with one wrapper removed it caught the
   divergence on the first turn (6 against 5).
9. **On an accumulating workload the phases are not interchangeable.** A
   duplicate baseline at the END of a long rise did not reproduce the one at the
   start: `mongodb insert` p50 88.6 → 105.0, the share in the band ×14. A
   million inserts had gone into the same collection with a 0.25 GB WiredTiger
   cache. Compare only NEIGHBOURING phases, or keep the rise short.

Two more that belong with them, from `bench/suite/README.md` §3.1:

* **Only points from ONE bring-up may be compared.** Between bring-ups not only
  p99 fails to reproduce (up to 1.6×) but the median as well: an A/B gave mysql
  10/0/0 by median, a fresh stand three hours later gave 2/3/5, with our numbers
  moved by 20 % and Linux's by 5 % the other way.
* **The bottom ladder point is taken FIRST and pays for the warm-up** — a cold
  block cache, a cold buffer pool, lazy faults. The free discriminator is to walk
  the ladder in reverse order on the same bring-up.

---

## 2. The instrument lies more often than the system

Every one of the following actually happened, and every one of them faked a
finding that was then "explained". Half of the hypotheses rejected by
measurement in sessions 28-36 had to be re-opened because of them. **The classes
matter more than the instances.**

**The instrument measures itself.**
* `talaria.stat=` printed fifty lines into an emulated serial port at ~12 µs per
  byte *out of a network waiter's spin* — and drew its own p99 (1.1 % of requests
  ≥ 100 µs). Fix: `talaria.statidle=1`, dumping only in network silence.
* With diagnostics on, the console polling period must be 5-8 s: with
  `talaria.stat=20000` under mongodb the whole 95 KB `get-console-output` window
  rolls over faster than 20 s. And `talaria.stat=5000` does not even DELIVER the
  run's snapshots — the line cannot keep up, snapshots are dropped, and what is
  left in the 64 KB buffer is neighbouring IDLE snapshots with identical counters
  that look entirely real.
* A tick-hole instrument whose diagnostics lengthen the handler makes the failure
  it is looking for three times more frequent.

**The instrument's own side is not asleep.** Our load generator is Talaria too,
and it did not sleep: the host took an 8 ms quantum away from the GENERATOR and
the number was written into the SERVER's tail. That fabricated a whole class
("foreign 8 ms") across three sessions. Two independent witnesses closed it on
one bring-up: events ≥ 4 ms 638 → 0 at the same median, and `talaria.netidle=1`
on the generator → 0 everywhere. `netidle=1` on the generator is now the
instrument's DEFAULT, not an option; removing it is legitimate only for an A/B of
the instrument itself, and such a run may not be judged.

**Arithmetic.** The "ns per call" column overflowed `u64` (`ticks * 1e9 / hz / s`)
and printed 8.19 ms instead of 420 — 51× off from the column next to it. The age
of a dirty block was computed as an UNSIGNED difference, so a fresh block looked
infinitely old. Both are now gated (`sysns`, `agewrap`). **A diagnostic that
prints a plausible number is worse than one that crashes.**

**Cumulative counters.** The `stat:` counters count from boot. Judge only the
DIFFERENCE between neighbouring snapshots: mixed into the absolute numbers is the
workload's start-up, and for mongod that is 31 ms per call against 2.4 ms in the
steady state — a factor of thirteen.

**Selection by the wrong key.** The top-ten-by-time syscall table never contains
`write`/`pwrite64` under mongodb — and they are exactly who pays for the
write-ahead flush. Hence `talaria.sysrow=<numbers>`: ask for the rows you need.

**A dead agent costs hours.** A dead generator agent silently cost a whole
workload's scoreboard (4.9 hours, 12 points of 40). Now `job.py` returns code 3
and the runner abandons that side's ladder with a `!!` line — see it in the log
and raise the agent instead of waiting for the run to end.

**The harness has a regression of its own** — `bench/suite/selfcheck.sh`, half a
minute on the host. Every check in it is a former defect. Run it before believing
a stand.

---

## 3. A gate must prove that it catches

A green gate that cannot fail is not a gate. This has cost real defects.

* **A race gate must prove the race was there.** The first `shmrace` passed
  without checking anything: the section was microseconds long, the threads never
  met, the APs never came up. Cure: a delay knob inside the window, a counter of
  waits printed by the kernel (`stat: shm: creators waited …`) and an assert on
  THE COUNTER, not only on the outcome.
* **A whitespace-only gate without word boundaries LIES.** A canonicalisation
  that drops all whitespace considers `bar_` + `baz` the same as `bar_baz`. Every
  gate must be demonstrated against forgeries — the 7.4 line-splitting gate was
  proved on four (a break inside an identifier, an eaten space inside a string, a
  line of code moved into a comment, an altered kernel message) and two honest
  breaks.
* **Compare the regression's TOTAL with the previous number; never read
  "FAIL=0".** A failed `apk add mtools` while building an image sent five gates
  to SKIP, and the run printed 481/481 FAIL=0 — green, and five checks shorter.
  Likewise `grep -c FAIL` over the log lies: some asserts have the word FAIL in
  their text (`schedbal: … not a single FAIL from the workload`). Count `^  FAIL`.
* **Program strings and gate patterns go in ONE commit**, otherwise a green run
  means nothing.
* **A gate must wait for a SIGN, not for seconds.** `sleep 11` after a guest boot
  was enough until the host's load average hit 25 and a healthy busybox could not
  print its line in time. Wait for the sign, up to a generous ceiling, and exit
  as soon as it appears.
* **An image failure may not go to `/dev/null`.** Five BIOS-path gates vanished
  silently and were caught only by comparing the counter with the previous run.
* A gate that requires EXACTLY zero where the meaning is a *share* will flake.
  `pollring` was rewritten to require "fewer than one percent", not "none".
* **A dead end `loop { hlt }` at the end of a mode is not "the end", it is
  "forever"** — in a suite it eats the rest of the list and the last log lines
  never reach the disk. Every mode ends through `suite::park`.
* **The knob registry is sorted and a unit test checks it**
  (`registry_is_sorted_unique_and_namespaced`): insert a new knob alphabetically.

---

## 4. The local regression and the build

* **Do not edit or rebuild the kernel or `test.sh` while a run is in progress.**
  Bash re-reads the script as it executes, so editing it mid-run breaks the run
  with a syntax error. Run a copy.
* **`kernel/test.sh` executes INSIDE a docker container.** Killing the host
  process does not stop the run; the killed run keeps writing into the same
  `/tmp/o.log` that the second one reads (21 false FAILs once). Stop it with
  `docker stop`.
* **Do not load the host during the regression.** Builds, exports and AWS CLI
  calls in parallel produced two false FAILs (`evictbatch=64` — a gate that gives
  the boot a fixed 14 s; `forkap` — 40 fork/exit in an 8 s window). Both are
  green on a quiet host in a single repeat, and both are in the flake list in
  `CONTRIBUTING.md`.
* **`--no-default-features` LEAVES a kernel without `selftest` in `target/`.**
  Clippy in the second configuration is part of the gate, and after it the kernel
  must be rebuilt with the defaults, or the regression judges the wrong binary:
  8 false FAILs in ring 3 (`user`, `musl`, `printf`, `threads`, `cpp*`). The
  order in `CONTRIBUTING.md` (build → clippy of the same configuration →
  regression) is not optional.
* **Rebuilding `kernel/user/*` without rebuilding the kernel is pointless** —
  the test binaries are `include_bytes!`-ed in (`user_elf!`).
* **The kernel binary CHANGES from comments alone**: added lines shift the numbers
  that end up in panic locations. A previous regression run cannot be credited
  after a comment-only edit; verify with sha256 and re-run.
* **A freshness rule based on mtime lies.** `test.sh` rebuilt `uefi-stub` (which
  carries the kernel ELF inside via `include_bytes!`) when the kernel was *newer*
  than the `.efi`. After building with a different feature set, cargo returns a
  binary from its cache: the file is newer, the bytes are foreign, and the rule
  declares a stub built around SOMEBODY ELSE'S kernel fresh. Freshness is now
  judged by the sha256 of the kernel, and the gate is proved in three states.
* **`bash -n` does not see a container's inner script** (`<<'INNER'` inside
  `kernel/test.sh`): extract the heredoc and check it separately.
* A quotation mark inside a description breaks a row of the mode table
  (`"cmdline|pattern|desc"`).
* **`cargo clippy` CACHES.** Measuring "how many places does this lint hit"
  without `touch src/main.rs` before each run prints 1 instead of 146 — only the
  summary. A whole permission table was once "measured" that way.
* **`clippy --fix` for `collapsible_match` CHANGES SEMANTICS**: the inner `if`
  becomes a match guard, and a failing guard falls through to the next arm (in the
  editor the guard would additionally have EATEN the pressed key — `getkey()` in
  a condition). Autofixes in the kernel are read, not accepted.
* **`rustfmt` at the pinned version produces code that does not compile.**
  `--file-lines` with `max_width=160` split a match arm with a trailing block
  comment (`4 => 2 /*ILL_ILLOPN*/,`) into `4 => 2,` and a separate line
  `/*ILL_ILLOPN*/,`. Reproduced on four lines. **A bulk formatting edit ends with
  a BUILD, not with a look at the diff.**
* **Inserting a comment by matching the substring `unsafe fn …` lands it AFTER
  `#[inline]`** (`#[inline] /// SAFETY: …`). It compiles and reads as an error —
  check bulk edits by eye.
* **A raw NUL byte inside a string literal makes the file BINARY for `grep` and
  `file`.** `grep -c unsafe` over `syscall.rs`/`user.rs` silently found nothing
  until the byte became the escape `\0`. (In general: `grep` misbehaves on some
  files in this tree — fall back to `awk`/`sed`.)
* **`export-public.sh` archives HEAD**: uncommitted work does not exist for the
  export gate. That is exactly how the glossary once "did not travel".
* **`git check-ignore` STAYS SILENT about a path that is in the index** — without
  `--no-index` the gate "what would a fresh commit drop" passed while checking
  nothing.
* **Background jobs.** `nohup … &` inside an already backgrounded launch returns
  immediately, and the harness reports "the command finished, code 0" one second
  into a twenty-minute run — wait for a SIGN in the log (`TOTAL (host)`). A
  `nohup … &` in one command with a `cd` and a variable assignment gives the whole
  chain to the background, leaving the parent's variable empty. And a watchdog
  `until ! pgrep -f '<pattern>'` SEES ITSELF and therefore never ends (the pattern
  is in its own command line); `pkill -f 'until ! pgrep'` then kills its own shell
  as well.
* **An `apk add` avalanche of "ERROR: … Permission denied" is neither permissions
  nor the network.** `alpine-build` failed that way once after a first `I/O
  error` and built on the second attempt without a single change. Repeat before
  fixing.

---

## 5. The EC2 stand

* **The runner's address is refreshed before EVERY run** (`bench/suite/infra/sg-refresh.sh`,
  called from `run-remote.sh` and `ab-phase.sh`). A VPN reconnects several times
  a day and takes the whole stand's security group with it.
* **`talaria.stat=` is a BASE user-data token**: it survives a change of the
  cmdline tail, so it must be removed explicitly (`talaria.stat=0`) or the
  diagnostics ride into the next phase and spoil it.
* **An option parsed BEFORE the user-data merge must be re-read after it.** Three
  times in a row this bit: the network was configured before the kernel learned
  its own suite item; `talaria.ip=dhcp` from the item silently did not apply; and
  `acpi::button_arm` was armed before the merge, so `talaria.pwrbtn=0` from
  user-data silently did nothing — which made the first power-cut test pass
  FALSELY (`stop --force` on Nitro presses the ACPI button, and the kernel shut
  down cleanly; what was tested was not a power cut).
* **AMI builds refuse a DIRTY tree** — commit a harness fix BEFORE bringing the
  stand up. And `stand-up.sh --reuse-ami` after `stand-down.sh` never works
  (tearing down removes the image too).
* **The EC2 console does not carry COM1** (neither on metal nor on `c6i`): the
  trace goes to disk (`talaria.trace=<LBA>`) and is read by
  `infra/aws/read-disk-trace.sh` — but unroll it with `--keep-disk`, or the
  script stops the instance to free the volume, and a volume with
  `DeleteOnTermination=true` disappears along with the trace.
* **The EC2 console lags behind reboots**: `get-console-output` returned the text
  of the PREVIOUS boot (check the "final cmdline" line). Take the snapshot right
  after the phase and verify the tail.
* **A ladder is built from THIS rise's ceiling**, so "the same point as last
  session" does not exist: a ceiling of 25048 against 26827 gave point 2313
  instead of 2040, and `ONLY_RATES=2040` would have skipped both phases silently.
  Take the point from the FILE NAMES of phase A.
* **The first phase after phase A is cancelled with "the server does not
  answer"** even with a 240 s budget: mongod does not come up in time after the
  node reboots. The probe budget is now `BOOT_WAIT + 60` s and the failure says
  how long it waited; the workaround is to set the tail separately
  (`bench/suite/infra/talaria-cmdline.sh`), wait for the server in your own loop and take the
  phase with a tail of `=` ("do not touch the cmdline").
* **After ANY tail change through user-data, check that the SERVER answers.** The
  generator check appeared one session before the server's did, and that cost
  half a run.
* **`talaria-cmdline` skips an IDENTICAL tail** (change the token order to force
  it) and once GLUED duplicate knobs together so that the boot was rejected.
* **A knob that arrives from user-data must be in the registry in `cmdline.rs`**,
  and a `talaria.net*` knob that is not in `NET_LIVE_OPTS` REOPENS the NIC. The
  price of a needless reopen is `DEV_STS=0x21` — a dead network on the node and a
  lost run. The kernel prints the guilty token by name.
* **The stand's volume runs out.** After nine `insert` ladders on a 1536 MB
  image, "0 blocks free", mongod exits and phases fail with "the server does not
  answer" — a sign indistinguishable from "the knob broke the server". The
  discriminator is a control phase with the OLD tail.
* **`aws ec2 modify-instance-attribute --user-data` wants BASE64** (unlike
  `run-instances`, which encodes for you) and has to be passed as
  `--attribute userData --value`; otherwise a phase starts with an empty tail and
  silently checks nothing. `get-console-output --output text --query Output`
  already returns decoded text in CLI v2 — a blind `| base64 -d` gives emptiness,
  which looks like "the instance is silent".
* **The vCPU quota is counted per instance-type bucket**: a terminated 128-core
  metal holds its place for minutes, and the next launch fails with
  `VcpuLimitExceeded`. Scripts that launch metal need a retry loop.
* **Waiting by timer costs more than waiting by fact.** An instance at $6/hour
  that has "probably finished by now" is money paid for uncertainty. A machine
  that powers itself off (ACPI S5, `talaria.poweroff=1`) turns that into an
  unambiguous signal — and on a machine without a console it is the only
  "it reached the end" there is.
* **An image run locally carries the suite's STATE with it.** The item counter
  and the logs live on the disk, i.e. inside the image file: a local check left
  "4 of 4 done", and on the instance the same file would have run empty. The
  check "the trace sectors are zero before the run" now STOPS the run.
* `ssh` reads stdin to the end — a `while read` loop around a script that goes
  over ssh loses the rest of the list after the first iteration (read the list
  from fd 3 and give the ssh call `</dev/null`).
* Spread between instance launches is larger than it looks: one measurement gave
  UC 113.8 against WC 91.2, and it turned out to be placement noise, not an
  effect. A single measurement cannot be trusted; take a series.

### 5.1. The arm64 KVM stand (`infra/aws/kvm-stand-aarch64.sh`, `c7g.metal`)

* **A metal type that is out of capacity in a zone looks like a SLOW launch.**
  `terraform apply` printed "aws_instance.stand: Still creating..." for thirty
  minutes with NO instance behind it: RunInstances answered
  `InsufficientInstanceCapacity` for `c7g.metal` in ap-northeast-1c every 1-4
  minutes and the AWS provider retried it silently. Terraform never says so;
  `aws ec2 describe-instances` (nothing there) and CloudTrail
  (`lookup-events --lookup-attributes AttributeKey=EventName,AttributeValue=RunInstances`,
  the `errorMessage` names the zones that DO have capacity) do. `main.tf` now takes
  `az` and the script walks a list of zones (default 1d, 1a, 1c), giving each six
  minutes of apply and printing CloudTrail's last error before the next. The zone
  with capacity CHANGED between two launches ten minutes apart (1c refused; then 1a
  refused, naming 1c and 1d) — do not hard-code one.
* **A Graviton metal comes up in about a minute**, not the 10-20 an x86 metal takes:
  ssh answered 76 s after `apply` started (the instance existed after ~60 s),
  provisioning (docker, git, gcc, binutils, the pinned nightly with
  `aarch64-unknown-none`) took 89 s, the stand image and the kernel built natively
  in under two minutes. The whole first bring-up, three launches included, cost
  under an hour of `c7g.metal`.
* **AL2023 has no `cc`.** The kernel's build scripts (`proc-macro2`, `heapless`,
  `compiler_builtins`...) link host programs, and cargo fails with "linker `cc`
  not found" — `gcc` goes into the provisioning list, and on a native host the
  vDSO's cross assembler is plain `gcc` (`VDSO_CC_AARCH64=gcc`).
* **The container does not see `/dev/kvm` unless it is handed in.** The arm64 gate
  and `runq-aarch64.sh` ran under TCG on x86 hosts and never passed `--device
  /dev/kvm`; on the stand QEMU said "Could not access KVM kernel module" and the gate
  reported 31 failures in eight seconds — every assertion, because nothing booted.
  Both scripts now pass the device when it exists (`test.sh` always did).
* **A gate whose boots run to their timeouts costs its timeouts.** Under KVM every
  boot of the arm64 gate ended in a second and then sat until `timeout` killed
  QEMU: 6 min 37 s for five boots, and a fifty-run loop would have been five and a
  half hours. `talaria.poweroff=1` (PSCI `SYSTEM_OFF`, on which QEMU exits) at the
  end of the arm64 sequence made the gate 5.1 s under KVM and 22 s under TCG —
  the same knob, the same meaning as on x86.
* **A detached series over ssh hangs the ssh** even with `setsid nohup ... &`,
  `>/dev/null 2>&1 </dev/null` — the docker client keeps something of the session
  open; the series RUNS, only the ssh call does not return. Launch it under a
  `timeout`, or with `ssh -f`, and poll for a done-file.
* **Copy the logs off BEFORE the destroy stands in the same command line.** The
  hand-run series wrote to `out/` on the instance; the `scp` that was to fetch them
  ran silenced (`2>/dev/null`), failed on its two-remote-source form, and the
  `terraform destroy` chained after it went ahead — the raw files went with the
  instance. The verdict lines had been read over ssh and are in the docs, but the
  record is a transcript, not the files. The stand script tees every step into
  `infra/aws/logs/` as it goes; a hand-run series must do the same (`| tee` on the
  runner's side), and a destroy never follows a copy whose result was not checked.

* **Under KVM "UNKNOWN at reset" is a poison, and TCG hides it.** The architecture
  leaves `TPIDR_EL1`, `TPIDR_EL0`, `CNTKCTL_EL1`, `AMAIR_EL1`, `MDSCR_EL1`, … UNKNOWN at
  reset; KVM's `reset_unknown` fills them with `0x1de7ec7edbadc0de`, QEMU under TCG with
  zero. The exception stubs took the poisoned `TPIDR_EL1` for the per-core block and
  stored into it — so the first fault's handler faulted, and so did its handler, in
  silence, until the stack had walked through the image (the QEMU monitor's `info
  registers` showed the PC at the vector entry and the poison in x8/x9; that was the
  whole diagnosis). `boot.rs` zeroes them on both entry paths, and a register the
  kernel cares about is written whole, never read-modify-written. The same poison in
  x1..x17 after an `hvc` is KVM enforcing SMCCC (only x18..x30 are preserved): an
  `asm!` for a firmware call declares those clobbers or it corrupts a live pointer.
* **Code the kernel wrote is not code the core will fetch** until `dc cvau` + `ic ivau`
  (`arch::icache::sync`); and a translation the old root left is one the new root's
  first fetch may use. A process's first instruction was UNDEF one boot in three while
  the data side held the right word: every space shares ASID 0, and
  `msr ttbr0; tlbi; dsb nsh; isb` lets a speculative walk under the old root allocate
  an entry after the invalidate. Invalidate on both sides of the switch, `dsb ish`.
  x86 needs neither, which is exactly why the shared loaders had no such call.
* **A probe's first pass measures the boot, not the primitive.** `svccost` reported the
  FP/SIMD save at 191 ns under KVM against 14 ns in the morning: the first `svc` of the
  run printed 32 "unknown number" lines (a u16 table compared against a u64 — the
  same wild number never matched itself) and the print was inside the timed pass. A
  warm-up pass that is not judged, and the table compares what it stores.
* **Two soaks on one instance are no soak.** A driver killed on the runner's side goes
  on running on the stand (`ssh` does not kill what it started), and a second driver
  then shares `/tmp/soak` and the run disk with it: `cp` over an image QEMU has open,
  consoles that never booted, a fail count nobody can read. `pgrep` on the stand before
  a series, one series at a time, and a done-file for the runner to poll — the
  ssh itself breaks ("Broken pipe") long before a soak ends.

---

## 6. Defect classes worth knowing by name

These are patterns, not individual bugs. Each one cost at least a session, and
each recurs.

1. **"A syscall returned success without doing what it promised."** The largest
   family in this project: `sched_setaffinity` to a non-existent core returned 0
   (failure to migrate was indistinguishable from success — now EINVAL);
   `cmdline::value` did not accept a key with a trailing `=`, so
   `talaria.wakebal=` silently did nothing and a whole "variant 1 vs 2 vs 3"
   sweep compared three copies of the same code; a device limit (MDTS) was
   enforced on one write path and not on another, so a 2 MB command was rejected
   and the data was silently not written. **Silent success is worse than a
   refusal.**
2. **"A mechanism was measured while it was switched off."** 2 MB huge pages had
   a counter of EXACTLY zero handed out against hundreds of reserves, and two
   previous conclusions of "they give no win" were made on a disabled mechanism.
   Before believing "X does not help", check that X ran.
3. **"A constant with the caveat 'that is enough for us'."** `pci::MAX_DEV` lived
   with 32 records through three EC2 generations and died on the first real
   machine (52 bridges, 307 functions) — the table ran out BEFORE the walk
   reached the NVMe, which from the outside looked like "the kernel is silent".
   **Any fixed array that FOUND entities are put into must complain when it
   fills up.**
4. **"Works under a hypervisor" is not "works".** QEMU zeroes memory (so a
   kernel that does not zero its own `.bss` is fine locally and starts with
   garbage statics on hardware); QEMU gives dense APIC ids (so logical core
   number and APIC id coincide until the first real MADT); QEMU has a live COM1
   (so an unbounded poll of an absent port never returns, and on metal every
   register reads `0xFF` = "a byte is ready" forever). The whole catalogue is
   `docs/BARE-METAL.md`.
5. **"Two different meanings in one number."** They live exactly until the first
   machine where they diverge — see the logical core vs APIC id above. Check not
   "does it work" but "what does it stand on".
6. **"A test metric can be correct with the thing under test entirely absent."**
   `speedup = 2*t1/tboth` gives ~2.0 both when there is parallelism and when the
   worker stayed on the same core and took half the measurer's slices. The claim
   "the thread is on another core" is verified DIRECTLY (`getcpu`).
7. **"A diagnostic channel that fails softly."** A failure that goes to
   `/dev/null`, a trace threshold that hides exactly the last lines, a
   `conv=sparse` optimisation that makes unwritten blocks read as garbage on an
   encrypted volume — each cost sessions. **Before trusting silence, prove the
   channel sounds**; before trusting a number, ask what it costs to produce.
8. **"A comment on the wrong item."** A trailing comment on `RQ_MAX_EV_N_GOT`
   described the layout of `RQ_MAX_CTX`; four `///` blocks were separated from
   what they documented by a blank line (one documented the wrong static, one the
   wrong function, one hung over a section, one was left over from code that had
   moved to another module). This class is found by READING, not by running, and
   it is the cheapest class to find.
9. **"Static storage does not survive re-initialisation by itself."** smoltcp's
   socket storage is static, so `netsys::teardown_keep_lease` must clear it or the
   second `SocketSet` panics with "adding a socket to a full SocketSet".
10. **"Inspecting a device turns it off."** `MmioTransport` resets the device on
    `Drop`, so scanning virtio-mmio windows killed an already working second NIC
    — and latently would have killed the network when mounting the FS on a machine
    whose disk comes after the cards.
11. **"Bisect from working to broken, not from broken to working."** Three
    sessions went "from us towards the reference" (patch our volume until it
    matches) and produced nothing. One run of "the reference's bytes through our
    pipeline" cleared the entire pipeline in a single step.
12. **"A byte-level trace of somebody else's driver costs less than any
    hypothesis."** The ENA datapath gap resisted three sessions of reasoning
    (queues, RSS, offloads, moderation, ring depth, buffer addresses, instance
    generation, interrupts) and fell in an hour to decoding an already-captured
    admin-command log: one unset field (`accel_mode`). If you are comparing
    against somebody else's implementation, compare BYTES, not descriptions.
13. **"Measure what you ran into, not what you got."** The first receive
    measurement gave 557 k pkt/s and looked like a result; it was the
    GENERATOR's ceiling. With a fatter generator the number became 1 000 000
    pkt/s with a spread of ±3 — and that stability itself means a shaper, not
    code.
14. **"A retry budget counted in attempts lies."** Eight IMDS attempts look
    persistent; with a 10 s connect timeout they are 90 seconds, and the service
    needed more. Count time.
15. **"A heartbeat as a diagnostic for HANGS is useless"** — it is itself a
    source of the tails it is meant to observe. Kernel diagnostics running with
    `IF=0` are in the same family: a spin with interrupts disabled must service
    the mailbox, or a TLB shootdown initiator gives up on timeout and leaves a
    live TLB entry pointing at a frame already handed to somebody else (that was
    ClickHouse's "Checksum doesn't match").

### 6.1. What a second architecture found in the shared code

The arm64 port (`docs/ARM64-PLAN.md` §10.4, §10.5) ran the same ring-3 set on a second
machine and found four defects in code x86 had been running for a year — and then,
when its gate ran on a LOADED host, four more (22-25 below). They are
one class: **a race or an assumption that the first machine's timing or layout
never exposed.** Each surfaced under TCG, where every window is a hundred times
wider, and each is real under KVM.

16. **"The unmask before the entry."** `install(); irq_enable(); start()` — the
    tick that lands between the unmask and the entry saves the boot context
    into the first task's slot. x86's LAPIC timer had not expired yet; arm64's
    generic timer, armed long before, fired at the unmask (1 boot in 10). The
    task's own frame turns interrupts on; nothing before the entry should.
17. **"An ignored signal that interrupts."** A child's `SIGCHLD` is sent a moment
    before the child has left its core; the parent's `wait4` saw a pending signal
    and returned `EINTR` for a signal nobody handles. Linux discards an ignored,
    unblocked signal when it is SENT, and so does `proc::send` now. x86 under KVM
    never lost the race; TCG lost it 30 times in 40.
18. **"A constant that was a coordinate."** `vaddr < 1 GB` meant "the identity
    gigabyte" — GB0 on x86, the RAM's first gigabyte on arm64, which starts AT 1 GB.
    `SA_RESTART`'s syscall list, matched against the process's raw number, was
    the same thing with numbers (arm64's 61 is `getdents64`). An `mxcsr` read at
    offset 24 of an FPU image was the same thing with a layout. The tell is a
    literal where a function of the machine belongs (`user_gb()`,
    `abi::to_internal`, `arch::switch::fpu_image_check`).
19. **"A structure written in the other machine's layout overflows the caller."**
    `struct stat` is 144 bytes on x86-64 and 128 on arm64: the kernel's write did
    not only show a wrong mode, it overwrote the 16 bytes past the caller's
    structure — its saved frame pointer and return address — and `main` returned
    into address 0 AFTER printing its verdict. A layout difference is a memory
    safety bug at the process's side; the translation belongs at the dispatcher's
    door (`arch::abi::{enter, leave}`), with the process's sizes in the pointer
    gate, not in the dozen places that fill the structure.
20. **"A per-core register set on the boot core only."** `CNTKCTL_EL1.EL0VCTEN`
    let EL0 read the counter — on the core that booted; the first thread the
    scheduler put on a secondary trapped inside the vDSO. The AP pickup loop's
    stack was the same defect from the other side: never set for arm64's
    secondaries, so the first thread to exit on one put the core on `sp = 0` — a
    recursive fault with no console, which looks exactly like a hang. Whatever
    the boot core does to itself before ring 3, the secondaries' entry does too.
21. **"A request masked into another request."** The shared code sends an IPI on
    the TIMER's vector to make a core enter its scheduler; the arm64 side masked
    the vector to four bits and delivered SGI 11, which nobody handled — `execve`
    waited a second for sisters that never left their cores, and a dedicated core
    (no tick) could not be kicked at all. An interface with a magic value needs
    the value named on both sides (`RESCHED_SGI`).
22. **"An acknowledgement that asked for a lock."** The card's line handler cleared
    the device's interrupt status through a `try_take` of the shared queue. When it
    had interrupted a pass that HELD the queue, the status stayed set. An edge-routed
    line does not care; the GIC's SPI of a virtio-mmio window is a level, it stayed
    raised, and the handler was re-entered 600 thousand times a second while the
    interrupted pass — the one that would have released the queue — got not one
    instruction (`diskserver` with `talaria.netirq=1` on arm64: one boot in four
    never answered; the tell was `NIC_IRQS` racing and the device's status register
    reading 1 from the QEMU monitor). What silences a line must not depend on
    anything the interrupted code can hold: `NetDev::intr_regs` gives the two
    registers, the handler writes them directly.
23. **"Ten milliseconds is not a death certificate."** `sched::stack_wait` — a core
    that takes a task waits until the core that released it has left its kernel
    stack — gave the wait up after 10 ms ("the releasing core died") and entered
    anyway. A releasing core that is merely SLOW (a vCPU its host took away, an
    emulator on a loaded machine; the 8 ms steals of a shared EC2 host are the same
    thing) was still on the stack: two cores on one kernel stack, a return into
    address 0x10, and a crash dump whose frame was overwritten by the other core's
    interrupt WHILE IT WAS BEING PRINTED (two different pcs for one exception — the
    tell). A timeout may detect a leak only when it can tell a leak from a delay:
    the releasing core names the stack it is leaving (`percpu::off_stack`), and the
    wait ends early only when no core names it.
24. **"A transaction's middle read as the truth."** The boot core's switch announces
    `CURRENT = next`, checks whether a thief took `next`, and rolls back when one
    did. For the length of that transaction `CURRENT` names a task the core is not
    on — and the task it IS on looked free to a second thief, which took it and
    entered the frame the tick had just saved while the boot core returned into the
    same frame. The crash dump showed it in one line: "core 0 runs t1, core 1 runs
    t1". A word that is written at a commit (`percpu::cur_task`) and not at an
    announcement is the one a third party may trust: a task is the boot core's if
    EITHER word names it (`sched::bsp_on`).
25. **"A hole made in one address space, a frame reused in all."** A kernel stack's
    bottom page is a guard: not present, so that an overflow faults. `paging::guard`
    made the hole in the space that was ACTIVE when the stack was allocated (a
    parent's, during its `fork`) — and a stack freed later left the hole behind in
    that space. The frame went to another kernel stack, and the scheduler, running
    under the space with the hole, faulted reading the frame of the task it was
    about to enter. `sched::free_kstack` takes the guard back in every live space
    (`paging::unguard_in`). The general rule is §6's "mine in the identity window":
    whatever punches the identity map of ONE space must be undone in that space,
    not in whichever is current when the memory comes back.

All four of 22-25 are races of the SHARED code that a quiet KVM host never lost.
They surfaced when the arm64 gate ran next to eight other QEMUs on eight host
cores (a failure in 60-160 boots of `forkap`), and what made them diagnosable was
the crash dump itself: once it printed whose kernel stack the core was on, which
task every core believed it ran, and the last decisions of every core's scheduler
(`arch/aarch64/trap.rs`), each took one failing console to name. **Run a gate
under load on purpose** (`N` copies of one boot in parallel, more vCPUs than host
cores): a host that steals a vCPU for 10 ms is the cheapest race detector there is.

26. **"A derived value written where the source belongs."** ext2's directory hash has
    a version (`s_def_hash_version`: legacy, half_md4, tea) and a signedness (the
    superblock flag `UNSIGNED_HASH`, set by `mkfs` on a machine whose `char` is
    unsigned); the hashing code folds the two into one number (+3 when unsigned). The
    kernel wrote THAT number into the `dx_root` of every index it built itself, where
    the format wants the raw version. On every volume made on an x86 host the two
    coincide and three gates said "clean" for a year. The first arm64 host — `mkfs`
    there flags the hash unsigned — put a 4 into the root; e2fsck refused the tree,
    and Linux would have hashed those directories as signed half_md4 and found no
    name in them. The gate now builds the unsigned volume on purpose on both
    machines (`mkext2.sh HASHU=1`): a check whose verdict depends on the host it
    runs on is not a check.
27. **"An interrupt window the emulator cannot open."** A return to user space that
    restores the saved state in several instructions must do it with interrupts
    masked: an interrupt taken in the middle rewrites the very registers the restore
    is setting up (ELR/SPSR on arm64) and comes back into the middle of it with the
    stack already moved. The svc path came back from a parked syscall with
    interrupts open, and under KVM a NIC interrupt landed there 3 times in 4 (the
    `netirq` diskserver fault); under TCG never — the emulator takes interrupts at a
    translation block's end, and the restore is one block. The x86 return path had
    the same window, between `pop %rsp` and `sysretq`. Masking at the restore's head
    closes both; a fault that only real hardware raises is diagnosed from the dump,
    not from a repro under the emulator.
28. **"A firmware table's field that is a placeholder."** EC2's SPCR names its console
    twice: an address (`0x8000_0000`) and, in the revision-2 PCI fields, a PCI
    function. The address is the ENA's BAR; the console is the function's BAR 0, with
    memory decoding left off by the firmware. A kernel that trusts the address writes
    into another device and says nothing. When a table offers a structured locator
    (a PCI function), that is the authority, and the address beside it is checked
    against it — or ignored.
29. **"A limit that was the machine's shape, not the kernel's."** The frame allocator
    stopped at 4 GB (the identity window), and on an x86 `c6i.xlarge` that cost five
    of eight gigabytes — a "known" line for months. A `c7g.large` puts three quarters
    of its RAM at 16-18.9 GiB: above the window AND above the COW refcount table's
    16 GB, so a workload got 850 MB of 3995. The fix (session 58, `mem::high_init`)
    hands the RAM above 4 GB to the workload's own pages — the consumers that touch a
    frame only at its identity address and never guard it — and grows the refcount
    table to the top of RAM. Doing it named two holes the old limit had hidden: a
    `MAP_FIXED` over a gigabyte mapped by a 1 GB identity leaf (the NUMA areas, the
    MMIO windows) turned the kernel's next write to a frame there into a write into the
    process's page, and a NUMA node's area could cross the gigabyte of the process
    heap (`PROC_HEAP_BASE`, 192 GB) on a 256 GB machine. Both are refused now. And
    the first live run named a third: the workload's 2 MB pages still came from the
    low memory, and on the real machine's shape (850 MB low) `forktls` ate it and the
    kernel panicked for a page table — under QEMU the low memory was 3 GB. **The
    emulator's memory shape is not the machine's**: the gate now cuts the low
    allocator to the machine's size (`talaria.lowmem=850`) and reproduces the panic.
30. **"One word that both sides read, change and write back."** The ordering audit
    (`docs/ARM64-PLAN.md` §10.10) was meant to find `Relaxed` where a `Release` belongs;
    most of what it found was older and blunter. A pipe kept ONE count that the reader
    decremented and the writer incremented with plain stores — two cores at once, and one
    end's update was gone (bytes lost or read twice). An eventfd's counter, a task's and a
    process's pending-signal sets, the COW refcount of a frame shared between processes,
    the task-slot claim of `fork`/`clone`, the queue of dead address spaces: each was a
    read, a change and a store, and each was written when one core ran the workload.
    None of it is about arm64 — x86 loses these races the same way — and none showed in a
    year of gates, because no gate put two cores ON THE SAME WORD. A new one does
    (`piperace`: a 4 MB stream through a pipe between threads, an eventfd from three
    writers), and on the old kernel it failed 2 runs of 3 under TCG (4 million wrong
    bytes, an eventfd sum of 304 930 for 300 000) and hung the third. **The question for
    a shared word is "who else writes it?" before "with what ordering?"** — an atomic RMW,
    a lock, or one writer per index (the pipe is SPSC now: the reader owns the head, the
    writer the tail). The ordering half found what the plan expected — an SGI sent before
    the `dsb` that makes its data visible, TLB invalidations without the `dsb ishst` that
    must precede them and a large one that stayed on the local core, a kernel stack freed
    by a plain store before the spills on it, frames given back before the flush of the
    translations to them — plus one with no ordering in it at all: arm64's `membarrier`
    waited for an acknowledgement nobody sent, so every call spun its 1 ms insurance and
    ordered nothing.
31. **"A level where the code expected an edge."** The kernel lends the core's timer to a
    waiter's deadline and gets it back when the waiter wakes. On x86 the TSC-deadline fires
    once; arm64's generic timer is LEVEL-sensitive — with the compare in the past it stays
    raised. When a lent deadline fired, the vector found "nothing to re-arm" (the timer is
    lent) and did nothing to it, so it was re-entered the moment it ended: 1.1 million
    interrupts on one core, until the waiter got the core back. Nothing crashed; libtsync's
    MPSC phase simply never finished on arm64, and it took a watchdog printing the queue AND
    the core's timer state to see that the "lost wakeup" was a core drowning in its own
    interrupt. **Port the semantics of an interrupt source, not its call sites**: what
    silences it, and who does when the usual silencer is away (`trap.rs` stops the timer
    itself when it has nothing to re-arm).
32. **"A register read by its x86 position."** The tick's profile took the interrupted frame
    pointer as the frame's ninth word and the privilege as `cs & 3` — RBP's slot and the CS
    selector on x86. On arm64 the ninth word is x8 and `& 3` reads SPSR's mode bits, so every
    ring-3 sample was filed as the kernel's and unwound to depth 1. Nothing failed loudly:
    arm64 simply had no profile gate, so nobody looked. The frame has accessors (`switch::ctx_fp`, `mode_is_user`);
    a shared module that indexes a frame by number is an x86 module in disguise. The
    debugger's port found three of the stub's own on the way: a memory read gdb asks for at a
    stale address took the kernel down from inside the stub (both machines — `m` goes through
    the profiler's guarded load now), x86's `qAttached` compared a 10-byte slice with a 9-byte
    literal and never answered, and a stop reply the stub sent unasked crossed gdb's opening
    `qSupported` on a shared line and was taken for its answer (the arm64 stub waits for `?`
    until gdb is attached, as kgdb does).
33. **"A bit that a rewrite forgot."** Redis on arm64 forks at start-up to test the kernel
    for Linux's ff1712f953e2 (the dirty bit lost when a written page is write-protected) and
    refuses to run if it is. It refused here — first for want of `/proc/self/smaps`, then,
    with the file in place, for the bug itself, twice over: `paging::protect` rebuilt every
    leaf from the frame and the rights and left D out (both machines — an ext2 MAP_SHARED
    page written, then `mprotect(PROT_READ)`, was skipped by the writeback), and arm64's
    encoding kept D in AP[2], which a read-only page needs for its protection (fork's COW
    lost it too). D is now carried through `protect`, a 2M split, and in a software bit of a
    read-only arm64 entry; `talaria.test=dirtyprot` fails on each half of the old code alone,
    and Redis on the old kernel says "Your kernel has a bug". **A workload's own sanity
    check is a test someone else already wrote** — run the real program before writing the
    gate, and when it refuses, read why before reaching for its "ignore" switch.
34. **"A promise in the auxiliary vector."** arm64's `AT_HWCAP` had bit 11, `HWCAP_CPUID`,
    copied from a Graviton's list — and that bit is not a feature of the core but a promise
    of the kernel: "a `mrs` of the ID registers from EL0 traps, and I answer it" (Linux's
    `emulate_mrs`). OpenSSL's armcap believes it at load, so Alpine's PostgreSQL died with
    a SIGILL on its first instruction past `main`'s prologue. The emulation is in
    `trap::emulate_id_read` (`talaria.test=idregs`); the built-in test binaries' auxv had
    no `AT_HWCAP` at all. **Every bit handed to a process is a contract** — before copying
    a capability word, read what each bit obliges the kernel to do.
35. **"A flag read only together with its sibling."** `mmap` took `MAP_FIXED_NOREPLACE`
    (0x100000) into account only when `MAP_FIXED` (0x10) was set too — Linux needs no
    `MAP_FIXED` beside it. A NOREPLACE probe fell to the bump region and returned another
    address; HotSpot gave it back and tried the next. On x86 the probes are for the heap,
    which has an "anywhere" fallback, so they failed silently for two months; on aarch64
    the compressed class space must sit at an encodable base, and Temurin 21 died with
    "Could not allocate compressed class space" after 64 refused candidates. **A silent
    fallback in the workload hides a kernel defect on one machine and shows it on the next.**
36. **"A placeholder that was a number."** `netsys`'s counter rate `HZ` is set when the
    first card is added and was `1` before that; `ms_to_cycles` divided it by 1000. On a
    machine with no NIC every `epoll_wait`/`poll` timeout was zero cycles — a 5-second wait
    returned 0 at once (both machines; a test of mixed epoll sets found it). Timeouts now
    take the counter's rate from `tsc::hz()` when the card has not set one.
37. **"The emulator multiplies the design's price."** Every PostgreSQL process waits in
    `epoll_pwait` on a set with its latch's signalfd (Alpine's build; x86's self-built one has
    a self-pipe there — the same class), and a waiter whose set is not only sockets SPINS
    (`yield_window`: the product's model is a dedicated core per waiter). Under TCG six
    such spinners left a backend 26 % of an emulated core and 18 thousand address-space
    switches a second (each a full soft-TLB flush in QEMU): `select 1` took 0 s or 147 s
    depending on how they met — which read for an hour as a lost wakeup. `talaria.top=`
    showed it in one snapshot. The knob `talaria.netmix=<ns>` parks the non-polling ones
    (the producers of a pipe's, an eventfd's and a signal's readiness kick them); it is off
    by default, the TCG gate turns it on. **Before diagnosing a hang under an emulator, ask
    `top` who is eating the cores.**
38. **"A context rebuilt from what one register convention promises."** A syscall that
    moves its own thread to another core (`sched_setaffinity` from the boot core, a
    `prctl(PR_SET_NAME)` a layout rule acts on) builds the thread's return context anew on the
    target, and it carried only the CALLEE-saved registers — the ones a FUNCTION call
    preserves. A syscall preserves everything but its return value (x86: also rcx/r11), and
    compiled code keeps temporaries across an inline `svc` in x9..x18 and lr. ClickHouse's
    threads, laid out by name, came back with x0..x18 and lr zero and returned to address 0
    (6 runs of 6 with its stealing push off, ~30 % with it on — the push moved them first);
    x86 lost rdi..r10 the same way and nothing had noticed. `talaria.test=migregs` (both
    machines) fills the registers around a raw moving syscall; the old kernel fails it on each.
    The hunt took a frame ledger and a page-table journal to exclude a lost write, and then
    the registers of the crashed thread said it in one line: x0..x18 zero, x19..x29 sane.
    **Print the whole register file of a crash before theorising about memory.**
39. **"A diagnostic that read a page base as an address."** On the way there a probe of the
    crashed thread read its stack through `paging::phys_of_in`, which returns the PAGE's
    physical base, not the address's — and reported a zeroed stack frame, which sent the
    hunt after a lost store for two hours. The page dump next to it disagreed with it. **When
    two views of one memory disagree, the probe is the first suspect** — validate a new
    instrument on a value you know before reading one you do not.
40. **"The identity map answered a null pointer."** On arm64 the kernel reached GB0's devices
    (the GIC, the UART, virtio, the 32-bit BARs) by their identity, and every process had that
    gigabyte mapped. Moving the devices onto an alias (`mmu::DEV_SLOT`) to free GB0 for
    ClickHouse's ET_EXEC unmapped it — and the first allocation faulted at address 0x20: the
    per-core block's accessors read `TPIDR_EL1 + 0x20` before `percpu::init`, when the register
    is 0, and had been reading `virt`'s flash all along. The accessors are null-safe now. A
    mapping at address 0 hides null dereferences; the first thing that unmaps it finds them.
41. **"A per-core init with a global half."** `syscall::init` armed THIS core's syscall entry
    and, on the side, the workload's common brk heap: the first call allocated it (128 MB,
    made NX), a repeated one zeroed its used part — for a workload's restart. The secondary
    cores called it as they came up (both machines). A secondary up before the launch path
    allocated the heap before the static image's range was reserved, over the image (the code
    became NX: the first instruction "not executable"); one up after the workload started
    zeroed the heap — the code under it (`udf`, a SIGILL), or faulted on the read-only page.
    Only on the KVM stand, in the parallel detector (futexlat 17 of 96, syncbench 13 of 96),
    never under TCG in any CPU model: a real core comes up later relative to the boot core.
    `init_core` is the entry alone; the secondaries call it. Two detours on the way, worth
    keeping: reserving the embedded image's range (`user::reserve_embedded`, right in itself)
    looked like the fix because the run that tested it had the page-table journal on — the
    journal's writes moved the timing and the failures went to 0 of 288; the same kernel
    without it failed 31 of 288. **Test a fix with the instruments that change timing OFF**,
    and **an `init` called per core must not own anything global.**

Two that were the STAND's, not the kernel's, and cost an hour each:

* **The gate's image is part of the gate.** The dev box's `talaria-qemu-aarch64` was
  older than its Dockerfile — no `ip`, no `socat` — so the tap was never configured
  and "the NIC does not answer" was the container. Check the tools the block needs
  before the block (`test-aarch64.sh` does, and says SKIPPED with the reason).
* **A client that gives up before a slow machine answers.** `socat` closes 0.5 s
  after its input ends; under TCG the answer of a forking server came at 0.8 s and
  the "kernel lost the connection" was the client's reset. The packet trace on the
  tap (a ten-line `AF_PACKET` reader) showed the guest answering every time.

---

## 7. Already closed — do not re-open without new evidence

Each of these was written, measured and rejected. Re-opening one costs a stand
phase; re-opening it without new evidence costs a day.

| Hypothesis | What the measurement said |
|---|---|
| Group commit / async commit for `fsync` | Ceiling exactly 0 µs: there is nothing to coalesce (the FS lock taken 597/597 in 0 µs; the journal busy with a neighbour 1 time in 597; 1235 transactions per 1247 `fdatasync`). |
| FUA per write is what costs 4 ms | Killed by the `talaria.fua` knob: p99.9 stayed at 4325 µs. The real root was our own spin — `io_may_yield()` forbade yielding while the device lock belonged to the CORE. |
| The dirty-data volume feeds the `insert` tail | `dirtyfrac=32` (flush at 1/32 of the cache) gave the same p99.9 of 4.34 ms. |
| Age-based pre-emptive flush as a default (`dirtyage=30`) | Takes 138 µs off `fsync` and adds 501 µs to `write` — 76× more expensive; over a window, +5490 ms against −330 ms. Default stays 0. |
| Cure the 140-400 µs band with the quantum length (`tickmin=200000`) | A third FEWER stops and a WIDER band (0.49-0.97 % against 0.22-0.75). The band is fed by the LENGTH of a stop. |
| Cure it by priority against the network waiter (`reqfirst=`) | The mechanism works (four times fewer stops mid-request, 94 % → 58 % of rotations going to the waiter) and the band does not narrow (p99.9 0.91, inside the baseline's spread with itself). Default stays 0. |
| Pack logical cores onto physical ones (`smtpack=1`) as a default | The knob works (the host is left a whole physical core: 0+8 ms against 24+11) but costs 5-8 % of the median, and the class it was written for did not reproduce on the same bring-up. |
| Park the polling network waiter without a NIC interrupt line | Median 1502 µs instead of 61.7. The counters separate two populations: non-pollers are woken by delivery in 99.8 % of cases, while the poller sleeps to the ceiling in 99 % — the doorbell is addressed to the socket's OWNER, and the poller is not it. |
| `talaria.netidle=1` on the SERVER | A catastrophe: `find` p50 97 → 217, overload. (On the GENERATOR it is the default — see §2.) |
| `slice=150000` + `wakepreempt=20000` for mysql | 16-22 % worse. The same pair had removed a mongodb tail by 20-70×: **scheduler knobs are per workload**, which is why seven of them moved into `cell.manifest`. |
| `talaria.idlepoll=` (an idle stack pass by an empty core) | The mechanism never fired: a sleeping waiter goes into `hlt` inside its own context and never returns to `ap_loop`, where the hook was cut in. Default 0. |
| Tail classes B (`mongodb insert` p99.9, five points) and C (`mongoose dyn`) | Not classes: they did not survive a second bring-up (nginx 20/20, mongoose 40/40 with no losses). |
| The "foreign 8 ms" class | Not our code: 97 % of the hole is vCPU theft by the hypervisor (8092 µs of 8277), it arrives on both cores at once, and the class MIGRATED between workloads without a line of code changing. |
| SMAP | Rejected with a reason: the kernel copies the process's memory with ordinary loads, and that is what makes a syscall cheap. SMEP is on where the CPU has it. |
| Deferred block allocation (delalloc) in ext2 | Deliberately not done: a cache slot is addressed BY BLOCK NUMBER, so a data page must have its number at write time. What delalloc is done for was obtained by run-based allocation: bitmap trips 33 → 7 per 512 data blocks. |
| The audit's wake-path price on arm64 is its new barriers (the SGI's `dsb ishst`, the membarrier ack, the dead-space scan's Acquire) | All three reverted at once over HEAD on the KVM stand: futex RTT 577 ns, condvar 2486 — where HEAD is 572/2437 and the commit before the audit 518/2008. Not them. |
| Acquire loads as `ldar` (RCsc) are the price; `+rcpc` makes them `ldapr` | 566/2458 against HEAD's 569/2465 (six interleaved rounds). No effect. |
| The step is code alignment luck | Both sides built with every function and loop on 64 bytes: 530/2102 against 580/2438 — the same step. |
| x86: the waiter's registration (eight SeqCst stores = eight `xchg`) costs the futex RTT | Seven made Relaxed, WADDR Release, the fence kept: 446-471 ns against 438-463 under KVM. Within the spread; not done. |
| More cores is always better | The opposite, and it depends on the workload. Databases on 2 cores beat themselves on 4 (extra cores breed spinning waiters that steal a core from the computing thread); ClickHouse wants 4 and is measurably worse on 2. The number of cores belongs in the workload's `meta.env`, not in a global default. |

---

## 8. If you are an AI agent

The working cycle, the places to look up facts instead of guessing them, and the
environment's quirks are in `docs/DEVELOPMENT-GUIDE.md` §0. Four things from
this file matter most for an autonomous session:

1. **One item, one commit, one gate.** A feature without a gate does not exist;
   a gate that has not been shown to fail does not exist either (§3).
2. **Before writing an edit for performance, produce the number that justifies
   it** (§1, rules 2-3). "It is probably X" has been wrong more often than right
   in this project's history, and the instrument is cheap compared with a day.
3. **Verify the totals, not the verdict.** `FAIL=0` with five gates in SKIP is a
   regression that got shorter, not a green one (§3).
4. **When a result surprises you, suspect the instrument first** (§2). The
   harness in this project broke more often than the system it measured, and
   always silently.

---

Related: `docs/DEVELOPMENT-GUIDE.md` (architecture, invariants, recipes),
`docs/BARE-METAL.md` (what QEMU hides), `docs/MEASUREMENTS.md` (the numbers and
how they were taken), `docs/ROADMAP.md` (what is open), `bench/suite/README.md`
(the comparative suite), `CONTRIBUTING.md` (the gate and the flake list).
