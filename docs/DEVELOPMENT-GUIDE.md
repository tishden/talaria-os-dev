# Talaria OS — Development Guide

> **Before any work with bare metal read `docs/BARE-METAL.md`** — the catalogue
> of what QEMU hides (zeroed memory, dense APIC ids, a live COM1,
> assigned BARs, a fast disk, an emulated APIC bus), the diagnostics toolbox and
> the techniques of reproducing the hardware at home. Every item there is paid for by a run on a
> `c6i.metal` and is closed by a gate in the regression.

The developer's guide to Track B (the bootable kernel `kernel`): the architecture, the boot
flow, the map of the modules, the APIs/the subsystems, the invariants and step-by-step recipes for improvements.
A separate section is about how to carry out the development effectively **with an AI agent**.

The audience: whoever **changes the kernel/the ABI itself**. How to *run* a workload — see
`docs/USER-GUIDE.md`.

---

## 0. For an AI agent: how to find one's way and work

**The working cycle (proven in this project):**
1. **An edit** to one module for one task (an item of the TODO/PROD-GAPS).
2. **The build:** `cd kernel && cargo -Z json-target-spec build --release   # the toolchain is pinned in rust-toolchain.toml`.
3. **A run of one mode** in QEMU (docker), an assert of the needed line in the serial output (it is faster
   than the whole regression) — the template is in §11.2.
4. **The regression** `kernel/test.sh` (the current count of the asserts is in the README) — keep it green.
5. **A commit** per item (the message says what and why), then the next item.

**Where to look for the facts (rather than guess them):**
- The list of the gates: the names in the `match` inside `kernel/src/selftest/*.rs`;
  the production modes are in `kernel/src/runmode.rs`. The parsing of a name is `kernel/src/cmdline.rs`.
- The list of the syscalls: the match arms in `kernel/src/syscall.rs` (`rust_syscall`).
- The modules: `grep '^mod ' kernel/src/main.rs`.
- The test's asserts: the lines `talaria.test=…|<pattern>|<name>` in `kernel/test.sh`.

**The environment's pitfalls (important):**
- A `cd` in a compound command may trigger a prompt — use `cd A && …` or
  absolute paths; **run docker with QEMU from the repository's root** (`-v "$PWD":/w`, while the kernel's
  path is `kernel/target/...`).
- `grep` sometimes "goes deaf" on some files — use `awk`/`sed`.
- In Rust strings/in asm there must be **ASCII only** where that matters for the build; in
  the logs it is ordinary UTF-8 text.
- The tap/QEMU network is flaky — at a "No route to host" repeat the launch.
- The first TCP connection after a boot is more reliable (a socket's re-listen races — see §12).

**The invariants that are easy to break:** §12 (a CR3 switch on a shared stack, the CR0.WP,
the identity DMA, a guard+the reuse of a frame, musl's linking address). Read it before editing
paging/sched/syscall.

---

## 1. An overview

Talaria's Track B is an x86-64 microkernel in Rust (`no_std`, `panic=abort`), booted as a
**PVH image** under QEMU/KVM. The application (a "cell") executes in **ring 3** through
a subset of the Linux syscall ABI. The kernel is responsible for the memory (its own page tables, W^X),
the process model (a per-process AS, fork/exec, the scheduler), the devices (virtio-blk/net),
the observability and the operator's side.

~6000 lines of Rust + a little asm (the boot, the syscall/exception stubs, setjmp/longjmp).

---

## 2. The build

- The target: `kernel/x86_64-talaria.json` (a custom one, with SSE2), `build-std`
  (`core`/`alloc`/`compiler_builtins`, `compiler-builtins-mem`).
- The linking: `linker.ld`, `relocation-model=static`, `-Tlinker.ld` (see
  `.cargo/config.toml`).
- The release profile: `opt-level=3`, `lto=true`, `codegen-units=1`, `panic="abort"`.
- The dependencies: `smoltcp 0.14` (ethernet/ipv4/udp/tcp, no default features),
  `virtio-drivers 0.11`.

```sh
cd kernel
cargo -Z json-target-spec build --release   # the toolchain is pinned in rust-toolchain.toml
# -> target/x86_64-talaria/release/talaria  (a PVH image, loaded by QEMU's -kernel)
```

The user binaries (`kernel/user/*`, `services/*`) are built **separately**
with a static musl at `0x10000000` (`as`/`ld` for the asm tests; `musl-gcc` for C/C++).
See §10.4.

---

## 3. The boot flow

0. **Two entries and four bootloaders, one contract.** The boot information in
   all the cases comes down to the PVH `hvm_start_info`, and lower down the stack the kernel knows nothing
   about the bootloader:
   - **PVH** (QEMU/Cloud Hypervisor): the entry is `pvh_start`, `%ebx` = the start_info's address;
   - **UEFI** (EC2): `kernel/uefi-stub` — a PE application with the kernel's ELF inside; it
     assembles the start_info out of the EFI memory map itself and jumps into `pvh_start`;
   - **multiboot2 through GRUB** (BIOS, v1.2): the entry is `mb2_start` (set by the tag
     "entry address" in the `.mb2` header — the bootloaders must not be told apart by the `eax`,
     PVH does not stipulate its value); `boot::mb2_convert` translates the MBI into a
     start_info (the memory map is tag 6, the cmdline is tag 1, the RSDP is tags 14/15);
   - **our own bootloader tboot** (BIOS, v1.6; that is how Talaria boots on a `*.metal`):
     `tools/tboot/stage1.S` (72 B) + `stage2.S` (~1.7 KB) read the kernel off the disk
     themselves (`int 13h`, pieces of 127 sectors, unreal mode), take the memory map
     (`int 15h E820`), assemble a multiboot2 and go into the same `mb2_start`. There is no GRUB
     in that chain at all. The RSDP tags (14/15) tboot does NOT assemble — the kernel looks for the
     RSDP itself (see below), and that turned out to be decisive on bare metal.
   The images: `tools/mk-ami.sh` (UEFI/an ESP), `tools/mk-bios.sh` (an MBR+GRUB; `HYBRID=1`
   is one disk for both paths), `tools/mk-tboot.sh` (our own bootloader; `SUITE=` is
   a set of modes on one image, §11.4).
1. **`boot.rs`** — the PVH/multiboot2 headers and the 32-bit entries. The first thing both
   entries do is **zero the `.bss`** (`ZERO_BSS`, the bounds from `linker.ld`): a bootloader is not
   obliged to fill NOBITS, and on hardware the memory before us is dirty — under QEMU it is
   zeroed, so the whole class of errors is invisible locally. Then the assembler
   identity-maps the first 4 GiB (temporary boot tables), enters long mode,
   loads the GDT, `ljmp`s into 64-bit and calls `rust_main(start_info)`.
2. **`main.rs::rust_main(start_info)`** (`#[no_mangle] extern "C"`) — the order of the initialisation:
   - `serial::init()` → the log into the ttyS0;
   - if the boot was a multiboot2 (`boot::is_multiboot2`) — `boot::mb2_convert`;
   - `read_cmdline` — AT ONCE (it is only a read of memory): the points
     `talaria.rebootat=` and the early trace on the disk wait for it;
   - `fr::record(BOOT)`; `idt::init()` (the handlers of the exceptions);
   - `mem::init(start_info)` — the parsing of the `hvm_start_info.memmap`, the choice of the largest
     RAM region BELOW 4 GB (everything the allocator gives out the kernel touches by its
     identity address), a bump+free-list allocator;
   - `paging::init()` — **our own** page tables (a 1G identity, the GB0 split into 2M),
     instead of the boot tables;
   - **the early trace on the disk**, if a `talaria.trace=<LBA>` is set: `pci::init()` +
     `fs::blk_open()` + `pci::reset_access()` — here, before everything else, so that
     on a machine without a console a hang in any following step is distinguishable
     (§11.4). Everything the disk can do without stands AFTER it;
   - `percpu::init(0)`; `smp::legacy_quiesce()` (the masking of the PIC/LVT; inside there is
     `force_xapic`: at an `IA32_APIC_BASE.EXTD` any access to the LAPIC's MMIO window
     gives a #GP, and a bare-metal firmware is entitled to leave x2APIC on);
   - `enable_avx()`; `tsc::calibrate()`/`set_hz()` (on hardware it is CPUID 0x15/0x16,
     the calibration by the i8254 is BOUNDED by the number of polls: there may be no legacy
     timer); `sync::init()`;
   - `acpi::init(rsdp)` — the address comes from the bootloader, and if there is none (the BIOS path), the kernel
     **looks for the RSDP itself**: the EBDA (the segment from the BDA at 0x40E) and the ROM `0xE0000..0xFFFFF` with
     a checksum. Without that there is no MCFG, that is, no ECAM window;
   - `pci::init()` (the full enumeration) and a SECOND attempt to bring the disk up: on bare
     metal the root volume's NVMe stands not on bus 0 and is visible only through the ECAM;
   - `talaria.suite=<LBA>` — take one's own item of the set and append it to the cmdline;
   - on EC2 with a `talaria.userdata=1` — an early bring-up of the netsys, the reading of the cmdline's
     tail out of the IMDS and the tearing of the stack down (`netsys::teardown_keep_lease`), so that the mode's
     branch opens the card anew already with all the options;
   - `selftest::run(cmdline)` — the built-in regression: the name from the `talaria.test=` is
     parsed ONCE and compared as a whole in a `match` (`cmdline.rs`);
   - `runmode::run(cmdline, tsc_hz)` — the production modes `talaria.mode=`; `rust_main` goes
     there with its last line and does not return;
   - if no mode is set there is the default inside `runmode`: bring an AP up for the datapath
     (`smp`), while the BSP becomes the control plane (the scheduler of the tasks
     `task_control_plane`/`task_housekeeping`).

The point where a new mode is added is `selftest/` or `runmode.rs` (§10.2).

---

## 4. The map of the modules (`kernel/src/`)

| The file | Lines | The responsibility | The key pub APIs |
|------|------:|-----------------|------------------|
| `main.rs` | 539 | ONLY the machine's bring-up: from the bootloader's entry to the choice of the mode; it ends with a `runmode::run(...)` | `rust_main` |
| `cmdline.rs` | 122 | the parsing of the command line by TOKENS (not by substring): a mode's name is compared as a whole | `value`, `has`, `flag`, `num`, `test_is`, `mode_is`, `test_name`, `selftest` |
| `selftest/` | ~1220 | the built-in regression `talaria.test=<name>`, by theme: `platform`/`storage`/`memory`/`process`/`concur`/`netsvc` | `selftest::run(cmdline)` |
| `runmode.rs` | 427 | the production modes `talaria.mode=<name>`: the shell, the supervisor, a workload by a manifest, the servers, the datapath, the control plane on the BSP | `run`, `open_nic`, `run_datapath`, `nic_handoff_put` |
| `boot.rs` | ~220 | the PVH header and the entry into long mode (asm); **v1.2 §3.1: the multiboot2 header (the `.mb2` section, only under a `KEEP()` in linker.ld) + the entry `mb2_start` and the translation of the MBI into the same `hvm_start_info`** — the kernel does not tell the bootloaders apart | `pvh_start`, `mb2_start`, `is_multiboot2`, `mb2_convert` |
| `serial.rs` | 72 | the output into a 16550 UART (ttyS0/COM1), `println!` | `init`, `putb`, `print` |
| `idt.rs` | 293 | the IDT, the handlers of the exceptions, the **COW fast path for a #PF**, the crash dump, the IPIs | `init`, `rust_exception` |
| `paging.rs` | 510 | our own page tables, the split 1G→2M→4K, W^X/NX, guard/unguard, a per-process AS, COW | `init`, `protect`, `guard`, `unguard`, `map_leaf`, `new_process_space`, `switch`, `cow_fault`, `virt_to_phys`, `query` |
| `mem.rs` | ~360 | the physical frame allocator (a bump + a free-list reclaim), the COW refcount + **v1.9: the per-node NUMA areas by the SRAT (`alloc_contig_node`) with a free list OF THEIR OWN (a frame of a foreign node lies above 4 GB and must not get into the common list) and with an identity mapping AT ONCE in `numa_init` — a process's space copies the kernel's tables at the moment of its creation, so a deferred mapping would not reach it** | `init`, `numa_init`, `alloc_frame`, `alloc_contig`, `alloc_contig_node`, `free_contig`, `rc_*`, `free_frames` |
| `sched.rs` | ~935 | the pre-emptive scheduler, a per-task CR3, spawn/reap/exit, a per-process heap; **v0.51+: the CPU_OF affinity, `ap_loop` (pinned tasks on an AP), the migration `migrate_current_to`, a directed switch (`HINT`/`yield_to`/`yield_now`), an AP's doorbell**; **v1.9: a scheduler on EVERY core of the pool (`ap_schedule`), the pool's mask `POOL`, a thread's name `COMM`, the cheap migration of a pre-empted task `migrate_idle_task`, the BSP's idling `start_idle`** | `spawn`, `spawn_process`, `set_task_cr3`, `reap`, `exit_current_code`, `set_heap`, `start`, `start_idle`, `install`, `ap_loop`, `yield_to`, `cpu_of`, `set_comm`, `tasks_on`, `migrate_idle_task` |
| `affinity.rs` | ~830 | **v1.9: the layout of the threads over the cores** — the manifest's rules (`pin=`), NAMED POOLS of cores (`pool=`), dedicated cores (the OS chooses the number), the spreading of the unpinned ones, the levelling by DEMAND with a hysteresis, the NUMA locality of the threads AND of the memory by the SRAT, the routing of the MSI-X (`irq=`); the world's snapshot `World` was extracted for the sake of a self-test without hardware | `configure`, `place_main`, `place_new_thread`, `on_set_name`, `balance_tick`, `pick_in_pool`, `imbalance_in_pool`, `allowed_mask`, `pick_launch_node`, `route_device_irqs`, `dump`, `selftest` |
| `sync.rs` | ~330 | **PLAN-SYNC**: the detection of the wait primitives (WAITPKG/MONITORX/MONITOR/hlt), `kwait_once` (MONITOR→a recheck→sti;mwait), the registry of the futex waiters (WADDR/WOKEN/WDEADLINE), `wait_on_ex` (kind0 a futex / kind1 "until a target" / kind2 a doorbell+a pair), the asymmetric `membarrier` (an IPI ack), the TSC deadline, the ring 0 self-test | `init`, `mode`, `kwait_once`, `wait_on`, `wait_on_ex`, `wake_addr`, `membarrier`, `kick`, `blocked_for_sched`, `runnable_peek` |
| `percpu.rs` | ~80 | the per-CPU data through the GS.base (without a swapgs; gs:0 the ursp, gs:8 the kstack, gs:16 the sig_newrsp, gs:24 the cpu, gs:32 the cur_task), the ONLINE bitmap | `init`, `cpu_id`, `set_kstack_top`, `set_sig_newrsp`, `cur_task` |
| `syscall.rs` | 532 | SYSCALL/SYSRET (the MSRs), the dispatcher `rust_syscall`, brk/mmap, fork/clone/execve, the fd stubs, the **syscall filter** | `init`, `rust_syscall`, `filter_enable`, `filter_denied`, `heap_base` |
| `user.rs` | 555 | the GDT/TSS, the ELF loading (built-in/from the disk), a SysV stack, run/run_disk/run_threaded, exec/run_processes/execve, run_processes_disk | `run`, `run_disk`, `run_threaded`, `exec`, `run_processes`, `run_processes_disk`, `execve`, `run_expect_fault` |
| `fs.rs` | ~400 | TFS1/TFS2 over virtio-blk/NVMe, the table of descriptors, **the parsing of a GPT (the data partition BY THE NAME `talaria-data`) and of an MBR (the BIOS boot image)**, `blk_write_many` | `init`, `open`, `read`, `read_all`, `lseek`, `fstat`, `close`, `readlink_fd` |
| the format modules: `ext2dir.rs`, `ext2dx.rs`, `ext2bmap.rs`, `ext2sb.rs`, `jbd2.rs`, `ext2hash.rs`, `elfhdr.rs`, `elfnote.rs`, `tfs.rs`, `routes.rs`, `placement.rs`, `clock.rs`, `uaccess.rs` | ~100-300 each | the arithmetic over bytes that the FS, the loader and the dumps are built on: a directory's records and its htree index, the bitmaps, the superblock, the JBD2 log together with its REPLAY, the htree hash, the ELF headers and the notes of a core dump. They touch neither the cache nor the device, so they are compiled for the host and judged by tests (§11.0) rather than by a boot | see §11.0 |
| the decision modules: `rqpick.rs`, `waitpred.rs`, `placement.rs`, `fastring.rs` | ~100-300 each | the scheduler's and the synchronisation's decisions as pure functions over a snapshot: whom the core takes next and the policy `reqfirst` (`rqpick`), whether a steal improves the balance (`placement`), whether a sleeper on a word keeps waiting and whether a waker may declare it woken (`waitpred`), the layout and the index arithmetic of the zero-syscall rings plus the ARP the cell answers itself (`fastring`). `sched.rs`/`affinity.rs`/`sync.rs`/`fastpath.rs` supply the state and the device, these decide — so the decision is judged by tests (§11.0) rather than by a latency somebody else measures | see §11.0 |
| `netsys.rs` | ~900 | the socket syscalls over smoltcp (an epoll pool), TCP/UDP; **v1.11: N INTERFACES** — each has its own `SmolDev`/`Interface`/`SocketSet`/DHCP, a socket is bound to an interface (otherwise smoltcp would send its packet into BOTH cards: it does not check the source address for a match), a listener spreads its backlog over all the interfaces | `socket`, `bind`, `listen`, `accept4`, `epoll_*`, `read`/`write`, `udp_*`, `add_iface`, `if_info`, `if_set_addr`, `if_set_up`, `if_set_dhcp` |
| `l1.rs` | ~580 | the `Nic` (virtio/the ENA, **by the reference `&'static mut`**) + the **`NicQ`** — slot-oriented rings over any card (common to smoltcp and to a cell, with batched doorbells); the `idle_watch` is now in virtio too (a sleep on the word of the receive used ring); the kernel-mode HTTP/UDP servers | `NicQ`, `SmolDev`, `run_httpd`, `run` |
| `fastpath.rs` | ~200 | the zero-syscall datapath: a cell (an AP) ↔ SPSC rings in shared memory ↔ the application (the BSP); a batched TX notify; **v1.1 §7.4: on the `l1::NicQ` — it works on the ENA as well** | `run_cell`, `init_region`, `region`, `set_region` |
| `smp.rs` | ~215 | the bring-up of the APs (INIT-SIPI, the EFER.NXE, **a stack of 32 KB + a guard page** — the card now travels by reference, v1.2 §1.1), the LAPIC, the timer, the IPIs, the `AP_MODE` (0 the datapath / 1 the sync self-test / 2 the scheduler of pinned threads) | `lapic_enable`, `lapic_timer_init`, `send_ipi`, `start_ap`, `AP_MODE` |
| `hal.rs` | 42 | the HAL for virtio-drivers (a DMA share/unshare through `virt_to_phys`) | `TalariaHal` |
| `crypto.rs` | 136 | the AES-128-GCM self-test (for the trade mode) | `Aes128` |
| `supervise.rs` | 75 | the isolation of a workload's failure (setjmp/longjmp) + the auto-restart | `supervise`, `tal_setjmp`, `tal_longjmp`, `RECOVERY*`, `CRASH_*` |
| `cellman.rs` | ~420 | the parser of the workload's manifest (`binary`/`args`/`restart`/`core`) + **v1.9: the pools of cores `pool=<name>:<cores>`, the layout rules `pin=<pattern>:<target\|a pool's name>`, `spread=`/`balance=`/`numa=`/`node=`/`irq=`** (two passes: the pools are parsed first, so the order of the lines does not matter) | `parse`, `CellManifest`, `PinRule`, `CorePool`, `match_pin`, `pool_index`, `add_pin_spec`, `add_pool_spec`, `selftest` |
| `console.rs` | ~640 | the operator's console: the LINE EDITOR (the history, the arrows, Ctrl-A/E/U/K/W, the Tab completion of the commands and of the paths), the table of the commands with groups and help, a hint at a typo, the dispatcher. Two contexts: `run` (an interactive loop on the BSP) and `poll_tick` (on top of a workload, from the tick — one must not block there) | `feed`, `line`, `exec_line`, `run`, `poll_tick`, `selftest` |
| `shell.rs` | ~600 | the operator's shell in the kernel: the cwd and the parsing of the paths, `ls/cat/less/head/tail/hd/stat/mkdir/rm -r/cp/mv/touch/write/append/df/sync`, the completion of the paths. Over ext2 (RW) and TFS (RO) alike. NOT busybox: busybox is a workload in ring 3, while the console is needed when there is no ring 3 | `resolve`, `cwd`, `cmd_*`, `list_dir`, `complete_path`, `fopen/fread/fclose`, `selftest` |
| `edit.rs` | ~520 | the console's editor — a subset of vi (hjkl/w/b/gg/G, i/a/o/x/dd/D/J/r/u, a `/`search, `:w :q :q! :wq`), a full-screen ANSI drawing with a redraw of the changed lines. A file that did not fit into the static buffer is opened READ-ONLY | `run`, `selftest` |
| `filed.rs` | ~490 | the uploading/downloading of files over TCP (something like rsync): a `STAT`+the CRC32 of the whole file — it matched, do not transfer; `SUMS` is the CRC32 per block, `PATCH` is only the differences. A state machine (Cmd/Recv/Send), it does not block the console. The token is mandatory | `start`, `stop`, `poll`, `cmd`, `crc32_update`, `selftest` |
| `netcfg.rs` | ~470 | the network's configuration as a model: the routing table (the longest prefix, the connected networks are stronger than a gateway), the rules `talaria.net=`/`talaria.route=` (an interface by name OR by MAC), the commands `ip`/`route`/`dhcp`/`dns` | `egress`, `default_ifx`, `add_route_f`, `del_route`, `apply_iface`, `apply_pending`, `cmd_ip`, `cmd_route`, `selftest` |
| `ctlplane.rs` | 204 | the control plane over TCP: the auth of `/ctl` (a token from the cmdline) + the telemetry's body | `configure_from_cmdline`, `is_ctl`, `authorized`, `build_response`, `body`, `selftest` |
| `metrics.rs` | ~250 | the atomic counters: syscalls/page_faults/cow/frame_allocs — in total and PER TASK | `inc_syscall_of`, `inc_page_fault`, `add_frames`, `syscalls_of`, `frames_of`, `snapshot`, `dump` |
| `obs/mod.rs` | ~230 | the observability: the formatting of the quantities (bytes, rates, shares, durations), the periodic snapshot `talaria.top=` | `bytes`, `rate`, `brate`, `pct`, `dur`, `miss_llc`, `periodic_tick`, `configure_from_cmdline` |
| `obs/top.rs` | ~330 | `top`/`ps`: the processes and the threads, the %CPU by the TSC's cycles, the RSS by a walk over the tables, the rates of the allocations/the switches/the syscalls | `cmd`, `cmd_ps`, `pmu_line` |
| `obs/netstat.rs` | ~130 | `netstat`: the interfaces with the card's counters, the routes, the sockets with the owning thread, the datapath | `cmd`, `ifaces`, `sockets` |
| `obs/kstat.rs` | ~190 | `kstat <section>`: sched/mem/paging/fs/net/sync/irq/cache/syscalls | `cmd`, `sched`, `mem`, `paging`, `fs`, `net` |
| `obs/perf.rs` | ~280 | `perf`: the hardware counters per core, a `poke` into a dedicated core, the sampling profile with a stack, the top of the leaves and the top of the stacks, the dump of the samples | `cmd`, `counters`, `poke`, `top_addrs`, `top_stacks`, `record`, `verdict` |
| `coredump.rs` | ~420 | an ELF `ET_CORE` for the workload and for the kernel: an `NT_PRSTATUS` per thread, an `NT_PRPSINFO`, a `PT_LOAD` with the stacks; it is read by an ordinary `gdb` | `dump_process`, `dump_kernel`, `dump_task`, `configure_from_cmdline` |
| `obs/unwind.rs` | ~230 | the unwinding of the stack by the RBP chain with a RECOVERY POINT on a #PF (a garbage RBP must not bring the kernel down); a self-test on a synthetic chain of frames | `walk`, `try_read`, `fault_fixup`, `faults`, `selftest` |
| `telemetry.rs` | 68 | the cell's telemetry: the pkts, the p50/p99 (a log2 histogram), the gaps, the restarts, the uptime | `record_lat`, `inc_gap`, `inc_restart`, `pctl`, `dump` |
| `watchdog.rs` | 44 | the workload's health: there is no progress of the pkts → a stall→a restart | `arm`, `tick`, `disarm`, `stalls_total` |
| `fr.rs` | 62 | the flight recorder: a ring of events (boot/paging/fault/restart/syscall-deny/…) | `record`, `dump`, the events' constants |
| `pmu.rs` | ~330 | the hardware counters: the layout is CHECKED against the deed (not by the CPUID), an initialisation on every core, the per-core snapshots from the tick, a `poke` through an IPI | `init`, `init_cpu`, `sample`, `note_tick`, `note_here`, `poke`, `last`, `Sample::delta` |
| `gdb.rs` | 277 | the gdb stub (the GDB Remote Serial Protocol over COM2) | — |
| `tsc.rs` | 71 | the calibration of the TSC, `rdtsc`, the Hz cache | `calibrate`, `calibrate_cached`, `rdtsc`, `set_hz` |
| `vdso.rs` | 86 | the vDSO: `clock_gettime` in a process WITHOUT a syscall (the page is mapped into the process, the base is republished by the clock's servo) | `init`, `map_into`, `publish` |
| `vga.rs` | 123 | the VGA text screen (0xB8000) — the kernel's second output channel: on `*.metal` `get-console-output` returns the FIRMWARE's console, not COM1 | `clear`, `enable`, `putb`, `peek` |
| `trace.rs` | 166 | `talaria.trace=<LBA>`: the early log into memory and its flush ONTO THE DISK — on `*.metal` the kernel has no other channel before the drivers | `arm`, `push`, `stop`, `set_eager` |
| `suite.rs` | 164 | `talaria.suite=<LBA>`: a LIST of modes from one image, across reboots — one POST of a metal instance instead of one per mode | `lba_from_cmdline`, `take`, `more`, `park`, `reboot` |
| `cat.rs` | ~390 | §6.3: the partitioning of the L3 (Intel CAT, the manifest's `llc=<to whom>:<mask>`) and the cache's size — a neighbour washing the LLC out is also a tail | `init`, `present`, `l3_bytes`, `mask_valid`, `apply`, `apply_local` |
| `ioapic.rs` | 102 | the I/O APIC: the delivery of interrupts from DEVICES (the network one — without it the stack moved only from a syscall) | `note`, `available`, `lines_of`, `route` |
| `imds.rs` | 141 | a mini client of the EC2 metadata service (IMDSv2, `talaria.imds=1`): the token, the instance-id, the hostname, the user-data | `fetch`, `user_data` |

`kernel/user/` — the user binaries/sources (asm: `hello.s`, `uaftest.s`,
`forktest.s`, `devnull.s`, `sfilter.s`; C: `memtest.c`, `launcher.c`, `fastapp.c`,
`epsrv.c`, `srv.c`, …; C++: `cpptest*.cpp`; ready musl binaries `mhello`, `mprintf`, …).

`kernel/user/tsync/` — libtsync (PLAN-SYNC): `tsync.h` (SPSC fast path — static inline)
+ `tsync.c` (a wait word/a mutex/MCS/a condvar, the slow paths of the SPSC), `fiber.[ch]` (green threads),
the tests/benchmarks `tsynctest.c`/`syncbench.c`/`fiberbench.c`, the musl patch `musl-1.2.5-talaria.patch`
+ `build-musl.sh` (→ `user/dyn/ld-musl-x86_64.so.1`).

`kernel/vendor/virtio-drivers/` — the vendored crate: + `transmit/receive_begin_no_notify`,
`transmit/receive_notify` (a wrap-safe `vring_need_event` per batch, with an mb inside) — a batched TX kick;
+ `used_watch`/`rx_used_watch` — the word of the used ring for a sleep while idle (v1.2 §1.2).

**The disk images.** `tools/mk-ami.sh` — a GPT+an ESP (UEFI, EC2); `tools/mk-bios.sh` —
an MBR+GRUB (BIOS) and, with a `HYBRID=1`, a HYBRID GPT disk (a bios-boot + an ESP + a /boot +
a `talaria-data`) that boots by both paths. The embedding of the core.img is done
by hand (grub-bios-setup requires guessing the HOST's root device and on
an image file it fails): 440 bytes of the boot.img in the MBR, the `kernel_sector`@0x5C, the list of
the boot.img blocks diskboot.img@0x1F4, the segment 0x820.

`services/<svc>/` — real open source: a `README.md`, an `mkdisk.py` (TFS1), a `run.sh`.

---

### 4.1. The tools (what lies next to the kernel)

| The file | What for |
|---|---|
| `kernel/test.sh` | the regression: it boots every mode in QEMU and asserts the output; the host checks (e2fsck, `talaria-prof`, the core dumps through `gdb`) go as a separate total |
| `kernel/validate.sh` | the validation: real workloads (the DBMSes, the JVM, ClickHouse) |
| `kernel/build-prof.sh` | the kernel WITH FRAME POINTERS — the ring 0 stack in the profile and in the core dumps; the price has been measured (~4 % on a syscall), hence a separate build |
| `kernel/mkfs.py`, `kernel/mkext2.sh` | the TFS and ext2 images |
| `kernel/tools/talaria-pack` | a disk out of a directory + a manifest |
| `kernel/tools/talaria-check` | the compatibility of somebody else's binary with the ABI |
| `kernel/tools/talaria-file` | the uploading/downloading over TCP (the `filed` client) |
| `kernel/tools/talaria-prof` | the parsing of a profile: the symbols, the stacks, a folded output for a flame graph; the fixtures are in `kernel/tools/testdata/` |
| `kernel/uefi-stub/build.sh` | the UEFI stub (it carries the kernel's ELF inside — it goes stale at a rebuild of the kernel) |
| `tools/mk-ami.sh`, `mk-bios.sh`, `mk-tboot.sh` | an AMI, a BIOS disk, our own bootloader |
| `infra/aws/read-disk-trace.sh` | read the `talaria.trace=` off a volume |

The rule about the host tools: **what reads somebody else's format is checked by somebody else's
tool.** The core dumps in the regression are opened by a real `gdb`, the ext2 images are checked
by `e2fsck`, the profile is resolved by `addr2line`. Our own checks of a format
let through exactly the errors for whose sake the format was chosen (see the story with
the four bytes of alignment in the `prpsinfo` note).

---

## 5. The memory and the paging (`paging.rs`, `mem.rs`)

### The layout

- **The kernel:** 0–32 MB (`KERNEL_TOP`), supervisor. The code/the stack/the data/the tables.
- **The user's region:** 32 MB–1 GB, identity, U/S (the GB0 is split into 2M leaves).
- **GB1–GB3:** 1G pages (RAM + MMIO, the GB3 is uncached for MMIO).
- **The frame allocator:** a bump above 32 MB + a free list (a reclaim with coalescing), 4 KB.
- **The heap/brk of the common AS:** 128 MB (`HEAP_FRAMES`), NX. **A process's private heap:** 16 MB
  at `0x30_0000_0000`. **A process's stack:** the top is `0x7fff_f000_0000`.
- The user binary is linked at `0x10000000`.

### The page tables

- `init()` builds a PML4+a PDPT (a 1G identity), the GB0 → a PD with 512×2M (for a per-page protection).
- `pte_ptr(vaddr)` is the walker that splits the large leaves along the way (`split_1g`,
  `split_2m`; the sub-entries inherit the physical base+the flags). `walk_entry` is a read without
  a split. `query(vaddr) -> (present, writable, user, exec)`.
- `protect(vaddr, len, writable, exec)` — set present+US, the RW by the flag, the NX if
  `!exec` (W^X); the physical base is preserved (identity).
- `guard(vaddr)` — clear present (a trap page; an access → a clear #PF).
- **`unguard(vaddr, len)`** — return present WITHOUT allocations/a split. It is critical: see
  §12 (guard+reuse).
- `nx_range(vaddr, len)` — mark a range NX, **preserving the 2M leaves** where it is
  aligned (less TLB pressure on a large heap/on arenas).

### The per-process address spaces and COW

- `new_process_space()` — its own PML4 with a private GB0 PD (2M leaves), the kernel's GB0–3 are shared
  (a copy of the PDPT's entries). `map_leaf(pml4, v, phys, w, u, x)` — a 4K leaf (it sets the `PTE_PRIV`).
- `switch(pml4)` — load the CR3. `virt_to_phys(v)` — a walk for a DMA under a non-identity AS.
- **COW:** `fork_cow_space` marks the entries `PTE_COW` (bit 9), RO, and shares the refcount
  of the physical frame (`mem::rc_share`). `cow_fault(cr2)`: if `rc<=1` we write in place (RW,
  a reset of the refcount); otherwise a copy, an `rc_drop`, a `COW_COUNT++`. It is called from the COW fast path
  in `idt.rs::rust_exception` (a write #PF into a present COW-RO page) with a return to
  retry the instruction (an iretq).
- `PTE_PRIV` (bit 10) is the mark "a process's private page".

### The allocator (`mem.rs`)

- `alloc_contig(n)` — a first fit over the free list (a reclaim) → otherwise a bump. **When giving out of the
  free list it calls `paging::unguard`** (a frame could have been put out by a munmap guard, but
  is reused by the kernel for the tables/a DMA — an access by the identity address must not
  give a #PF). It increments the `metrics::add_frames`.
- `free_contig(addr, n)` — return a block with coalescing. `REFCNT[1<<18]` is the refcount of the
  frames for COW (`rc_set/get/share/drop`).

---

## 6. The scheduler and the process model (`sched.rs`, `user.rs`)

- A pre-emptive scheduler by the LAPIC timer. Every task has: an entry, a user stack,
  a kernel stack, a **per-task CR3** (`SCHED_NEXT_CR3`), a heap range, an FS_BASE (the TLS).
- The CR3 is switched in the timer's asm stub **after** the `mov rax, rsp` (the stack is in the common GB0),
  otherwise the switch would bring the stack down — see §12.
- `spawn(fn, kstack)` is a kernel task; `spawn_process(entry, ustack, kstack, pml4)` is
  a ring 3 process with an AS of its own; `spawn_user(...)` is a thread (a shared AS, for threads).
- The termination: `exit_current_code(code)`, `reap`, the `EXITCODE`/`REAPED`. At the termination of
  all the tasks there is the print "all the N tasks have finished (COW copies: …)" + a `metrics::dump()`;
  under the supervisor (`RECOVERY_ARMED`) there is a `longjmp(RECOVERY, 2)` into it. `exit_all(code)`
  marks as dead all the tasks with the same CR3 (an exit_group); `reset()` zeroes the
  scheduler before a restart of the workload.
- **A CPU core is in one of two states (v1.9)**, and that is a property of the CORE rather than of the task:
  *dedicated* — one task, the LAPIC timer is NOT armed, run-to-completion (that is how
  every AP lives until a layout is set, and that is how the cores handed out by the
  `pin=` rules live); *of the common pool* — a round robin by the tick among that core's tasks. The BSP is always in
  the pool. `rust_schedule` diverges by the `percpu::cpu_id()`: the BSP path (the supervisor,
  the housekeeping, "all the tasks have ended") and `ap_schedule` (only the rotation of its own
  queue). Three things became per-CPU and are bound to stay so: the target's CR3 (`gs:40`),
  the flag of a software yield (`SOFT_YIELD[cpu]`) and the LAPIC timer's configuration
  (`smp::TIMER_INITIAL[cpu]`). A task may be put only onto a core from
  `sched::POOL` — the ONLINE mask includes the cores that do not pick tasks up too.
- **The housekeeping from the tick** (`rust_schedule` → `supervise::sched_tick()` when the supervisor
  is armed): the status line `cell:` once every `STATUS_SECS`, the watchdog by the
  rx progress (`WATCHDOG_SECS`, the manifest's `watchdog=`), the operator's console on COM1
  (`console::poll_tick` — `status/net/telem/restart…`), a requested restart
  (`request_restart`) → a `longjmp(RECOVERY, 4)` out of the timer's context.
- **The loading/the launch (`user.rs`):**
  - A built-in ELF: `run(elf)` (the common AS), `run_threaded(elf)` (threads).
  - From the disk: `run_disk(path, args)` (the common AS, single-threaded — the services);
    **`run_disk_threaded(path, args, envs)`** — the common AS + the scheduler (clone/futex),
    the argv+the **envp**, kernel stacks of 16 KB (`alloc_contig(4)`), a stack of 32 MB reused
    between the restarts (`DT_STACK`), a `mem::reserve` of the image's identity region — the path
    `talaria.mode=cell`. `NTASKS=32`; `spawn_user_ctx` → `usize::MAX`
    at an overflow (a clone → an EAGAIN).
  - A private AS: `exec(elf)`, `run_processes(&[elf…])` (several processes),
    `execve(path, argv)` (replace the current task's image), **`run_processes_disk(&[path…])`**
    (disk workloads, each in its own AS).
  - `load_elf_into_space(pml4, elf)` — a PT_LOAD by vaddr with frames of its own and W^X from
    the p_flags. `build_stack_args[_off]` — the initial SysV stack (the argc/argv/auxv, the AT_PHDR).
  - `map_proc_stack/map_proc_heap` — the private stack/heap; `harden_stack` — a guard+NX.
- A subtlety of execve: the new AS's stack is built through its phys (`woff`) BEFORE the CR3 is
  switched, while the `mov cr3 + iretq` is a single asm (no accesses to locals after the `mov cr3`).

---

## 6b. Synchronisation without a busy spin — how it is implemented (PLAN-SYNC, v0.51–v0.52)

The design and the measurements are in `docs/PLAN-SYNC.md` (§0 the results, §13 the files/the limitations). Here are the mechanics.

**The kernel's waiting (`sync.rs`).** The only sleep primitive is `kwait_once(addr, val)`:
a MONITOR on the line → a recheck of the word → an `sti; mwait` (the arm/check race is closed architecturally:
a write between the monitor and the mwait sets the armed flag). The mode is chosen at the boot (`sync::init`,
the CPUID): WAITPKG/MONITORX (tier 1, ring 3 — on future hardware) → MONITOR/MWAIT in ring 0 (KVM:
`-overcommit cpu-pm=on`) → the fallback `sti;hlt` + an IPI kick (the vector WAKE 0x43, the stub = an EOI +
`lock inc WAKE_ACKS`).

**The futex registry.** A task waiting in `wait_on_ex` is registered in the `WADDR/WOKEN/WDEADLINE`
(one slot per task). One sleeping in the mwait mode monitors the futex's user word ITSELF —
the waker's write (an ordinary `mov` in user space) wakes the core without a syscall and without an IPI. `wake_addr`
(the FUTEX_WAKE/WAKE_OP/REQUEUE, a `clear_ctid` at an exit) sets the WOKEN and finishes the job by address:
a same-core BSP — a directed switch (a `HINT` + an `int 0x41` → `rust_schedule` chooses the hint),
a BSP sleeper with an AP — a `HINT` + an IPI TIMER_VECTOR (an immediate reschedule), an AP sleeper — a kick (for
the hlt mode; in mwait the store has already woken it). The BSP's scheduler skips the waiters
(`blocked_for_sched`; the backstop: once every 64 skips a task gets a quantum — a spurious wake
is allowed by the futex ABI); a waiter with live runnable BSP tasks does not sleep in an mwait but gives the CPU up
(`yield_now`). The timeouts: the BSP has the tick; an AP has a one-shot TSC-deadline LAPIC onto the WAKE vector.

**The predicate upark (the syscalls 4600–4602).** `4601 tsync_kmode()` is the kernel's mode for the user's detection.
`4602 upark(addr, val, deadline, kind, state, extra)`: kind0 is the futex semantics; kind1 is to wait
"while (i32)(*addr−val) < 0" (the kernel re-arms the monitor itself at every change of the word —
a hysteresis without a syscall per change); kind2 is a sleep on a SEPARATE doorbell line, and the predicate over the
pair `extra → {idx_ptr, target}` is rechecked by the kernel **after a `membarrier()`** before the first
sleep (otherwise there is a deadlock "the final store of the partner's index was not visible"). The `state` word is written by
the kernel: 1 = it sleeps in an mwait on the word (the waker's store is enough, a wake syscall is NOT needed), 2 =
it is parked/in a hlt (a futex wake is needed; before setting 2 there is a membarrier). A `membarrier()` =
an IPI ack to all the ONLINE cores (an iret on the target serialises the store buffer) + a fence; the measurement is
`talaria.test=sync` (SYNC-MB, the p50 ≈ 7.5 us per IPI ack — only the sleeper's slow path).
`4600 swtchto(tid)` is a directed switch past the rotation.

**The SMP threads (`percpu.rs`, `sched.rs`, `smp.rs`).** The GS.base on every core is ALWAYS the kernel's
(without a swapgs): the syscall stub and the interrupts read the gs:0/8/16/32 at any moment; the user code is forbidden the GS
(an `arch_prctl(ARCH_SET_GS)`→an EINVAL). A per-CPU TSS/GDT (`setup_gdt_cpu`), the syscall MSRs
on every core. The migration: a `sched_setaffinity` of ONE'S OWN thread BSP→an AP builds a fresh context
of a return from the syscall (rax=0, the callee-saved ones from the `raw` Frame, an fxsave image) on the new kstack,
marks the `CPU_OF` and wakes the `ap_loop` with a doorbell; the old BSP context stays in limbo
(the scheduler does not save the RSP/FPU of the tasks with a CPU_OF≠0), and the old kstack goes into the queue of deferred
freeing. An AP is run-to-completion (without a timer): `ap_loop` picks a task up, and an exit
returns the core to the loop (`ap_reenter`, a fresh stack). The limitations are in PLAN-SYNC §13.

**libtsync (`kernel/user/tsync/`).** Wait-word (tier0 pause → tier1 umwait/mwaitx →
the futex/upark), a Drepper 3-state mutex, an MCS (a sleep on one's own node, a wake syscall only if the waiter
really fell asleep), a condvar with a seq+waiters. The SPSC v3: the fast path is a static inline in `tsync.h`
(the call boundary in a hot loop = ~4x of the throughput), the layout of the fields by the ownership of the lines,
both sides park on their own doorbell lines (kind2; the partner's progress does not ping the
monitor), the "doorbell" is once and is bound to the target (not to the state — the window 1→0→1 an observer
may miss), a producer's hysteresis (a batch, the default cap/4), an adaptive pre-spin
poll-then-sleep, a kfast detection through the 4601 (on Linux it is a portable protocol with a fence).
The fibers (`fiber.[ch]`): a switch of the callee-saved+the rsp (11 ns), a fiber mutex/cv, a `signal_switch`
handoff. The musl patch (`build-musl.sh`) is tier 1 in the `__wait`/`__timedwait`/`__futexwait`.
The MPSC (v0.53, the actor model — a receiver has many senders): `tsync_mpsc` is a Vyukov
bounded one, a producer publishes a slot by a store of the per-slot seq (it wakes a kernel mwait without a syscall),
a consumer sleeps in a kind1 upark on the seq of ITS OWN slot with a poll-then-sleep pre-spin; at a state==2 the
futex wake is sent only by the producer of the position `c_waitpos`. The application: the tsync dispatcher of
SObjectizer (`services/actors/`, an actor benchmark Linux vs Talaria — the TODO §11).

**The batched virtio notify (`vendor/virtio-drivers`, `fastpath.rs`).** The cell takes the TX ring
off entirely through a `transmit_begin_no_notify` and notifies once (`transmit_notify` →
`prepare_notify_batched`: a wrap-safe `vring_need_event` for the whole batch + an mb; a naive
`avail_idx >= avail_event+1` breaks after 65536 descriptors — the notify disappears forever).
The vhost busy poll (the `POLL_US` in the harness) suppresses the notify entirely through the EVENT_IDX.

The tests: `sync` (+SYNC-MB), `smp2`, `tsync` (10 checks, including a broadcast, the portable branch
kfast=0 and a cap=2), `syncbench`, `futexlat`, `fiber`, `dynsync`, `shmwake`, the fastflood section.

## 7. Syscall ABI (`syscall.rs`)

- `init()` — the MSRs STAR/LSTAR/FMASK, the EFER.SCE; it allocates a 128 MB heap for the brk. The selectors
  the user ones: the CS `0x2B`, the SS `0x23` (see the GDT in `user.rs`).
- The asm stub `syscall_entry` pushes the frame `Frame { nr, a1..a6, rip, rflags, rbx }` and calls
  `rust_syscall(&Frame) -> u64` (the value → the rax). The Linux x86-64 convention:
  the nr is in the rax; the arguments are the rdi, rsi, rdx, r10, r8, r9.
- **At the start of `rust_syscall`:** a `metrics::inc_syscall()`, then the **filter** —
  `filter_blocks(nr)` → `-EPERM(-1)` + `SYS_DENIED++` + `fr::record(SYSDENY)`.
- The full list of the numbers is in the match arms of `rust_syscall` (see USER-GUIDE §11 for
  a summary). The range `1000..1002` is the Talaria-specific one (`fastmap`/nop/tsc_hz).
- An unknown syscall: an `-ENOSYS` (-38); the number is printed ONCE (without flooding the serial line).
- **The filter (A3.2):** `filter_enable(&[nr…])` switches the allowlist on (by default it is OFF),
  `filter_denied()` is the counter. The storage is the `SYS_ALLOW[512]`.
- **The trace:** a `talaria.strace=N` (the cmdline) → a `strace_enable(N)`: the first N syscalls
  are printed as `[strace tK] nr(a1,a2,a3,a4) = ret` — the first means of diagnosing
  "the workload keeps silent/has hung at the start".
- **The dispatch by the fd's type** (`do_read`/`do_write`): 0..2 the stdio(the serial line), 50.. UDP, 100..
  TCP, 200.. epoll, 300.. the TFS files, 500.. the null fds (an eventfd/a pipe/an O_CREAT sink/urandom).
- **The time:** `clock_gettime` — the REALTIME/REALTIME_COARSE/TAI out of `tsc::wall_ns()`
  (the kvmclock MSRs 0x4b564d00/01 at the start + the TSC; the override is `talaria.epoch=`), the rest are
  `mono_ns()`; `gettimeofday`, `time`, `clock_getres`, `nanosleep`/`clock_nanosleep`
  (including the TIMER_ABSTIME) — a sleep through an `sti;hlt` under the scheduler.
- **The threads/the locks:** a `futex` WAIT/WAIT_BITSET with a timeout (a relative/an absolute one)
  — the wait is an `sti;hlt` (a thread does not burn the CPU), the WAKE is a no-op (the waiters re-read the
  word themselves); `sched_yield` = an `sti;hlt`; the affinity/priority/rlimit are stubs with sensible
  values (an RLIMIT_STACK of 32 MB, a NOFILE of 1024).
- **The memory:** an anonymous `mmap` ZEROES the frames it gives out (the free list carries garbage; musl's
  mallocng relies on zeroes); a file mmap is a MAP_PRIVATE (a copy) and a MAP_SHARED
  (v1.1 §8.5: the pages of the ext2 block cache in the user AS, a write-back by the D bit;
  an `-ENODEV` if the window does not lie in a contiguous row of the cache's slots); an `mremap` → an `-ENOMEM`
  (musl falls back to a malloc+a memcpy); the `brk` at a repeated start of the workload is zeroed down to
  the maximum of the previous usage (`HEAP_HIGH`).
- **getrandom** — the RDRAND (the fallback is a TSC mixer); `/dev/urandom|random` → a null fd
  (a read gives an EOF: OpenSSL uses getrandom).
- **exit_group** — it terminates all the tasks with the same CR3 (the process's threads), then under
  the supervisor there is a `supervise::process_exited` (a longjmp 3), otherwise an `sti;hlt` until the summary.

---

## 8. The I/O

### The disk: TFS1 / TFS2 (`fs.rs`)

- TFS1: sector 0 is a `u32 magic=0x31534654("TFS1")`, a `u32 nfiles`, then the records
  `the name[24] + a u32 lba + a u32 len`; the body starts at sector 1, the files are aligned to 512 B.
- **TFS2** (`mkfs2.py`): sector 0 is the `magic=0x32534654("TFS2")`, the `count`, the `dirsec`;
  the records are of 96 B (`the path[84] + the lba + the len + the flags(bit0=a dir)`) one after another from byte 16 through
  the sectors 0..dirsec-1 (the directory is read into a `DIRBUF` of 32 sectors → up to 128 records); the files
  start at the LBA `dirsec`. The format is determined by the magic at `init()` (a repeated `init` is a no-op).
- The lookup by path: a `normalize()` (a leading `/`, `//`, `./`) → a `lookup()` over the DIR, then over the
  **virtual files** (the `VF_NAME`: `etc/resolv.conf`, `etc/hosts`, `proc/self/cmdline`;
  the content is set by a `vf_init_net(dns)` out of `main`). `exists()`, `stat_path()` (a directory →
  an S_IFDIR), `set_exe()/exe_path()` (`/proc/self/exe`), `reset_open()` (a restart of the workload).
- `open` gives out a **separate** descriptor per call (the tables `OF_DIR[64]`,
  `OF_OFF[64]`) — that matters: the services open a file twice (a stream + a stat); an indexing by
  the file's number used to alias them. The DMA read goes through the static `SECTBUF`.
- `read`/`lseek`/`fstat` work through the `dir_of(handle)+OF_OFF`. `read_all(path, buf)`
  is the whole thing (for an execve/an ELF). `readlink_fd` is the `/proc/self/fd/N` (for musl's realpath).

### The network

- **`netsys.rs` (v2)** — the BSD model of sockets over smoltcp: `NS=16` TCP slots (each with its own
  buffers, an rx of 128K/a tx of 64K), `NU=8` UDP ones, `NE=8` epoll instances; the fds: TCP `100+i`, UDP
  `50+i`, epoll `200+i`. A TCP slot: `K::{Free,Unbound,Listen,Backlog,Conn}` —
  a listener holds up to `BACKLOG=8` backlog slots in Listen on its port (an `arm`),
  `accept4` gives out a ready backlog slot as a new fd and re-arms; **`connect`** is
  an active open (a `tcp::Socket::connect` with an ephemeral port 40000..60000): blocking
  (it waits for Established/Closed/a timeout of 10 s) or O_NONBLOCK (an `-EINPROGRESS`, then
  a `refresh_conn` → the readiness EPOLLOUT / the `SO_ERROR`=an ECONNREFUSED). The configuration is
  `configure_from_cmdline` (`talaria.ip/gw/dns`), the default route is in `init`.
  `readiness(fd)` is a single mask (IN/OUT/ERR/HUP/RDHUP) for epoll and for `poll_fds`
  (poll/ppoll, the timeouts by the TSC). The blocking waits are a `yield_window()`
  (`sti; pause; cli` — under the scheduler the other threads get the CPU). The trace is
  `net: connect/established/close rx= tx=` (`set_trace`), `dump_conns()`, `totals()`,
  `reset_all()` (an RST to everyone at a restart of the workload). UDP: a bind(0) → an ephemeral port,
  `connect` remembers the addressee, recvfrom writes the source's sockaddr (musl's resolver
  compares it with the nameserver).
- **`netdev.rs`** — THE MODEL OF A NETWORK DEVICE: the trait `NetDev` (what the kernel needs from
  a card), the binding rules `Bind::{Pci, PciClass, Platform, OnDemand}` and the table of
  drivers `DRIVERS`. `probe_all()` is the search and the bring-up; a `talaria.nic=<name>`
  demands a specific driver (the operator is senior to the automatics). How to add a driver of one's
  own is §10.6. The drivers: `virtnet.rs` (virtio-net over MMIO), `ena.rs` (the ENA on
  PCIe), `nicloop.rs` (the loopback card: the model's gate and a live example).
- **`l1.rs`** — the `NicQ` (slot-oriented rings over ANY `dyn NetDev`:
  the frame buffers, the batching of the doorbells, the receive counters) + the kernel-mode servers:
  `run_httpd` (TCP :80, keep-alive; the `/ctl` route from `ctlplane` is integrated),
  `run` (a UDP echo). The IP is `10.0.0.2/24`.
- **`fastpath.rs`** — the zero-syscall datapath: a cell on an AP (ring 0) bridges virtio ↔ two
  SPSC rings in **shared** (user-accessible) memory; the application on the BSP (ring 3) polls
  the rings **without syscalls**. The rings: a prod/cons + the slots (`HDR+FSLOT`). The latency/the pkts
  go into the `telemetry`. The shared region is given to the application by the syscall `talaria_fastmap(1000)`.
- **`hal.rs`** — the HAL for virtio-drivers; `share()` uses `paging::virt_to_phys`
  (the DMA is correct under a process's non-identity AS).
- **`pci.rs`** — PCI(e) through ECAM (v1.1 §4.3): the base from the ACPI MCFG (UEFI/q35/EC2)
  or `0xE000_0000` (a microvm with `pcie=on`); the full enumeration of bus/dev/func with a recursion
  over the bridges, the table of the devices (`find`/`find_class`), the MSI-X **and MSI** capability
  (`msi_enable`/`msi_readback`/`intr_kind` — a card without MSI-X used to come up
  silently and silently not interrupt). The BARs:
  the firmware's ones we respect, the ones above 4 GB we reassign into the window `0xC000_0000` (the kernel's
  identity map is the first 4 GB), and without firmware we assign them ourselves. The ivshmem-plain (1af4:1110) BAR2 →
  `ivshmem()`. It is switched on by a `talaria.pci=1` (a `talaria.test=pci` is a host↔guest marker).
- **`acpi.rs`** (v1.1 §4.2) — the RSDP (the `start_info.rsdp_paddr` from the uefi-stub/QEMU) →
  the XSDT/RSDT → the MADT (the cores' LAPIC ids; the `nr_cpus`/`apic_id` are the topology for smp) and the MCFG
  (the `ecam_base`). Without an RSDP there are silent fallbacks (the LAPIC ids 1..N).
- **`nvme.rs`** (v1.1 §4.5, §8.1) — a minimal NVMe: an admin+an IO qid=1, polling
  (without interrupts), an Identify NS/Controller (the MDTS). A **PRP list** — up to 128 KB
  in one command (it used to be 4 KB), and a PIPELINE: `rw_many` submits up to QD-1 commands without
  waiting, the doorbell is one per batch, and the completions are collected in a heap (a cheap fsync
  rests on that). `fs.rs` uses it through
  an `enum Blk {Virtio, Nvme}` (virtio-mmio first, the NVMe is the EBS path on EC2); a GPT
  (an mk-ami image) is recognised by the signature at LBA1 — the data comes from the second partition.
- **`ena.rs`** (v1.1 §4.4) — the driver of the EC2 (Nitro) network card, polling-first:
  the admin queue (the AQ/ACQ/AENQ, the phase bits), a pair of IO queues per direction,
  a datapath without interrupts. **Nitro's requirements, found out on a live instance**
  (a violation of any of them -> the CREATE_CQ/CREATE_SQ answer with an UNKNOWN_ERROR without details):
  a `SET_FEATURE(HOST_ATTR_CONFIG)` as the first command after the admin init; the driver's version
  in the host_info of the level of a modern ena (2.x); the MSI-X is on at the function;
  the rings' depth is 512/1024 aligned by the size. The datapath: bit 5 in the ctrl of an
  RX descriptor must NOT be set, the rings' counters are monotonic (the index is the lower bits of
  the depth), the CQ's head is acknowledged by a doorbell into the `cq_head_db` (in v1.4 it turned out
  that a modern `ena_com` does not make that doorbell at all and without it the numbers are the
  same — `talaria.ena=nocqdb`; it is left on by default because once without
  it the reception stalled).
  **§7 (session 3):** the doorbells are batched (the TX's tail goes per pass over the stack, the replenishment of the
  receive ring once every 64 buffers, the CQs' heads once every quarter of the ring, as in `ena_com`);
  the **LLQ** (`talaria.ena=llq`) is a TX SQ with a `placement_policy=DEV`, the descriptor and
  the frame's beginning are written into the BAR2 (a write of 256 B, an inline header of up to 224 B): that
  gave an RTT of 150 -> 86 us, the main lever on Nitro. `talaria.netidle=1` is a sleep on
  the receive ring (before the sleep the card's interrupt and a TSC-deadline timer are armed:
  under Nitro MONITOR/MWAIT are not given to a guest, and there is nobody to wake a HLT).
  **§A1 (session 6): the LLQ and the accelerated mode are on by default.** In the
  `SET_FEATURE(LLQ)` an `accel_mode = DISABLE_META_CACHING |` is now declared
  LIMIT_TX_BURST` (we did not fill that field in at all — in the trace of a live driver
  there is `0x0003` there), a meta descriptor is put into EVERY packet (the FIRST moves onto
  it), and the doorbell into the TX's tail goes no less often than once every `max_tx_burst_size` bytes
  of descriptors (on a c6i.large that is 512 B = 2 writes). Without the declaration of the mode
  the device works along the unaccelerated path — and that was the whole gap against
  Linux: an **RTT of 92 -> 34 us** against Linux's 43 in a Linux -> Linux on the same segment.
  The meta descriptor by itself (`talaria.ena=meta`) does NOT help — it makes things worse;
  what decides is exactly the declaration of the mode.
  The LLQ window is mapped **write-combining** — the same way
  the reference driver maps it (a `devm_ioremap_wc`; its reg BAR is an ordinary
  `ioremap`, that is UC). The writing of the descriptor itself is the same in ours and in the reference one —
  a stream of 32 stores of 8 bytes; the difference comes only from the memory's type, because on
  UC every store goes as a separate PCIe transaction. The price has been measured
  (`talaria.ena=txbench=N`): a transmit of **1 266 664 packets/s on WC against
  188 235 on UC** (789 against 5312 ns per frame, x6.7). WC does NOT affect the LATENCY
  in the process — with the accel_mode a UC window gives the same 34.6 us. The mechanics: in the PAT table
  there is no WC slot at all by default, so `paging::pat_init` burns in
  `PA4 = WC` (on every core — the type is set by the current processor's PAT), while
  `paging::map_mmio_wc` converts the window with a 4K granularity: in the same gigabyte
  and even in the same 2M page there lies the BAR0 with the registers, for which WC
  is contraindicated. The gate is `talaria.test=wc`.
- **`netprof.rs`** (v1.1 §7.1) — a profile of the datapath by stages (`talaria.netprof=N`):
  rx->app / app / app->tx / rx->tx, the price of a pass over the stack and the price of a doorbell, the passes and the
  doorbells per round trip. It is with it that one measures where the time really is: inside Talaria
  a round trip is ~1 us, a write into a BAR is 101 ns.
- **`uefi-stub/`** (a separate crate, v1.1 §4.1) — a PE loader: the EFI memmap → the PVH
  format, the cmdline out of the LoadOptions (the fw_cfg `-append`) or the `\talaria.cfg` (the ESP), the RSDP out of the
  ConfigurationTable, a descent into `pvh_start`. The build is `uefi-stub/build.sh`; the disk image is
  `tools/mk-ami.sh` (a GPT: an ESP + a data partition).
- **`/dev/shm/<name>`** (syscall.rs, the `SHM_*`): shared memory objects with the semantics of musl's POSIX shm
  (an `open("/dev/shm/..")`+an `ftruncate`+an `mmap(MAP_SHARED,fd)`): with an ivshmem all the names go to the
  BAR (the region is remapped by a `protect()` as user+RW+**cacheable** — the GB3 is otherwise UC), otherwise it is
  anonymous identity memory; `stat`/`fstat` give out the size; a `munmap` of an ivshmem is ignored.
  This is the channel of the challenge's harness (a producer/consumer on the host, a sender/receiver in the guest —
  `bench/task-harness/`).

---

## 9. The observability and the operator's side

- **`coredump.rs`** — the core dumps in the `ET_CORE` format; the knob is `talaria.core=<path>[,full]`,
  the triggers: a fatal signal to the workload (`idt::rust_exception`), an exception in the kernel,
  the console's `core [tid]` for a LIVE task. What the format is concerned with is not
  beauty but compatibility: the file is read by SOMEBODY ELSE'S tool, and it has to be checked
  by somebody else's too — in the regression the dump is opened by a real `gdb`. The very first
  check justified that: the `prpsinfo` note had lost four bytes of
  alignment before the `pr_flag`, and readelf reported a corruption of the NEXT
  note. By default into the dump go the registers of all the threads and their stacks, but not the heap:
  in ClickHouse that is 700 MB, and writing it at the moment of a crash means with a high
  probability writing nothing.
- **`obs/`** — the observability as a whole: `top.rs` (`top`/`ps`), `netstat.rs`,
  `kstat.rs`, `perf.rs`, `unwind.rs` (the call stack by the RBP chain), `mod.rs`
  (the formatting of the quantities and the periodic snapshot `talaria.top=`). The module's rule: everything collected CONSTANTLY costs one
  Relaxed add per event that is two orders of magnitude dearer anyway (a context
  switch, a syscall, a fault); everything dearer — the walk over the tables for the RSS, a `perf poke`
  into a dedicated core — is done ONLY at the operator's explicit request, and that is
  said in the command's help. The gates: `talaria.test=obs` (the arithmetic and
  the formatting without a workload) and `talaria.test=obslive` (the numbers after a live
  multithreaded workload — `selftest/observ.rs`).
- **`metrics.rs`** — the counters (Relaxed): the syscalls (in `rust_syscall`), the page_faults and
  the cow_faults (in `rust_exception` at a vec==14), the frame_allocs/frames_given (in
  `alloc_contig`) — and all the same **per task** (`syscalls_of`/`page_faults_of`/
  `frames_of`): the sum answers "is it a lot", but not "who". `snapshot()` is for the enforcement's
  window, `dump()` is for the `stat`.
- **`telemetry.rs`** — the datapath: `record_lat(cycles)` (the pkts, the min/max/sum, a log2
  histogram → a `pctl(p)`), `inc_gap` (an overflow of the RX ring), `inc_restart` (a supervise/
  a watchdog), `dump()` for the `telem` and for the cell's periodic serial channel.
- **`watchdog.rs`** — `arm(progress)`, `tick(progress) -> bool`: there is no progress
  `STALL_LIMIT` times → a stall→a restart (the telemetry+the flight recorder). It is ready to be called from
  a timer/a health loop.
- **`fr.rs`** — the ring of the events (BOOT/PAGING/MODE/USER/SYSINIT/FAULT/RESTART/MPROTECT/
  NOTE/SYSDENY); it is printed in the crash dump and by a `log`.
- **`console.rs`** — the operator's console: the table of the commands `CMDS` (the name, the arguments,
  a one-line help, the group) + the dispatcher `exec` + the LINE EDITOR `feed(byte)`
  (the history, the arrows/Home/End/Delete, Ctrl-A/B/D/E/F/K/L/U/W, the Tab completion of the commands
  and of the paths). Two contexts: `run()` is the interactive loop on the BSP (everything is available),
  `poll_tick()` is a non-blocking read of COM1 from the scheduler's tick, that is FROM THE BODY
  OF AN INTERRUPT HANDLER: there the commands that wait for a key (`less`, `edit`) are bound to
  refuse (the flag `IN_TICK`), otherwise the kernel would lose control rather than a cycle.
- **`shell.rs`** — the file part of the operator's side IN THE KERNEL (not busybox: that is a workload in
  ring 3, while the console is needed exactly when there is no ring 3). The current directory, the parsing of
  the paths (`..` does not go beyond the root), an `ls -l` with columns and sorting, a `cat`,
  a `less` (the pagination), a `head`/`tail`, an `hd`, a `stat`, an `mkdir`/`rm -r`/`cp`/`mv`/
  `touch`/`write`/`append`, a `df`/`sync`, the completion of the paths. A unified access to a file
  (`fopen/fread/fseek/fclose`) hides the difference between ext2 (RW) and TFS (RO).
- **`edit.rs`** — the console's editor, a subset of vi. The buffer is static; a file
  that did NOT FIT into it is opened read-only — to write
  the truncated thing over the original is worse than not to open it.
- **`filed.rs`** — the uploading/downloading of files over TCP. The delta is like rsync's without
  a rolling window: a `STAT` (the size+the file's CRC32) cuts off the matching files entirely,
  a `SUMS` (the CRC32 per block) + a `PATCH` + a `TRUNC` are only the differences. The service is a
  state machine, so a transfer of 200 KB does not "freeze" the console.
  The client is `kernel/tools/talaria-file`.
- **`kernel/tools/talaria-prof`** — the parsing of a profile ON THE HOST: it takes the output of
  `perf record` (the lines `SAMPLE <rip> <ring> <tid> <cpu> <the name>`) straight out of the log,
  together with the surrounding text, and turns the addresses into names through `addr2line`
  (in ONE call for the whole batch — starting a process per address turns
  the parsing of a thousand samples into a minute). The modes: `report` (a table by function +
  a breakdown by thread and by core), `folded` (the input for a flame graph), `raw` (without
  the symbols). The symbols live on the host together with the binaries, and there is no point in dragging an ELF parser with
  DWARF into the kernel for the sake of a line that is printed once an hour.
- **`netcfg.rs`** — the network's configuration: the routes (the longest prefix; a connected network is
  stronger than a gateway), the rules from the cmdline (`talaria.net=`, `talaria.route=`; an interface
  may be named by its MAC), the commands `ip`/`route`/`dhcp`/`dns`. The operator is senior to
  the automatics: DHCP removes only ITS OWN routes.
- **`ctlplane.rs`** — auth `/ctl`: `is_ctl(req)`, `authorized(req)` (query `token=`
  or an `Authorization: Bearer`), `build_response` (a 200+the `body()` / a 401). The token
  is set ONLY from the command line (`talaria.ctltoken=`,
  `configure_from_cmdline`); there is no default value — if it is not set, the route is closed.
  The token's comparison goes through `crypto::ct_eq` (constant time). The gate is
  `talaria.test=ctlauth` (`selftest()`).
- **`supervise.rs`** — `supervise(path, argv, max_restarts)`: setjmp (`RECOVERY`),
  the start of the workload; a ring 3 exception → a recovery longjmp → an auto-restart; on the exhaustion of
  the limit there is a halt. **`supervise_threaded(path, argv, envp, max_restarts)`** is
  a multithreaded workload: the longjmp codes 1=a fault, 2=all the threads have finished, 3=an exit/
  an exit_group (`process_exited`; a code 0 → a halt without a restart), 4=the watchdog/the operator;
  between the attempts there are a `cli`, a `lapic_timer_stop`, a `syscall::reset_process_state()`
  (the null fds, the fs, the netsys::reset_all), a `sched::reset()`. `sched_tick()` is the housekeeping.
  `cellman.rs` is the manifest `/cell.manifest`: `binary= args= env= restart= watchdog=
  core=` (up to 48 arguments, 16 envs) + the layout rules `pin= spread= balance=
  numa=` (up to 12 rules), which `affinity.rs` applies.
- **`pmu.rs`** — the hardware counters through MSRs + `rdpmc`, **on every core**
  (`init_cpu` is called at the bring-up of every AP: the counters' MSRs are per-core, and
  the former single initialisation on the BSP meant zeroes exactly where the workload
  lives). The counters' layout is the RESULT OF A CHECK rather than the CPUID: KVM
  declares three fixed counters and an arch-perfmon v2, while `rdpmc` on them
  gives out zero, so `init_cpu` sets them up, spins a short loop and looks at whether
  they have moved; they have not — the cycles and the instructions move over onto GP ones, and what there
  were not enough slots for is declared absent. The per-core snapshots are collected by the scheduler's
  tick (`note_tick`), while a dedicated core (there is no tick) is taken by an explicit
  `poke` through an IPI.
- **`gdb.rs`** — the GDB RSP over COM2 (`-M pc` is needed); it is switched on by a `talaria.mode=gdb`
  or by the command `gdb on` on a live machine.

---

## 10. Step-by-step recipes for improvements

### 10.0. Add a command to the operator's console

There are three places, and nowhere else:

1. a line in the `CMDS` (`console.rs`) — the name, the arguments, a one-line help, the group
   (`G_SYS`/`G_FILE`/`G_NET`/`G_DIAG`). Both `help` and the Tab take it from there;
2. a branch in `exec` — the parsing of the `rest` and the call;
3. a line in the `selftest()` (the gate `talaria.test=console`), if the command decides something
   rather than prints.

A command that WAITS FOR A KEY is bound to begin with a `need_interactive("the name")`:
`poll_tick` executes the commands from the scheduler's tick, and one must not block there.
`top` is arranged differently — it can do BOTH modes: `cmd(arg, interactive)` in a
non-interactive context prints one snapshot instead of a loop waiting for a key, while
the flag `-1` makes it do the same in an interactive one too (the regression's set and the log of a machine
without an operator need that).

### 10.0b. Debug what has crashed (one's own and somebody else's)

| what crashed | what to look at it with |
|---|---|
| the workload | the stack is already in the log (`the stack workload (N frame(s))`); the dump is `talaria.core=/core.elf`, then `gdb ./app ./core` |
| the workload HAS HUNG | a `core [tid]` in the console: a snapshot of a live task, the machine does not stop; `top -t` shows the threads' states |
| the kernel | the crash dump is always printed; a `talaria.core=` gives an `ET_CORE` readable by `gdb kernel/target/.../talaria ./core` |
| names are needed in the kernel's stack | rebuild with `kernel/build-prof.sh` (frame pointers, ~4 % on a syscall) |
| a suspicion of a stale TLB | a `talaria.sigtrace=N`: a signal is printed together with what the kernel sees in the tables at the fault's address |

To take a file off a live node there is `kernel/tools/talaria-file <ip> pull /core.elf ./core`.

### 10.0a. Add a quantity to the observability

There is one rule: **a counter is started where it can be taken for free**, while a quantity
is computed where it has been asked for.

1. Does the event already happen (a context switch, a syscall, a fault, a call of
   the allocator)? Then one `fetch_add(Relaxed)` at that point is a price that
   will not be noticed. That is how the time on the processor, the switches, the syscalls and the frames per
   task are done.
2. Does the event NOT happen by itself (the RSS, a dedicated core's counters)? Then compute it
   on request, at the moment of printing, and write in the command's help how much that costs.
   Starting a counter on the hot path for that is not allowed: a walk over the page tables once
   a second is cheaper than an increment on every fault.
3. A derived quantity (per second, a share, the %CPU) is computed ONLY from the difference of two
   snapshots — the `obs::top::Prev`. The first call has no window: then the window taken is
   the whole uptime, and that is an honest answer to "how much since the boot" rather than a division by
   a conventional unit.
4. The formatting goes through `obs::bytes/rate/brate/pct/dur`, and they have a gate
   (`talaria.test=obs`): a mistake by an order of magnitude here does not crash, it lies by the scale.
5. A number is printed next to what it is compared with. "12000 misses" is not
   a fact; "12000 misses out of 3.4 million (0.35%)" is a fact.

### 10.1. Add a new syscall

1. In `rust_syscall` (`syscall.rs`) add a match arm by the Linux x86-64 number.
2. The arguments are the `f.a1..f.a6`; return the value as a `u64` (for an error it is `(-errno) as u64`).
3. If a resource is needed (an fd/a socket) — use the `fs`/`netsys`; for memory it is `mem`+`paging`.
4. The test: build/take a binary that calls it, add a mode (§10.2) and an assert in
   `test.sh` (§10.5).

### 10.2. Add a mode (`talaria.test`/`talaria.mode`)

The gate is an arm of the `match` in the file of `kernel/src/selftest/` that fits by theme
(`platform` / `storage` / `memory` / `process` / `concur` / `netsvc`):

```rust
"mydemo" => {
    println!("talaria: my demo");
    // ... the logic; to start a binary there is crate::user::run(ELF)
    //     or crate::user::run_disk(path, args)
    crate::suite::park()   // if the mode is terminal
}
```

A production mode (`talaria.mode=`) is the same, but in `kernel/src/runmode.rs::simple`.

**The name is compared AS A WHOLE, and that is not a matter of style.** The mode used to be chosen by a
chain of `if cmdline.contains("talaria.test=<name>")`, and a name that turned out to be
a prefix of another started SOMEBODY ELSE'S mode: with a `talaria.test=fsbig` the block
`fs` was executed too. The class affected 11 names out of 106, and two had to get manual
patches `&& !contains(...)`. In a `match` on the value of the `talaria.test=` such a
collision is impossible by construction — do not bring `contains` back even "for a minute".
A typo in a name is now printed into the log rather than taking the run into the default
mode.

A built-in ELF is `static E: &[u8] = include_bytes!("../../user/mybin");` (from
`selftest/` to `kernel/user/` there are two levels). From the disk it is
`crate::fs::init()` + `crate::user::run_disk(b"/mybin\0".as_ptr(), &[b"mybin", b"arg"])`.

### 10.3. Add an open-source service

Copy the structure of `services/<svc>/`: a `README.md` (the recipe of a musl build at
`0x10000000`), the disk is `kernel/tools/talaria-pack` (TFS1/TFS2/ext2 + a manifest),
a `run.sh` (docker+QEMU+the probes). To run it a kernel mode is NOT needed — there is `talaria.mode=cell`
(threads) or `proc` (fork/dynamics) by the `/cell.manifest`; a mode of one's own (§10.2) is only
needed if a non-standard wrapping is required. Enter an assert marker into `validate.sh` (B6).
Sources with an incompatible licence (GPL) must not be vendored — a fetch script
(an example: mongoose/fetch.sh).

### 10.4. Build a user binary

- **An asm test:** `docker run --rm -v "$PWD":/w talaria-qemu2 bash -c 'cd /w/kernel/user
  && as x.s -o x.o && ld x.o -o x -Ttext=0x10000000'` (the entry is 0x10000000).
- **C/C++:** `musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o app app.c`
  (the image `talaria-musl`; for C++ it is the g++ from `talaria-alpine-build`). Check the entry:
  `objdump -f app | grep 'start address'`. All the regression's test binaries are built by
  `make -C kernel/user all` (see user/README.md); the compatibility of somebody else's binary is
  `kernel/tools/talaria-check`.

### 10.5. Add a test to the regression

`test.sh` is a list of specs `"talaria.test=MODE|REGEX|The name"`; for the network ones there are separate
sections with a `boot`/`socat`. `validate.sh` holds the heavy gates (open source/latency/soak/fuzz).
Keep `test.sh` green (FAIL=0; the counter is in the README) after every item.

### 10.6. Add a network card driver

The full list of the motions is three items, and not one of them touches the datapath,
the stack or the cell.

**1. The file `kernel/src/<name>.rs` — the driver's structure and an `impl NetDev`.**

`NetDev` (`netdev.rs`) has twelve methods, five of them with a default, that is,
seven are mandatory:

| The method | What it does | Mandatory |
|---|---|---|
| `name` | the driver's name in the log | yes |
| `mac` | the card's MAC | yes |
| `rx_post(slot, addr)` | lay a buffer out into the receive ring, WITHOUT a doorbell | yes |
| `rx_poll` | a received frame: `(the slot, the payload's offset, the length)` | yes |
| `tx_alloc(ntx)` | a free transmit slot | yes |
| `tx_post(slot, addr, n)` | put a frame, WITHOUT a doorbell | yes |
| `tx_reap` | free the completed slots | yes |
| `flush` | ring about what has accumulated over the pass; `true` — the doorbell went out | yes |
| `rx_capacity(want)` | how many buffers the card will take | no (by default `want`) |
| `tx_hdr(slot, addr)` | the card's header before the frame → the payload's offset | no (0) |
| `arm_intr` | allow the interrupt before a sleep | no |
| `idle_watch` | the word to sleep on (an `mwait`) | no |
| `doorbells` | the counter of the MMIO doorbells (diagnostics) | no |

**A slot is the index of a frame's buffer** owned by the `NicQ`. How the driver
maps a slot onto its own descriptor is its own business and **its own memory**: virtio
gives out a token and keeps the table `a token -> a slot` at its own place (`virtnet.rs`), while the ENA
takes the slot's number directly. These tables used to live in the common `NicQ`, that is,
on the cell kernel's stack — and they had to be squeezed into `u16` so that the stack did not
overflow; now they are not there.

The frame buffers are in the kernel's `.bss`, identity, `phys == virt`, so into `rx_post` /
`tx_post` comes a physical address fit for a DMA as it is.

**2. In the same file — a `pub static DRIVER: Driver`.**

```rust
pub static DRIVER: Driver = Driver {
    name: "mynic",
    bind: &[Bind::Pci(0x8086, 0x10fb)],   // or a PciClass / Platform / OnDemand
    probe,
};
```

The binding rules:

- `Bind::Pci(vendor, device)` is a specific pair;
- `Bind::PciClass(class, mask)` is by the function's class (`0x020000`/`0xFFFF00` is
  any Ethernet controller);
- `Bind::Platform` — the device is declared by the platform, and there may be no PCI bus
  at all (that is how virtio-mmio lives under a microvm);
- `Bind::OnDemand` — only at an explicit `talaria.nic=<name>`, it is not picked up by itself.

`probe(Option<&PciDev>)` brings the card up and returns a reference to **its own
static slot**: the card lives until the reboot and travels through the code by reference rather than
as a copy (§1.1 — otherwise an AP's stack would have to be kept at 128 KB).

**3. A `mod <name>;` in `main.rs` and one line in the `netdev::DRIVERS`.** The order in
the table = the priority of binding.

Nowhere else. `NicQ`, `SmolDev`, `netsys` and `fastpath::run_cell` know nothing
about the cards.

**The interrupts.** They are not the driver's business: after the device has come up call
`affinity::route_device_irqs(&pcidev, "the name")` — it will take the MSI-X itself if there is
one, otherwise the MSI (with a read-back), while a card with INTx alone it will name out loud:
the kernel does not wire INTx up, and there will be no interrupts from such a card. Where exactly
to direct them the layout decides (the `irq=` in the manifest); a dedicated core is not given
interrupts.

**The check.** `talaria.test=nicdev` is the gate of the model itself: the binding rules on
synthetic devices (including those that stand in the dev machine and do not exist under
QEMU), the classification of the interrupts, the bring-up of the loopback card **through the table** and a run of the
datapath over it with a comparison of the bytes. `nicloop.rs` (≈130 lines) is at the same time a
live example of a driver connected by exactly those three items.
The console's `nics` shows the table and what it sees in the machine.

### 10.6b. A console command / a telemetry field / a manifest field

- A command: an arm in the `console.rs::exec` + a line in the `help`.
- The telemetry: an atomic field in `telemetry.rs` + the output in the `dump()` (and, if wished, in
  `ctlplane::body`).
- The manifest: a field in the `CellManifest` + a key in the `cellman::parse` + a check in the `selftest`.

---

## 11. The testing

### 11.0. The host unit tests (`kernel-unit/`)

The kernel is `no_std` on its own target, so its modules cannot be tested in
place. The ones whose logic does not touch the hardware are compiled a second
time for the host, straight from `kernel/src` through `#[path]`, and the tests
live next to the code:

```sh
cd kernel-unit && cargo test && cargo clippy --all-targets -- -D warnings
```

**The rule for new kernel code:** logic that can work without the machine lives
in a module wired into `kernel-unit` and comes with tests. The kernel's glue
inside such a module (the page tables, the port I/O, the global state) goes
behind `#[cfg(not(talaria_host))]` — `build.rs` sets that cfg for this crate
only.

What is covered this way: the syscalls' pointers (`uaccess`), the knobs
(`cmdline`), the manifest (`cellman`), the layout of the threads (`placement`),
the clock's servo (`clock`), AES-GCM (`crypto`), the routes (`routes`), TFS
(`tfs`), and the FS's and the loader's formats — the htree hash (`ext2hash`), a
directory's records (`ext2dir`) and its index (`ext2dx`), the bitmaps
(`ext2bmap`), the superblock (`ext2sb`), the JBD2 log with its replay (`jbd2`),
the ELF headers (`elfhdr`) and the notes of a core dump (`elfnote`).

And the DECISIONS, which have no bytes of their own but the same property: the
choice of the next thread on a core with the policy `reqfirst` (`rqpick`), the
rule "a steal must improve the balance" (`placement`), the predicates of a wait
on a word (`waitpred`), and the zero-syscall rings — their layout, the index
arithmetic that must survive the wrap of a 32-bit counter, and the ARP the cell
answers by itself (`fastring`).

Why exactly these: they are the places where a mistake does not crash. A
`rec_len` off by four gives a directory that is still self-consistent but in
which a file is not found; a bit taken twice in the bitmap gives two files
sharing a block; a lost byte of padding in a note shifts the parsing of ALL the
following ones. A replay that walks one transaction too far puts a former
epoch's blocks over live metadata, and it is seen only after a power cut. A
steal that mirrors the skew instead of removing it seats two active threads on
one core for good; a wait predicate that compares two indices instead of their
difference loses a wakeup at the counter's wrap — once in two billion steps, and
then the thread stands for ever. None of that shows up in a counter or in a
boot — but all of it is decided by arithmetic, which a test judges in a
millisecond.

### 11.1. The regression/the validation

- `kernel/test.sh` — all the modes (the docker image `talaria-qemu2`, a tap for the network).
  The current count of the asserts and of the host unit tests is in the `README.md`
  (the single source); here the numbers are deliberately not duplicated, they go
  stale. Which gates SKIP on a fresh clone, and why, is in `CONTRIBUTING.md`. A `TAL_NOBIOS=1` skips
  the longest block (`bios:` — the assembly of a hybrid image on the host).
- `kernel/validate.sh` — the open-source compat, the latency SLA, a soak, fuzzing, the DBMSes (B10),
  the JVM (B11), ClickHouse (B12); parts of it are SKIPped without the artefacts.

### 11.2. A run of one mode (quickly, for an iteration)

```sh
cd "$(git rev-parse --show-toplevel)"
K=kernel/target/x86_64-talaria/release/talaria
timeout 40 docker run --rm -i --device /dev/kvm -v "$PWD":/w talaria-qemu2 bash -s "$K" <<'INNER'
set -u; K=$1
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=MY_MODE" -display none \
  -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
QP=$!; sleep 5; kill $QP 2>/dev/null; grep -iE 'THE_NEEDED_LINE' /tmp/o.log
INNER
```

For a disk add a `-drive …virtio-blk-device`; for the network a tap + `-smp 2` +
`--device /dev/net/tun --device /dev/vhost-net --cap-add NET_ADMIN`.

### 11.3. Iterations on a live EC2 (`infra/aws/`)

The cycle "assemble an image -> a snapshot -> an AMI -> an instance" costs 10–15 minutes, so
it is done ONCE, while the hypotheses are driven by a launch of an instance (~2 minutes) with
a tail of the cmdline through the user-data (`talaria.userdata=1`, see USER-GUIDE §7.1).

| The script | What it does |
|---|---|
| `lab-up.sh [the type] [the directory]` | bring the stand up: a builder with an empty volume + a Linux neighbour in one cluster placement group, and build `udp_ping`/`tcp_ping`/`udp_echo` on the neighbour; everything with the tag `Project=talaria-v11` |
| `lab-down.sh` | take the whole stand down by the tag (the instances, the AMIs, the snapshots, the key) |
| `ec2-boot-talaria.sh "<the cmdline>" [the type] [data.img]` | assemble the image, upload it onto the builder, a snapshot, a `register-image`, a launch. `HYBRID=1` is a hybrid image (BIOS+UEFI), `HYBRID=bios` is a pure MBR, `BOOTMODE=uefi\|uefi-preferred\|legacy-bios` |
| `ec2-run.sh "<the cmdline's tail>" [the type] [the ami]` | launch the NEXT experiment on a ready AMI through the user-data; it waits for the serial output and puts the instance out (`KEEP=1` leaves it) |
| `ena-rtt.sh "<the tail>" [the type] [N]` | a measurement of the RTT "the neighbour -> Talaria" in one command (the kernel echo + a `udp_ping` from the neighbour); at a `talaria.ena=txbench=N` it takes the transmit run straight out of the console |
| `ena-matrix.sh [the type] [N] [the repeats]` | a SERIES of configurations (`CONFIGS=` is one's own list): the base one and one difference from it at a time. The spread of the p50 between launches of instances is ~10 us, comparable to the effect sought, so one has to compare series rather than separate runs |
| `ena-load.sh "<the tail>" [the type] [N] [pps]` | a udp_ping in silence and UNDER a background (`bench/udp_flood.c` on the neighbour) on ONE instance — that way the spread of the placement does not get in the way |
| `ec2-crash-test.sh "<phase 1>" "<phase 2>"` | crash consistency on a live EBS: the role of "the pulled-out power" is played by a `stop-instances --force`, and the second phase starts on the same volume with another user-data |
| `validate-remote.sh <the type>` | bring a *.metal up with terraform, upload the tree, build the kernel, run `test.sh` (+`FULL=1` — the workloads' images, the DBMS clients and `validate.sh`) |

The pitfalls of this path (each of them cost half an hour or more) are collected in the header of `docs/TODO.md`,
the sections "The pitfalls of session …": the decoding of the `get-console-output`, the base64 in the
`modify-instance-attribute`, the limited serial buffer, the vCPU quota, and also two
fresh ones — an AMI is bound to be assembled with the canonical cmdline
`console=ttyS0 talaria.pci=1 talaria.ip=dhcp talaria.userdata=1` (otherwise the PCI does not
come up and the kernel honestly writes "no NIC was found"), and `ssh` reads the stdin
entirely, because of which an ordinary `while read` around any of these scripts loses
the whole rest of the list after the first iteration.

### 11.4. The diagnostics on bare metal (`*.metal`): there is nothing to measure with but the disk

On a real machine the kernel has NO output channel. A `get-console-output` on a metal is
a snapshot of the firmware's screen: a raw COM1 does not get there, an `int 10h` from sector 0 does not
get there even if one holds the line for 8 seconds, and a direct write into the 0xB8000 does not get there
either. The only instrument that always works there is what the kernel has itself
written onto the DISK: the root volume survives the instance and is read by a helper.

**The tools (all of them are v1.7, session 9):**

| What | How | What for |
|---|---|---|
| `talaria.trace=<LBA>` | the kernel's whole output accumulates from the first line in a buffer of 64 KB and goes out as raw sectors: the header `TALARIA-TRACE len=… flushes=…` at the `LBA`, the text from the `LBA+1`. The flush happens at every phase mark and by itself every 512 B | to see HOW FAR the kernel got |
| `talaria.suite=<LBA>` + `mk-tboot.sh SUITE="a mode;a mode;…"` | a list of cmdline tails and a counter on the disk; the kernel takes its own item, writes the log into its own slot (`trace_lba + (i+1)*129`) and reboots the machine for the next one | one launch of an instance is the whole list of tests; the counter grows BEFORE the run, so one sees what it hung on, and the continuation goes from the same item |
| `talaria.rebootat=<mb2\|idt\|lapic\|mem\|paging>` | on reaching that point the kernel reboots the machine | a signal in the zone where there is no disk yet |
| the boot counter in the bootloader's trace | stage2 reads its own trace sector and puts an incremented number there | a hang and an eternal reboot loop on a metal look THE SAME IN THE CONSOLE (the same banner); only this counter tells them apart |
| the dump of the MBI/E820 by the bootloader | stage2 puts the assembled multiboot2 structure into the sectors before its own trace | to find out the machine's memory map without having a single line from the kernel |
| `infra/aws/read-disk-trace.sh <instance-id\|volume-id> [the log's LBA] [the bootloader's LBA]` | it puts the instance out, attaches the volume to a helper and prints the bootloader's trace, the kernel's log and (at a `SLOTS=n`) all the set's slots | to read all that |

**The rule that cost three sessions: before measuring, check that the
instrument is empty.** The upload of the image onto an EBS volume goes WITHOUT a `conv=sparse`, while
`metal-ref-probe.sh` before launching an instance reads the trace's sectors back and
prints "non-zero bytes N (we expect 0)". The reason: on an account with EBS encryption by
default an unwritten block reads as pseudo-random garbage rather than as a zero — and
`conv=sparse` wrote neither the zero areas of the trace nor the 23 MB of `.bss` inside the image
of the kernel (in our ELF the `.bss` lies INSIDE the `p_filesz`). The kernel started with statics
full of garbage, while "there is no trace" meant "there was nowhere to write".

**A cheap check of everything except the hardware itself:** the same image on a `c6i.large`
(~$0.1 against ~$2 per run on a metal). There the BIOS path works and THE CONSOLE IS VISIBLE,
so most of the mistakes are caught before one pays for them at the metal's price.

The order of the price on a metal: one probe is ~15 minutes / ~$2; every item of a set is a full
reboot (the POST on a machine with 256 GB takes minutes), and one launch of an instance
gets through 3–6 items.

---

## 12. The invariants and the pitfalls (read before editing)

1. **The CR3 switch on a shared stack.** In the scheduler the CR3 is switched in asm **after**
   the `mov rax, rsp`, because the stack lives in the common GB0. execve does a `mov cr3 + iretq`
   as a single asm without accessing locals after the change of the CR3.
2. **The CR0.WP is off.** Switching the WP on breaks the ring 0 writes of the syscall stub into a COW-RO stack
   after a fork (a syscall executes on the user stack). The right solution (a separate
   kernel stack for a syscall) is future work; for now **do not switch the WP on**.
3. **The identity DMA.** The kernel accesses physical memory by its identity address. Therefore:
   - a `guard()` of a frame that is then reused for a table/a DMA would give a #PF —
     `alloc_contig` calls `unguard` when giving out of the free list (do not delete it!).
   - the virtio HAL's `share()` goes through `virt_to_phys` (a DMA under a process's private AS).
4. **A munmap = a guard + a free.** The guard clears present (it catches a UAF before the reuse),
   the free returns the frame; at a reuse there is an unguard. Do not break that bundle.
5. **musl's linking address `0x10000000`.** By default musl links at `0x400000`
   (the kernel's area) — a `-Wl,-Ttext-segment=0x10000000` is mandatory.
6. **ASCII in asm/in the critical strings.** The `b"..."` literals for
   the paths/the asserts are ASCII.
7. **The environment:** docker with QEMU goes from the repository's root; `grep` misbehaves on some files
   (use `awk`/`sed`); the tap network is flaky (repeat it); the first TCP connection after a
   boot is more reliable (the socket's re-listen in `run_httpd` races — the test sends the requests in one
   keep-alive connection right after the boot).
8. **The fd table is not thread-safe** (it is designed for 1 datapath core).
9. **Interrupts inside syscalls.** A syscall enters with IF=0 (the FMASK). Any wait in
   the kernel is bound to give the CPU up in `sti … cli` windows (a `yield_window`, an `sti;hlt` in a futex/
   a sleep/a wait4), and only when NO network/file state is held: another
   thread may enter the same syscall in the window. The timer ISR (`rust_schedule`) may
   call a `sched_tick` (the console/the status/the watchdog) — it only reads the netsys.
10. **An anonymous mmap gives zeroes.** The frames from the free list carry garbage; musl's mallocng breaks
    on non-zero memory (an `a_crash` → a #GP in the `__libc_free`). `mmap` zeroes them; the `brk`
    is zeroed at a restart of the workload. Do not "optimise" that.
11. **getrandom(buf, len, flags)** — the order of the arguments (buf=a1, len=a2); at a return of
    0 for the len `std::random_device` spins forever.
12. **The workload's identity image is reserved** in `mem::reserve` (load_elf_from_disk):
    at a RAM > 768 MB the bump allocator would otherwise reach `0x30000000` and overwrite the code of a workload linked at that address.
13. **kvmclock** under a `-M microvm,rtc=off` is the only source of the wall clock (there is no RTC);
    Binance rejects signed requests at a divergence > the recvWindow — check the
    `wall:` in the `talaria.test=connect`.
14. **A virtio TX kick = a VMEXIT (~3–11 us) on EVERY `transmit_begin`** (virtio-drivers
    notifies on every add if vhost is not in a busy poll). A zero-syscall cell in QEMU
    runs into that rather than into the syscalls: a `cell-diag: avg_transmit_begin≈11µs` was measured under
    HT contention (the task harness, IO=talaria). The cure is a batched notification (vendoring
    virtio-drivers: an add without a notify + one notify per batch) or a real NIC (Track A).
15. **The pinning on a 4C/8T host:** the HT pairs are (0,4)(1,5)(2,6)(3,7); the spinners (the vCPU, the producer,
    the consumer) go on different physical cores; the vhost kthreads (`comm vhost-<pid>`, the cmdline is empty —
    `pgrep -f` does not see them) are pinned through /proc; otherwise vhost shares a core with a busy-poll vCPU
    (the CFS 50/50, slices of 4 ms → tails in milliseconds and drops).
16. **A device's limitation is checked on ALL the paths rather than on "the main" one.**
    `nvme::rw_buf` cut the request by the MDTS, while `nvme::rw_many` did not. While the files
    lay scattered, the ranges were short and that did not surface; as soon as the
    allocation of the blocks became runs (v1.2 §2.4), the very first fsync of 512 blocks went out as
    ONE command of 2 MB and was rejected — the data was silently not written. Under QEMU it
    reproduces the same way as on EBS if the file is contiguous.
17. **The `pvh_start` entry is bound to coincide with the segment's `p_paddr`** (the uefi-stub relies on
    that) — everything new in the `.text.boot` is put AFTER it. And: a section without
    references is thrown away by the linker's garbage collector, so the multiboot2 header
    in the `.mb2` lives only under a `KEEP()` in the `linker.ld`.
18. **The window of the ext2 preallocation is returned into the bitmap BEFORE it is written to the disk.**
    `prea_release` is called from `flush_all`/`jrn_commit`/`close`/`truncate`/
    `unlink`; otherwise blocks marked occupied but belonging to nobody would go out onto the disk, and
    `e2fsck` would see that. Any new path leading to a write of the bitmap is bound to
    flush the windows.
19. **The ext2 block cache: an evictable way must always be left in a set.**
    A file mapping pins the slots (the `CPIN`), so `mmap_prepare` counts the set's
    pins and refuses to occupy the last free way — otherwise there would be nowhere to put a `cblk`
    for a miss. The cache's layout is PER WAY (`way*NSETS + bn%NSETS`)
    exactly for the sake of the coalescing of §8.2: a per-set one would have spread the neighbouring blocks over the memory
    and the gluing of the writes would have disappeared.
20. **Static storages do not survive a repeated initialisation by themselves.**
    smoltcp's socket storage is static: `netsys::teardown_keep_lease` is bound to
    clear it, otherwise the second `SocketSet` panics with "adding a socket to a full
    SocketSet". The same class: the options parsed BEFORE the merge of the user-data
    (`talaria.kecho=` and the rest of the network ones) have to be re-read after the merge.
21. **The ENA's accelerated LLQ mode is a CONTRACT rather than a performance flag.**
    Having declared in the `SET_FEATURE(LLQ)` an `accel_mode = DISABLE_META_CACHING |
    LIMIT_TX_BURST`, the driver is bound to fulfil both requirements: a meta descriptor in
    EVERY packet (and the FIRST moves from the data descriptor onto it) and a doorbell into the
    TX's tail no less often than once every `max_tx_burst_size` bytes of entries. A violation of
    the first one — the device will not understand the packet; of the second — `ena_com` at that place
    returns an ENOSPC, that is, the frame is lost. And the other way round: NOT to declare the mode
    (as it was before v1.4) means silently to get a twice worse latency and a four times
    lower throughput without breaking anything in the process. The price of a mistake in
    either direction is not visible in a single local test: under QEMU there is no card.
22. **A page's caching type is set by the PAT table of the CURRENT core.**
    `paging::pat_init` (the slot `PA4 = WC` for the LLQ window) is bound to be executed on
    the BSP and on every AP — otherwise one and the same page would be write-combining on
    one core and write-back on another, while the datapath in a `mode=fast` lives exactly on
    an AP. For the same reason the WC window is converted with a 4K granularity: in the ENA the BAR0 with
    the registers (for which WC is contraindicated) lies in the same 2M page as
    the BAR2 with the LLQ ring.
23. **The `.bss` is zeroed by the KERNEL ITSELF.** No bootloader is obliged to fill NOBITS
    with zeroes (our tboot reads exactly the `p_filesz` off the disk), while on real hardware
    the memory before us contains garbage. Under QEMU it is zeroed — which means that locally this
    class of errors is COMPLETELY invisible: the kernel simply works. The bounds are given by
    `linker.ld` (the `__bss_start`/`__bss_end`), and the zeroing is `ZERO_BSS` in both
    32-bit entries, BEFORE the first access to any static.
24. **An overflow of the device table is bound to be LOUD.** `pci::MAX_DEV`
    lived with 32 records through three EC2 generations and died on the first real machine: a
    two-socket Xeon has 52 bridges and 307 functions (the uncore/PCU), and the table ran out
    BEFORE the walk reached the NVMe. From the outside that looked like "the kernel keeps silent":
    there is no disk — which means there is neither an FS, nor a trace, nor a line. Now there are 512 places and an explicit
    warning at an overflow. The same class: any fixed array
    into which FOUND entities are put is bound to complain when it fills up.
25. **"The first controller that turns up" is an assumption rather than a fact.** On a Nitro metal
    every EBS volume is given out by a SEPARATE NVMe controller; the boot one is
    determined by the signature of our bootloader (`TBOT`) in sector 0
    (`fs::open_nvme_boot`) rather than by the order of the buses. Otherwise the kernel diligently writes the trace onto
    a foreign disk, and from the outside that is indistinguishable from silence.
26. **The cores' APIC ids are taken from the MADT rather than counted in a row.** On Nitro guests and
    under QEMU the second core always has the id 1; on a `c6i.metal` the MADT gives out 0, 2, 4, …
    (with the HT twins between them), and a `start_ap(1)` sends an INIT-SIPI into the void. The entry
    point is `smp::first_ap()` (the fallback is 1 if the ACPI was not parsed).
27. **A wait for an external signal in the kernel is bounded in time.** COM1 (the bit
    THRE) and the calibration by the i8254 on bare metal may NEVER answer: the port
    is held by the firmware, and there may be no legacy timer at all. An unconditional `while`
    there hangs the kernel on the very first line — with the picture "control has been received,
    silence". Both are bounded; a COM1 that keeps silent is switched to a short wait,
    but is not switched off for good (silence at the beginning does not mean silence further on).
28. **The frame allocator is bound to begin ABOVE the supervisor leaf with the kernel's tail.**
    The GB0 is mapped by leaves of 2 MB (`paging::init`), and the leaf the image's end landed in
    is marked supervisor ENTIRELY. The allocator, however, handed the frames out right from
    the `kernel_hi` — that is, from inside that leaf — and everything that went into ring 3
    (the workload's stack, its heap) lay on a page ring 3 has no right to write to:
    a `#PF` "a protection, a write" on the first access. The failure depended on the IMAGE'S SIZE:
    while the kernel's tail lay close to the 2 MB boundary, a few frames fell into the dangerous
    zone and they went to the kernel itself; as soon as the image grew (in that
    session — by 70 KB of statics for the observability) the workload's stack drove into the
    zone. From the outside that looked like "we added code and printf broke".
    Now `mem::init` rounds the base up to 2 MB (the price is less than two
    megabytes), while `paging::init` prints the check of the invariant on EVERY boot;
    the gate is `talaria.test=obs`.
29. **The PMU counters are per-core and the CPUID's declaration must not be trusted.** The counters' MSRs
    are each core's own: `pmu::init_cpu` is called at the bring-up of EVERY AP, otherwise
    an `rdpmc` on a secondary core gives out zero — that is, perf keeps silent exactly where
    the workload lives. On top of that, KVM declares three fixed counters and an
    arch-perfmon v2, while one must not count with them: `init_cpu` sets them up, spins a short
    loop and checks whether they have moved; they have not — the cycles and the instructions move over
    onto the general-purpose counters. The layout is printed at the boot.
30. **A new page table is put into the parent entry ONLY through a CAS.**
    The tables are shared by all a process's threads, while the split of a 2M/1G leaf and the starting of
    a missing level happen on the fault-handling path — that is,
    simultaneously on several cores. Two cores splitting one entry both
    allocated a table and both wrote its address by a plain write: the last one won,
    while everything the loser had managed to materialise into its own table DISAPPEARED from the address
    space. Outwardly it was a SIGSEGV at a perfectly lawful address once in a
    hundred rounds, and the probability depended on the tick's length: diagnostics that lengthened the
    handler made the failure three times more frequent. Now `install_table`/`new_level`
    change the entry with a compare-and-swap from the observed value, the loser
    frees its own table and takes the other one, while `pt_for` RE-READS the entry after
    the split. The price is one atomic operation per first access to a 2 MB region.
    The gate is `talaria.test=memrace` on four cores.
31. **The unwinding of the stack is bound to be FAULT-TOLERANT rather than "check and
    read".** The RBP in code without frame pointers is an ordinary general-purpose
    register: there is anything at all there. A check of the page before a read is not enough —
    a neighbouring core may remove the mapping between the check and the load (ClickHouse
    calls `munmap` constantly), and then a #PF would arrive IN THE TIMER'S HANDLER, that
    is, the kernel would crash because of its own diagnostics. So the read goes with a recovery
    point: the return address is put into a per-CPU slot, and `rust_exception`,
    having seen the slot filled at a ring 0 fault, returns control right after
    the load. The check against the page tables has stayed — but as a cheap filter rather than
    as a guarantee.
32. **`rdpmc` reads the counters of ITS OWN core ONLY.** So a core's snapshot is taken by
    the core itself: in the scheduler's tick (`pmu::note_tick`) or by an IPI (`pmu::poke`).
    A DEDICATED core has no tick — and that is not a breakage but its definition; `top` and
    `perf` write "no snapshot (there is no tick)" and name the numbers, while a `perf poke <N>`
    takes one by force at the price of one interruption of a latency-critical thread. Do not
    do that automatically: peeping into a dedicated core is exactly the
    jitter for the elimination of which the core was dedicated.

---

## 13. The roadmap and the status

- **Closed (with automated tests)** — see `docs/PROD-GAPS.md`: the memory's correctness (a UAF,
  the COW refcount, a per-process AS for the disk workloads), the observability/the enforcement
  (the counters, the hot-path zero-syscall, the telemetry), the robustness (a soak, the watchdog),
  the operator's side/the security (the control-plane auth, the syscall filter, the manifest's `core=`),
  the test plan B1–B8, the kernel's outgoing network (DNS, TLS in user space) and a multithreaded
  disk workload under the supervisor. The regression's counters are in the README (the single
  source); the full map of the product's boundaries/limits is docs/PRODUCT.md, and the plan of
  the finalisation of v1.0 is docs/RELEASE-1.0.md (talaria-pack/talaria-check/mode=cell,
  the memory at restarts, the LICENSE/CONTRIBUTING).
- **Bare metal (v1.7, session 9): IT IS UP.** On a `c6i.metal` (128 cores, 256 GB, without
  a hypervisor) the kernel goes through the whole bring-up via OUR OWN BIOS chain: a memory
  map of 22 records, the ACPI (the RSDP was found by a scan, the MADT has 64 CPUs, the MCFG has the ECAM),
  PCIe through the ECAM (52 bridges + 307 functions), an NVMe at `c2:00.0`, ext2 on a live EBS,
  an `ext2-test: ALL OK`. The benchmarks were run too: fsbench (an fsync of 7.4 ms for 512
  blocks, a repeated one of 4 us), the PMU (`rdpmc`, an IPC of 1.93 — a real counter),
  `sync` (**the tier `mwait(ring0)`**, a cross-core wake-up with a p50 of 901 ns — on
  Nitro guests the same detection gives `hlt+ipi`, that is, on hardware PLAN-SYNC
  works on the MAIN path). The analysis is in `docs/PLAN-V1.7.md`, the diagnostics is §11.4.
  What is left on the hardware: **SMP** (an AP does not come up — A10) and the **ENA network** (it has not been
  checked even once — A11), see the header of `docs/TODO.md`.
- **Outside the environment (section C, hardware is needed):** in-kernel NIC drivers
  (ixgbe/mlx5), a real NIC on the hot path, PTP/hardware timestamps, the tail SLA p99.9
  against the hardware's baseline; a production exchange (the mainnet) is at the owner's decision.
- **v0.48 — a DB on Talaria:** PostgreSQL 16 (a postmaster + forks) and MariaDB 10.11 (InnoDB, threads)
  work as network servers with persistence: `ext2.rs` (an RW ext2 on virtio-blk/NVMe,
  a write-back cache, e2fsck-clean images; **v1.1 §8: a JBD2 journal with barriers and
  a replay at the mount, the coalescing of the flush into ranges, a per-file fsync,
  a file MAP_SHARED through the cache's pages**), `proc.rs` (per-process fd tables with a refcount, pipes,
  rt_sigaction/sigprocmask/kill/rt_sigreturn + SA_RESTART, cwd), `talaria.mode=proc`
  (a process from an ext2 disk by the `/cell.manifest`, a streaming ELF loader into a private AS),
  SysV/POSIX shm, select, an `rt_sigtimedwait`, a `FUTEX_WAKE_OP`. The invariants added in v0.48:
  (14) the kernel is built with SSE — the user's SSE/x87 is saved by an fxsave/fxrstor in the syscall/
  timer/IPI/#PF stubs and per task in the scheduler, is copied at a fork and is put into a signal's frame;
  (15) the `CR0.WP=1`: a kernel write into a user's COW page is a #PF→a cow_fault; since the syscalls
  execute on the user stack, a process's stack at a fork is copied eagerly (otherwise a ring 0 #PF on a
  COW-RO stack = a #DF); (16) the pages that are already COW after a previous fork get a
  reference at a new fork (`rc_share`), otherwise the parent makes them shared with the child; (17) the identity range of
  a process's image is reserved from the allocator (`mem::reserve`) — mmap gives out identity addresses;
  (18) a `munmap` of its own stack by a thread (musl's `__unmapself`) is deferred until the thread's death
  (`sched::defer_unmap`). The diagnostics: under a `talaria.strace=N` the `[sched] a tick`,
  the `[futex] ... > 5 s` and the unsupported futex ops are printed. The `*at` syscalls with a dirfd: ext2 keeps the absolute path
  of an open directory (`ext2::fd_path`) — `proc::resolve_at`. The regression: `talaria.test=ext2`, `talaria.test=sigtest`.
- **v0.49 — a dynamic ELF + MongoDB:** `user.rs::load_elf_into_space_disk` loads an ET_EXEC and
  an ET_DYN (a PIE at `0x5555_5555_0000`), reads the PT_INTERP and loads the ld.so at `0x7f00_0000_0000`,
  the entry goes into the interpreter, the auxv is full (`build_stack_full`: the AT_BASE/AT_ENTRY/AT_PLATFORM/AT_EXECFN/
  AT_HWCAP/AT_CLKTCK/AT_UID…); the pages shared by two segments are one frame + an OR of the rights
  (`paging::phys_of_in/or_perms_in`). An `mmap` in the process mode: a MAP_FIXED and a file MAP_PRIVATE go
  by the virtual address (not identity) through a `paging::map_fresh_pages` (PRIV, COW at a fork) with
  a read of the file into the frames; the bump region is `proc::mmap_alloc` (`0x6000_0000_0000`, inherited at a
  fork). The invariants: (19) `paging::protect` is bound to preserve the PTE_PRIV/PTE_COW (otherwise after an
  mprotect the ld.so/guard pages drop out of the COW copying at a fork) and to break the COW at a
  request for RW (a `cow_fault`); (20) a file MAP_SHARED (v1.1 §8.5) maps the pages of the block cache
  ITSELF — `read/write` and the mapping see the same bytes; the "dirtiness" is taken off the D bit
  of the page table in the msync/munmap/fsync, while the slots under a mapping are pinned (the `CPIN`), otherwise
  an eviction would take the page away from under the user;
  (21) the eventfd/timerfd are real ones (the null fds of kind 4/5: a counter/a deadline, with a readiness for epoll/poll),
  without them mongod's ASIO reactor would never wake its timers. The regression: `talaria.test=dyn` (a PIE + an ld-musl
  + threads + a dlopen of libz, the image `disk_dyn.img` from `kernel/user/dyn/`).
- **v0.50 — the debts of the process model are closed:** the syscalls execute on the task's **kernel stack**
  (the stub `syscall_entry`: the user's rsp → the scratch `SYSCALL_URSP`, the rsp = the `KSTACK_TOP` (= the TSS.rsp0),
  the `Frame.ursp`; the kernel stacks are 16 KB everywhere); the user stack at a fork is an ordinary COW (the invariant 15 is lifted:
  a ring 0 write into a COW page → a #PF → a cow_fault, and the exception's frame is already on the kernel stack).
  A process's memory is freed at its exit: a `sched::queue_dead_space(cr3)` → the scheduler out of
  ANOTHER task (`reap_dead_spaces`, the active CR3 ≠ the one being freed, there are no live tasks with that CR3)
  → a `paging::free_address_space`: the PRIV frames by the refcount (`release_priv_frame`: an rc>1 → a −1,
  otherwise a free), the tables of the GB0 copy and of the private region, the PDPT/PML4. The new invariants: (22) at a
  fork **all** the private pages (RW, RO, already COW) get a COW bit + an `rc_share` — an RO after
  an `mprotect(PROT_READ)` is not "code forever", an `mprotect(RW)` through a `protect()` breaks the COW
  (a `cow_fault`), otherwise a write went into a frame shared with the child; (23) the kernel accesses the frames by their
  identity address (the zeroing at an allocation, the `copy_nonoverlapping` in the cow_fault, the kernel stacks,
  the tables) — so in a process's AS at an identity vaddr there must be nothing but real
  identity pages: a process's anonymous MAP_PRIVATE → the non-identity bump region (`proc::mmap_alloc`,
  `map_fresh_pages`), identity is only an anonymous MAP_SHARED/shm (a file MAP_SHARED
  is mapped by the virtual address through a `map_leaf`); (24) a signal to a process (a `kill`, a SIGCHLD)
  is kept in the `PPEND[the leader]` and is taken by any thread without the mask (`take_pending_in`), while a tkill goes into
  the `PENDING[the thread]`; (25) the ctid at an exit is written only into a mapped page (musl 1.1 keeps it
  on a stack already removed by a munmap); (26) a thread of a `clone(CLONE_VM)` inherits the creator's signal mask
  (`proc::thread_inherit_sigmask`) — otherwise a signal to the process (a SIGTERM to mongod) was taken by a thread without the mask and
  the process died by the default action instead of a clean stop through the sigwait thread. ext2 is write-back: the dirty blocks go at an eviction/an `fsync`/
  a `sync`/an `exit_group`/a `talaria.stop`; the cache of 8 MB is allocated at the mount (not in the kernel's image).
  The minimum of procfs/sysfs is the null fds of kind 7/8 (`virt_content`), the signalfd is kind 6. A `talaria.stop=SECS`
  (the `sched::STOP_AT_NS`) is a clean stop of the workload. The glibc ld-linux: the `uname` release `5.15.0-talaria`,
  an `rseq`→an ENOSYS, an `arch_prctl(ARCH_CET_*)`→an EINVAL. The regression: `talaria.test=sigtest` (+the freeing of the processes' memory, the sigwait thread/the clone's mask,
  the signalfd, the eventfd/timerfd, the procfs, an mprotect-RO after a fork, an openat(dirfd), a file mmap),
  `ext2v` (the persistence across a reboot), `stop` (a talaria.stop → a SIGTERM → a clean exit),
  a `talaria.mode=proc` by a manifest (the DB path), `dyn` (musl) and `dynglibc` (glibc 2.35,
  `kernel/user/glibc/`); on the host: an `e2fsck -fn disk_ext2.img` after the run. validate.sh B10:
  PostgreSQL/MariaDB/MongoDB through their run.sh (a SKIP without the assembled artefacts).
- **v0.51 — PLAN-SYNC (synchronisation without a busy spin), the new invariants 27–33:**
  (27) the per-CPU data goes through the GS.base (percpu.rs), which on every core is ALWAYS the kernel's (without a
  swapgs): the syscall stub uses the gs:0/gs:8/gs:16, and the interrupts may read the gs at any moment;
  the user code is forbidden to change the GS (an `arch_prctl(ARCH_SET_GS)`→an EINVAL; a `mov %gs` in ring 3 would break
  the thread). The `SYSCALL_URSP`/`KSTACK_TOP`/`SIG_NEWRSP` from v0.50 became fields of the PerCpu. (28) The handlers
  of the syscalls get a COPY of the frame (the fd translation in `rust_syscall_inner`) — any ADDRESS arithmetic
  from the frame (the fpu_area and the like) is bound to go from the `raw`, not from the `f`. (29) An AP executes ring 3 tasks:
  it needs its own TSS/GDT (the TSS's busy bit), a `syscall::init` (the per-core MSRs) and an EFER.NXE in the trampoline
  (otherwise the NX bits of a PTE = an RSVD #PF). (30) A task with a `CPU_OF[t]!=0` belongs to an AP: the BSP's scheduler
  does not save its RSP/FPU and does not choose it (a migration leaves a limbo context, and the old kstack goes into
  the freeing queue). (31) The kernel's waiting is only `sync::kwait_once` (a MONITOR → a recheck
  → an sti;mwait: the arm/check race is closed architecturally); before a sleep on the BSP with live runnable ones there is a
  `yield_now()`, do not sleep in an mwait holding the core. (32) A futex WAKE is bound to wake through
  `sync::wake_addr` (the WOKEN + a directed switch/an IPI/a kick) — the scheduler PUTS the waiters TO SLEEP (a skip + a backstop of
  1/64), and "a no-op wake, the waiter will check by itself" does not work any more; any kernel code that changes a
  futex word (for example a `clear_ctid`) is bound to call a `wake_addr`. (33) An sti;mwait/sti;hlt in new
  modes goes only after a `lapic_enable()` (the masking of the LVT/LINT), otherwise there are garbage vectors (a #DE).
  The build of the user libraries: `kernel/user/tsync/` (musl-gcc, the docker image talaria-musl); the patched
  ld-musl: `build-musl.sh` → `user/dyn/ld-musl-x86_64.so.1`. (34, SPSC v3/v0.52) `upark`(4602)
  kind1/kind2 (the predicate "the index < the target", a doorbell pair) and a `tsync_kmode`(4601): in the kernel's wait loop
  the kwait is bound to recheck the PREDICATE (not only "the word == the one read" — a lost wake-up),
  and before the first kind2 sleep there is a `sync::membarrier()` (an IPI ack through the WAKE vector, the `WAKE_ACKS`) +
  a recheck: without that there is a deadlock "the final store of the partner's index was not visible". In the user protocols
  the "doorbell" is bound to the target rather than to the state flag (the window of the state 1→0→1 an observer may
  miss). The SPSC's fast path is a static inline in tsync.h: moving it beyond the call boundary costs ~4x of the
  throughput. (35, v0.52b) The virtio batched notify: `virtio-drivers` is vendored in `kernel/vendor/`
  (a `transmit_begin_no_notify` + a `transmit_notify` once per batch); with a batch an mb() before the
  `should_notify` is MANDATORY (the VirtIO spec: the publication of the avail_idx vs the read of the avail_event — otherwise there is a lost
  kick; with a kick per frame the race was masked). An AP's stack is 16 KB (`start_ap`): the locals of `run_cell` are
  >6 KB (the rx_slot/tx_slot/tx_len at a QS=256); 4 KB overflowed into the neighbouring frames (the garbage in the
  TX ring was caught as "broken" frame lengths).
- **v1.2 — the third boot path and the next layer of the FS:** `boot.rs` got a
  multiboot2 header and the entry `mb2_start` (BIOS through GRUB; `tools/mk-bios.sh`,
  a `HYBRID=1` is one disk for BIOS and UEFI), and `fs.rs` got the parsing of a GPT BY THE PARTITION'S NAME
  (`talaria-data`) and of an MBR table. In `ext2.rs`: the block cache became 4-way with an
  LRU and a PER-WAY layout (the invariant 19), the file mapping became
  per page (a contiguous window of the cache is no longer needed; the ENODEV stayed only for the
  identity AS), the journal writes the descriptor and the copies in one submission, the allocation of the
  blocks goes in runs with a preallocation window (the invariant 18) and with a cursor over the group,
  and the mode `talaria.ext2=journal=data` was added. The check of §8 on a LIVE EBS found
  two bugs (the invariant 16 and the read of a freshly allocated block off the disk — the `cblk_new`).
  The card is passed to an AP by reference (an AP's stack is 32 KB + a guard page),
  and `talaria.netidle=1` started working on virtio. The ENA's diagnostics: `talaria.ena=phc`
  (the device's clock), `rss`/`rsstbl`/`rsshash`/`rsscq`, `hibuf`.
  `talaria.userdata=1` is the cmdline's tail from the EC2 user-data (see §11.3).
- **The known technical debt:** a hw watchpoint
  is fixed at 4 bytes. **The gap of the ENA's datapath against Linux is closed**
  (session 6: the accelerated LLQ mode, an RTT of 34 us against Linux's 43) — the story
  of the search is in `docs/PLAN-V1.4.md`.

---

Related documents: `docs/USER-GUIDE.md` (the build/the launch of a workload),
`kernel/README.md`, `kernel/BUILD.md`, `services/README.md`,
`docs/PROD-GAPS.md` (the gaps/the test plan), `docs/POC-RESULTS.md` (the measurements),
`docs/TODO.md` (the roadmap), `docs/talaria-os.html` (the design doc TAL-RFC-001).
