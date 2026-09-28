# Talaria OS

**A latency-first single-application OS (unikernel-style) that runs unmodified Linux binaries.**

Talaria is its own x86-64 kernel, written in Rust (`no_std`). It runs
unmodified Linux applications (static musl, dynamic ELF through ld-musl, glibc)
so that the application owns the CPU cores and the OS stays a control plane.
It is built for latency-critical workloads: trading, embedded, dedicated
appliance servers behind an external security perimeter. It boots under
QEMU/KVM, as an AMI on EC2 and on bare metal (`c6i.metal`).

**Against Linux on real workloads** (`docs/TABLO-41.md`): six servers
(PostgreSQL, MySQL/MariaDB, MongoDB, Redis, nginx, mongoose) on four EC2
`c6i.xlarge` in one zone. Each server has its own load generator, the loop is
open, and the rate ladder starts from the ceiling both systems share.

| 295 comparable points | faster | parity | slower |
|---|---:|---:|---:|
| p50    | 66 | 7 | 1 |
| p99    | 69 | 5 | 0 |
| p99.9  | 65 | 7 | 2 |
| p99.99 | 63 | 2 | 8 |

Every lost point is analysed by name (`docs/TABLO-41.md`, `docs/FSYNC-42.md`,
`docs/SCHED-43.md`).

> **Status: v1.13.** Provided as is, without warranty. Interfaces are not
> stable before 1.0: the `cell.manifest` format, the `talaria.*` boot
> parameters, the Talaria-specific syscalls (1000+, 4600+) and the libtsync API
> may change without backward compatibility. Most of the documentation under
> `docs/` is still in Russian; the English translation is in progress.

## What is different

- **Synchronisation without busy-spinning.** A futex sleeps *in the kernel* with
  `mwait` on the user's own word, so the waker's store wakes it with no IPI and
  no syscall. Plain pthread mutexes and condition variables in an ordinary
  binary: futex round trip 165 ns, condvar 1.3 µs across cores, 0 % CPU while
  waiting.
- **Real software, unmodified:** PostgreSQL 16, MariaDB 10.11, MongoDB 4.0
  (persistent, on a journalled ext2), OpenJDK 21, ClickHouse 25.8, Redis, nginx,
  mongoose, SObjectizer, busybox, Lua, SQLite, QuickJS.
- **The workload decides core placement:** the manifest gives a thread a
  dedicated core (no timer tick, run to completion), a named pool, or the
  shared pool, by thread name. NUMA-aware, up to 64 cores, with Intel CAT
  shares of L3.
- **A process model:** a private address space per process, fork with COW,
  execve, signals, pipes and shared memory, a dynamic ELF loader, and a vDSO.
- **Protection from workload bugs:** syscall pointers are validated (EFAULT
  instead of the kernel writing on the process's behalf), W^X per segment, NX,
  guard pages, a supervisor with restart and watchdog, and a syscall allowlist.
- **Observability built in:** `top`/`ps`/`netstat`/`perf` in the kernel
  console, a sampling profiler with stacks, ELF core dumps readable by `gdb`, a
  gdb stub, and a flight recorder.
- **Datapath:** ENA on EC2 at 33.6 µs UDP RTT (Linux on the same VPC path: 43.0),
  1.28 Mpps transmit, zero-syscall rings, 496 ns cross-VM wakeup over ivshmem.

## Quick start

Requirements: Linux with `/dev/kvm`, docker, rustup (the kernel toolchain is
pinned in `kernel/rust-toolchain.toml` and installs itself).

```sh
# the sources
git clone https://github.com/tishden/talaria-os-dev.git && cd talaria-os-dev
# 0) toolchain images (docker): QEMU stand, musl, Alpine build
services/docker/build-images.sh
# 1) ring-3 test binaries and the regression disk
make -C kernel/user all && (cd kernel && python3 mkfs.py)
# 2) the kernel (without step 1: add --no-default-features — no docker needed)
(cd kernel && cargo -Z json-target-spec build --release)
# 3) the regression: boots the kernel in QEMU a few hundred times
kernel/test.sh
# 4) your own application in five minutes (build -> talaria-pack -> QEMU)
cd examples/hello-talaria && make run
# 5) a hands-on tour: multithreaded C++ from build to core dump (docs/HANDS-ON-GUIDE.md)
cd examples/mdpipe && make && make run
```

Running your own binary needs no kernel changes (`docs/USER-GUIDE.md` §6):

```sh
kernel/tools/talaria-check ./myapp          # ABI compatibility: verdict + syscalls
kernel/tools/talaria-pack -o disk.img --app myapp --args="--port 80" --restart 3
qemu ... -append "console=ttyS0 talaria.mode=cell" -drive file=disk.img,...
```

## Documentation

| Document | Contents |
|---|---|
| `docs/PRODUCT.md` | product boundaries, trade-offs, performance tables, limits, roadmap |
| `docs/POSITIONING.md` | who it is for and where the proven boundary lies |
| `docs/HANDS-ON-GUIDE.md` | hands-on tour on a multithreaded C++ app (`examples/mdpipe`): monitoring, placement, profiling, live debugging, core dumps, EC2 |
| `docs/USER-GUIDE.md` | building, running your own and open-source applications, operations |
| `docs/DEVELOPMENT-GUIDE.md` | architecture, module map, recipes, invariants |
| `docs/PERFORMANCE-OPTIMIZATION-GUIDE.md` | how to measure, profile and tune a workload |
| `docs/BARE-METAL.md` | what QEMU hides: pitfalls of real machines and how to diagnose them |
| `docs/talaria-os.html` | design document TAL-RFC-001 (motivation, prior art, architecture) |
| `docs/GLOSSARY.md` | the words this repository uses in one fixed sense (cell, gate, stand, knob, scoreboard, rise, band) |
| `CHANGELOG.md` | release history (in Russian — it is a working journal too) |

Everything else under `docs/` is the WORKING JOURNAL of the sessions this system
was built in — the measurements, the dead ends and the reasons
(`POC-RESULTS`, `PLAN-*`, `TABLO-*`, `FSYNC-*`, `SCHED-*`, `TODO.md`), and so is
`CHANGELOG.md`. They are published as they were written, in Russian: their value
is in the detail, and a translation would cost exactly that. The guides in the
table above are the ones a user needs, and they are in English.

## Repository layout

- `kernel/` — the kernel: `src/`, the QEMU regression `test.sh` (486 assertions
  in QEMU plus 22 host checks; on a fresh clone 483 — three gates want
  third-party loaders, see `CONTRIBUTING.md`), the heavy validation
  `validate.sh`, and the tools `talaria-pack`/`talaria-check`.
- `kernel-unit/` — host unit tests of the kernel's pure modules: 264 tests,
  `cargo test` (`docs/DEVELOPMENT-GUIDE.md` §11.0).
- `services/` — open-source workloads on Talaria (databases, Redis, JVM,
  ClickHouse, ...) and the docker toolchains.
- `examples/` — `hello-talaria` (the reference path) and `mdpipe` (the hands-on tour).
- `bench/` — comparisons with Linux: the EC2 suite `bench/suite`, transport,
  actors, ENA latency, the 82599 datapath.
- `infra/` — EC2 deployment (`infra/aws/talaria-deploy.sh`) and machine diagnostics.
- `control-plane/` — experimental: a Linux-hosted control plane for VFIO cells.
- `tools/` — boot loaders and images (BIOS, tboot, AMI), the public-tree export.

## Contributing, security, support

The repository is <https://github.com/tishden/talaria-os-dev>: issues and pull
requests go there. See `CONTRIBUTING.md` (contributions require a CLA),
`SECURITY.md` and `SUPPORT.md`. Commercial support is available by separate
agreement.

## License

Apache License 2.0 (`LICENSE`, `NOTICE`). © 2026 Denis Tishkov.
The code and the platform as a whole were developed with the assistance of an
AI agent (Claude Code, Anthropic), under the direction and review of the
copyright holder.
