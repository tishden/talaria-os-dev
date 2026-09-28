# kernel — the Talaria kernel

Its own x86-64 kernel (Rust, `no_std`): a partitioned runtime without Linux.
The application (a cell) runs in ring 3 through the Linux syscall ABI; the
kernel provides memory, devices and observability.

## Build and regression

```sh
# kernel only (no docker): the vDSO is assembled by build.rs with the host cc
cargo -Z json-target-spec build --release --no-default-features
# kernel with the embedded ring-3 test binaries (default feature `selftest`)
make -C user all && python3 mkfs.py && cargo -Z json-target-spec build --release
# binary: target/x86_64-talaria/release/talaria        (details: BUILD.md)

kernel/test.sh        # QEMU regression: boots every mode, asserts its output (needs /dev/kvm)
kernel/validate.sh    # heavy: open-source workloads, latency SLA, soak, fuzzing, databases
cd ../kernel-unit && cargo test    # host unit tests of the pure modules
```

A mode is chosen with the boot parameter `talaria.test=<name>` or
`talaria.mode=<name>`; every `talaria.*` parameter is listed in
`src/cmdline.rs` (`KNOBS`), and an unknown one refuses to boot.

**UEFI boot** (EC2 and hardware without QEMU): `uefi-stub/` is a PE application
(`x86_64-unknown-uefi`, no dependencies) with the kernel ELF inside: it calls
ExitBootServices, converts the EFI memory map to the PVH format and jumps
through a 32-bit trampoline (< 4 GB) with paging off into the existing
`pvh_start`. Build it with `uefi-stub/build.sh` after every kernel build; it is
exercised by the `uefi` block of `test.sh` (OVMF q35). The RSDP from UEFI goes
into `start_info.rsdp_paddr`.

**BIOS boot without GRUB** (the `*.metal` path): `../tools/tboot/` — stage1 (72 B)
and stage2 (~1.7 KB) read the kernel from disk themselves (`int 13h`, unreal
mode), take the memory map (`int 15h E820`), build multiboot2 and jump to
`mb2_start`. The image: `../tools/mk-tboot.sh` (including `SUITE=` — a set of
modes on one image). On this path nobody passes the RSDP and the kernel finds
it itself (EBDA/ROM). For machines without a console: `talaria.trace=<LBA>`
writes the whole kernel log to raw disk sectors (`docs/DEVELOPMENT-GUIDE.md`
§11.4).

## Capabilities by area

### Ring-3 runtime (`user.rs`, `syscall.rs`, `uaccess.rs`)
- SYSCALL/SYSRET, ELF loading (embedded and from disk, static and dynamic with
  PT_INTERP), the SysV initial stack (argv/auxv), musl and glibc, full C++
  (STL, exceptions, iostream), threads (clone/futex), a vDSO.
- Every pointer argument of a syscall is validated before the handler runs
  (`uaccess.rs`, gate `talaria.test=efault`).

### Memory protection (`paging.rs`)
- Its own page tables (GB0 in 2 MB pages, GB1-3 in 1 GB), a walker that splits
  1 GB → 2 MB → 4 KB, demand-zero reservations, COW with reference counts.
- **W^X per segment** from `p_flags`, **NX**, **guard pages** below stacks, a
  real **mprotect**, `PROT_NONE`. Test: `talaria.test=wxorx` (live #PF).

### Process model (`paging.rs`, `sched.rs`, `proc.rs`)
- A private address space per process (its own PML4 and GB0 page directory),
  CR3 switch in the scheduler.
- **exec**, **fork** with copy-on-write, signals, pipes, SysV/POSIX shared
  memory, per-process fd tables.

### Scheduling (`sched.rs`, `affinity.rs`, `placement.rs`, `sync.rs`)
- A scheduler on every core, dedicated cores without a tick, named pools,
  NUMA-aware placement driven by the manifest; futexes sleep in `mwait` on the
  user's word.

### Observability
- **top/ps/netstat/kstat/perf** in the operator console, a sampling profiler
  with stacks (`obs/`), ELF core dumps of the workload and of the kernel.
- **flight recorder** (`fr.rs`), **PMU** (`pmu.rs`, rdpmc), **gdb stub**
  (`gdb.rs`, GDB RSP on COM2, `talaria.mode=gdb`), **crash dump** with
  registers, stack and flight recorder on an exception.

### Operations
- **supervise** (`supervise.rs`): failure isolation and restart of the
  workload, a watchdog, the workload manifest (`cellman.rs`, `/cell.manifest`).
- **operator console** (`console.rs`, `shell.rs`, `edit.rs`): line editor,
  file commands, a vi-like editor, network configuration.
- **file transfer** (`filed.rs`, `talaria.mode=filed` + `tools/talaria-file`)
  and the **control plane over TCP** (`ctlplane.rs`, `/ctl` with a token).
- **syscall filter**: an allowlist that returns `-EPERM` (`talaria.test=syscallfilter`).

## Main modes

| Mode | What it does |
|-------|-----------|
| `talaria.mode=cell` | the workload from `/cell.manifest` under the supervisor, in its own address space |
| `talaria.mode=proc` | the workload as a process: fork, signals, dynamic ELF — the database path |
| `talaria.mode=console` / `gdb` / `filed` | operator console / debugging / file transfer |
| `talaria.mode=server` / `smoltcp` / `httpd` / `fast` | network demo modes (sockets, smoltcp, HTTP, zero-syscall rings) |
| `talaria.test=<name>` | one of the built-in self-tests (`src/selftest/`, run by `test.sh`) |

## Known limitations

- `run_disk` (the embedded demo services such as `mode=redis`) shares the
  kernel's address space; the production modes `cell` and `proc` run the
  workload in its own.
- The hardware watchpoint is fixed at 4 bytes.
- Limits, trade-offs and the roadmap: `docs/PRODUCT.md`.
