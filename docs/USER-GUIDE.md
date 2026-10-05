# Talaria OS — User Guide

The user's/operator's guide to Track B (the bootable kernel `kernel`): how to build the
kernel, how to run the built-in demos and **your own or open-source applications** in ring 3, how
to configure the launch parameters and how to run the operator's side (the console, the telemetry, the control
plane, the supervision).

The audience: whoever wants to **run a workload** on Talaria. If you are improving
the kernel itself, see `docs/DEVELOPMENT-GUIDE.md`. An end-to-end example with real output
(multithreaded C++, monitoring, a profile, debugging, a core dump, files, EC2) is in
`docs/HANDS-ON-GUIDE.md`.

---

## 1. An introduction: the architecture and the concepts

### What Talaria is

Talaria is a special-purpose OS for latency-critical workloads (trading, embedded,
dedicated servers behind an external security perimeter). The key principle is:
**the application owns the CPU cores and the devices, while the OS is a control plane**, not
a middleman on the hot path. General-purpose operating systems are optimised for fairness and
throughput; Talaria is optimised for a **predictably low latency of the tails** (the p99/p99.9).

### The two tracks

- **Track A** (host-VFIO): the workload's "cell" executes on its own hardware through VFIO,
  while the control plane is on Linux (`control-plane`, the binary `talaria`, the manifests
  `control-plane/manifests/`). The model is "Linux as the control plane".
- **Track B** (this guide): an x86-64 kernel of our own, `kernel` (Rust, `no_std`,
  a PVH boot under QEMU/KVM) — **our own OS without Linux**. The application executes in ring 3
  through a subset of the Linux syscall ABI; the kernel gives it memory, the devices (virtio-blk/net),
  the observability and the operator's side.

### The main concepts of Track B

| The concept | What it means for the user |
|-----------|------------------------|
| **A cell** | Your workload in ring 3 — a static binary (musl/C/C++) executed by the kernel. |
| **The Linux syscall ABI** | The kernel understands ~185 Linux syscalls (SYSCALL/SYSRET). Static musl binaries and dynamic ELFs (ld-musl/ld-linux) are run **without changes to the source**. |
| **The control plane** | The kernel (and/or separate tasks) observe and control the workload: the console, the telemetry, `/ctl` over TCP, the supervision. |
| **A mode (mode/test)** | What the kernel does on this boot, chosen by the cmdline parameter `talaria.mode=…` / `talaria.test=…`. |
| **The zero-syscall fast path** | For an ultra-low latency the datapath works through rings in shared memory without syscalls on the hot path (`talaria.mode=fast`). |
| **TFS1/TFS2/ext2** | The file systems on virtio-blk: TFS1/TFS2 are simple read-only ones (a binary+files), ext2 is an RW one with write-back (the persistence of a DBMS). Everything is packed by `talaria-pack`. |
| **A cell's manifest** | A declarative launch: `binary=`, `args=`, `env=`, `restart=`, `watchdog=` (`/cell.manifest`, the modes `cell`/`proc`). |
| **The layout over the cores** | In the same manifest: `pin=<a pattern of a thread's name>:<a core\|dedicated\|shared>`, `spread=`, `balance=`, `numa=`. A dedicated core has no timer tick and no neighbours; the other threads spread out over the common pool and are levelled. See §6.6. |

### The model of protection and isolation

- The kernel builds **page tables of its own** (an identity 1G → split down to 2M/4K on
  demand). In a process's address space (`cell`, `proc`) the identity window of the
  physical memory is available **only to the kernel**: the workload sees its own segments, its stack,
  its heap, its `mmap`s and what has been given to it explicitly (`MAP_SHARED`, shm, the zero-syscall region).
  The modes in the kernel's identity AS (`run`, `talaria.cell=identity`) still see
  the region 32 MB–1 GB as U/S — that is their model, there is no isolation there.
- **W^X / NX**: the ELF's segments get their rights from the `p_flags` (`.text`=R-X, `.rodata`=R--/NX,
  `.data/.bss`=RW-/NX); the heap and an anonymous `mmap` are NX; under the stack there is a guard page.
- **Per-process address spaces**: every process has its own PML4, with a switch of the CR3
  in the scheduler. `fork` is copy-on-write with a refcount of the physical frames.
- **A syscall filter** (optional): an allowlist from the workload's declaration; outside the list there is
  an `-EPERM` + an entry in the flight recorder.

---

## 2. The requirements

- **A Linux host with KVM** (`/dev/kvm`). For the network there are `/dev/net/tun`, `/dev/vhost-net`,
  capability `NET_ADMIN`.
- **Docker** with the toolchain's images — they are built by one script
  `services/docker/build-images.sh`:
  - `talaria-qemu2` — QEMU + the utilities (used by `test.sh`, `validate.sh`, `run.sh`);
  - `talaria-musl` — `musl-gcc` for static user binaries;
  - `talaria-alpine-build` — the Alpine build-base (C++/large open source statically against musl).
- **Rust** with `rustup` — for building the kernel; the nightly version is PINNED in
  `kernel/rust-toolchain.toml` (rustup will pick it up itself at the first `cargo`).
- `python3` — for packing the disks (`kernel/tools/talaria-pack`); for ext2 there is
  `e2fsprogs` (mkfs.ext2) on the host.

> All the QEMU launches go inside a container with the repository mounted at `/w`.
> The commands `run.sh`/`test.sh`/`validate.sh` bring the container up themselves.

---

## 3. Building the kernel

```sh
cd kernel
cargo -Z json-target-spec build --release   # the toolchain is pinned in rust-toolchain.toml
# the result: kernel/target/x86_64-talaria/release/talaria
```

The details: the custom target `x86_64-talaria.json`, `build-std` (`core`/`alloc`/
`compiler_builtins`), the linking by `linker.ld`, `relocation-model=static`, `no_std`,
`panic=abort`, LTO. The dependencies: `smoltcp 0.14`, `virtio-drivers 0.11`.

The built `talaria` is a PVH image: QEMU loads it directly through `-kernel`.

**arm64 (AWS Graviton, QEMU `virt`).** The same tree builds for `aarch64-talaria.json`:

```sh
sh kernel/build-aarch64.sh            # -> kernel/target/aarch64-talaria/release/talaria{,.img}
make -C kernel/user aarch64           # the ring-3 test set (needs aarch64-linux-musl-gcc)
qemu-system-aarch64 -M virt,gic-version=3,its=on -cpu max -smp 4 -m 1024 -nographic \
    -kernel kernel/target/aarch64-talaria/release/talaria.img -append "talaria.mode=userver talaria.smp=4"
```

`talaria.img` is a Linux-format `Image` (QEMU loads it at RAM base + 0x80000); the ELF next to
it is for `gdb-multiarch`. The workloads are the same Linux binaries built for aarch64 — static
musl, or dynamic through `ld-musl-aarch64.so.1`/glibc for PostgreSQL, MariaDB, MongoDB, the JVM
and ClickHouse (`services/build-aarch64.sh`, `services/build-aarch64-db.sh` unpack the
distributions' aarch64 packages without an arm64 builder). The gates: `kernel/test-aarch64.sh`
(TCG on any host, KVM on an arm64 one) and `kernel/test-aarch64-db.sh`.

---

## 4. A quick start: the built-in demos

The kernel by itself contains dozens of demonstrations/self-tests. The mode is chosen by
the parameter `talaria.test=<name>` (an internal self-test) or `talaria.mode=<name>`
(a service/the network/the operator's side) in the cmdline.

A minimal launch (without the network, without a disk):

```sh
# from the repository's root
K=kernel/target/x86_64-talaria/release/talaria
docker run --rm -i --device /dev/kvm -v "$PWD":/w talaria-qemu2 bash -c '
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
    -kernel /w/'"$K"' -append "console=ttyS0 talaria.test=musl" \
    -display none -serial stdio -no-reboot'
```

The simplest way is to run the ready regression, which boots every mode and asserts the output:

```sh
kernel/test.sh        # the quick regression (in docker) — for the counter see the README
kernel/validate.sh    # the heavy one: open source/latency/soak/fuzzing
kernel/soak.sh 1800   # 30 min of a DBMS under load (the mariadb artefacts are needed)
```

### The map of the main modes

**The self-tests of the runtime/the memory/the processes (`talaria.test=…`):**

| The mode | What it shows |
|-------|----------------|
| `musl`, `printf`, `cpp`, `cpp2`, `cpp3`, `pthreads`, `threads` | musl/C++/threads (clone/futex/mutex/join) in ring 3 |
| `wxorx` | the memory's protection: W^X + NX + a guard page + a live #PF |
| `uaf` | `munmap` really removes the mapping (there is no use-after-free) |
| `mem` | `mmap`/`munmap` + the allocator's reclaim |
| `vmspace`, `exec`, `procs`, `procdisk`, `forkp`, `fork` | the process model: the isolation of the AS, exec, the processes, fork+COW, disk workloads in private ASes |
| `soak` | the allocator's churn over 20k iterations (without a leak/crashes) |
| `watchdog` | the detection of a hang of the workload → a restart |
| `syscallfilter` | the syscalls' allowlist: a forbidden one → an EPERM |
| `ctlauth` | the authentication of the control plane (a token) |
| `manifest`, `console`, `devnull`, `pmu`, `fault`, `crypto` | the operator's side/the observability/the runtime |
| `shell`, `edit` | the operator's shell in the kernel (the paths, ls/cat/write/cp/rm -r, the completion) and the console's editor (editing, an undo, a search, the refusal to write a truncated file) — §9.1 |
| `filed`, `netcfg` | the checksum of the file service (the CRC32 over chunks == over the whole) and the arithmetic of the routing (§9.2, §9.3) |
| `affinity`, `msix` | the layout of the threads over the cores: the policy on synthetic snapshots (the pools, NUMA, the convergence of the levelling) and the routing of the devices' interrupts (§6.6) |
| `schedspread`, `schedpin` | the same decisions on live cores: the spreading over the cores and the pre-emption on a secondary one; the pinning by a thread's name into a pool / onto a dedicated core |
| `clock` | the wall clock's discipline: the servo on a synthetic drift (the convergence, the monotonicity, the clamp) + a live adjustment by kvmclock (§6.8) |
| `cat` | the shares of the common L3: the validation of the masks, the footprint of the DMA rings against the L3, the end-to-end path manifest -> pool -> hardware (§6.7) |
| `nicdev` | the model of a network device: the rules of binding the drivers, the classification of the interrupts (MSI-X/MSI/INTx), the bring-up of a card through the table and a run of the datapath over it |
| `fs`, `diskexec` | reading a file and starting a binary from the disk (virtio-blk/TFS1) |
| `ext2`, `ext2v` | the RW file system ext2: the operations and the persistence across a reboot |
| `fsbench` | the price of an fsync: the coalescing of neighbouring blocks into ranges, a repeated fsync is almost free |
| `journal1` / `journal2` | the ext2 journal: the commit of a transaction and a SIMULATION OF A CRASH / the recovery of the metadata at the mount |
| `mapshared` | a file `mmap(MAP_SHARED)`: the coherence with read/write in both directions, msync, the preservation after a munmap |
| `wc` | write-combining: the burning of the `IA32_PAT` (the slot `PA4 = WC`), the conversion of a page into WC with a 4K granularity, a neighbouring page does not change its type. The mechanics by which the ENA's LLQ window is mapped — it is checked without a card |
| `phc` | the ENA device's clock (the PHC): its rate is compared with the local one. Only on EC2 and only where the ENA declares the feature; there is no local regression |

**The services, the network and the operator's side (`talaria.mode=…`):**

| The mode | What it does |
|-------|-----------|
| `mongoose`, `lua`, `sqlite`, `busybox`, `quickjs`, `redis` | real open source in ring 3 (see §5) |
| `httpd`, `server`, `smoltcp`, `userver`, `eserver`, `diskserver` | the network modes (a kernel HTTP / a musl server / smoltcp / epoll) |
| **`cell`** | the canonical launch of YOUR OWN multithreaded workload by `/cell.manifest` (the supervisor, the restarts, the watchdog) — see §6; since v1.1 in ITS OWN address space (`talaria.cell=identity` is the former behaviour) |
| **`proc`** | the workload as a process in a private AS: fork/COW, signals, a dynamic ELF — the DBMS path (§9.9) |
| `fast` | the zero-syscall fast path (a cell on an AP + the application on the BSP) |
| `supervise` | the supervision of a single-threaded workload by a manifest (the historical path; for a new one there is `cell`) |
| `console` | the operator's console (the files, the editor, the network) — §9.1; it brings up ALL the machine's cards |
| `filed` | the same plus the service of uploading/downloading files over TCP (§9.3) |
| `gdb` | the gdb stub (the GDB RSP over COM2, `-M pc` is needed; on arm64 over the console's line) |
| `procdemo` | the full cycle of a process: `fork`+`execve`+`wait4` (a real busybox) |

---

## 5. Running an open-source application

In `services/` there are real unmodified programs, built statically against musl
and working on the kernel: mongoose 7.23, Lua 5.4.7, SQLite 3.46.1, busybox 1.36.1,
QuickJS-ng, Redis 7.2.5, and also three full-cycle DBMSes — **PostgreSQL 16**
(the postmaster + fork backends), **MariaDB 10.11** (InnoDB, threads) and **MongoDB 4.0**
(a dynamic ELF: ld-musl + 18 .so's, WiredTiger) — all of them with persistence on an RW ext2.
Every directory contains a `README.md`, a script that builds the disk and a `run.sh` (the launch + the probes).

The binaries and the disk images are build artefacts (in `.gitignore`), rebuilt by
the instructions from the service's `README.md`. After the kernel and the disk have been built:

```sh
services/redis/run.sh      # brings redis-server up, sends RESP probes (PING/SET/GET/...)
services/mongoose/run.sh   # brings the web server up, sends an HTTP GET + keep-alive
services/postgresql/run.sh # PostgreSQL: psql from the host, DDL/DML, the data survives a restart
# similarly: lua, sqlite, busybox, quickjs, mariadb, mongodb
```

What is needed from an application:
- **Static linking against musl** (`-static -no-pie`) at `0x10000000` — or a
  **dynamic ELF** (a PT_INTERP ld-musl / a glibc ld-linux) on an ext2 disk with the
  libraries (the launch goes through `talaria.mode=proc`).
- The use of a **supported subset of the syscalls** (§11). It is checked by
  the checker: `kernel/tools/talaria-check ./binary` (see §6.3). An unfamiliar syscall
  is printed into the log and returns an `-ENOSYS`.
- The network: listeners + a pool of incoming connections, an **outgoing `connect`/DNS/UDP** (v0.46+),
  epoll/poll/select — see the limits in §13.

---

## 6. Running YOUR OWN application

The canonical path is **without editing the kernel**: build the binary → pack it with `talaria-pack`
→ run `talaria.mode=cell` (or `proc`). A live example is `examples/hello-talaria/`
(`make run` — from the source to a working VM; there are C and C++ variants).

### 6.1. Build a static musl binary

The canonical recipe (C; for C++ there is the g++ from the `talaria-alpine-build` image, see
`examples/hello-talaria/Makefile`):

```sh
docker run --rm -v "$PWD":/w -w /w talaria-musl sh -c \
  'musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o myapp myapp.c'
```

The key part is `-Wl,-Ttext-segment=0x10000000`. By default musl links at `0x400000`
(that is Talaria's kernel area) — without the move to `0x10000000` the binary will not work.
(Dynamic ELFs — PIE ones — do not require that: the kernel loads them at its own base, see §6.4.)

### 6.2. Pack the disk: `talaria-pack`

One tool assembles the image (TFS1/TFS2/ext2) and generates the `/cell.manifest`:

```sh
# the minimum: the binary + the manifest (TFS1, read-only)
kernel/tools/talaria-pack -o disk.img --app myapp

# the arguments/the environment/the restarts/the pinning + extra files and directories
kernel/tools/talaria-pack -o disk.img --app myapp \
  --args="--port 80" --env HOME=/ --restart 3 --watchdog 10 --core 2 \
  config.ini:/cfg/config.ini data/:/data     # the host's path[:the path on the disk]

# the formats: --fs tfs1 (a flat RO one, names <=24 B) | tfs2 (RO directories) |
#          ext2 --size 128M (RW; for writing/persistence and for dynamic ELFs)
#          ext2 --journal (the same with a JBD2 journal — transactions of the metadata, §7.1)
```

The generated `/cell.manifest` can also be put in by hand:

```ini
binary=/myapp
args=myapp --port 80   # the whole argv (the first token = argv[0])
restart=3              # the auto-restarts on a failure
watchdog=10            # (opt.) a restart if there is no network progress for N s
core=2                 # (opt.) the LEGACY alias of `pin=main:2`
env=HOME=/             # (opt., repeatable)

# the layout of the threads over the cores (§6.6)
pin=main:dedicated     # the main thread gets a dedicated core, the OS chooses its number
pin=md_*:dedicated     # every thread whose name starts with md_* gets a dedicated core
pin=bg*:shared         # these go into the common pool
spread=on              # the unpinned ones spread over the free cores (the default)
balance=on             # the levelling of the common pool's load (the default)
numa=on                # bring the threads of one process together onto one node (the default)
```

### 6.3. Check the compatibility: `talaria-check`

```sh
kernel/tools/talaria-check ./myapp --secs 15 [--net]
# -> the top of the syscalls, the list of the unsupported ones (-> ENOSYS),
#    the verdict: COMPATIBLE / HAS GAPS / INCOMPATIBLE-CRASH
```

The checker packs the binary itself, runs it in QEMU with a trace of the syscalls and parses the serial output.
If there are gaps, the syscall is implemented as per DEVELOPMENT-GUIDE §10.1.

### 6.4. Run it: `talaria.mode=cell` or `talaria.mode=proc`

```sh
K=kernel/target/x86_64-talaria/release/talaria
docker run --rm -i --device /dev/kvm -v "$PWD":/w talaria-qemu2 bash -c '
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
    -kernel /w/'"$K"' -append "console=ttyS0 talaria.mode=cell" \
    -display none -serial stdio -no-reboot \
    -drive id=d0,file=/w/disk.img,format=raw,if=none \
    -device virtio-blk-device,drive=d0'
```

- **`talaria.mode=cell`** — a multithreaded workload (clone/futex, C/C++/pthreads) under
  the supervisor: the isolation of a failure, the auto-restart `restart=`, the watchdog, the instance's memory
  is returned at a restart. The NIC is optional (without it the sockets will return an error).
- **`talaria.mode=proc`** — a process in a private address space: `fork`/COW,
  signals, fd tables, **dynamic ELFs** (ld-musl/ld-linux from an ext2 disk) — the path of a
  DBMS (PostgreSQL/MariaDB/MongoDB). It requires a virtio-net.
- With the network, add a tap (see §7.3); for more cores there are `-smp N` + `talaria.smp=N` (§7.2).

> The path "add a mode of your own by editing `main.rs`" stays for the kernel's developers
> (DEVELOPMENT-GUIDE §10.2) — for running applications it is no longer needed.

### 6.6. Lay the threads out over the cores

A core in Talaria is in one of two states, and the difference between them is the whole
point of this OS:

| The core's state | What is on it | The timer | For whom |
|---|---|---|---|
| **Dedicated** | exactly one thread | **not armed** | latency-critical threads: the market's intake, the datapath, a hot loop |
| **Shared** | any number of threads, round-robin | the LAPIC tick | everything else: the logs, the GC, the I/O, the auxiliary |
| Core 0 | the control plane, always shared | the LAPIC tick | the OS; it cannot be given to the workload |

The shared cores, in their turn, are divided into **named pools** (`pool=`) and
the **common pool** — everything not named into any pool.

The layout is declared in the manifest — **by rules rather than by numbers**:

```ini
pool=fast:1,2          # A NAMED POOL: these cores are given to one class of threads
pool=bg:3-7,10         # the numbers and the ranges separated by commas

pin=main:dedicated     # the main thread gets a dedicated core, the OS will choose its number
pin=md_*:fast          # the threads whose name starts with md_* go into the pool fast
pin=strategy:3         # an exact name -> a specific core (when the number really matters)
pin=*:bg               # a trailing rule: all the rest go into the pool bg
spread=on balance=on numa=on
node=0                 # (opt.) which NUMA node to put the workload and its memory on
irq=0                  # (opt.) where to send the devices' interrupts
```

- **The pattern** is either the keyword `main` (the process's main thread) or a thread's name
  with a `*` at the start and/or at the end (`rx*`, `*_worker`, `*io*`). The name is set by the thread itself:
  `pthread_setname_np()` / `prctl(PR_SET_NAME)`. The rules are ordered — the first
  matching one is applied.
- **The target** is a core's number, `dedicated` (`any`, `exclusive` are synonyms), `shared`
  or **a pool's name** (one may write `@fast` if the name looks like a keyword).
  A core's number is a property of the machine rather than of the workload: on another machine it is a different one,
  so by default ask for `dedicated` or for a pool and let the OS choose the core.
- **A pool** (`pool=<name>:<cores>`) is the middle ground between "a specific core" and
  "anywhere": the operator names the cores ONCE and in one place, while
  laying the threads out inside the pool and levelling them between its cores is the OS's work.
  The pools **do not intersect**: a core belongs to at most one of them, and everything
  unnamed forms the common pool. The cores of a named pool are given neither to
  dedicated ones nor to the threads of the common pool, while the levelling goes INSIDE a pool — a skew
  between the pools is not a skew but your decision. The order of the lines does not matter: a rule
  is entitled to refer to a pool declared below.
- A thread that has landed in a pool sees **exactly its cores** in `sched_getaffinity` — by
  that answer the workload will size a thread pool of the needed size for itself. The child
  threads inherit the creator's pool, while the rule `*` also applies to the threads that have not
  named themselves (for the targets "a pool" and "a specific core" — `dedicated` waits for a name).
- **`spread=on`** (the default) — the threads for which there is no rule spread over
  the cores of the common pool: a new thread sits down on the least loaded one.
- **`balance=on`** (the default) — if there are more threads than cores, the load
  is levelled: once every 64 ticks ONE non-executing thread moves from the busiest
  core onto the freest one, and only at a skew of 2 tasks or more. In
  the steady state the migrations are zero — the goal is an even load rather than even
  statistics of rearrangements. The load is counted by **demand** rather than by heads:
  a thread asleep on a socket does not load the core, and a core with five such threads is
  freer than a core with two working ones.
- **`numa=on`** (the default) — the threads of one process are brought together onto one NUMA node
  **together with their memory**: from that node come the stack, the heap, the private mappings
  (`mmap` — malloc's arenas), the COW copies and the FS's block cache (the nodes and their ranges
  of memory are taken from the ACPI SRAT). "Where possible": the node gives way
  when it is overloaded relative to the machine by more than one task. `node=<N>` is
  to name the node explicitly (the operator knows which socket the card and the disk are attached to, while the OS
  does not see that from the ACPI); a non-existent node is rejected with a warning.
- **`irq=<core>`** — where to deliver the devices' interrupts (MSI-X). By default it is
  the control plane. They will not be given to a dedicated core: an interrupt on the core of a
  latency-critical thread is jitter that comes not from us and not on our
  schedule, that is, a cancellation of the whole idea of a dedicated core.

What happens to a dedicated core if by the moment of the request it is occupied: a rule is
senior to the spreading, so the OS takes the core on which only unpinned
threads sit and resettles them. A core where a thread placed by ANOTHER rule lives is not
taken away — two rules are not entitled to cancel each other silently. If there are no free and no
freeable cores at all, the thread goes into the common pool, while an explicit refusal goes into the log
(`sched-plan: there is NO DEDICATED core`) — a silent success is worse than a refusal.

What the kernel prints at the start (`sched-plan:`) is the whole plan: how many rules have been
parsed, how many have NOT been parsed, which pools have been declared, which cores the
scheduler has, how many NUMA nodes there are. The live
picture is shown by the console command **`cores`**:

```
cores: the scheduler's cores 0xf, migrations 3 (of them by the balancing 1), refusals of a dedicated core 0
cores:   the core 0 the node 0 shared (common): t0(main)*
cores:   the core 1 the node 0 dedicated: t1(md_feed)*
cores:   the core 2 the node 0 shared the pool bg: t3(bg_log) t6(bg_io)
cores:   the core 3 the node 0 shared the pool bg: t4(bg_gc) t7(bg_stat)
```

(`*` means the thread was placed by a rule; `ps` also shows a thread's core and name.)

Without a manifest the same thing is set by the command line — `talaria.pool=`,
`talaria.pin=` and `talaria.sched=` (§7.1). One must not forget to give out the machine's cores:
`-smp N` + `talaria.smp=N`.

What the layout does NOT do (deliberately):

- it does not move a thread that is executing right now — a move costs one
  write only for a pre-empted thread, while to interrupt a latency-critical
  workload for the sake of the balance would be exactly the jitter we are getting away from;
- it does not cancel the workload's `sched_setaffinity`: if the application itself knows where
  it wants to live, it stays right — the manifest's rules are for those who do not know
  (such a thread is marked and the balancer does not move it any more);
- it does not divide core 0: the control plane is bound to stay responsive.

### 6.7. Divide the shared cache (`llc=`, Intel CAT)

You have already laid the cores out — but their L3 is shared, and a neighbour driving a large working
set washes yours out of it. That is measured rather than assumed: on an `m5zn.metal`
an LLC hog on a neighbouring core raised a latency-critical thread's p99 from 650 to 932 ns, and its
p999 — from 686 to 1216 ns.

The cache's share is hung on the **same group of cores** you have already named:

```ini
pool=fast:1,2          # the cores — as in §6.6
llc=fast:0xff000       # the upper 8 ways of the L3 go only to this pool
llc=0:0x00fff          # core 0 (the control plane) gets the lower 12
```

- On the left there is a pool's name, a core's number or a `*` (all the scheduler's cores). The cache
  deliberately has no numbering of cores of its own: two independent numberings of one and the same
  thing sooner or later diverge, and they diverge silently.
- On the right there is a mask of the L3's associativity ways, hexadecimal or decimal.
  **The bits are bound to be contiguous and the mask non-zero** — that is a requirement of the hardware
  rather than ours; a mask with a hole is rejected before the write into the MSR (otherwise there would be a `#GP`).
- How many ways your chip has and how many classes it supports the kernel prints
  at the bring-up (`cat: there is L3 CAT — ways in the mask 20, classes 16 ...`).
- The class 0 (everything not named) the kernel **narrows** down to the remainder: otherwise the isolation would
  be one-sided — the latency thread is protected from the background one, while the background one still
  washes its share out.
- The class is assigned to a core ONCE rather than at a context switch: the hot
  path does not get dearer by a single cycle.

The live picture is the same `cores` command (the second block):

```
cat: ways in the mask 20, classes 16, the L3 50688 KB
cat:   the class 0 = 0xfff (12 ways, ~30412 KB) the cores 0x1
cat:   the class 1 = 0xff000 (8 ways, ~20275 KB) the cores 0x6
```

**Where this works.** CAT is a feature of server Intels (RDT-A), and it is not given to a KVM
guest: under QEMU and on an ordinary EC2 instance the kernel will honestly say `cat: L3 CAT
is unavailable` and will **reject the `llc=` rules out loud**. The working stand for it is
a `*.metal`. There is no separate "DDIO knob" and there will not be: on server Intels DDIO
is always on and cannot be switched off by software, while the undocumented MSR `0xC8B` we
will not touch (on an i7-3820 it gave out an `EIO`). CAT is the only
documented lever here.

The second, free lever of the same conversation is the **footprint of the DMA rings**. A receive
ring that does not fit into the L3 gives "leaky DMA": a fresh buffer is evicted before
it has been read (the measurement of v0.15: 251 against 1407 LLC misses per 1000 packets,
x5.6). The kernel computes the footprint itself and says so if it has grown out:

```
cat: the footprint of the ENA receive rings — 1 queues, 520 KB (the L3 50688 KB)
```

### 6.8. The clock

Talaria runs `CLOCK_REALTIME` by the TSC from a reference point, and that point is
**pulled towards an external clock** — kvmclock under KVM (x86), an SNTP server over the
network with `talaria.ntp=` (any machine; on EC2 the Amazon Time Sync Service, the only
reference a Graviton has). Without that one calibration would last exactly the length of a demonstration:
a quartz drifts by 10-50 ppm, that is by 1-4 seconds a day (on our dev stand
95 ppm was measured — 8.2 s/day), while on that clock stand the orders' timestamps, the files'
`mtime` and the one-way delays in measurements.

What is adjusted is the **rate rather than the value**: the time stays monotonic. A step
by value is left only for the case when the error is greater than 100 ms (that is no longer a
drift but another epoch), and such steps are counted separately — including the steps
backwards, after which timestamps must not be compared.

- `talaria.clocksync=<ms>` — the adjustment's interval (by default 1000; `0`
  switches the discipline off completely).
- `talaria.ntp=aws|<a.b.c.d>[,<poll s>]` — SNTP to `169.254.169.123` (`aws`) or the given
  server, a poll every 16 s by default. Each poll is a burst of four queries and the one with
  the shortest round trip is the sample (its error is bounded by half that round trip); the
  first sample before the workload starts SETS the base rather than stepping it, and the
  network reference owns the servo (kvmclock stands aside). The boot waits up to 3 s for that
  first sample and says so if the server is silent. On a `c7g.large` the clock stays within
  ±21 us of the Amazon Time Sync Service judged from a Linux neighbour; the servo's
  correction is the crystal's own (+15 ppm there).
- `talaria.epoch=<seconds>` — set the epoch's base by hand (when there is no source).
- The console command **`clock`** shows the reference, the correction in ppm, the residual
  error and the counters of the steps:

```
clock: unix=1787936553 s, the reference kvmclock, the correction 95 ppm, the error -4781 ns
clock: samples 20, adjustments 20, steps by value 0, steps BACKWARDS 0
```

What is NOT here: the PTP protocol (IEEE-1588) and hardware timestamps on the packets.
The ENA's PHC is read as a time reference, but Talaria does not speak 1588 into the network, while
per-packet hardware timestamps the ENA does not give out at all — see `docs/ROADMAP.md` §2.8.

---

## 7. The launch parameters

### 7.1. The kernel's cmdline (`-append`)

- `console=ttyS0` — the kernel's output into the first serial port (mandatory for the logs).
- `talaria.mode=<name>` / `talaria.test=<name>` — the choice of the mode (§4). If it is not set,
  the kernel goes along the default path (the control plane/the datapath).
- `talaria.ip=A.B.C.D/P`, `talaria.gw=A.B.C.D`, `talaria.dns=A.B.C.D` — the network
  configuration of the FIRST interface (by default a tap: `10.0.0.2/24`, the gw/dns
  `10.0.0.1`; for slirp: `10.0.2.15/24`, the gw `10.0.2.2`, the dns `10.0.2.3`). Out of it
  the kernel generates the virtual `/etc/resolv.conf` and `/etc/hosts` for musl's resolver.
- `talaria.net=<if|MAC>:<A.B.C.D/P[,gw=A.B.C.D] | dhcp | down>` — the configuration
  of ANY interface (`net0`, `net1`, … or a MAC with hyphens). The key may be
  repeated — one per interface (§9.2).
- `talaria.route=<net/length>,<gateway>,<interface>` — a route into the table
  (`talaria.route=10.9.0.0/16,10.0.1.1,net1`). Repeatable as well.
- `talaria.filed=<port>`, `talaria.filedtoken=<secret>` — the service of uploading/
  downloading files over TCP (§9.3); `talaria.mode=filed` brings it up on the default
  port (7373). **Without a `talaria.filedtoken=` the service rejects any
  command** — there is no default password.
- `talaria.ctltoken=<secret>` — the token of the `/ctl` route (§9.4). **If it is not set, the route
  is closed entirely**, and any request gets a 401.
- `talaria.rows=N`, `talaria.cols=N` — the size of the operator's terminal (80x24 by
  default): the pagination of `less` and the columns of `ls` depend on it.
- `talaria.epoch=<unix seconds>` — the base of CLOCK_REALTIME (by default it is taken from
  kvmclock — the host's exact time; under KVM nothing has to be set).
- `talaria.strace=N` — print the workload's first N syscalls (the diagnostics of a launch).
- `talaria.top=<ms>[,rss]` — print a snapshot of `top` into the serial line every N milliseconds
  (no more often than 100 ms). For a machine without an operator: a degradation that happened at night
  stays in the log rather than waiting for somebody to type `top`. The walk over the page
  tables for the RSS is off by default (`,rss` switches it on — it costs milliseconds
  per process).
- `talaria.prof=<hz>` — the sampling profile with the call stack from the boot itself
  (the same as `perf on` in the console). `talaria.prof=1` is a sample on EVERY tick
  (that is how it used to be), any other number is that many samples a second. To look at it there is
  `perf top`, to dump it there is `perf record`.
- `talaria.futexhang=<seconds>` — print "IT HUNG" about a thread that has not
  woken up within N seconds of waiting on a futex, together with a snapshot of all the tasks. By
  default it is SWITCHED OFF: in a real workload a thread asleep on a futex for longer than
  a few seconds is the norm (that is how any thread pool lives), and the kernel would be reporting
  a failure where everything is in order. Switch it on when you are looking for a lost wake-up.
- `talaria.pmuchain=1` (arm64) — use a declared CHAIN for 64-bit counter pairs. Off by default:
  a Graviton VM declares CHAIN and its pairs do not carry; below PMUv3p5 the kernel widens the
  32-bit counters itself, which is exact while a core reads its counters at least once per wrap
  (the tick's snapshot does, every 20 ms; a dedicated core unread for longer loses the wraps).
- `talaria.pmutick=0` — do NOT take the PMU counters in the scheduler's tick. By
  default they are taken (seven `rdpmc`s per tick — fractions of a per mille at a slice of 8 ms), and
  without them `top`/`perf` will not show the IPC and the misses per core.
- `talaria.console=nopoll` — do NOT poll the operator's console from the scheduler's
  tick. By default it is always polled (with a step of ~8 ms: the fingers do not notice that,
  while it is cheaper for the kernel than on every tick), including in the modes without a supervisor —
  `talaria.mode=proc`, where PostgreSQL, MariaDB, MongoDB and ClickHouse live.
  The flag switches the polling off completely: for a machine where every microsecond matters
  and the console is not needed. The values are listed separated by commas
  (`talaria.console=vga,nopoll`).
- `talaria.gdb=break` — enter the gdb stub (x86: COM2, `-M pc` is needed; arm64: the console's line) before the workload's first
  instruction: the breakpoints are set before anything has been executed.
- `talaria.core=<path>[,full]` — write **ELF core dumps** (`ET_CORE`, read by
  an ordinary `gdb`): at a fatal signal to the workload it is a dump of the process, at
  an exception in the kernel it is a dump of the kernel, at the `core` command in the console it is a dump of a live
  task. By default into the dump go the registers of all the threads and their STACKS; `,full`
  adds all the process's private memory (in a large workload that is hundreds of
  megabytes — think about the room). The analysis is in `docs/PERFORMANCE-OPTIMIZATION-GUIDE.md` §10.3.
- `talaria.sigtrace=N` — print the first N SYNCHRONOUS signals to the workload
  (the signal, the address, the rip, the thread, the core) together with what the kernel sees in the page
  tables at that address. It is needed when the workload dies silently in its own
  handler: the discrepancy "the processor says «there is no page» while in the tables it
  is there" is a stale TLB, and it is visible only from here.
- `talaria.console=vga` — a second output channel: the text screen (0xB8000). By
  default it is off; on the BIOS path it used to switch itself on until v1.7, but on a `*.metal` it
  turned out to be invisible, while accesses to the legacy window on a machine without VGA are an extra risk.
- `talaria.trace=<LBA>` — **the diagnostics on a machine without a console**: the kernel's whole output
  accumulates from the first line and is written as raw sectors onto the disk (the header
  `TALARIA-TRACE len=… flushes=…` in that sector, the text from the next one, 64 KB).
  The flush goes at every phase of the bring-up and by itself every 512 bytes, so after
  a hang the log lies on the disk exactly up to the place of the stop. It is read by
  `infra/aws/read-disk-trace.sh`. The image of `tools/mk-tboot.sh` is needed (room is reserved in its
  gap); the mode does not affect the datapath — outside it the collection is switched off.
- `talaria.suite=<LBA>` — run a LIST of modes in one boot of the image: the list is put in by
  `tools/mk-tboot.sh SUITE="talaria.test=ext2;talaria.test=wc;…"`,
  the kernel takes its own item, writes the log into its own slot and reboots the machine for
  the next one. The counter lives on the disk: one sees which item it stopped at, and
  a repeated launch continues from it. For bare metal, where every launch of
  an instance costs minutes of POST and money.
  The list's items are appended to the cmdline AND ARE RE-READ in full (the early options and
  the network ones): a `talaria.ip=dhcp` in an item works the same as in the base line.
- `talaria.rebootat=<mb2|idt|lapic|mem|paging>` — on reaching that point of the bring-up the kernel
  reboots the machine. A signal for the case when the disk is not up yet and there is nowhere to write
  the trace (the number of reboots is visible in the bootloader's counter).
- `talaria.heartbeat=N` — the kernel's pulse: once every N entries into the scheduler a line with
  the tick's number, **separately** the number of REAL timer interrupts and of software
  yields, the number of live tasks and a stuck ISR bit. The absence of a pulse means
  exactly one thing: there is no pre-emption any more. On a machine without a console that is the only
  way to tell "the workload is standing" from "the kernel has died".
- `talaria.sync=hlt|mwait|mwaitx|waitpkg` — the waiting tier forced instead of
  the one chosen by the detection. It is needed to tell "it is about the tier" from
  "it is about something else" in one run: under a hypervisor the tier is always `hlt+ipi`, on hardware it is `mwait`.
- `talaria.slice=N` — the length of the scheduler's slice **in nanoseconds** (by default
  500 000, that is 0.5 ms). It used to be the LAPIC's initial counter, and it meant
  different things on different hardware: at N=500000 the tick went once every 8 ms under QEMU and once every
  320 ms on a `c6i.metal` — the "slice" differed by forty times while it read as "500 us".
  Now the timer's frequency is measured by the TSC, and N means one and the same thing everywhere.
  The knob is needed for two things: to reproduce a long hardware slice at home and to **thicken
  the sampling profile** (a sample is taken by the tick, see §10.4) — but a short
  slice itself changes what you are measuring.
- `talaria.irr=N` — on the workload's first N syscalls print the caller's RFLAGS
  (the IF bit), the IRR and ISR bits at the timer's vector and the number of real ticks. The syscalls are
  the only thing that executes while the workload is alive: if there is no pre-emption,
  the scheduler is not entered and the pulse is silent, while the syscalls go on.
- `talaria.switches=N` — print the first N context switches: the target
  task, its RSP, CR3, RIP and RFLAGS.
- `talaria.sysprof=1` — count the PRICE of the syscalls by number (the calls, the total
  time in the kernel, the nanoseconds per call); it is printed in `stat`. It costs a pair of `rdtsc`s per
  call (~30 ns at a syscall price of ~100 ns) — that is diagnostics rather than a working mode:
  keep it switched off during a measurement.
- `talaria.stuck=N` — print if a task does not leave the core for N ticks in a row,
  TOGETHER WITH THE ADDRESS: an eternal loop and a lost wake-up are indistinguishable by the single fact
  "it does not leave", while by the address they are not.
- `talaria.pin=<pattern>:<target>[,...]` — the rules of the layout of the threads over the cores without
  a manifest (§6.6): `talaria.pin=main:dedicated,rx*:fast,*:shared`. The target is
  a core's number, `dedicated`, `shared` or a pool's name. The manifest is senior: if there is one,
  this knob keeps silent.
- `talaria.pool=<name>:<list of cores>` — declare a named pool; one declaration
  per token (`talaria.pool=fast:1,2 talaria.pool=bg:3-7`). Unlike
  the manifest, the command line is read once from left to right, so declare the pools
  BEFORE the rules that refer to them.
- `talaria.nic=<name>` — take a SPECIFIC network card driver instead of
  the automatic selection by the table (`virtio-net`, `ena`, `loop`). The operator is
  senior to the automatics: if the named driver did not come up, the kernel will say so and
  will not silently take another one — otherwise "the card is the wrong one" and "the card did not come up"
  would look the same. What is in the table and what it sees in the machine
  is shown by the console's `nics`.
- `talaria.clocksync=<ms>` — the interval of the clock's discipline (§6.8); `0` switches it off.
- `talaria.ntp=aws|<a.b.c.d>[,<poll s>]` — the wall clock's reference over the network (§6.8).
- `talaria.sched=spread|nospread|balance|nobalance|numa|nonuma|node=N|irq=N` —
  the same switches as the `spread=`/`balance=`/`numa=`/`node=`/`irq=` in
  the manifest, separated by commas.
- `talaria.test=timer` — a self-sufficient probe of the LAPIC timer: it programs
  the periodic timer, opens the interrupts and counts for a second. It tells apart
  "the timer interrupts the kernel", "an interrupt waits in the IRR (it is about the IF/TPR)" and "the timer
  counts but does not raise interrupts".
- `talaria.test=serial` — a check that an ABSENT port is not taken for a
  source of data (`LSR == 0xFF` is a floating bus, not a byte) and that the loop of polling
  the console is bounded.
- `talaria.poweroff=1` — having run the mode, **power the machine off** (ACPI S5: the FADT +
  the object `\_S5` from the DSDT/SSDT). On EC2 the instance goes into `stopped`, that is,
  the billing stops by itself, while the instance's state becomes an unambiguous answer
  "the run reached the end" — on bare metal, where there is no console, there is nothing else to tell that from
  a hang. In the set mode (`talaria.suite=`) the power-off at the end of the
  list happens without the flag too.
- `talaria.pwrbtn=<s>` (the default 20; 0 — do not catch it) — the **power button**.
  `aws ec2 stop-instances`, `virsh shutdown`, QEMU's `system_powerdown` are
  a press of the ACPI button: the hypervisor sets the PWRBTN_STS and waits, and after a few
  minutes de-energises the machine hard. The kernel polls the bit from the tick (4 Hz, no SCI is
  needed), at a press sends a SIGTERM to all the processes and gives them `<s>` seconds to leave
  by themselves (a DBMS closes in the regular way); they left — a sync of the FS and S5; they did not leave — a sync from
  the tick and S5. Before any power-off and before a reboot for the next item of a
  set the block cache is flushed: what has been written not under an fsync reaches
  the disk, and the journal is closed with a checkpoint. QEMU's microvm has no block of PM1 events —
  there a stop from the outside stays a power cut (the kernel prints that at
  the start). The console command `poweroff` goes along the same path.
- `talaria.userdata=1` (EC2 only) — read the TAIL of the cmdline out of the instance's user-data
  (the IMDS `/latest/user-data`) and glue it to the line from `talaria.cfg`.
  It lets one keep ONE AMI and change the experiment's parameters by launching an instance
  rather than by rebuilding the image. It requires the network, so `talaria.ip=`/`talaria.pci=1`
  are bound to be in the `talaria.cfg`. **A limitation:** the mode `mode=fast` cannot be chosen
  this way (the zero-syscall cell takes the card for itself before the stack comes up).

**The file system (ext2):**

- The journal is on if the image was created with one: `mkfs.ext2 -O has_journal` (that is what
  `kernel/mkext2.sh` does). Talaria runs transactions of the metadata in the JBD2 format and
  replays the journal at the mount; `e2fsck` understands our journal and the other way round.
  The guarantee: after a successful `fsync` a crash will not leave the metadata in an intermediate
  state. If there is no journal in the image, the FS works as before, without transactions.
- `talaria.ext2=nojournal` — do not use the journal even if there is one
  (a comparison/diagnostics).
- `talaria.ext2=journal=data` — journal the DATA together with the metadata (by
  default the mode is ordered: the data is written in place BEFORE the commit). It is dearer in the
  journal's volume, but after a crash the file's contents are recovered as well, not
  only the metadata's consistency. A transaction that does not fit into the journal entirely
  honestly falls back to ordered.
- `mmap(MAP_SHARED, fd)` is supported: the pages of the block cache are mapped, so
  `read/write` and the mapping see the same bytes. The changes reach the disk
  at an `msync`/`munmap`/`fsync`/the exit. Since v1.2 the pages are mapped ONE BY ONE,
  so a contiguous window of the cache is no longer required — a file scattered over the disk
  is mapped just as well. `ENODEV` stays only for the workloads in the kernel's identity address
  space (`talaria.cell=identity`), where a virtual address by
  construction equals the physical one.

**The workload's address space:**

- `talaria.mode=cell` now executes in ITS OWN address space (the segments,
  the stack and the heap are private pages). `talaria.cell=identity` returns the former
  behaviour (the kernel's identity memory) — for the case of workloads tied to the
  coincidence of the virtual and the physical address.

**The network on EC2 (the ENA):**

- **The LLQ + the accelerated mode are on by default** (since v1.4). The transmit descriptors
  and the frame's header are written into the device's memory (a Low Latency Queue, the
  window is mapped write-combining), the `accel_mode` is declared to the device
  (`DISABLE_META_CACHING | LIMIT_TX_BURST`) and a meta descriptor goes in every
  packet — exactly as in the reference driver. On a c6i.large that gives an RTT of
  **34 us against 43 in Linux -> Linux** on the same segment.
  `talaria.ena=llq` is left as a no-op (the old scripts write it);
  `talaria.ena=nollq` returns the descriptors into the host's memory,
  `talaria.ena=noaccel` removes the accelerated mode — both only for an A/B.
- `talaria.kecho=<port>` — a UDP echo straight in the kernel (without a process in ring 3);
  `talaria.kts=1` adds the kernel's clock into the first 8 bytes of the answer (the measurement of
  one-way delays by the utility `bench/udp_owd.c`).
- `talaria.netidle=1` — while idle, SLEEP on the receive ring instead of busy-polling.
  Since v1.2 it works on **virtio** as well (a sleep on the word of the receive used ring: the device's
  DMA write wakes an MWAIT without an interrupt and without a vmexit; the insurance is a timer).
  The price is about +14 us to the RTT; the gain on virtio under QEMU: the passes over the stack per
  round trip 8 000 000 -> 4 551.
- `talaria.netirq=1` — an interrupt from the card (a line through the I/O APIC; the line's number is
  derived from the layout of the mmio slots, by hand it is `talaria.netirq=<gsi>`, for a bisect there is
  `=all` or `=a-b`). Together with the line the PARKING of a waiter on a socket is switched on:
  a thread in a `read`/`epoll_wait`/`accept`/`recvfrom` stops polling the stack and
  SLEEPS, while it is woken by the delivery — the line's handler drives the stack and rings the socket's
  owner. They must not and need not be switched on separately: a line without the parking adds a
  handler ON TOP of the polling (measured: worse), while the parking without a line leaves
  the stack without a single pass.
- `talaria.netpark=<ns>` — the parking's ceiling (by default 1 000 000, that is
  1 ms). It is an insurance against a lost wake-up rather than a delivery mechanism:
  with a healthy line a waiter is woken by a frame, and it does not come to the ceiling.
  `talaria.netpark=0` switches the parking off (the former polling) even with
  a laid line. That it is exactly the delivery that wakes can be checked by
  `kstat net: waiter parkings ... exited on the ceiling N (M%)` — a large percentage on
  the ceiling means the line keeps silent.
- `talaria.pollhard=<ns>` — how often to drive the stack when the CARD IS SILENT (by
  default 100 000, that is 100 us). An idle pass is cancelled not by the clock but
  by the receive ring's word: the card has brought nothing — there is nothing to parse. A pass
  for the sake of the stack's own timers (retransmits, delayed ACKs, TIME-WAIT) is
  nevertheless made no less often than that deadline. `=0` returns the former threshold by time
  (`talaria.pollgap=`, 10 us). That the sign "the card is silent" is true on
  your hardware can be checked by `kstat net: ... the ring lied N times` — normally there is
  a zero there (under load fractions of a percent: that is a frame that arrived between the read of the word and
  the taking of the lock).
- `talaria.netprof=N` — a profile of the datapath by stages, a summary once every N round trips.

**The ENA's diagnostic flags (they are NOT included into the production path, only for measurements):**

- `talaria.ena=phc` — the device's clock (the AWS Time Sync through the ENA,
  `ENA_ADMIN_PHC_CONFIG`). It is switched on only if the device declares the feature;
  the self-check of its rate is `talaria.test=phc`. On a c6i.large the feature is NOT declared.
- `talaria.ena=rss` / `rsstbl` / `rsshash` — configure the RSS the same way as the
  Linux driver does: both commands, only the redirection table, only
  the hash function. `rsscq` is the former (wrong) variant of the table with a CQ index,
  left for comparison: with it the reception of frames disappears.
- `talaria.ena=hibuf` — keep the frame buffers in memory above 4 GB (the rings are there
  anyway). It did not affect the latency — the flag is left as a checking tool.
- `talaria.ena=nowc` — do NOT convert the LLQ window into write-combining (leave it
  uncached, as it was before v1.4). Only for an A/B: the reference ena driver maps
  this window through a `devm_ioremap_wc`, and by default we do the same.
- `talaria.ena=meta` — only the meta descriptor, without declaring the accelerated
  mode. Left as a separator of the effects: by itself it makes things WORSE
  (103 us against 92), what decides is exactly the declaration of the `accel_mode`.
- `talaria.ena=nocqdb` — do not ring the CQ's head (a modern `ena_com` does not make that
  doorbell at all).
- `talaria.ena=intr` — allow the device's interrupts once at the
  initialisation (the moderation intervals 0 + an unmask of both CQs); the datapath polls
  anyway. It differs from the removed hypothesis "unmask at the end of a pass" in that
  nothing is written in the datapath.
- `talaria.ena=txbench=N` — a run of the TRANSMIT right after the card's bring-up: N frames
  of 64 B go into the ring as fast as the device takes them, and the result is
  printed into the serial line. The frame is addressed to our own MAC with the ethertype 0x88B5 —
  the Nitro switch drops it, and the network is not littered.

### 7.2. QEMU's flags (the memory, the cores, the devices)

| The flag | The meaning |
|------|-------|
| `-M microvm,rtc=off` | a minimal machine (a fast start). For the gdb stub `-M pc` is needed. |
| `-enable-kvm -cpu host` | hardware virtualisation, all the CPU's features to the host's (AVX and so on). |
| `-smp N` | the number of vCPUs. `-smp 2` is needed for `mode=fast` (a cell on an AP + the application on the BSP) and is useful for the network modes; `-smp 1` is for the single-threaded demos. |
| `-m 512` | the volume of RAM (MB). The kernel chooses the largest RAM region above 32 MB for the allocator. |
| `-kernel <talaria>` | the kernel's PVH image. |
| `-serial stdio` / `-serial file:/tmp/o.log` | where to output the console/the logs. |
| `-no-reboot` | do not reboot at a `#TripleFault` (the crash dump is visible). |
| `-drive …,if=none -device virtio-blk-device` | a TFS1 disk (see §6). |
| `-netdev tap… -device virtio-net-device` | the network (see §7.3). |

### 7.3. The network

Talaria brings a virtio-net up and a static IP `10.0.0.2/24`; the host is `10.0.0.1`.

```sh
# inside the container (--device /dev/net/tun --device /dev/vhost-net --cap-add NET_ADMIN are needed)
ip tuntap add tap0 mode tap
ip addr add 10.0.0.1/24 dev tap0; ip link set tap0 up
# ...add to QEMU:
#   -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
#   -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56
```

The ports by mode: HTTP `:80` (httpd/mongoose/server), Redis `:6379`, the UDP echo
`:9999` (fast) and others (see the specific `run.sh`).

**Going out to the internet (the workload's outgoing connections).** The ring 3 stack supports
an outgoing `connect` (TCP, blocking and O_NONBLOCK), UDP, DNS (musl's `getaddrinfo`/
`gethostbyname` through UDP :53 to the `talaria.dns`) and a default route. The simplest way is
QEMU's slirp (NAT + a DNS relay, no NET_ADMIN is needed):

```sh
-netdev user,id=n0 -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56
-append "console=ttyS0 ... talaria.ip=10.0.2.15/24 talaria.gw=10.0.2.2 talaria.dns=10.0.2.3"
```
For a tap with a way out one needs NAT on the host's/the container's side (masquerade) and
a DNS server on the gateway (`kernel/tools/minidns.py` is a mini DNS for the regression).

### 7.4. The workload's memory (the kernel's constants)

- The user's region: **32 MB … 1 GB**, identity-mapped, U/S.
- The heap/`brk` (in the common AS): **128 MB**, NX.
- A process's private heap: **16 MB** at `0x30_0000_0000`.
- A process's stack: the top is `0x7fff_f000_0000`, below the bottom it is not mapped (a guard).
- The user binary is linked at **`0x10000000`**.

These values are in the kernel's sources (`mem.rs`, `syscall.rs`, `user.rs`); changing them is
the kernel developer's business (DEVELOPMENT-GUIDE §5).

---

## 8. The file systems TFS1 / TFS2

A flat read-only image:

```
[sector 0] the header: u32 magic="TFS1"(0x31534654), u32 nfiles,
           then nfiles records: the name[24] + u32 lba + u32 len
[sector 1..] the files' bodies, each aligned to 512 B
```

The limitations of TFS1: read-only, a flat namespace, a name ≤ 24 bytes, 16 files.

**TFS2** (for the workloads with a tree of configs/secrets) — paths of up to 83
bytes, directories, up to 128 records; the packer is `kernel/mkfs2.py`:

```
[sector 0] u32 magic="TFS2"(0x32534654), u32 count, u32 dirsec (the directory's sectors)
           records of 96 B one after another from byte 16 through the sectors 0..dirsec-1:
           the name/path[84] + u32 lba + u32 len + u32 flags (bit0 = a directory)
[sector dirsec..] the files' bodies, each aligned to 512 B
```
```sh
python3 kernel/mkfs2.py out.img ./bin/app:app ./cfg/app.ini:cfg/app.ini   # the ancestor directories are added by themselves
python3 kernel/mkfs2.py out.img @spec.txt                                 # the lines are host:disk/path
```
The kernel mounts either of the formats automatically (by the magic). The paths are normalised
(`//`, `./`, a leading `/`), `stat`/`newfstatat`/`statx`/`access` work for
directories too; a `mkdir` of an existing one → EEXIST, otherwise OK (the write goes into a null sink).
The kernel's virtual files: `/etc/resolv.conf`, `/etc/hosts` (from the `talaria.dns=`),
`/proc/self/exe` (the path of the workload's binary), `/proc/self/fd/N`.

For writing/persistence the application has to keep its state in memory (e.g. Redis is
built without persistence).

---

## 9. The operator's side

### 9.1. The operator's console (`talaria.mode=console`)

A control interface over the serial line. This is not a Unix shell (no pipelines, no scripts,
no substitutions) and not busybox: the console lives **in the kernel** on the BSP and depends only
on the serial line and the block device. That is how it is meant to be — it is needed exactly when
the workload has crashed, the CPU cores are busy with the datapath, and there is nothing and no point in bringing
up one more process in ring 3 for a shell.

What is typed works as in a usual shell: **Tab** completes the commands and the paths
(a second Tab shows the candidates), the **up/down arrows** are the history (16
lines), the **left/right arrows**, **Home/End/Delete**, **Ctrl-A/E** (the beginning/
the end), **Ctrl-U/K/W** (erase to the beginning/to the end/a word), **Ctrl-C** (cancel
the line), **Ctrl-L** (clear the screen). A typo gets a hint ("unknown
command: 'stauts' — did you mean 'status'"), while `help` prints the commands by group;
`help <command>` is the detail on one of them.

**The system:** `status` (a summary at one glance: the uptime, the memory, the FS, the network),
`uptime`, `mem`, `ps` / `top` (the processes and the threads: the core, the %CPU, the RSS, the allocations,
the syscalls — §10.1), `cores` (the layout over the cores + the L3 shares, §6.6/§6.7), `clock`
(the clock's reference, the correction, the residual error, §6.8), `history`, `echo`, `clear`,
`restart` (a restart of the workload through the supervisor), `reset` (a reboot),
`poweroff` (ACPI S5).

**The files** (ext2 — reading and writing, TFS — reading only; the modifying commands on a
read-only FS refuse clearly):

| The command | The action |
|---------|----------|
| `pwd`, `cd [path]` | the current directory (`..` does not go above the root) |
| `ls [-l] [path]` | the list; `-l` adds the rights, the size, the time (as a calendar rather than as seconds) |
| `cat <file>` | output it entirely |
| `less <file>` | page by page: space is onwards, `b` is back, `g`/`G` are the beginning/the end, `q` is quit |
| `head`/`tail [-n N] <file>` | the first/the last N lines |
| `hd <file> [offset] [length]` | a hexadecimal dump |
| `stat <path>` | the size, the rights, the modification time |
| `mkdir`, `rm [-r]`, `cp`, `mv`, `touch` | the usual operations on the tree |
| `write <file> <text>` / `append` | a one-line edit without an editor |
| `edit <file>` | a full-screen editor (below) |
| `df`, `sync` | the volume's occupancy and the journal's state; a flush of the FS cache to the disk |

**The network:** `netstat` (everything at once: the interfaces with the card's counters, the routes,
the sockets with the owning thread, the datapath — §10.2), `net` (the live connections), `nics`
(the table of the drivers and what it sees in the machine), `ip`, `route`, `dhcp`, `dns`
(§9.2), `filed` (§9.3).

**The diagnostics:** `kstat [section]` (the kernel's counters by section — §10.3), `perf`
(the hardware counters and the sampling profile — §10.4), `gdb [on]` (the debugger — §10.5),
`stat` without an argument (all the counters in one stream), `telem` (the cell's telemetry),
`log` (the flight recorder), `dump <address>`.

The full analysis of "what is measured by what and in which order to figure things out" is in
**`docs/PERFORMANCE-OPTIMIZATION-GUIDE.md`**.

The console works **on top of a running workload** as well — with the same set of commands, from
the scheduler's tick — including in the modes without a supervisor (`proc`); it is switched off by
`talaria.console=nopoll` (§7.1). There it is bound not to block (the tick is the body of an interrupt
handler), so the commands that wait for a key (`less`, `edit`) in that mode
honestly refuse: the rest is available entirely.

**The terminal's size:** `talaria.rows=`/`talaria.cols=` (by default 80x24) —
the pagination of `less` and the columns of `ls` depend on it.

#### The editor `edit` — a subset of vi

Editing a config in place is what the operator comes onto the node for. The set of
movements is the one the fingers make by themselves:

- the movement: `h j k l`, `w`, `b`, `0`, `^`, `$`, `gg`, `G`, the arrows;
- the editing: `i a I A o O` (insertion), `x`, `dd`, `D`, `r<character>`, `J`, `u` (an undo
  of one step);
- the search: `/pattern`, `n`;
- the exit: `:w`, `:q`, `:q!`, `:wq`, `:w <file>`, `:<line number>`, `ZZ`.

The boundaries are declared honestly: the buffer is static (400 lines of 160 bytes — this is the editing
of configs rather than the editing of gigabyte logs), the undo is of one step. A file
that **did not fit** into the buffer is opened **read-only**: to write
the truncated thing over the original is the worst an editor can do to a config.

### 9.2. The network's configuration: the interfaces, the routes, DHCP

The node sees **all** the machine's network cards: `net0`, `net1`, … (the numbering is the order
they came up in, which is set by the hardware). Every interface has its own address, its own DHCP client and
its own sockets; the routing table is common, and it is what decides which card a packet
will leave through — by the longest prefix match, and a connected network is always
stronger than a gateway.

From the console:

```
ip                                   # the interfaces: the address, the MAC, the frames received
ip addr add 10.0.1.5/24 net1         # an address for an interface
ip addr del 10.0.1.5/24 net1
ip link up|down net1                 # an interface put down resets its connections
route                                # the table: the connected networks + the routes
route add default via 10.0.0.1 dev net0
route add 10.9.0.0/16 via 10.0.1.1 dev net1
route del 10.9.0.0/16
dhcp net1 on|off|renew               # DHCP on a specific interface
dns 10.0.0.1                         # the resolver (it regenerates /etc/resolv.conf)
```

Declaratively (the cmdline, the user-data, the manifest) it is the same:

```
talaria.net=net1:10.0.1.5/24            # an address for an interface
talaria.net=net1:10.0.1.5/24,gw=10.0.1.1
talaria.net=net1:dhcp                   # DHCP on this interface
talaria.net=net1:down                   # do not bring it up
talaria.route=10.9.0.0/16,10.0.1.1,net1 # the network,the gateway,the interface
```

Instead of a name one may name a **MAC** — `talaria.net=52-54-00-12-34-56:10.0.1.5/24`
(with hyphens: the colon is occupied by the separator). The order of the enumeration of the cards is set by the
hardware, and it does not have to coincide with the order in which a human listed the cards;
a binding by MAC does not depend on the order.

The old keys `talaria.ip=`/`gw=`/`dns=` work as before and apply to the
first interface.

Two things the kernel says out loud, because silently they look like
a healthy configuration:

- a gateway not from its interface's subnet — an ARP for it will go into the wrong card;
- there has not been **a single** frame on a card (`ip` prints the receive counter) — that is
  what separates "the card is silent" from "the address is the wrong one".

The operator is senior to the automatics: a route entered by hand DHCP does not take down —
a lease removes only what it put there itself.

### 9.3. Uploading and downloading files over TCP (`filed`)

The service `filed` gives the node what it did not have: to put a file there and to take one
from there — without rebuilding the disk image.

```sh
# on the node (or by the command `filed on 7373` from the console)
-append "... talaria.mode=filed"          # the port is 7373 by default
-append "... talaria.filed=7373 talaria.filedtoken=<secret>"

# from the host
kernel/tools/talaria-file 10.0.0.2 ls /
kernel/tools/talaria-file 10.0.0.2 push ./cell /cell
kernel/tools/talaria-file 10.0.0.2 pull /var/trace.log ./trace.log
kernel/tools/talaria-file 10.0.0.2 sync ./dist /app     # a whole directory
kernel/tools/talaria-file 10.0.0.2 rm /old.bin
```

The client behaves like rsync in the part for whose sake rsync is needed: **not
to transfer what already lies on the node**. Before an upload a `STAT` is asked
(the size + the CRC32 of the whole file) — it matched, and nothing goes over the network. It differed —
`SUMS` gives out the CRC32 **per block**, and the client sends only the differing ones (`PATCH`),
trimming the tail (`TRUNC`). A measurement on the stand: an edit of 10 bytes in a file of 200 KB travels
as one block of 8 KB (4% of the volume). There is no rolling checksum (an insertion with a shift)
here deliberately: for binaries and configs that are overwritten entirely,
a per-block comparison is enough.

The integrity is checked on both sides (the CRC32); the token is mandatory, and before an `AUTH`
nothing is available except the greeting — the service writes to the disk, and "read-only
means safe" does not work here.

**The token is set only from the outside, and there is no default value.** It does not get into
the log: the command line and the audit of the knobs print the values of the `*token` keys
as asterisks (`talaria.filedtoken=***`) — on EC2 the serial output is read through
`get-console-output`. If the
`talaria.filedtoken=` is not set, the service answers any command with a refusal, and a warning goes into the log
at the bring-up. The token's comparison is in constant time
(`crypto::ct_eq`): a byte-by-byte comparison with an early exit would tell the one guessing
how many bytes he had already guessed.

That does not make `filed` a service for an untrusted network: there is no TLS here, the rate of the
attempts is not limited, and a `PUT`/`PATCH`/`TRUNC` writes into the node's FS. The threat model
keeps the control plane behind an external perimeter — as with IPMI. In an untrusted
network there is a tunnel.

**The boundary.** The network stack is not re-entrant, so `filed` is serviced where
the kernel itself owns the polling loop: `talaria.mode=console` and
`talaria.mode=filed`. On top of a running cell file access goes through the
serial console (`cat`/`less`/`edit`/`write`).

### 9.4. The control plane over TCP (`/ctl`)

In a network HTTP mode an authenticated route `/ctl` is available (port 80).
The token is mandatory — in the query `?token=<TOKEN>` or as the header
`Authorization: Bearer <TOKEN>` — and is set at the boot:

```sh
qemu ... -append "... talaria.mode=httpd talaria.ctltoken=<secret>"
```

```sh
# the right token -> 200 + the telemetry/the counters
printf 'GET /ctl?token=<secret> HTTP/1.1\r\nHost: x\r\n\r\n' | socat - tcp:10.0.0.2:80
# a wrong one, without a token OR the token was not set at the boot -> 401 Unauthorized
```

The answer includes: `syscalls`, `page_faults`, `syscalls_denied`, `pkts`, `gaps`,
`restarts`, `free_frames`.

**There is no default value.** If the `talaria.ctltoken=` is not set, the route is closed
entirely: any request gets a 401. That is done deliberately: before this the token was
a constant in the sources (`talaria-secret`), and in open source that would have meant that
every build goes out into the world with one and the same password on a network port. A mistake in
the deployment has to shut the door rather than open it.

The token's comparison goes in constant time (`crypto::ct_eq`): an ordinary byte-by-byte
comparison stops at the first discrepancy, and the time of the answer tells how many
bytes have been guessed. At the latencies for whose sake Talaria exists, such an oracle
is measurable over the network.

### 9.5. The supervision and the auto-restart (`talaria.mode=supervise`)

A cell executes under a supervisor: a failure of the workload (an exception in ring 3) is caught
through a setjmp/longjmp, is isolated from the kernel, and the workload is restarted up to `restart=`
times (from the manifest, §6.3). The restart events get into the flight recorder and the telemetry.

### 9.6. The health watchdog

The kernel watches the workload's progress (the counter `pkts`): if there is no progress for N checks
in a row, a hang (a stall) is recorded and a restart happens (the telemetry's `restarts`, the
flight recorder). The self-test of the mechanism is `talaria.test=watchdog`.

### 9.7. The syscall filter (harden)

The workload can be limited by an allowlist of syscalls: outside the list there is an `-EPERM` + a counter +
a `SYSDENY` entry in the flight recorder. By default it is **switched off** (the services use
a wide set). The self-test is `talaria.test=syscallfilter` (a getuid outside the allowlist → an EPERM).

### 9.8. A workload by a manifest at work (`talaria.mode=cell`): the serial line and the operator's side

Any multithreaded binary from the disk is started by a manifest (`/cell.manifest`,
§6) under the supervisor; the network, DNS, the outgoing TCP and the TLS of the workload itself work
(for TLS put a CA bundle onto the disk and point at it through an `env=SSL_CERT_FILE=…`).
An example of a network workload's manifest:

```
binary=/app
args=app --config /cfg/app.ini
env=SSL_CERT_FILE=/etc/ssl/ca.crt
restart=3
watchdog=45
```

What is visible in the serial line (the operator's channel):
- `net: connect fd=… -> IP:443`, `established`, `close … rx= tx=` — the trace of the connections;
- `cell: uptime=…s conns=… rx=… tx=… syscalls=… free=…MB restarts=… threads=…` —
  a status line every 10 s (from the kernel);
- `supervise: …` — a start/a restart (a ring 3 fault, a regular exit, the watchdog/the operator).

The operator's side on top of a running workload is the console over the same COM1 (the input goes into `-serial stdio`):
`status`, `net` (the connections + the total rx/tx), `stat`, `telem`, `mem`, `ps`, `log`
(the flight recorder), `restart` (a restart of the workload by the supervisor), `reset`.
The watchdog: a `watchdog=N` in the manifest — there has not been a single incoming byte for N seconds in a row →
the workload is restarted (it is counted in `restarts`). The previous instance's memory at a
restart is returned (the gate `restartmem`: a drift of 0–128 KB per restart).

### 9.9. PostgreSQL / MariaDB on Talaria (`talaria.mode=proc`, v0.48)

The process mode: the kernel takes the `/cell.manifest` from the ext2 disk (`binary=`, `args=`, `env=`) and
starts the binary **as a process in a private address space** — with `fork`, signals,
fd tables, threads and an RW file system. The ready wrappings:

```
services/postgresql/build.sh && services/postgresql/build-static.sh   # static musl postgres+psql, initdb
services/postgresql/mkdisk.sh 256M && services/postgresql/run.sh 40   # a QEMU tap 10.0.0.2:5432, a psql probe
services/mariadb/build.sh && services/mariadb/mkdisk.sh 512M && services/mariadb/run.sh 45  # :3306 (root@'%', the static mariadb client)
services/mongodb/mkdisk.sh 512M && services/mongodb/run.sh 100   # mongod 4.0 (a dynamic ELF), :27017
```

Dynamic ELFs (v0.49): the kernel loads a PIE + a `PT_INTERP` (`/lib/ld-musl-x86_64.so.1`), and the ld.so itself
pulls the `.so`s off the ext2 disk by the `LD_LIBRARY_PATH`/`/lib:/usr/lib` — ready Alpine binaries
run without a rebuild (checked on mongod and on `kernel/user/dyn/hello`).

A repeated `run.sh` without a `mkdisk.sh` is a check of the persistence (the data stays on the disk,
PG does a crash recovery). A clean stop: `run.sh <sec> "talaria.stop=N"` — after N s the kernel sends
a SIGTERM to the process pid 1 (MongoDB: `WAIT=70 ./run.sh 120 "talaria.stop=60"` → `shutting down with
code:0`), and the dirty ext2 blocks are flushed at the exit. The disk is an ordinary ext2: `debugfs -R "ls -l /pgdata" disk-pg.img`,
`e2fsck -fn disk-pg.img`. The diagnostics: `run.sh <sec> "talaria.strace=5000"` — the first N
syscalls + the `[sched]`/`[futex]` in `build/serial.log`. The details and the known limitations are in
`services/postgresql/README.md`, `services/mariadb/README.md`,
`services/mongodb/README.md`.

### 9.10. The deployment on AWS (EC2, including bare metal)

One command from a binary to a working instance:

```sh
infra/aws/talaria-deploy.sh --app ./myapp --args "--port 8080" \
    --type c6i.large --smp 4 --net dhcp
```

What it does: `talaria-pack` puts the workload and the manifest onto an ext2 image,
`tools/mk-tboot.sh` assembles a bootable image with OUR OWN bootloader (without GRUB),
the image goes onto an EBS volume through a one-off helper, a snapshot is taken,
an AMI is registered (`--boot-mode legacy-bios --ena-support`) and an
instance is launched. The useful keys: `--restart N` (the restarts by the supervisor),
`--stop-after SEC` (run and power the machine off — the instance will go into `stopped`),
`--disk-gb`, `--dry-run` (only assemble the image), `--keep-disk` (the root volume
survives the instance — it is needed on a metal, where the kernel's trace is read off the volume; without it
the volume is put out together with the instance and there is nothing left to pay for).

**The boot chain is one and the same for virtual instances and for a `*.metal`** —
our own bootloader on the BIOS path. That means that a cheap run on a `c6i.large`
(~$0.1) checks THE SAME image that will go onto a `c6i.metal` (~$6/hour).
The order of work: first the cheap one, then the dear one.

**There is no console on bare metal** — neither a COM1 nor a screen (`get-console-output` there
shows a snapshot of the firmware's screen). So the kernel's log is written onto the disk
(`talaria.trace=2048`, on by default) and is read like this:

```sh
infra/aws/read-disk-trace.sh <instance-id|volume-id> 2048 2047   # the kernel's log + the bootloader's trace
```

The instance is put out in the process, the root volume (`DeleteOnTermination=false`) is attached to
a one-off helper and is read as raw sectors.

For a matrix of modes in one launch of an instance there is a set (`talaria.suite=`, §7.1):
`tools/mk-tboot.sh SUITE="the mode;the mode;…"`, every item in its own log slot,
between the items the kernel reboots the machine itself, and at the end of the list it powers it off.
`SLOTS=<N> infra/aws/metal-ref-probe.sh` reads all the slots at once.

| The bare-metal pitfall | What to do |
|---|---|
| The instance "keeps silent" | That is how it should be: a guest's output is not visible on a metal. Look at the trace on the disk. |
| The run "has hung" | The set's counter on the disk (sector 1004) says which item it is at; `stopped` = it reached the end. |
| The image was run locally | Rebuild it: the set's counter and the logs live IN THE IMAGE FILE. The check in `metal-ref-probe.sh` catches that and stops the run. |
| A `VcpuLimitExceeded` | A terminated metal holds the quota for a few more minutes; the scripts have a retry loop. |

**Graviton (`c7g.*`, arm64).** EC2's arm64 instances boot UEFI only, so the path is the UEFI
stub (`sh kernel/uefi-stub/build.sh aarch64` after every kernel build — it carries the kernel
inside) on an ESP, and `tools/mk-ami.sh`/`infra/aws/ec2-boot-talaria.sh` take `ARCH=arm64`:

```sh
ARCH=arm64 ENVF=~/.talaria-lab/lab.env PEM=~/.talaria-lab/talaria-v11.pem \
  infra/aws/ec2-boot-talaria.sh "talaria.pci=1 talaria.ip=dhcp talaria.ntp=aws talaria.mode=proc" \
  c7g.large kernel/target/a64-services/ext2-ch.img
```

The one-command path takes arm64 too: `infra/aws/talaria-deploy.sh --app <aarch64 binary>`
reads the architecture from the binary's ELF header (or `--arch arm64`, or a Graviton
`--type`), builds the UEFI image and registers the AMI for arm64 (HANDS-ON-GUIDE §11.1).

Unlike an x86 metal, a Graviton HAS a console: the 16550 behind PCI that the SPCR names, and
`get-console-output` shows the kernel's log. Give it `talaria.ntp=aws`: the instance has no PTP
clock on its ENA and no paravirtual clock, and without a reference the wall clock drifts by the
crystal (+15 ppm on a `c7g.large`, 1.3 s a day); with it the clock stays within tens of
microseconds of the Amazon Time Sync Service (§6.8). `c7g.metal` serves as the KVM stand for
the arm64 regression (`infra/aws/kvm-stand-aarch64.sh`).

## 10. The observability

Talaria is made for the sake of a predictable latency, so the question "where the microseconds
have gone" is bound to be answered by the system itself — and exactly when it is hardest
to answer: the workload has degraded, a CPU core is busy with the datapath, and there is no spare
process in ring 3. Everything listed below lives **in the kernel** and works as in
an interactive console and on top of a running workload alike.

The methodology ("what is measured by what and in which order to figure things out") is in
**`docs/PERFORMANCE-OPTIMIZATION-GUIDE.md`**.

### 10.1. `top` and `ps` — who eats the machine

`ps [-t] [-m]` is one snapshot, `top [-t] [-m] [ms]` is a refreshing table
(`q` is quit, `t` is threads/processes, `m` is the RSS on/off, `c`/`s` is sorting by
the CPU/the memory, `+`/`-` is the interval). By default the rows are **processes**, `-t` is by
thread with their names.

The header: the uptime, the TSC's frequency, the slice's length, the composition of the tasks; the occupancy of **every
core**; the memory (in total / free / occupied by the kernel / the rate of handing frames out);
the syscalls and the context switches per second; the hardware counters over the interval;
the state of the FS and of the network.

The columns: `PID`, `TID`/`THRDS`, the name, the core, `CPU%`, `WAIT%` (the share of the interval
during which the thread was READY and stood without a core — including one pre-empted by the quantum;
for a process it is the sum over its threads), `RSS`, the shared memory,
the lazily reserved one, the rate of allocation, the switches/s, the syscalls/s; on
a wide terminal (`talaria.cols=` ≥ 100) also the faults/s, the L3 share, the state,
the processor time and the thread's lifetime.

Two differences from Linux that matter in practice:

* **The `%CPU` is counted in TSC cycles rather than in ticks.** The main thread usually lives on
  a dedicated core, where there is no timer at all — the tick counter there is always zero, and
  a `top` by ticks would have shown `0%` for exactly the thread for whose sake everything is done.
  The time when a core stands in a `hlt`/`mwait` belongs to nobody: a futex sleeps
  in the kernel without leaving the core, and before, all such sleep was written down to the sleeping thread
  (a writer making five writes a second showed 96 %). The wait for a device
  (`fsync`, `pwrite`) in Talaria is an active poll, and it does enter the thread's `%CPU`.
  thread's `%CPU`.
* **The RSS is counted by a walk over the page tables and only when asked.** A counter would have to be
  kept on the fault path — for the sake of a line that is read once a second.
  The walk costs milliseconds per process; `-m` switches it off.

### 10.2. `netstat` — the network as a whole

`netstat [-i|-r|-s|-d]`; without arguments it is everything at once: the interfaces (the address,
the state, the driver, the MAC, the card's counters and the **transmit drops**), the routes, the sockets
(the local and the remote address, the TCP state, the traffic, the interface and the **owning
thread**), the datapath (the profile of the stages and the telemetry).

A network question almost never sounds like "show me the routes" — it sounds like
"why does port 9000 not answer", and the answer to that is one screen where all the layers are visible
at once.

### 10.3. `kstat` — the kernel's counters by section

`kstat [sched|mem|paging|fs|net|sync|irq|cache|syscalls]`; without an argument it is all of them.
`stat` prints the same in one stream (a format for a post-mortem: the workload has ended,
the log has gone into a file). The counters whose right value is zero are marked with a
word: a silent non-zero counter of errors would be noticed by nobody.

### 10.4. `perf` — the hardware counters and the profile

`perf` is a table by core: the cycles, the instructions, the **IPC**, the LLC misses and the misses of
the branch predictor (per thousand instructions, and where there were enough counters, as a
share), plus a summary with recommendations.

`perf poke <core>` takes the counters of a **dedicated** core: there is no tick there, so
there is nobody to take them, and the only way is one IPI, which interrupts that thread.
Hence a separate command rather than automatics.

`perf on [hz]` / `off` / `top` / `record [N]` / `reset` is the sampling profile:
into a sample there go the **call stack** (up to eight frames), the thread, the core and the protection
ring. The rate is given in hertz (by default 200): the slice differs on different machines,
while "two hundred samples a second" means one and the same thing everywhere.

The stack is taken by the chain of frame pointers, so it is there for code
built with `-fno-omit-frame-pointer`; for the rest only the top is visible, and
`perf top` honestly prints the average depth. A kernel with stacks is built separately
(`kernel/build-prof.sh`) — the price of frame pointers in the kernel has been measured and equals
~4 % on a syscall, and there is no point in paying it constantly.

The symbols are resolved **on the host**: `kernel/tools/talaria-prof report --kernel
<the kernel> --user <the binary> < serial.log` — a table by function, the most frequent stacks,
a breakdown by thread and by core; `folded` gives the input for a flame graph.

`perf gdb` prints the order of attaching the debugger.

### 10.5. Debugging and the post-mortem

- **Core dumps** (`talaria.core=<path>`): at a fatal signal to the workload and at
  an exception in the kernel an ordinary ELF `ET_CORE` is written — the one that
  `gdb <the binary> <the file>` opens and shows a `bt` with the functions' names and the line numbers.
  The command `core [tid]` takes a dump of a LIVE task without stopping the machine: for
  a hung application that is the only way to find out where it stands.

  The dump is written even when **the workload has a handler of its own** for the fatal
  signal. Such applications (MariaDB, the JVM, ClickHouse) print their own trace,
  remove the handler and re-raise the signal — the process dies not through
  an exception but through the delivery of a pending signal. Before, there was no dump in that case
  exactly where it is needed. The frame in the dump is then the point of the
  RE-RAISE rather than of the original error; the original one is shown by the `[fault]` line
  below.
- **The `[fault]` line at a crash with a handler.** The path "an exception -> the
  workload has a handler" used to be mute: the application crashed, and not a line was left in the kernel's
  log, and from the outside that looked like "it crashed by itself".
  Now the first few such exceptions print one line with the `rip`, the
  address and the decomposition of the `err` — including "the page is NOT EXECUTABLE". That is exactly the
  address from which the analysis starts.
- **The call stack in the crash dump**: the chain of addresses is always printed, without an FS and without
  a debugger (`addr2line -f -C -e <the binary> <the address>`).
- **The gdb stub** (`talaria.mode=gdb`, a `gdb break` on a live machine — the stub
  is switched on and the machine stops in the debugger at once, or
  `talaria.gdb=break` — a stop before the workload's first instruction): the GDB Remote Serial
  Protocol over COM2 — `target remote :1234`. `-M pc` is needed (a `microvm` has no COM2),
  and the disk is then a `-device nvme`. The registers (g/G), the memory (m/M), a step/a continuation
  (s/c), a `detach`, software breakpoints (the kernel's text), **`hbreak`** — a hardware breakpoint
  on execution (the workload's code is mapped R-X, an `int3` cannot be written there; up to three),
  hw watchpoints (DR0/DR7), the threads (`info threads`, `thread N`). On SMP it is
  all-stop: while the stub talks to gdb, the other cores are parked, the debug
  registers are armed on all the cores, and a step releases only the stepping core. Stopping in the debugger stops
  **the whole kernel**, including the datapath. An example of a session is in `docs/HANDS-ON-GUIDE.md` §8.
- **The gdb stub on arm64** shares **the console's line** (a `virt` machine has one PL011, a
  Graviton guest one serial port): `-serial tcp::1234,server=on,wait=on` and `target remote :1234`
  to the console's port, as Linux's kgdboc. While the machine runs gdb skips the console's text;
  while it is stopped the other cores are parked and the line carries packets alone; gdb's ^C
  reaches the console's poll and stops the machine. The registers are gdb's `aarch64-core` (x0..x30,
  sp, pc, cpsr; the FP/SIMD set reads as unavailable); `hbreak` and watchpoints use the breakpoint
  and watchpoint registers and catch the WORKLOAD (the kernel runs with debug exceptions masked);
  in the kernel use `break` (a `brk` written into the text) and `stepi`. A resume from an `hbreak`
  or a watchpoint steps over the instruction first (arm64 has no resume flag) — invisible to gdb.
- **The crash dump** (`talaria.test=fault`): at an exception there is a full dump of the registers,
  of the stack and of the flight recorder.
- **The flight recorder** (`log`): a ring of the last events (boot/paging/syscall-init/
  user-enter/fault/restart/mprotect/syscall-deny) — it is printed in the crash dump too.
- **The datapath's telemetry** (`telem`, `/ctl`): the pkts, the p50/p99 (a histogram of the
  latency), the min/max, the gaps (an overflow of the ring), the restarts, the uptime.

### 10.6. A machine without a console

- `talaria.top=<ms>[,rss]` — a snapshot of `top` into the log every N milliseconds. The same
  trick as `vmstat 5 > file` on a machine on duty, only without a process and without a file.
- `talaria.trace=<LBA>` — the kernel's whole log as raw sectors onto the disk (§7.1).
- `talaria.heartbeat=N` — the scheduler's pulse; `talaria.stuck=N` — "a task does not
  leave the core for N ticks", together with the address.

---

## 11. The supported subset (what the ABI "can do")

**~185 Linux x86-64 syscalls** are implemented — enough for static musl programs
(full C++, threads), dynamic ELFs (ld-musl, the glibc ld-linux) and multi-process
DBMSes. A quick check of your own binary: `kernel/tools/talaria-check ./binary` (§6.3).
The main groups (the full list is the match in `kernel/src/syscall.rs`):

- **The processes/threads:** fork/vfork-through-clone, clone (threads and fork semantics), execve,
  wait4, exit/exit_group, futex (**it sleeps in the kernel with an mwait** — WAIT/WAKE/WAKE_OP/REQUEUE,
  the timeouts), set_tid_address, set_robust_list, sched_yield,
  sched_setaffinity/getaffinity (the migration of threads BSP→AP), gettid/getpid/getppid,
  kill/tkill/tgkill, rt_sigaction/procmask/return/timedwait, sigaltstack, signalfd.
- **The memory:** mmap (anonymous, a file MAP_PRIVATE/MAP_FIXED, /dev/shm MAP_SHARED),
  munmap, mprotect (a real one, per page), brk, madvise/msync; SysV shm
  (shmget/shmat/shmctl).
- **The files/the FS:** open/openat (+O_CREAT on ext2), read/write/readv/writev,
  pread/pwrite/preadv/pwritev, lseek, the stat family, getdents64, mkdir/rmdir/
  rename/unlink/link, readlink(at), access/faccessat(2), fsync/fdatasync/sync,
  ftruncate/fallocate, getcwd/chdir, flock, pipe/pipe2, dup/dup2/dup3,
  a minimum of procfs/sysfs (/proc/meminfo, cpuinfo, self/*, /sys/…).
- **The network:** socket/bind/listen/accept(4), **connect** (blocking and O_NONBLOCK),
  send/recv(from/msg/mmsg), shutdown, getsockname/getpeername, setsockopt/getsockopt,
  epoll (create/ctl/wait/pwait), poll/ppoll, select/pselect6, eventfd, timerfd,
  socketpair; DNS works through the ordinary musl resolver (UDP:53).
- **The time/the rest:** clock_gettime (a kvmclock wall clock + a monotonic TSC),
  nanosleep/clock_nanosleep, getrandom, uname (a release ≥3.2 for glibc), sysinfo,
  prlimit64, membarrier, the getuid family (→1000: PostgreSQL and MariaDB refuse to run
  under root, and the owner in `stat` is 1000 too), ioctl (FIONBIO; otherwise an ENOTTY).
- **The memory:** `mremap` grows and shrinks in place and moves the table entries on
  MAP_MAYMOVE (without it `realloc` degenerates into malloc+copy+munmap, and every munmap
  is a TLB shootdown on all the cores — that is where ClickHouse lost its scaling by
  cores). A file MAP_SHARED is supported (§7.1); since v1.2 the pages
  are mapped one by one, so the ENODEV stays only for the identity AS
  (`talaria.cell=identity`) — there an application falls back to pread/pwrite.
- **The deliberate ENOSYS/EINVAL** (the workloads have a fallback): rseq, clone3,
  ARCH_CET_*.
- **The Talaria-specific ones:** `talaria_fastmap(1000)` (the base of the zero-syscall region),
  `1001` (a nop microbenchmark), `1002` (the TSC's Hz), `4600` (`swtchto(tid)`: a directed
  switch of the remainder of the slice), `4601` (`tsync_kmode()`: the kernel wait's mode —
  by it libtsync tells Talaria from Linux), `4602` (`upark`: a sleep with a predicate,
  libtsync; DEVELOPMENT-GUIDE §10.1).

An unfamiliar syscall is printed into the log (once per number) and returns an `-ENOSYS` —
the application may survive that or crash; the improvement of the ABI is DEVELOPMENT-GUIDE §10.1.

---

## 12. The regression and the validation

- `kernel/test.sh` — the quick regression: it boots every mode in QEMU (the docker image
  `talaria-qemu2`, `/dev/kvm` is needed) and asserts the output. The gate is "TOTAL … GREEN";
  the current count of the asserts is in the README (the single source).
- `kernel/validate.sh` — the heavy one: the open-source compatibility (6 services),
  the latency SLA (the p50/p99), a soak (the allocator's churn), the fuzzing of the network layer, three DBMSes
  (B10, a SKIP without the artefacts), the JVM (B11), ClickHouse (B12).
- `kernel/soak.sh [seconds]` — a long run: MariaDB under a continuous client
  load (30–60 min); the criteria are liveness, a stable latency, consistency,
  the absence of a crash dump.

All the scripts are launched from the host and bring docker up themselves. The known flakes of the regression are
in CONTRIBUTING/the DEV-GUIDE (the `connect` test — a rare DNS flake: restart it).
The `bios:` block assembles a hybrid image on the HOST (it needs `sgdisk`, `mkfs.ext2`,
`mtools` and a docker with a GRUB i386-pc) — if the tools are absent it is honestly
SKIPped; `TAL_NOBIOS=1 kernel/test.sh` skips it entirely (it is the longest
in the set).

---

## 13. The limitations (the current stage)

The full map of the boundaries/the trade-offs/the limits is `docs/PRODUCT.md` (§4–§6). In brief:

- It is booted by four bootloaders: **PVH** (QEMU/KVM), **UEFI** (a stub of our own —
  that is how Talaria works on a real EC2 without QEMU, v1.1), **multiboot2 through GRUB**
  (BIOS, v1.2 — `tools/mk-bios.sh`, including a hybrid disk for both paths) and
  **our own bootloader tboot** (BIOS, v1.6 — `tools/mk-tboot.sh`, GRUB is not needed).
  For a sub-microsecond synchronisation under QEMU the `-overcommit cpu-pm=on` is mandatory (otherwise
  there is the hlt+IPI fallback at ~16 us).
- **Bare metal works (v1.7):** on a `c6i.metal` the kernel comes up entirely —
  ACPI, PCIe through ECAM, NVMe, ext2 on a live EBS; MONITOR/MWAIT there are real ones
  (the tier is `mwait(ring0)` rather than `hlt+ipi`, as on Nitro guests). Since v1.8 there also
  work **SMP** (a speedup of 2.00 on two cores and of 4.00 on four) and the **ENA network**
  (the device gives out limits of 32 pairs of queues and 132 MSI-X vectors; the driver
  brings one pair up). To diagnose things on a machine without a console there are
  `talaria.trace=` and `talaria.suite=` (§7.1), and the numbers are in
  `docs/MEASUREMENTS.md` §8.
- **SMP up to 64 cores** (`talaria.smp=N`), the layout of the threads over the cores is by the manifest
  (§6.6). The network stack is one per machine and is protected by a lock: the network syscalls
  execute on any core. TLS is in user space (OpenSSL in the workload's binary).
- The network: 64 TCP + 8 UDP sockets, a backlog of 8 (out of the same 64), 32 epoll instances.
- The FS: TFS1/TFS2 are read-only; **writing goes to ext2** (a write-back cache of a sixteenth
  of the free memory, at most half of the memory below 4 GB, within [2 MB, 1 GB],
  `talaria.fscache=`; 4-way
  with an LRU). The JBD2 journal has been there since v1.1: after a successful `fsync` a crash will not
  leave the metadata in an intermediate state, and with a `talaria.ext2=journal=data` the data
  is recovered as well. What has NOT been fixed by an fsync an unclean stop
  still loses; `talaria.stop=SECS` is a clean stop.
- The `/ctl` token and the syscalls' allowlist are the operator's (demo-level) ones, not a security
  perimeter; Talaria is put behind an external perimeter.
- The p99.9 tails in QEMU on a noisy host catch the pre-emption of a vCPU — for measurements
  one needs a quiet stand (isolcpus, the pinning of the vCPUs); the target tail gate is on bare metal.

---

## 14. The tools

Everything that is run **on the host** next to Talaria. None of them is needed for
the node to work — they are needed in order to do something with it.

| The tool | What for | Where it is described |
|---|---|---|
| `kernel/tools/talaria-pack` | assemble a disk (TFS1/TFS2/ext2) with the binary, the files and the manifest | §6.2 |
| `kernel/tools/talaria-check` | check your own binary for compatibility with the ABI BEFORE a launch | §6.3 |
| `kernel/tools/talaria-file` | upload/download files over TCP from a live node (a delta in the spirit of rsync) | §9.3 |
| `kernel/tools/talaria-prof` | resolve a profile (`perf record`) into the functions' names, the stacks and a flame graph | §10.4 |
| `kernel/mkext2.sh` | assemble an ext2 image out of a directory | §6.2 |
| `kernel/build-prof.sh` | build the kernel WITH FRAME POINTERS (the ring 0 stack in the profile and in the dumps) | §10.4 |
| `kernel/test.sh`, `kernel/validate.sh` | the regression and the validation | §12 |
| `tools/mk-ami.sh`, `mk-bios.sh`, `mk-tboot.sh` | the images: an AMI for EC2, a BIOS disk, our own bootloader | §3 |
| `infra/aws/read-disk-trace.sh` | read the `talaria.trace=` off the volume of a machine without a console | §7.1 |
| the ordinary `gdb` | open a core dump (`gdb <the binary> <core>`) or attach to the stub | §10.5 |

Two of them "read what the node gave out" and therefore have to be at hand in advance:
**`talaria-prof`** (the profile) and **`gdb`** (the core dump). Both work with ordinary
formats — the kernel does not invent ones of its own: the addresses are resolved by `addr2line`, and the dump is read by
the same gdb as a Linux core.

---

## 15. Troubleshooting

| The symptom | The cause / the solution |
|---------|-------------------|
| **The application has crashed** | The stack is already in the log: the line `the stack workload (N frame(s)): 0x…` — resolve it with `addr2line -f -C -e ./app <the address>`. For more details: boot with a `talaria.core=/core.elf`, take the file with `talaria-file <ip> pull /core.elf ./core` and open `gdb ./app ./core` (§10.5). For there to be names in the stack, build the workload with `-fno-omit-frame-pointer` and without a `strip`. |
| **The application has hung** | Do NOT put the machine out with the debugger: entering the gdb stub stops the whole kernel, including the datapath. In the console there is `core` — a snapshot of a live task, and the machine keeps working; `top -t` will show which thread is in which state. |
| **The kernel has crashed** | The crash dump is always printed: the registers, `the stack kernel (N frame(s))`, the flight recorder. The kernel's core dump comes from the same knob `talaria.core=` and is read by `gdb kernel/target/x86_64-talaria/release/talaria ./core`. There will be names in the stack in a build of `kernel/build-prof.sh`. |
| **It is slow and it is unclear why** | The order of the analysis is laid out in `docs/PERFORMANCE-OPTIMIZATION-GUIDE.md`: first `cores`/`kstat sched` (is the system configured the way you think?), then `top` and `perf`, and only then the code's profile. |
| On bare metal the workload "keeps silent" | The full catalogue of what QEMU hides, and what to ask it with, is `docs/BARE-METAL.md`. The first thing to do: a `talaria.trace=<LBA>,eager` + a `talaria.heartbeat=N`. |
| The binary crashes at once / a `#PF` at the start | It has not been moved to `0x10000000` (`-Wl,-Ttext-segment=0x10000000`), or it is not `-static -no-pie`. A quick diagnosis: `talaria-check ./binary`. |
| `[syscall] an unknown number N` | The application uses an unsupported syscall — improve the ABI (DEV-GUIDE §10.1) or rebuild it without that feature. The list for the whole binary: `talaria-check`. |
| The workload keeps silent | A `talaria.strace=2000` in the `-append` — the first N syscalls into the serial line; the `net:` trace is always printed. |
| The synchronisation is "slow" (tens of us) | The `-overcommit cpu-pm=on` has been forgotten at QEMU (look at the line `sync: the wait mode is` in the boot log: it must be mwait, not hlt+ipi). |
| There is no answer over the network | The tap/QEMU sometimes does not start the first time — repeat the launch; check the `--device /dev/net/tun --device /dev/vhost-net --cap-add NET_ADMIN`; the first connection after a boot is more reliable (a re-listen may race). |
| There is no output from the kernel | The `console=ttyS0` and the `-serial stdio`/`-serial file:` have been forgotten. |
| `mode=fast` does not echo | `-smp 2` is needed (a cell on an AP + the application on the BSP). |
| gdb does not attach | For the gdb stub `-M pc` is needed (not `microvm`). |
| A file is not read off the disk | Check the name (≤24 bytes) and that the disk is attached through a `virtio-blk-device`. |
| A crash dump `#PF CR2=…` | The registers, the call stack and the flight recorder are right there in the dump; a frequent cause is an access to unmapped memory or a wrong linking address. |
| "The depth is 1" in the stack | The code is built without frame pointers: for the workload there is `-fno-omit-frame-pointer`, for the kernel `kernel/build-prof.sh`. The kernel will not invent a chain (§10.4). |
| `[core] the FS is not mounted` | The dump is written onto a mounted ext2 (RW). On a TFS and without a disk there is nowhere to write — what is left is the stack in the log. |

---

Related documents: `docs/DEVELOPMENT-GUIDE.md` (the development of the kernel/the ABI),
`kernel/README.md`, `services/README.md`, `docs/ROADMAP.md` (what is open),
`docs/MEASUREMENTS.md` (the measurements), `docs/talaria-os.html` (the design doc TAL-RFC-001).
