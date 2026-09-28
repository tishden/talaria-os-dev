# Glossary

The words below are used throughout this repository — in the guides, in the
commit messages and in the working journals. Most are ordinary words used in one
fixed sense; a couple are this project's own. They are listed here because the
sense is not guessable, and guessing costs a reader an hour.

## The system

**Cell** (`talaria.mode=cell`) — a workload run as ONE multithreaded process
under a supervisor, in the kernel's scheduler, with a manifest
(`cell.manifest`). It is the production shape of "the application owns the
cores": threads are laid out by rule, a dedicated core gets no timer tick, and a
crash is caught and restarted. Not a container: there is one cell per machine,
and it is the reason the machine exists. Compare with `talaria.mode=proc` — the
same binary but with a full process model (fork, signals, private address
spaces), which is what the databases need.

**Datapath** — the path a packet or a message takes without entering the kernel:
rings in shared memory between the application (on the BSP) and a cell on
another core, polled instead of interrupt-driven. "Zero-syscall" means exactly
this and nothing more.

**Manifest** (`cell.manifest`) — the workload's declaration on its own disk:
the binary, the arguments, the environment, the restart policy, the layout of
the threads over the cores, the cache shares. It is read by the kernel at boot.
Changing how a workload is placed does not mean changing the workload.

**Knob** (`talaria.<name>=<value>`) — a kernel parameter on the boot command
line. Every knob is in one registry (`kernel/src/cmdline.rs`); an unknown name or
a typo REFUSES the boot rather than being ignored, because a knob that is read
and does nothing has cost this project whole measurement sessions. `stat:` lines
and the registry are how you find out which knobs exist.

**Identity window / identity address space** — the region where a virtual
address equals the physical one. The kernel always has it; a process does not
(its identity leaves are supervisor-only). The modes `talaria.cell=identity` and
the embedded demo modes run the workload INSIDE the kernel's own space — cheaper
and less isolated, and that trade-off is stated wherever it applies.

**Ring 3 / ring 0** — x86 privilege levels: the workload runs in ring 3, the
kernel in ring 0. "In ring 3" in this repository always means "the real,
unmodified application", not a simulation.

## Measuring

**Stand** — the machine (or the pair of machines) a measurement runs on, plus
what makes it comparable: the instance type, the pinning, the boot parameters,
the generator. "The same stand" is a claim about all of that, not just about the
hardware.

**Gate** — an assertion that a run must satisfy: a line the kernel prints,
matched by a regular expression in `kernel/test.sh` or `kernel/validate.sh`. A
feature "has a gate" when its absence makes the regression red. A gate that
cannot fail is not a gate.

**Scoreboard** (in the journals: "табло") — a table of comparable points against
Linux: a workload times a percentile, won / parity / lost. `docs/TABLO-41.md` is
the current one. A point enters it only if both sides ran the same binary through
the same generator on the same stand.

**Rise** (in the journals: "подъём") — one bring-up of the stand from which a
whole set of points is taken. The unit matters because numbers from DIFFERENT
rises are not comparable at the tails: `p99.9` does not repeat between rises
better than a factor of a few, which is why a change is judged by alternating
boots within one rise.

**Ladder** — a series of load levels (requests per second) climbing to the point
where the server stops keeping up. A "step" is one level; "the ceiling of the
rise" is the last level both sides survived.

**Band** — a cluster of latencies at a particular value (for example "the band
of 140-400 µs on an insert"), that is, a repeatable feature of the distribution
rather than a single outlier. Bands are what tail work is about: they have a
cause, and the cause is findable.

**Host** — the hypervisor and the machine under it (the noisy neighbour of every
guest measurement). In these docs "host" never means "the server being
measured"; that is "the guest" or "the server".

**Noise floor** — the spread a stand gives on a REPEATED run of the same thing.
A difference below the noise floor is not a result, and the floor is measured
before the experiment, not deduced after it.
