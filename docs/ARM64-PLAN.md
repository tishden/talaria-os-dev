# Talaria OS — the arm64 port: the plan

What it takes to run the kernel on AArch64 (EC2 Graviton, QEMU `virt`) with the
same product contract as on x86-64: unmodified Linux binaries in ring 3 (EL0),
the cores owned by the workload, the OS as a control plane. This is a WORKING
PLAN, written before a line of arm64 code exists: it names every x86 mechanism
the kernel leans on, what replaces it, in which order it is built, and by which
gate each step is judged. The numbers here are estimates and are marked as such;
the measurements come later and go to `docs/MEASUREMENTS.md`.

Status (2026-10-04, branch `arm64-port`): **the first pass of phase 5 (the ordering audit)
is done by reading, and phase 4's tails are closed under TCG** (§10.10). The audit found more
ATOMICITY than ordering: a pipe, an eventfd, the pending-signal sets, the COW refcount, the
task-slot claim — one word written by two cores with plain read-change-store, wrong on x86
as much as on arm64; a new gate (`piperace`) failed on the old kernel 2 runs in 3. The
ordering half: the SGI's `dsb`, the TLB invalidations' `dsb ishst` and their reach, a
store-release where a stack is freed, an arm64 `membarrier` nobody acknowledged. virtio-pci
drives the NIC on both machines (the UEFI paths have a network now), bridges are numbered
when no firmware did, the zero-syscall cell runs on arm64, the RAM above 4 GB is kept in up
to four pieces, libtsync runs on arm64 — and its port found the generic timer's interrupt
storm after a borrowed deadline (every timed wait). `test-aarch64.sh` 269/0 under TCG,
`test.sh` 496/0 + 22/0; on the KVM stand the gate 268/0, the soak and the parallel race
detector 0 failures with `piperace` in both, the netirq fix confirmed. The measured `SeqCst`
downgrades are what phase 5 still owes (§10.10's last list), with the price the audit put on
the wake path (futex/condvar RTT up 11 %/32 % on the stand).

Status (2026-10-01, branch `arm64-port`): **phase 4 is done on QEMU `virt` and the product
boot path of phase 6 runs under `edk2`.** The network over virtio-mmio (the card's line
through the GIC, the whole network block of the regression), PCIe through the tree's ECAM,
NVMe, MSI-X through the ITS (`talaria.test=its`: an LPI on the core the route names), the
ext2/jbd2 block on virtio-blk AND on NVMe with the power cut at the commit, the C++ set,
the kernel's own crash dump — and `edk2` → the arm64 UEFI stub → the kernel with the machine
described by ACPI alone (the MADT's GIC entries, the GTDT, the SPCR, PSCI from the FADT, the
ECAM from the MCFG, the BARs the firmware assigned). `test-aarch64.sh` is **229/0** under
TCG (it was 119); x86 stayed at `test.sh` 486/0 + 22/0, `kernel-unit` 289/0. Four defects of
the SHARED code came out when the gate ran on a loaded host (§10.5) — the classes are
`docs/ENGINEERING-NOTES.md` §6.1, items 22-25. **The next session is §10.6.** The
paragraphs below are the statuses of the days before, kept for what they list.

Status (2026-09-30 night, branch `arm64-port`): **phase 3's set runs on aarch64 — the
whole ring-3 regression that has an arm64 binary (42 of the 45 C sources, one source per
test for both machines), the disk-borne modes from ext2, fork/execve/wait, the core dump a
host gdb opens, the supervisor's crash and restart paths; `test-aarch64.sh` 119/0 under
TCG, `test.sh` 486/0 + 22/0, `validate.sh` 14/0, the census 0 (§10.4).** Thirteen defects
stood between "ring 3 runs" and the set, four of them in shared code that x86 had been
lucky with — the list is §10.4, the classes are `docs/ENGINEERING-NOTES.md` §6.1. The KVM
stand ran twice the same day (§9, §10.4): the `svc` cost (55 ns with the FP/SIMD save),
the four ordering probes and now the set's numbers are in `docs/MEASUREMENTS.md` §10.
**The next session is §10.3.**
On the x86 side the signal frame's layout is behind `arch::signal` and the syscall
`Frame` is read by neutral names (§4.5) — phase 3's design item. The paragraph below
is the status of the day before, kept for what it lists.

Status (2026-09-29, branch `arm64-port`): **phase 0 in progress; phases 1 and 2
have their logic proved under TCG.** `arch/x86_64/cpu.rs` holds the privileged-instruction primitives
and 21 shared modules call them instead of inline `asm!`; seven whole x86 modules
(`boot`, `percpu`, `ioapic`, `vga`, `pmu`, `cat`, `crypto`) moved under
`arch/x86_64/` (the x86 regression stayed at 486/0 across both changes).
`arch/aarch64/` has the `Image` header and entry, the early identity map, the
exception vectors, the PL011 and the device-tree parser (§4.2 — host-tested against
the stand's own tree, 11 tests), and the kernel boots on `qemu-system-aarch64 -M virt`
under TCG to a banner that names the RAM, the console, the GIC and its ITS, the
timer PPIs (29/30/27/26), PSCI (`hvc`), the cores, 32 virtio-mmio windows, the ECAM,
and translates the tree into the PVH `hvm_start_info` the x86 kernel reads — the
"one boot record" of §2.6 turned out to be the record that already exists, as it
did for GRUB. The facts §4.9 asked for are in: EL1, the DTB at `0x4800_0000`,
`CNTFRQ` = 62.5 MHz on QEMU 8.2.2, i.e. 16 ns per tick. Of phase 2, everything TCG can
judge is in and green: the GICv3 distributor/redistributors/CPU interface (§4.8), the
generic-timer tick re-armed from the IRQ vector (§4.9: 48 ticks in 50 ms at a 1 ms
period), the secondary cores brought up by PSCI `CPU_ON` with a stack each (§4.10:
4 of 4 on `-smp 4`, each taking its own tick), and an SGI delivered to every one of
them. What TCG cannot judge — the timing of any of it and the memory ordering — waits
for the KVM stand (§1). The first shared modules run on arm64: `paging.rs` computes in
its canonical (x86-shaped) descriptor format and `arch::mmu::{enc, dec}` translate on
every table access — the entry type is `Pte`, so a raw access does not compile; the
translation needs no level (a table and a 4K page share one encoding, a block is
"present without bit 1") and puts the OS bits at 55-58. `mem::init`, `paging::init`
(the identity gigabytes laid out by `arch::mmu::gb_kind`: the devices below the RAM,
the kernel's gigabyte on 2M leaves), `cmdline` and `read_cmdline` run unchanged; the
rights probe (`protect_none`/`protect`/`query` on a fresh frame) passes under TCG. The
neighbours they need but that have not come over are stand-ins in ONE file,
`arch/aarch64/pending.rs` — a list of debts that must shrink. Known gap: no dirty-bit
tracking (`take_dirty` never fires on arm64; `ext2`'s mmap writeback relies on it —
§4.3 will need `FEAT_HAFDBS` or a software dirty bit). Of phase 3, the first step is
proved: a probe assembled into a fresh frame runs at EL0 with its own stack, three
`svc`s come back through the lower-EL sync vector with the answer the handler wrote
into `x0`, and a `brk #0` returns the machine to EL1 with a verdict (`trap::sync_el0`;
the instruction cache is synchronised after the write, §4.12; `TPIDR_EL1` carries the
per-CPU block, §4.6). Two more pieces of phase 3 are written ahead of their callers:
`arch/aarch64/abi.rs`, the arm64 → internal syscall-number map (160 rows derived from
the two toolchains' headers, host-tested one-to-one), and the arm64 vDSO
(`user/vdso/vdso-aarch64.S`, assembled by `build.rs` with a cross compiler: the three
clock calls over `cntvct_el0` with a `dmb`-ordered seqlock, and the
`__kernel_rt_sigreturn` trampoline arm64 needs because it has no `SA_RESTORER`). Both are now EXERCISED: `vdso.rs`
runs on arm64 (`vdso::symbol` finds the clock call in the image, `init` refuses an image
without it — on either architecture), and the EL0 probe calls `__kernel_clock_gettime`
through the mapped image: the answer lands inside the kernel's own bracket around the
call. The dirty bit exists: FEAT_HAFDBS's DBM scheme where the core has it (`TCR.HD`;
`enc` writes a writable page as DBM + AP[2] = 1, the first write clears AP[2] without a
fault, `dec` reads D from that, `take_dirty` re-arms — x86's semantics exactly), and a
CONSERVATIVE "every writable page is dirty" without it; the probe (write → true, again →
false, write → true) passes under TCG, which models HAFDBS. Phase 0 is COMPLETE in the
sense the plan gave it: not one `asm!` remains outside `kernel/src/arch/` — the whole
x86 modules moved as files (`boot`, `percpu`, `ioapic`, `vga`, `pmu`, `cat`, `crypto`,
`idt`, `smp`, `gdb`, `tsc`, `serial`), and the mixed ones gave up their machine parts
(`gdt.rs` the GDT/TSS and the `iretq` into ring 3 from `user.rs`; `switch.rs` the FPU
state, the saved-context entry, `timer_stub_sched` and the soft-yield kick from
`sched.rs`; `syscall_entry.rs` the SYSCALL MSRs, the entry stub and the sigreturn
`iretq` from `syscall.rs`; `setjmp.rs` from `supervise.rs`; `unwind.rs` the guarded load
from `obs/unwind.rs`) — the x86 regression at 486/0 at every step. What phase 0 leaves
for phase 3 is the DESIGN work the plan named: the unified exception/syscall frame and
the `arch::abi` decoder inside `dispatch`, which is where `syscall.rs`, `sched.rs` and
`user.rs` will come over. The gates in `main.rs` went 92 → 84: that count is the meter. The x86-64 tree is the release (`README.md`); the
port changes its behaviour nowhere.

---

## 0. The verdict in one page

**Feasible, and the shape of the code helps.** Of 56 100 lines in `kernel/src/`
about 15 000 sit in the twenty-three modules that touch the machine directly
(`boot`, `idt`, `syscall`, `sched`, `smp`, `percpu`, `paging`, `user`, `sync`,
`tsc`, `pmu`, `serial`, `gdb`, `acpi`, `ioapic`, `pci`, `crypto`, `vga`, `cat`,
`vdso`, `coredump`, `uaccess`, `main`), and inside those the architecture is a
minority: `asm!` appears in 31 files, 187 times, and the hardware-shaped
structures (the IDT, the GDT/TSS, the LAPIC, the syscall frame, the XSAVE area,
the page-table bits) are a few hundred lines. The file system (`ext2*`, `jbd2`,
`fs`, `tfs` — 9 500 lines), the network stack (`netsys`, `l1`, `fastpath`,
`fastring`, smoltcp), the drivers' logic (`ena`, `nvme`, `virtnet`), the
scheduler's POLICY (`rqpick`, `affinity`, `placement`), the process model
(`proc`), the observability (`obs/`, `metrics`, `telemetry`), the operator's side
(`console`, `shell`, `edit`, `filed`), the control plane and `bench/suite` do not
know which CPU they run on. They need one thing each: the memory-ordering audit
of §4.13.

**Three things are genuinely new, not translated.** (1) The GICv3 interrupt
controller with its ITS — MSI-X on arm64 goes through an in-memory translation
service the kernel has to program, where x86 writes one MSI address into the
device; there is no small version of this. (2) The Linux arm64 ABI is not the
x86-64 ABI with other numbers: `clone` swaps two arguments, `struct stat` and
`epoll_event` have another layout, `sigaction` has no restorer and the kernel
must supply the `rt_sigreturn` trampoline in the vDSO, the `O_*` flags differ,
`arch_prctl` does not exist. (3) The memory model is weak: the kernel today has
1 444 `Ordering::Relaxed`, 877 `SeqCst` and 367 `read_volatile`/`write_volatile`
sites, and a part of them are correct ONLY because x86 is TSO. QEMU on an x86
host cannot show that (TCG executes on a TSO machine); only real hardware can.

**Three things get simpler.** The TLB shootdown IPI machinery
(`syscall::NO_SHOOTDOWN`, `ask_host_flush`, the identity-hole shootdowns) is
replaced by broadcast `TLBI ...IS` — the architecture does it. SMP bring-up is a
PSCI call per core instead of a real-mode trampoline. The timer frequency is
read from a register instead of being calibrated against a PIT or asked from
the hypervisor.

**What is lost on purpose.** No BIOS path (arm64 EC2 is UEFI-only, so `tboot`,
`mk-bios.sh`, multiboot2 stay x86). No CAT (`llc=` — Graviton exposes no MPAM).
No `umwait`/`mwait` tiers — their arm64 counterpart is WFE on the exclusive
monitor, one tier for ring 0 and ring 3 both. No SVE for the workload in the
first version (§4.7). No KVM PV EOI / PV IPI / kvmclock — arm64 KVM offers none
of them, and the tick-cost programme of session 19 has to be redone with the
arm64 levers (§4.9).

**The size, honestly.** Nine phases (§5), about 18–22 weeks of one person's
focused work to the point where the x86 regression set passes on QEMU/KVM on a
Graviton `*.metal` and the six workloads run on a `c7g`. The scoreboard against
Linux (295 points, 9 hours per run on x86) is a separate programme after that —
the comparison baseline changes too (AL2023 arm64), so nothing carries over.

---

## 1. The target and the stands

| | x86-64 today | arm64 |
|---|---|---|
| The instruction set baseline | x86-64 + SSE2/AES-NI/PCLMUL (`x86_64-talaria.json`) | ARMv8.2-A + NEON + Crypto Extensions + LSE atomics: the common floor of Neoverse N1 (Graviton2), V1 (Graviton3), V2 (Graviton4) and QEMU `-cpu max`/`cortex-a76` |
| Rust target | custom JSON, `build-std` | custom `aarch64-talaria.json` derived from `aarch64-unknown-none` (hard-float, `+v8.2a,+neon,+aes,+sha2,+lse,+crc`; code model `small`; no red zone question — AArch64 has none) |
| The regression stand | `qemu-system-x86_64 -M microvm/q35 -enable-kvm`, docker `talaria-qemu2` (QEMU 8.2.2) | `qemu-system-aarch64 -M virt,gic-version=3,its=on -cpu host -enable-kvm` — KVM needs an arm64 HOST, i.e. a Graviton `*.metal` (`c7g.metal` / `c6g.metal`; EC2 does not offer nested virtualisation on non-metal arm64 instances). On the x86 dev box the same command with `-cpu max` runs under TCG: 10–30× slower, single-vCPU semantics, TSO — good for "does it boot", useless for timing gates and for the ordering audit |
| The production stand | `c6i.*` (Nitro, ENA, NVMe on EBS), UEFI or BIOS | `c7g.*`/`c8g.*` (Nitro, the SAME ENA and NVMe devices), UEFI only; `c7g.metal` for bare metal (also UEFI) |
| The firmware for the UEFI block | OVMF | `edk2-aarch64` (`QEMU_EFI.fd`, packaged in Fedora) |
| The serial console | 16550 on port 0x3F8 | PL011 at `0x0900_0000` on QEMU `virt`; on EC2 whatever ACPI SPCR names (the interface type field says PL011 or 16550-MMIO — the driver is chosen from it, both are ~50 lines) |
| The comparison baseline | AL2023 x86-64 | AL2023 arm64 (the same `bench/suite` generator on a second `c7g`) |

**The dev box has none of it today**: no `qemu-system-aarch64`, no aarch64 musl
toolchain (only the Xilinx `aarch64-linux-gnu-gcc`, glibc-targeted), no rustup
target. Phase 0 installs them.

**Why a `*.metal` and not TCG for the regression.** `test.sh` is 486 assertions
in ~90 boots; a good third judge time (the slice length, futex latency, the
tick's cost, the barrier's cost). Under TCG they are meaningless, and under TCG
the weak memory model is invisible. A `c7g.metal` on demand for the hours a run
takes is the cheap way; the terraform in `infra/aws/` already brings metal up and
down (`validate-remote.sh`). The stand image `Dockerfile.qemu` grows a second
package (`qemu-system-aarch64-core`) and is built for `linux/arm64` on that host.

---

## 2. The principles

1. **One tree, one kernel crate, `cfg(target_arch)` at the module boundary.** The
   arch-bound code moves under `kernel/src/arch/{x86_64,aarch64}/` with the SAME
   public surface (`arch::percpu::cpu_id()`, `arch::mmu::switch(...)`,
   `arch::irq::eoi()`, ...). No trait objects, no generics over "the CPU": the
   architecture is fixed at compile time and the hot paths stay inlined.
2. **The x86 kernel does not change behaviour.** Phase 0 is judged by the x86
   regression count staying exactly what it is, and every later phase repeats
   that. Where a refactor for arm64 improves x86 (e.g. an explicit `Release`
   where a `Relaxed` was TSO-correct) it is a separate, named commit with its own
   measurement — never smuggled in.
3. **The same gates, the same names.** `talaria.test=<name>` keeps its meaning;
   a gate that is x86-only says so in a table (§6) instead of silently passing.
   New arm64-only mechanisms get their own gates (GIC ITS, PSCI, icache
   maintenance, TLBI semantics, WFE).
4. **The same ABI to the workload where Linux is the same, and Linux's arm64 ABI
   where it differs.** The point of the product is unmodified binaries; the
   reference is what `musl`/glibc for aarch64 emit, not what is convenient.
5. **Measured, not assumed.** Every "arm64 is faster/slower here" in this plan
   is a hypothesis until `docs/MEASUREMENTS.md` has the number. In particular
   the tick cost, the syscall cost and the wakeup latency get the same
   instruments (`mbench`, `futexlat`, `talaria.test=tick`) before anything is
   tuned.
6. **DTB for the fast loop, ACPI for the product.** QEMU `-kernel` hands a
   device tree; EC2 hands ACPI through UEFI. Both feed ONE `BootInfo` (the same
   idea as today's four loaders → one `hvm_start_info`), so the kernel below the
   boot layer knows neither.

---

## 3. The inventory: what the kernel leans on today and what replaces it

Size: **S** — under a day, **M** — days, **L** — a week or more. Risk: what can
go wrong that QEMU will not show.

| x86-64 mechanism (module) | arm64 counterpart | Size | Risk |
|---|---|---|---|
| PVH/multiboot2 32-bit entry, static 2M tables, `lgdt`, long-mode switch (`boot.rs`) | Linux arm64 `Image` header (64-byte, `text_offset`, `x0 = DTB`) so QEMU `-kernel` loads us; EL2→EL1 drop if entered at EL2 (QEMU `virt` without `virtualization=on` enters EL1; KVM guests enter EL1); zero `.bss`; a static identity map with 1G/2M blocks; `SCTLR_EL1.M` | M | The `Image` header must be the FIRST bytes of the file — linker script order; entering at EL2 on some firmware |
| `hvm_start_info` (cmdline, memmap, RSDP) | `BootInfo` filled from the DTB (`/memory`, `/chosen/bootargs`, UART, GIC, timer PPIs, virtio-mmio nodes) or from UEFI (memory map, ACPI RSDP from the config table) | M | A flat DTB parser is ~300 lines; must not allocate |
| IDT, exception stubs, `ExcFrame`, `#PF` with CR2/error code (`idt.rs`) | `VBAR_EL1` with 16 vectors; `ESR_EL1` (the class + ISS), `FAR_EL1`; SP_EL1 per core; the frame is x0–x30, sp_el0, elr, spsr | M | Alignment faults are REAL on arm64 for device memory and for atomics; the `packed` `epoll_event` write path |
| `SYSCALL`/`SYSRET`, LSTAR/STAR/FMASK, `swapgs`-free `gs:` scratch (`syscall.rs` stub) | `SVC #0` from EL0 → the sync vector; nr in `x8`, args `x0–x5`, return in `x0`; `ERET` back; per-core scratch in `TPIDR_EL1` | M | `SA_RESTART` rewinds `rip - 2` today; `SVC` is 4 bytes |
| The Linux x86-64 syscall NUMBERS (144 match arms in `dispatch`, 64 in `uaccess::gate`) | The asm-generic table: `openat`=56, `clone`=220, `mmap`=222, `rt_sigreturn`=139, ... and ~30 legacy calls do not exist at all (`open`, `fork`, `stat`, `pipe`, `dup2`, `select`, `poll`, `epoll_create`, `epoll_wait`, `getdents`, `time`, `access`, `mkdir`, `unlink`, `rename`, `arch_prctl`, `eventfd`) | M | An `abi::Nr` layer: the entry translates the arch number into ONE internal enum, `dispatch`/`gate` match on the enum. Two tables, one dispatcher. The Talaria-private numbers (1000+, 4600+) stay identical |
| The ABI STRUCTS the kernel writes into the process (`fs.rs` `stat`, `netsys.rs` `epoll_event` packed 12 bytes, `proc.rs` `sigaction` with restorer, `syscall.rs` `ucontext` 936 bytes + XSAVE, `utsname` "x86_64", `AT_PLATFORM` "x86_64") | `struct stat` arm64 (u32 `st_nlink` @ 20, `st_blksize` i32, another padding), `epoll_event` 16 bytes aligned, `sigaction` {handler, flags, mask} — NO restorer, `sigcontext` {fault_address, regs[31], sp, pc, pstate, `__reserved[4096]` with `fpsimd_context` magic `0x46508001` and `esr_context`}, `uname` "aarch64", `AT_PLATFORM` "aarch64", `AT_HWCAP` bits (FP, ASIMD, AES, PMULL, SHA2, CRC32, ATOMICS, CPUID...) | M | Silent: a wrong `stat` layout does not crash, `ls -l` shows garbage. Every struct gets a host unit test in `kernel-unit` against the C layout |
| `clone(flags, sp, ptid, ctid, tls)` | `clone(flags, sp, ptid, TLS, CTID)` — the last two swap | S | Silent: musl's `pthread_create` sets both; a swap gives a thread with a garbage TLS |
| `O_DIRECTORY`=0x10000, `O_NOFOLLOW`=0x20000, `O_DIRECT`=0x4000 | 0x4000, 0x8000, 0x10000 | S | Silent: `O_DIRECT` on x86 IS `O_DIRECTORY` on arm64 — a database opening its data file would get `ENOTDIR` or the opposite |
| TLS through `IA32_FS_BASE`, `arch_prctl(ARCH_SET_FS)` (`sched::FSBASE`) | `TPIDR_EL0`, set by `clone`'s tls argument and by musl's `__set_thread_area` → there is no syscall, libc writes the register itself | S | `FSBASE[t]` becomes `TPIDR[t]` in the same slot |
| `fxsave`/`xsave` 512/1024 bytes on syscall entry and on the switch (`sched::FPU`, `XSAVE_MASK`, `enable_avx`) | 32 × 128-bit `V` registers + `FPCR` + `FPSR` = 528 bytes; `CPACR_EL1.FPEN`; SVE OFF (`ZEN=0`) and not advertised | M | If SVE is later enabled the state is up to 32 × 2048 bits + predicates — a different save area and a different `sigcontext` record (`sve_context`) |
| GS.base per-CPU block, `mov gs:[off]` (`percpu.rs`) | `TPIDR_EL1` → `PerCpu*`; `ldr x, [tpidr, #off]` needs one `mrs` first — cache it in a callee-saved register inside the stubs | S | none |
| `cli`/`sti`, `pushfq` (`sync.rs`, `paging.rs`, everywhere) | `msr DAIFSet/DAIFClr, #2`, `mrs DAIF` | S | A `sti; hlt` window becomes `msr daifclr; wfi` — WFI wakes on a PENDING interrupt even if masked, so the one-instruction-window invariant W4 holds differently: check with a gate |
| `hlt`, `pause` | `wfi`, `yield`/`isb` | S | none |
| CR3 per task, `invlpg`, TLB shootdown by IPI (`paging.rs`, `syscall::NO_SHOOTDOWN`, `smp::ask_host_flush`) | `TTBR0_EL1` + a 16-bit ASID per process (`TCR_EL1.AS=1`); `TLBI VALE1IS`/`ASIDE1IS` broadcast to the inner-shareable domain — **no shootdown IPIs at all**; `DSB ISH; ISB` after every table write that is published; **break-before-make** on a change of a live block mapping (write invalid, TLBI, write new) | L | The break-before-make rule has no x86 analogue: the COW split of a 2M leaf into a 4K table (`install_table` P3) and the `protect` of a live range must go through an invalid entry. QEMU tolerates a violation; Neoverse cores do not (a TLB conflict abort or, worse, a stale translation) |
| 4-level PML4, `PTE_P/RW/US/NX/G`, PAT for WC/UC, 1G identity for GB1–3 with GB3 uncached (`paging.rs`) | 4K granule, 4 levels, `TTBR1` disabled (`TCR.EPD1=1`) so ONE table covers the whole low half exactly as today; `AP[1]` = EL0 access, `UXN/PXN` = NX per privilege, `nG` = not-global (a process page), `AF` must be set by software (no accessed-bit walk — set it on map); memory attributes through `MAIR_EL1` indices: Normal WB, Normal-NC (the LLQ write-combining window), Device-nGnRE (MMIO); `SCTLR.WXN` for W^X | L | `PXN` must be set on every user page or a kernel jump into user memory executes it; the "uncached GB3" trick becomes an attribute index, not a table flag |
| CR0.WP (the kernel faults on a user RO page — the COW protection of the kernel's own writes) | EL1 writes to a page that is RO for EL1 fault by construction (`AP[2]`); but a page RW for EL0 is RW for EL1 — the COW-on-fork protection relies on the page being RO for BOTH, which `AP` gives | S | none — but `paging::protect` must set `AP[2]` (RO) not only for EL0 |
| SMAP not used; kernel reads user memory with plain loads (`uaccess.rs`) | PAN exists (v8.1); leave `SCTLR_EL1.SPAN=1` so `PSTATE.PAN` is NOT set on entry and plain loads keep working — the same model as today's "no SMAP" | S | `uaccess::gate` (pointer validation) is arch-neutral and stays the protection |
| SMEP (`paging::enable_smep`) | `PXN` on user pages (see above) | S | none |
| LAPIC timer (periodic + TSC-deadline), `lapic_calibrate`, `tickmin`, the borrowed LVT (`smp.rs`, `sync.rs`) | The generic timer: `CNTV_CVAL_EL0`/`CNTV_CTL_EL0` per core, PPI 27 through the GIC; `CNTFRQ_EL0` is the frequency — no calibration; deadline mode is the ONLY mode, so periodic = re-arm | M | The counter's frequency is platform-defined (QEMU `virt` and Graviton differ; some SoCs run it at tens of MHz) — `clock.rs`'s `mult` arithmetic and the vDSO assume nanosecond-class resolution; add a gate that prints and judges `CNTFRQ` |
| `rdtsc`, `rdtscp`, TSC frequency from CPUID 0x15/0x4000_0010 (`tsc.rs`) | `CNTVCT_EL0` (with `ISB` before it — otherwise it can be read speculatively early); `CNTKCTL_EL1.EL0VCTEN=1` so EL0 and the vDSO read it | S | Architecturally synchronised across cores — the per-core TSC offset worry disappears |
| kvmclock / KVM CPUID leaves (`tsc.rs`, `smp::kvm_here`) | None. Hypervisor detection by SMCCC `ARCH_FEATURES`/`HYP_CALL_UID` or the ACPI OEM id; time reference on EC2 — the ENA PHC as on metal today (ROADMAP §2.3), or PTP over the network | M | The wall-clock servo (`clock.rs`) has NO cheap reference under KVM on arm64 — the metal problem arrives on every instance |
| LAPIC IPIs, `send_ipi`, `send_nmi`, PV IPI/PV EOI, x2APIC (`smp.rs`) | GICv3: SGIs through `ICC_SGI1R_EL1` (one register write, a target list per cluster); EOI through `ICC_EOIR1_EL1` (a system register — no MMIO window, no PV EOI needed); priority drop/`ICC_PMR`; `ICC_IAR1_EL1` to acknowledge | M | The GIC distributor/redistributor MMIO init (`GICD_CTLR`, `GICR_WAKER`, per-core `GICR` frames found through the DTB/MADT GICR structures) is real work; the NMI analogue is a pseudo-NMI by priority or plain SGI |
| I/O APIC for the virtio-mmio line (`ioapic.rs`) | SPIs in the distributor: enable, priority, route to a core (`GICD_IROUTER`) | S | none |
| MSI-X → a LAPIC vector by writing the address/data into the device (`pci.rs`, `ena.rs`, `nvme.rs`) | **The ITS**: a device table, a collection table, per-device interrupt translation tables in memory, the command queue (`MAPD`, `MAPC`, `MAPTI`, `INV`, `SYNC`), LPIs ≥ 8192 with a property table and per-redistributor pending tables; the MSI address is `GITS_TRANSLATER`, the data is the event id | **L** | The largest single new piece. Under QEMU `virt` with `its=on` it is checkable locally; on Graviton the ITS is present (Linux uses LPIs for MSI-X there). Without it: NO ENA and NO NVMe interrupts — polling only |
| INIT-SIPI-SIPI, the 16-bit trampoline, `start_ap` (`smp.rs`) | PSCI `CPU_ON` (an `HVC` under KVM/QEMU, an `SMC` on metal — the conduit is in the DTB `psci` node / the FADT ARM boot flags); the entry gets `x0 = context id`; the AP sets up its own SCTLR/MAIR/TCR/TTBR, `VBAR`, `TPIDR_EL1`, the redistributor, the timer | M | The MPIDR (the affinity id) plays the APIC id: `smp::logical_of` maps it densely as today; the SMT/physical-core topology (`smtpack`, CPUID 0x0B) comes from the PPTT ACPI table or the DTB `cpu-map` |
| The wait tiers: `monitor/mwait`, `mwaitx`, `umwait`, `hlt+IPI` (`sync.rs`, libtsync's ring-3 tier) | `LDAXR` (arms the exclusive monitor on the line) + `WFE`: a write to the line clears the monitor and wakes the core — the same "the waker pays nothing" contract as MONITOR/MWAIT, and it works from EL0 too (`SCTLR.nTWE=1` — WFE at EL0 does not trap). The timeout: `WFET` where `ID_AA64ISAR2_EL1.WFxT` reports it, otherwise the generic timer as today's borrowed LVT. `SEV`/`SEVL` for an explicit kick | M | WFE also wakes on ANY event (an interrupt, a spurious event) — the predicate re-check loop already exists (`waitpred`). The "one tier for both rings" is a simplification, but the measured wakeup numbers (201 ns umwait, 212 ns futex) must be re-taken |
| `lock`-prefixed RMW, `xchg`, implicit TSO | LSE atomics (`ldadd`, `cas`, `swp`) — `+lse` in the target; the ORDERING is explicit: see §4.13 | L | The audit |
| `mfence`/`fence(SeqCst)` before doorbells, `write_volatile` to MMIO (`ena.rs`, `nvme.rs`) | `DMB OSHST` (data to a device after descriptor writes), `DSB SY` where a doorbell must not pass a table write; MMIO through Device-nGnRE is ordered against itself but NOT against Normal memory without a barrier | M | ENA LLQ pushes 128-byte descriptors through a write-combining window (`talaria.test=wc`): on arm64 that is a Normal-NC mapping plus a `DSB ST` before the doorbell; Linux's `ena` does exactly that on Graviton, so the device side is known-good |
| DMA coherency assumed (`hal.rs` identity phys==virt, no cache maintenance) | On Nitro and on QEMU `virt` PCIe DMA is coherent (the Normal-WB mapping is what the device sees). Keep the assumption, state it | S | An arm64 board WITHOUT coherent PCIe would need `DC CIVAC` per buffer — out of scope, written down |
| The instruction cache is coherent with data writes | It is NOT: after loading an ELF's `.text`, after a COW copy of an executable page, after `mprotect(PROT_EXEC)`, and after `filed` writes a binary the kernel must `DC CVAU` + `IC IVAU` (or `IC IALLUIS`) + `DSB ISH; ISB` | M | Silent and intermittent: a stale icache line executes the previous instance's code. The JVM and QuickJS manage THEIR OWN JIT caches (they issue `IC IVAU` themselves) — only the kernel's writes matter. A gate: load a binary, run it, load a different one at the same address, run it |
| Port I/O `in`/`out` (`serial.rs`, `smp::legacy_quiesce`, `pci.rs` PORTIO, the PIT, the PIC) | None. PL011/16550-MMIO for the UART; PCI config through ECAM only (already the primary path); no PIT (the generic timer), no PIC | S | `serial::inb/outb` are used by `tsc.rs` (PIT) and `gdb.rs` (COM2): both become MMIO |
| VGA text window `0xB8000` (`vga.rs`) | None; `talaria.console=vga` refuses | S | none |
| ACPI: MADT (LAPIC/x2APIC entries), MCFG, FADT `\_S5` poweroff, PM1a (`acpi.rs`) | MADT with GICC (per-core: MPIDR, the redistributor base, the PPI numbers), GICD, GICR, ITS entries; GTDT (the timer PPIs); MCFG (identical); SPCR (the UART); PPTT (topology); poweroff/reboot through PSCI `SYSTEM_OFF`/`SYSTEM_RESET` — no AML at all | M | The MADT walk is a new set of subtable types; the `\_S5` hack goes away |
| PCIe ECAM, BAR assignment, bridge windows (`pci.rs`) | Identical (the ECAM base from MCFG or the DTB `pcie` node); the 64-bit BARs above 4G are the norm on `virt` — the code already handles 64-bit BARs | S | The `virt` machine's ECAM sits at `0x4010000000` (above 4G): the identity map must cover it (a 1G block) |
| The vDSO in x86 asm (`user/vdso/vdso.S`, `rdtsc`, `shrd`) | An aarch64 `vdso.S`: `isb; mrs cntvct_el0`, `umulh` for the 64×64→128 multiply; the symbol names are `__kernel_clock_gettime`, `__kernel_gettimeofday`, `__kernel_clock_getres`, `__kernel_rt_sigreturn`, version `LINUX_2.6.39` — musl and glibc look for THOSE on arm64 | M | The `rt_sigreturn` trampoline is MANDATORY: without `SA_RESTORER` the kernel points `lr` at it when building a signal frame (`syscall::deliver`) |
| The ELF loader checks `EM_X86_64`, writes `AT_PLATFORM`/`AT_HWCAP` (`user.rs`, `elfhdr.rs`) | `EM_AARCH64` (183); arm64 ELFs commonly carry `p_align = 0x10000` (64K) to be loadable on 64K-page kernels — with a 4K granule that is fine, but the loader's "segment alignment == 4K" assumptions must accept 64K | S | `-Wl,-Ttext-segment=0x10000000` stays the link recipe |
| Signal delivery: the frame on the user stack, `restorer`, `fpstate` (`syscall.rs` §2455) | The arm64 frame: `rt_sigframe` = `siginfo` (128) + `ucontext` with the 4096-byte `__reserved` area holding `fpsimd_context` and `esr_context` records, 16-byte aligned; `lr` = the vDSO trampoline; `x0 = sig, x1 = &siginfo, x2 = &ucontext` | M | glibc's and musl's `sigsetjmp`/unwinders read the record chain — a wrong magic and the handler returns into garbage |
| The gdb stub: x86 registers, DR0/DR7 watchpoints (`gdb.rs`) | The aarch64 register file (`x0–x30, sp, pc, cpsr`, the `V` file in the `g` packet), `target.xml`; hardware breakpoints/watchpoints through `DBGBCR/DBGBVR`, `DBGWCR/DBGWVR` (the watchpoint width is a byte mask up to 8 bytes — the "fixed at 4 bytes" limitation lifts) | M | none |
| Core dumps `EM_X86_64`, `NT_PRSTATUS` as `user_regs_struct` (27 words), `fs_base` (`coredump.rs`, `elfnote.rs`) | `EM_AARCH64`, `NT_PRSTATUS` as `user_pt_regs` (31 + sp + pc + pstate = 34 words), `NT_FPREGSET` as `user_fpsimd_state`, `NT_ARM_TLS` | S | `elfnote` is host-tested — the new layouts get the same tests |
| Frame-pointer unwinding along `rbp` (`obs/unwind.rs`) | `x29`: the frame record is `[fp] = previous fp, [fp+8] = lr` — the SAME shape as x86's `[rbp], [rbp+8]`, so the walker changes only its register source; `-C force-frame-pointers` is the norm for aarch64 Rust and the 4 % measurement of `config.toml` must be re-taken | S | none |
| The PMU through MSRs, `rdpmc`, architectural events (`pmu.rs`) | PMUv3: `PMCR_EL0`, `PMCNTENSET_EL0`, `PMEVTYPERn_EL0`/`PMEVCNTRn_EL0`, `PMCCNTR_EL0`; common events `0x08` INST_RETIRED, `0x11` CPU_CYCLES, `0x17` L2D_CACHE_REFILL, `0x2B`/`0x2A` L3D_CACHE(_REFILL), `0x10` BR_MIS_PRED, `0x21` BR_RETIRED; `PMUSERENR_EL0` for EL0 reads; a PPI (23 on `virt`) for the overflow used by `obs::perf` sampling | M | The "LLC misses" column has no architectural guarantee — the L3 events are IMPLEMENTATION DEFINED (Neoverse has them; QEMU's TCG PMU counts almost nothing). Under KVM Graviton exposes the PMU to guests |
| CAT (`cat.rs`, `llc=` in the manifest) | MPAM exists in the architecture; Graviton does not expose it; QEMU does not model it. `llc=` refuses with a named reason, `cat::report_footprint` (L3 size from the cache-geometry registers `CCSIDR_EL1`/`CLIDR_EL1`) stays | S | none |
| AES-128-GCM on AES-NI + PCLMULQDQ (`crypto.rs`) | ARMv8 CE: `aese`/`aesmc` for the rounds, `pmull`/`pmull2` for GHASH, `core::arch::aarch64` intrinsics under `#[target_feature(enable = "aes,neon")]` | M | The 111 ns/96-byte record number is x86's; re-measure |
| KVM steal time from the PV `steal` MSR (`smp::steal_ns`, `vcpu_preempted`) | ARM PV time (`PV_TIME_ST`, SMCCC `0xC5000021`) if the host exposes it — Nitro does not announce it today | S | The "vCPU theft" analysis of scoreboard 41 (five lost points) cannot be reproduced by instrument on arm64 without it; the "tick hole" instrument (`talaria.test=tickhole`) still works — it names the hole, not the owner |
| The UEFI stub (`x86_64-unknown-uefi`, drops to 32-bit, copies the kernel to 2M, jumps to `pvh_start`) | `aarch64-unknown-uefi`: no mode drop; `ExitBootServices`, copy the `Image` to its load address (or run in place — arm64 kernels are position-independent by convention, ours is `ET_EXEC` at a fixed address as on x86), hand `BootInfo` in `x0`; the ACPI RSDP from the configuration table exactly as today | M | The MMU is ON with UEFI's tables when the stub runs (unlike the x86 stub which turns paging off): the copy target must not be under a live UEFI mapping — `AllocatePages(AllocateAddress)` at the kernel's address before `ExitBootServices`, or a relocatable kernel |
| `tboot` (MBR stage1/2), `mk-bios.sh`, multiboot2, `mk-tboot.sh` | Not applicable: there is no BIOS on arm64 EC2. The metal path IS the UEFI stub | — | none |
| `mk-ami.sh` (register-image x86_64, boot-mode) | `--architecture arm64 --boot-mode uefi`; the same `talaria-cmdline`/user-data plumbing | S | none |
| `kernel/user/*` (musl-gcc x86_64 in docker), libtsync's `umwait` tier and `rdtsc` (`tsync.c` 18 asm sites) | An `aarch64-linux-musl` cross toolchain image (musl.cc or Alpine `--platform linux/arm64` under binfmt) — `make ARCH=aarch64`; libtsync: `LDAXR/WFE` tier, `cntvct_el0`, `yield` | M | The same sources, a second output directory (`user/aarch64/`), `user_elf!` picks by `target_arch` |
| `services/*` (twelve open-source workloads built static with musl at `0x10000000`) | The same recipes on an arm64 BUILDER (a `c7g` with docker), because cross-building PostgreSQL/MariaDB/MongoDB/JVM/ClickHouse from an x86 box under binfmt is days of emulated compile time. ClickHouse and OpenJDK have official aarch64 builds; the patched musl (`ld-musl-aarch64.so.1`) is rebuilt with the same patch | L (mostly machine time) | The dynamic-ELF path (`mongodb`) exercises `ld-musl-aarch64`, relocations `R_AARCH64_*` in `user.rs`'s loader if it does any (it defers to the dynamic loader — check) |
| `bench/suite`, `tlgen`, `run-remote*.sh`, terraform `main.tf` | Arch-neutral; `instance_type` and the AMI's architecture become parameters; the generator node is a `c7g` running Linux arm64 | S | none |
| `kernel-unit` (host tests of pure modules) | Runs on the x86 host as today; the new ABI-layout tests (§4.4) are host tests too; nothing arm64 executes there | S | none |
| CI (`.github/workflows/ci.yml`) | `cargo build` for both targets, clippy for both, `kernel-unit`; the TCG smoke (boot to the prompt) is possible on a hosted x86 runner; the KVM subset needs the arm64 metal (self-hosted, on demand) | M | The x86 CI has never run either (ROADMAP §2.1) |

---

## 4. Design decisions, subsystem by subsystem

### 4.1. The source layout

```
kernel/src/arch/mod.rs            pub use of the selected arch; the SHARED surface
kernel/src/arch/x86_64/{boot.rs, trap.rs (idt), syscall_entry.rs, percpu.rs, mmu.rs (the leaf ops of paging.rs),
                        timer.rs, irq.rs (lapic + ioapic), smp.rs (the trampoline), cpu.rs (cpuid/msr/sti/hlt),
                        fpu.rs, uart.rs (16550 pio), pmu.rs, crypto.rs, unwind.rs, gdb_regs.rs, coredump_regs.rs, abi.rs}
kernel/src/arch/aarch64/{boot.rs, trap.rs, syscall_entry.rs, percpu.rs, mmu.rs, timer.rs, gic.rs, its.rs, psci.rs,
                         cpu.rs, fpu.rs, uart_pl011.rs, uart_16550_mmio.rs, pmu.rs, crypto.rs, unwind.rs, gdb_regs.rs,
                         coredump_regs.rs, abi.rs, dtb.rs, icache.rs}
kernel/aarch64-talaria.json, kernel/linker-aarch64.ld
kernel/user/vdso/{vdso-x86_64.S, vdso-aarch64.S}
```

The rule for the split: a function moves under `arch/` if and only if it
contains `asm!`, a hardware register layout or an ABI constant. `paging.rs` keeps
the walks, COW, the refcounts and the process spaces (2 000 of its 2 660 lines);
`arch::mmu` gets the PTE bit definitions, `switch`, `flush_*`, the attribute
indices and — new for arm64 — `break_before_make`. `sched.rs` keeps the policy
and the task tables; `arch::switch` gets the two stubs (`timer_stub_sched`, the
context entry) and the FPU save/restore. `smp.rs` keeps the topology and the
per-core timer knobs; `arch::smp` gets the bring-up and the IPI send.

Everything in `arch::x86_64` is TODAY's code moved, not rewritten — that is how
phase 0 can be judged by an unchanged regression count.

### 4.2. Boot

**QEMU `virt`, the fast loop.** The kernel image starts with the 64-byte Linux
arm64 header (`code0/code1` as a branch to the entry, `text_offset`, `image_size`,
flags = LE, 4K, `MAGIC "ARM\x64"`), so `qemu-system-aarch64 -kernel talaria` loads
it, puts the DTB's address in `x0`, and enters at EL1 with the MMU off and the
caches in an unknown state. The entry: `.bss` zero (the A9 lesson applies —
under QEMU memory is zero, on a board it is not), the static identity tables
(1G blocks for RAM and the PCIe windows, Device attributes for the UART/GIC
region), `MAIR/TCR/TTBR0/SCTLR`, the stack, `bl rust_main(x0 = &BootInfo)`.
`BootInfo` is filled by `arch::aarch64::dtb` from the flat device tree: memory
ranges, `bootargs`, the UART base and type, the GIC distributor/redistributor/ITS
bases, the timer PPIs, the virtio-mmio windows and their SPIs, the PSCI conduit.
A device tree is a nested byte structure with a string table — ~300 lines, no
allocation, checked by a host unit test against a DTB dumped from QEMU.

**EC2 and `edk2`, the product path.** The `aarch64-unknown-uefi` stub does what
`kernel/uefi-stub` does today minus the mode drop: `LoadOptions` → cmdline, the
RSDP from the configuration table, `GetMemoryMap` → `ExitBootServices`, then
jumps into the same entry with `BootInfo` pre-filled from ACPI (`MADT` GICC/
GICD/GICR/ITS, `GTDT`, `MCFG`, `SPCR`, the memory map). The kernel below sees
one `BootInfo` in both cases; `acpi.rs` keeps parsing the RSDP for what the DTB
path does not carry (PPTT, the poweroff method) when the RSDP is present.

**Where the kernel sits.** `ET_EXEC` at a fixed physical address exactly as
today (`KBASE`, default 2M), identity mapped. On `virt` RAM starts at
`0x4000_0000`, so `KBASE` becomes a per-arch default (`0x4020_0000`) — the
`ClickHouse at 2M..526M` collision logic of `linker.ld` is unchanged in kind.
On EC2 arm64 the RAM base is whatever the UEFI memory map says: the stub
relocates the image to `KBASE` if that range is conventional memory, and refuses
with a message if not (the same contract as x86's "copy to 2M").

### 4.3. The MMU

One table, `TTBR0_EL1` only, `TCR_EL1.EPD1 = 1`: the process model of x86
("the kernel is identity-mapped in every process's tables, user pages are
marked user") carries over exactly, and the per-process address spaces
(`new_process_space`, COW, `protect`) keep their shape. Per-page rights map
1:1: `US` → `AP[1]`, `RW` → `!AP[2]`, `NX` → `UXN` (user) and `PXN` (kernel —
set on EVERY user page, that is the SMEP), `G` → `!nG`. `PTE_A`/`PTE_D` have no
hardware-set equivalent without `FEAT_HAFDBS` — the kernel sets `AF=1` on every
mapping it creates (a missing `AF` is an access fault, not a slow path) and does
not use the dirty bit anywhere today (checked: the block cache tracks dirtiness
itself).

**ASIDs.** Each process space gets a 16-bit ASID (`TCR.AS = 1`) allocated with
the space and written into `TTBR0[63:48]` on the switch; the switch is then
`msr ttbr0_el1; isb` with no flush — the analogue of PCID that x86 Talaria
never used. When the allocator wraps (65 536 processes) the whole TLB is
flushed (`TLBI VMALLE1IS`) and the generation bumps. `NTASKS` is small enough
that the wrap is theoretical; the gate `talaria.test=asidwrap` forces it.

**Shootdowns disappear.** `paging::shootdown` becomes `TLBI VALE1IS, <va|asid>`
(one page) or `TLBI ASIDE1IS` (a space) followed by `DSB ISH` — the hardware
broadcasts to every core in the inner-shareable domain, including the cores
that are in `WFI`. The IPI path, `NO_SHOOTDOWN`, the `ask_host_flush` PV hint
and the "a neighbour in hlt does not confirm" special case of session 27 are
x86-only and stay under `arch/x86_64`.

**Break-before-make.** Changing a LIVE valid entry to another valid entry
(splitting a 2M leaf into a table for a COW write, changing the output address
or the attributes) requires: write the invalid entry, `DSB ISH`, `TLBI` for the
range, `DSB ISH`, write the new entry, `DSB ISH; ISB`. Two places do this today
with a CAS from valid to valid (`install_table`, P3) and one with a plain store
(`protect` of a live range). Both get an `arch::mmu::replace_live(entry, new)`
that does the dance on arm64 and is a plain CAS on x86. The window in which
the entry is invalid is visible to another thread of the same process as a
fault — the fault handler already retries on "the entry changed under me" (P3's
loser path), so the retry is reused.

**Memory attributes.** `MAIR_EL1` index 0 = Normal WB (RAM), 1 = Normal-NC (the
ENA LLQ push window, today's `PAT` write-combining type), 2 = Device-nGnRE
(all MMIO: the GIC, the UART, ECAM, BARs), 3 = Device-nGnRnE (the ITS command
queue doorbell, where a strongly ordered write is wanted). `talaria.test=wc`
(today: PAT and the page-type change) becomes "the LLQ window is Normal-NC and
a `DSB ST` precedes the doorbell", checked by reading the attribute back
through `AT S1E1R` + `PAR_EL1`.

**Alignment.** Device memory faults on unaligned access; Normal memory does not
(`SCTLR.A = 0`) but atomics do. The `packed` `epoll_event` write
(`netsys.rs:3457`) is a Normal-memory unaligned store — legal — but the arm64
`epoll_event` is not packed anyway (§4.4). Every `read_volatile` of a device
register is naturally aligned already (checked in `ena.rs`/`nvme.rs`); a gate
turns `SCTLR.A = 1` on for one boot and runs the storage and network tests to
catch any that are not.

### 4.4. The Linux arm64 ABI

**Numbers.** `arch::abi::Nr` is an enum of the calls the kernel implements
(today's ~140 + the private ones). `arch::x86_64::abi::decode(nr) -> Option<Nr>`
is the x86 table; `arch::aarch64::abi::decode` is the asm-generic one.
`dispatch` and `uaccess::gate` match on `Nr`. The legacy calls that arm64 lacks
(`open`, `fork`, `stat`, `pipe`, `dup2`, `poll`, `select`, `epoll_create`,
`epoll_wait`, `getdents`, `time`, `access`, `mkdir`, `unlink`, `rename`,
`readlink`, `eventfd`, `signalfd`, `arch_prctl`, `alarm`, `utimes`) keep their
handlers — they are reached only through the x86 table. The arm64 table adds
what musl-aarch64 actually uses and x86 musl does not: `openat` (already there),
`fstatat`/`newfstatat` (already there), `pipe2`, `dup3`, `ppoll`, `pselect6`,
`epoll_create1`, `epoll_pwait`, `getdents64`, `faccessat`, `mkdirat`,
`unlinkat`, `renameat`, `readlinkat`, `eventfd2`, `signalfd4`, `clock_gettime`
(the vDSO handles the hot one) — most exist on x86 too and are already
implemented; the gap is checked by running the whole `kernel/user` set with
`talaria.strace=1` and listing every `ENOSYS`.

**Argument order.** `clone` on arm64: `(flags, stack, parent_tid, tls,
child_tid)`. The decoder normalises the arguments into `Frame.a1..a6` in the
INTERNAL order (x86's), so `sys_clone` does not know. The same for the two
other arch-swapped calls Linux has (`clone3` is uniform; `mmap` is uniform).

**Structs the kernel writes.** A table in `arch::abi` of layouts with host tests
in `kernel-unit` that compile the C struct through `bindgen`-free hand-written
offsets and compare:

| struct | x86-64 | arm64 | where the kernel writes it |
|---|---|---|---|
| `stat` | 144 bytes, `st_nlink` u64 @16, `st_mode` u32 @24 | 128 bytes, `st_mode` u32 @16, `st_nlink` u32 @20, `st_blksize` i32 @56 + pad | `fs.rs:1853`, `ext2.rs`, `syscall.rs` virt files |
| `epoll_event` | 12 bytes packed | 16 bytes, `data` @8 | `netsys.rs:3457` |
| `sigaction` (kernel) | {handler, flags, restorer, mask} 32 bytes | {handler, flags, mask} 24 bytes | `proc.rs:337` |
| `ucontext`/`sigcontext` | `gregs[23]`, `fpstate*`, 936 bytes + XSAVE | `fault_address`, `regs[31]`, `sp`, `pc`, `pstate`, `__reserved[4096]` (record chain: `fpsimd_context` magic `0x46508001` size 528, `esr_context` `0x45535201`, terminator) | `syscall.rs` §2455 |
| `siginfo` | 128 bytes | same layout | unchanged |
| `user_regs_struct` (core dump) | 27 words | `user_pt_regs` 34 words + `NT_FPREGSET` | `coredump.rs` |
| `utsname.machine` | "x86_64" | "aarch64" | `syscall.rs:1480` |
| `O_DIRECTORY/O_NOFOLLOW/O_DIRECT/O_LARGEFILE` | 0x10000/0x20000/0x4000/0x8000 (libc shows `O_LARGEFILE` as 0) | 0x4000/0x8000/0x10000/0x20000 | every `open*` handler, the `fcntl` F_GETFL path |
| `auxv`: `AT_PLATFORM`, `AT_HWCAP`, `AT_HWCAP2` | "x86_64", the x86 bits | "aarch64"; `HWCAP_FP|ASIMD|EVTSTRM?|AES|PMULL|SHA1|SHA2|CRC32|ATOMICS|FPHP|ASIMDHP|CPUID|ASIMDRDM|...` — announce ONLY what `CPACR`/`ID_AA64ISAR0_EL1` allow; NOT `SVE` | `user.rs:186` |
| `struct termios`, `winsize`, `rlimit`, `sysinfo`, `timespec`, `iovec`, `msghdr`, `sockaddr*`, `dirent64`, `statx`, `statfs` | same | same | unchanged |

**TLS.** `TPIDR_EL0` is the thread pointer; musl's `__set_thread_area` on arm64
writes it directly (no syscall), and `clone`'s `tls` argument sets it for a new
thread. `sched::FSBASE[t]` is renamed to a neutral `TP[t]`; the save/restore on
the switch reads/writes `TPIDR_EL0`. `arch_prctl` returns `ENOSYS` (it is not in
the arm64 table anyway). `TPIDRRO_EL0` is left zero.

**Syscall restart.** `SA_RESTART` today rewinds `rip -= 2` and reloads `rax`
with the number. On arm64: `pc -= 4`, `x8` is untouched by the kernel (the
return value goes to `x0`, the number stays in `x8`) — the restart needs the
ORIGINAL `x0`, which the frame keeps.

### 4.5. Exceptions and the syscall entry

`VBAR_EL1` points at a 2K-aligned table of 16 entries (four exception types ×
four sources); only "current EL with SP_EL1" (the kernel faulting or an IRQ in
the kernel) and "lower EL, AArch64" (EL0 syscalls, faults, IRQs) are populated;
the others land in a panic that prints `ESR`. The frame pushed by the stubs is
`x0..x30, sp_el0, elr_el1, spsr_el1, esr_el1, far_el1` (37 words) — it plays
`ExcFrame` for `idt::handle` AND `Frame` for `rust_syscall`: on arm64 they can
be ONE structure, which removes today's two layouts (`idt::ExcFrame`,
`syscall::Frame`, and `gdb::Frame` that mirrors them).

*Done on the x86 side (2026-09-30):* `syscall::Frame` is now
`arch::syscall_entry::Frame`, and the shared `syscall.rs` reads it by NEUTRAL names
only — `nr`, `a1..a6`, `pc`, `flags`, `usp`; the callee-saved registers (rbx, rbp,
r12..r15 on x86) are read by nobody outside `arch::signal`, which holds the whole
of the `ucontext`/`gregs` layout, the fork child's and the core dump's register
picks, and the `rt_sigreturn` restore (`arch/x86_64/signal.rs`). The dispatcher's
copy with translated arguments is `Frame::with_args`. So the arm64 `Frame` is a
struct with the SAME leading field names, and its SVC stub simply stores in that
order: x8 → `nr`, x0..x5 → `a1..a6`, `elr_el1` → `pc`, `spsr_el1` → `flags`,
x19..x29 → the callee-saved slots, `sp_el0` → `usp`, and the rest of the file
(x6, x7, x9..x18, x30) in trailing slots — the Linux arm64 contract preserves EVERY
register but x0 across `svc` (musl's wrapper clobbers only x0 and memory), so a
full save is owed anyway, and only the ORDER is the port's choice. The signal
delivery's writes into `a1..a3` and `pc` then land in x0..x2 and `elr` for free. The sync-exception
handler dispatches on `ESR.EC`: `0x15` SVC → `rust_syscall`; `0x20/0x24` data
and instruction aborts from EL0 → the page-fault path with `FAR` as CR2 and the
`ISS.WnR` bit as the write flag; `0x07` FP/SIMD trap (if lazy FPU is ever
done); `0x0E` illegal state; `0x3C` BRK → the gdb stub; `0x22`/`0x26` PC/SP
alignment → SIGBUS as Linux does.

The kernel stack per task and the entry switch: on EL0→EL1 the hardware
switches to `SP_EL1` automatically — the "`mov %gs:8, %rsp`" of the x86 stub is
`SP_EL1` being set to `kstack_top` when the task is scheduled, and `sp_el0` in
the frame is the user stack. `sig_newrsp` (the user-sp substitution on signal
delivery) becomes a write into the frame's `sp_el0` slot before `ERET`.

The FPU state on entry: the kernel is built hard-float and clobbers `V`
registers, so — as with `fxsave` today — the stub saves the 528-byte FP/SIMD
state on the kernel stack on every entry and restores it on exit. That costs on
the order of the `fxsave` today (measured on x86: it is inside the 95 ns
syscall). A LAZY scheme (save only if the kernel touched `V`, i.e. compile the
kernel soft-float and save the user state only on a context switch) is the
first arm64-specific optimisation candidate and is measured, not assumed
(`mbench`).

### 4.6. Per-CPU

`TPIDR_EL1` holds the `PerCpu*`; the offsets stay (`ursp` 0, `kstack_top` 8,
`sig_newrsp` 16, `cpu` 24, `cur_task` 32, `sched_ttbr` 40, `pv_eoi` 48 — unused
on arm64, kept for the layout — `off_stack` 56). The stubs do `mrs x9,
tpidr_el1` once and address off `x9`. `percpu::cpu_id()` is `mrs; ldr`, two
instructions against x86's one — noted, not a concern.

### 4.7. FP/SIMD, SVE

The kernel enables FP/SIMD for EL0 (`CPACR_EL1.FPEN = 0b11`) and keeps SVE
disabled (`ZEN = 0b00`) in the first version: a workload that executes an SVE
instruction traps (`EC = 0x19`) and is killed with SIGILL, and `AT_HWCAP` does
not advertise SVE, so libc and the JVM do not select SVE paths. The reason is
size and layout: the SVE state is up to 32 × 2048-bit `Z` + 16 predicates +
FFR, it needs its own `sve_context` record in the signal frame, and a scheme
where the vector length is per-thread. ClickHouse's official aarch64 build and
OpenJDK run on NEON; PostgreSQL/MariaDB/MongoDB/redis/nginx do not care.
Enabling SVE later is an item with its own gate, not a blocker.

### 4.8. The interrupt controller and MSI-X

GICv3 in three parts. **The distributor** (`GICD_*`, one MMIO frame): enable
group 1, program SPIs (the virtio-mmio lines on `virt`, nothing on EC2 — every
device there is MSI-X). **The redistributor** (`GICR_*`, one 128K frame per
core, found through the DTB `reg`/the MADT GICR/GICC entries): wake the core's
frame (`GICR_WAKER`), enable PPIs (the timer 27, the PMU 23), set the LPI
property/pending tables. **The CPU interface** (`ICC_*` system registers):
`SRE = 1`, `PMR` to unmask, `IGRPEN1`, `IAR1`/`EOIR1` per interrupt, `SGI1R`
to send IPIs. Vectors become INTIDs: SGIs 0–15 are the IPI "vectors" (the
scheduler's doorbell, the migration kick, the NMI analogue at a higher
priority), PPIs 16–31 are per-core, SPIs 32–1019 are the wired lines, LPIs
8192+ are MSI-X.

**The ITS** (`GITS_*` frame + in-memory tables): `GITS_BASER` device and
collection tables, the command queue (`GITS_CBASER/CWRITER/CREADR`), one
collection per core mapped to its redistributor (`MAPC`), one device per
PCIe function (`MAPD` with a per-device ITT sized for its MSI-X count), one
`MAPTI` per MSI-X vector (device id = the PCIe requester id `bus:dev.fn`, event
id = the vector index, LPI = an allocated INTID, collection = the target core),
`INV`/`SYNC` to publish. The device's MSI-X table entry gets
`address = GITS_TRANSLATER, data = event id`. Under QEMU `virt,its=on` this is
checkable locally; the gate `talaria.test=its` maps a virtio-pci or NVMe
vector, fires it, and checks the LPI arrives on the chosen core — TCG models
the ITS faithfully enough for the control path. On Graviton the device ids are
what the SMMU/IORT would say; without an SMMU (we do not use one) the requester
id is the device id, and the `IORT` table names the ITS group — read it when
present.

ENA's per-queue MSI-X and NVMe's completion interrupt then work as today; the
"the frame ITSELF wakes the machine" property of session 21 is preserved.

### 4.9. Time, the tick and the wait tiers

The generic timer is per core, deadline-only, and its counter is the clock:
`tsc::rdtsc()` → `arch::clock::now()` = `isb; mrs cntvct_el0`; `tsc::calibrate`
→ `mrs cntfrq_el0`. The tick is `CNTV_CVAL = now + period` re-armed in the
handler (TSC-deadline mode is today's `talaria.tsc_deadline` path made the only
one); `lapic_timer_stop/restore/borrow` map to `CNTV_CTL.ENABLE`. The
`tickmin` floor, `netslice`, `idleslice`, `retune_timer` are arithmetic on
nanoseconds and stay.

Two consequences to measure early. First, the RESOLUTION: `CNTFRQ` is 62.5 MHz
on QEMU `virt` of the pinned 8.2 line (16 ns; recent machine types report 1 GHz) and
1 GHz on Graviton; the vDSO's
`mult` and `clock.rs`'s servo assume a fine counter — a gate prints the
frequency and refuses below 10 MHz. Second, the wall-clock REFERENCE: there is
no kvmclock; under KVM on arm64 the guest has only the counter, so the servo's
reference is the ENA PHC (the metal problem of ROADMAP §2.3 now on every
instance) or NTP/PTP over the network — decide when the stack is up, and until
then `CLOCK_REALTIME` is set once from IMDS at boot as on metal today.

The wait tiers collapse to one: `sync::MODE_WFE`. The sleeper does `ldaxr` on
the word (arming the exclusive monitor), re-checks the predicate, `wfe`; any
store to that line clears the monitor and wakes it. From EL0 the same sequence
works with `SCTLR.nTWE = 1` — libtsync's ring-3 tier is that. The timeout uses
the core's own timer as today's borrowed LVT; `WFET` (v8.7) replaces it where
`ID_AA64ISAR2_EL1.WFxT` says so. The `hlt+IPI` fallback is `wfi` + an SGI and
stays as the mode when a hypervisor traps `WFE` (KVM does NOT trap WFE by
default but may on contention — `kvm_arm_vcpu_wfx` — measure). The wakeup
latency numbers (201/212 ns) are re-taken with `futexlat`.

### 4.10. SMP

`PSCI_CPU_ON(mpidr, entry, context)` per core, through `HVC` under a hypervisor
and `SMC` on metal (the conduit from the DTB or the FADT). The AP entry is the
same asm as the BSP's minus the `BootInfo` parse: MMU registers from the BSP's
values, `SP_EL1` = its bring-up stack, `TPIDR_EL1`, `VBAR`, its redistributor
and timer, then `rust ap_main(cpu)` exactly as `ap_entry` today. `MPIDR_EL1`
(Aff0..Aff3) is the "APIC id": `smp::logical_of` maps it densely, and the
physical-core/SMT topology (`smtpack`, `phys_topology`) comes from PPTT or the
DTB `cpu-map` — Graviton has no SMT, so the pack order is the identity there,
and the whole SMT programme of sessions 34–35 is x86-only by nature.

### 4.11. Devices

PCIe through ECAM is unchanged in logic; the `virt` ECAM is at `0x4010_0000_00`
(above 4G) and the 64-bit BAR window at `0x80_0000_0000` — the identity map
covers them with 1G Device blocks placed from the DTB/MCFG rather than a
hard-coded "GB3 is uncached". No port I/O anywhere: `pci::PORTIO` is x86-only.
virtio-mmio on `virt` is found from the DTB (`virtio_mmio@a000000` nodes,
SPIs 48+) instead of the microvm's fixed windows. ENA and NVMe change only in
their barriers (§3): `fence(SeqCst)` → `arch::barrier::dma_wmb()` (`dmb oshst`)
before a doorbell, `dma_rmb()` after reading a completion; the LLQ push through
Normal-NC with `dsb st`. Their DMA arena and descriptors are Normal-WB and
coherent on both stands.

### 4.12. The instruction cache

Every kernel write of code the process will execute is followed by
`arch::icache::sync(range)`: `dc cvau` per line, `dsb ish`, `ic ivau` per line
(or `ic ialluis`), `dsb ish`, `isb`. The places: `user::load_elf*` after the
segment copy, the COW fault handler when the faulting page is executable
(`UXN = 0`), `mprotect` to `PROT_EXEC`, `filed` uploads followed by an
`execve`, and `sigreturn`'s trampoline page once at boot. The gate
`talaria.test=icache`: run a binary, overwrite its `.text` in place through the
kernel's path with another that prints a different line, run again, expect the
second line — under TCG the icache is coherent, so the gate is meaningful only
under KVM; it stays in the set because the failure mode on hardware is silent.

### 4.13. The memory-ordering audit

The kernel's cross-core protocols — the SPSC rings (`fastring`), the wait
predicates (`waitpred`, `sync`), the scheduler's doorbells and `AP_TAKEN`/
`CURRENT` claims (`sched`, C1/C2), the CAS-installed page tables (`paging` P3),
`netprof`'s counters, the metrics snapshots — were written on x86 and verified
by measurement on x86, where every store is seen in order by every core and
`Relaxed` differs from `Release` only in what the COMPILER may do. On arm64 a
`Relaxed` store followed by a `Relaxed` flag store can be observed in the other
order. The audit is one file at a time, by the counts above (`sched` 240
Relaxed + 68 volatile, `netsys` 98, `netprof` 95, `ext2` 74, `metrics` 71,
`mem` 59, `acpi` 59, `affinity` 58, `sync` 57 + 16, `smp` 55, `paging` 40 +
64, `ena` 38 volatile), with the question "is this a publish/consume pair?" —
a counter read for a printout stays `Relaxed`; a flag that says "the data is
ready" becomes `Release`/`Acquire`; a `read_volatile` of a word another core
writes becomes an atomic load with the ordering it needs. The 877 `SeqCst` are
correct by definition but cost `dmb ish` each on arm64: the hot ones (the
syscall path, the tick, `rqpick`) are downgraded to the weakest correct order,
and each such downgrade is a measured x86 commit too (principle 2).

The audit is NOT judged under TCG (the host is TSO) — it is judged by
`kernel/soak.sh` plus `shmrace`/`memrace`/`stealbal`/`schedbal` under KVM on
the Graviton metal for hours, and by reading. It is the phase most likely to
find bugs that were latent on x86 (the compiler reorders too, TSO or not).

**The first pass by reading (2026-09-29), the protocols that cross the ring-0/ring-3
or the core boundary:**

* `sync.rs` / `waitpred.rs` — the wait/wake protocol is ALREADY written for a weak
  machine: the registration (`WSLEEP`) and the wake marks (`WOKEN`) are `SeqCst`, and
  both sides of the Dekker store-load do an explicit `fence(SeqCst)` (`park_recheck`,
  the waker's scan) — a `dmb ish` on arm64. Nothing to change; the comment there
  records why the fence is on the wait side too.
* `fastpath.rs` / `fastring.rs` / `user/fastapp.c` — the zero-syscall SPSC rings. The
  producer published with `fence(Release)` and the consumer read the slot behind
  `fence(Acquire)` — correct — but the consumer published its index with a bare store,
  so on arm64 the producer could overwrite a slot whose bytes the consumer was still
  loading: a `fence(Release)` now precedes the `cons` store. The ring-3 side had
  compiler barriers only (`asm volatile("" ::: "memory")`): it now uses
  `__atomic_thread_fence` acquire/release at the three points (after reading `prod`,
  before publishing `prod`, before publishing `cons`) — nothing on x86, `dmb` on arm64.
* `paging.rs` — the table protocol (P3) is a `SeqCst` CAS with the table filled before
  it; `pte_w` is a volatile store and the TLB flush follows. Correct as is.
* `sched.rs` — the claims (`AP_TAKEN`, `CURRENT`, `BSP_IDLE`) and the doorbells are
  `SeqCst`; the `Relaxed` stores sampled are diagnostics (`GAP_*`, `ON_SINCE`, the
  snapshot registers) or knobs. Left for the KVM stand: `NW_IN`, `IO_YIELD`,
  `READY_CTX`, which a neighbour reads to decide — a `Relaxed` there is a decision on
  a stale flag, not a crash, and only a soak shows whether it matters.
* Device doorbells (`ena.rs`, `nvme.rs`): `fence(SeqCst)` before a doorbell is `dmb ish`
  on arm64, which orders the inner-shareable domain — a PCIe device is outer-shareable.
  When these drivers come over, the fences become `arch::barrier::dma_wmb` (`dmb oshst`)
  as §4.11 says; noted here so the audit does not count them as done.

**What the KVM stand found (2026-09-30, `c7g.metal`, Neoverse V1; the numbers in
`docs/MEASUREMENTS.md` §10):**

* `ring` — the `fastring` fence set as corrected above, 200 million 32-word slots
  between core 1 and core 0 at `-smp 2`, 4 and 8: zero corrupted slots, zero wrong
  lengths, in every run (about 9-10 million slots a second, 20-22 s a run). The
  producer/consumer protocol of `fastpath.rs` is correct on a weak machine as written.
* `bbm` — 48 regions × 2000 rights flips on one core under a reader on another: 49
  million reads, 776 thousand faults in the PROT_NONE phases, zero wrong values, zero
  TLB conflict aborts. The break-before-make discipline of `paging.rs` (§4.3) holds.
* `icache` — the frame rewritten WITH `icache::sync` answered anew every time; the
  rewrite WITHOUT it was ALSO observed coherent on this core in every repeat — the
  failure §4.12 warned about did not show in this form (one page, one core, a fresh
  frame). It is not evidence that the sync is unneeded; it is evidence that a gate for
  it needs a harder shape (a hot line on another core), and that is noted for phase 3.
* `svccost` — 55 ns with the eager 528-byte FP/SIMD save, 41 ns without: the save is
  14 ns, a quarter of the entry. The lazy-save decision (§4.5) stays open for phase 9.
* Fifty runs of the bring-up gate in a loop (250 boots of 4 cores: PSCI, the
  redistributor wake, SGIs): **0 of 50 failed**, 36/36 every time, about 5 s a run.

**The audit by reading, file by file (2026-10-04)** — what it found and changed, and what is
left for the stand and for the measured downgrades, is §10.10. Most of it was not ordering
but atomicity: words two cores rewrote with plain stores, as wrong on x86 as on arm64.

### 4.14. Observability

`obs/unwind` walks `x29` (`[fp] = caller's fp, [fp+8] = lr`); the kernel is
built with frame pointers on arm64 (the AAPCS64 default in Rust), so the 4 %
measurement behind `config.toml`'s "no frame pointers" is re-run and the
decision re-taken per arch. `pmu.rs` programs PMUv3 events with the common
numbers and reads `ID_AA64DFR0_EL1` for the counter count; the columns keep
their names, and the L3 columns show `n/a` where the implementation lacks the
event. The gdb stub speaks the aarch64 register set (and gains 8-byte
watchpoints). Core dumps carry `EM_AARCH64`, `NT_PRSTATUS` as `user_pt_regs`,
`NT_FPREGSET`, `NT_ARM_TLS` — verified as today by opening them in `gdb` and
reading the names.

### 4.15. Crypto

`crypto.rs` gets an `arch::aarch64::crypto` twin: AES rounds through
`vaeseq_u8`/`vaesmcq_u8`, GHASH through `vmull_p64`/`vmull_high_p64`, under
`#[target_feature(enable = "aes,neon")]`; the same API (`Aes128::new`,
`seal_in_place`). The TLS-in-place item of ROADMAP §4.6 quotes 111 ns/96 bytes
on AES-NI; the arm64 number is measured with the same harness.

---

## 5. The phases and their gates

Each phase ends with (a) the x86 regression at its current count, (b) the named
arm64 gate green, (c) a `docs/MEASUREMENTS.md` entry if a number was taken. The
weeks are estimates for one person who knows the tree.

| # | Phase | What is built | The exit gate | Est. |
|---|---|---|---|---|
| 0 | **The split** | `arch/x86_64/` by moving today's code (§4.1); `arch/aarch64/` as stubs that compile; `aarch64-talaria.json`, `linker-aarch64.ld`; `cargo build` for both targets, clippy for both; the toolchains on the dev box (rustup target, `qemu-system-aarch64`, `aarch64-linux-musl-cross`, `edk2-aarch64`) | x86: `test.sh` 486/486, `validate.sh` all PASS, `kernel-unit` green — UNCHANGED COUNTS. arm64: the ELF links | 1–2 w |
| 1 | **Boot to the console** | The `Image` header, the entry, the static tables, PL011, the DTB parser → `BootInfo`, `mem::init` from it, `paging::init` with the arm64 leaf ops (§4.3), the exception vectors with `ESR` diagnostics, `panic` to the UART | `qemu-system-aarch64 -M virt -kernel talaria` prints `talaria: boot`, the memory map, `free frames`; `talaria.test=paging|mem|cow` (kernel-only tests) pass under TCG | 2 w |
| 2 | **Cores and time** | GICv3 distributor/redistributor/CPU interface, the generic timer tick, `percpu` on `TPIDR_EL1`, PSCI bring-up, SGIs as IPIs, `TLBI IS` in place of shootdowns, `WFE` wait tier, `kwait_once` | `talaria.test=threads|ipi|tick|slice|smp2|smp4` under TCG for the logic, then under KVM on `c7g.metal` for the numbers; `talaria.smp=4` boots 4 cores | 2 w |
| 3 | **Ring 3** | The `SVC` entry and the unified frame (§4.5), `arch::abi` with both tables and the struct layouts + host tests (§4.4), TLS via `TPIDR_EL0`, FP/SIMD save/restore, the aarch64 vDSO with `__kernel_rt_sigreturn`, the signal frame, the ELF loader (`EM_AARCH64`, 64K `p_align`), `icache::sync`; `kernel/user` cross-built with `aarch64-linux-musl` into `user/aarch64/` | `hello|mhello|mprintf|pthreads|sigtest|futexlat|mbench|forktls|efault|cpptest*|fputest|memrace|shmrace` pass; `talaria.strace=1` over the set lists zero `ENOSYS`; `mbench` syscall cost recorded | 3–4 w |
| 4 | **Devices** | virtio-mmio from the DTB (the regression's disk and NIC), PCIe ECAM at `virt`'s addresses, **the ITS** (§4.8) with NVMe and virtio-pci MSI-X, ENA's barriers, the storage and network gates | `talaria.test=its`; the ext2/jbd2/fs block on virtio-blk AND on NVMe (`-device nvme`); the network block (`epsrv|mtsrv|tcp_bench|udp_ping|netshoot|connect`); the UEFI block with `edk2-aarch64` and the `aarch64-unknown-uefi` stub | 3 w |
| 5 | **The ordering audit** | §4.13 file by file; `arch::barrier`; the downgrades measured on x86 | `soak.sh` 4 hours under KVM on the metal with zero failures; `shmrace|memrace|stealbal|schedbal|fsyncconc` ×50 each; the x86 `mbench` unchanged or better after the downgrades | 2 w |
| 6 | **EC2** | ACPI → `BootInfo` (MADT GIC entries, GTDT, SPCR, PPTT, IORT), `mk-ami.sh --architecture arm64`, `talaria-cmdline` and user-data as today, the wall-clock reference decision (§4.9), ENA on Graviton (LLQ through Normal-NC), `validate-remote.sh` for `c7g` | `infra/aws/ec2-boot-talaria.sh` on a `c7g.large` reaches the mode prompt; `talaria.test=ena` + `phc`; `netsys` RTT against a Linux `c7g` neighbour recorded; `c7g.metal` boots through the stub | 2 w |
| 7 | **Workloads** | An arm64 builder (`c7g` + docker) for `services/*`; the patched `ld-musl-aarch64.so.1`; redis, nginx, mongoose, lua, sqlite, busybox, quickjs first, then PostgreSQL, MariaDB, MongoDB (dynamic), OpenJDK 21 aarch64, ClickHouse aarch64 | `validate.sh` B1–B12 PASS on the metal stand under KVM; the power-cut test on a `c7g` with EBS | 2–3 w |
| 8 | **Observability and docs** | PMUv3, `unwind` on `x29`, the gdb stub, core dumps, `obs::perf` sampling on the PMU PPI; `docs/DEVELOPMENT-GUIDE.md` §3/§5/§6/§7 gain the arm64 column; `BARE-METAL.md` gains the Graviton section; `USER-GUIDE.md` the arm64 build | `talaria.test=perf|unwind|coredump|gdb` under KVM; a `crashme` core opens in `gdb-multiarch` with names; `HANDS-ON-GUIDE` run once end-to-end on a `c7g` | 1–2 w |
| 9 | **The scoreboard** | `bench/suite` on `c7g` vs AL2023 arm64 on `c7g`: the same six workloads, the same ladders; the tick/syscall/wakeup instruments; the first arm64-specific optimisation (lazy FP save, §4.5) if `mbench` says the eager save is visible | A first arm64 scoreboard in `docs/MEASUREMENTS.md` with the same judging rules (§2.3's "two bring-ups", the 5 % boundary) | open |

The total to the end of phase 8: **18–22 weeks**. The critical path is
3 → 4 → 5: nothing about the workloads can be trusted before the audit, and the
audit needs the metal stand, which needs phase 6's boot path only partially
(KVM on the metal runs the QEMU `virt` machine — EC2's own boot is not on the
audit's path).

---

## 6. The regression on two architectures

**Today** the arm64 gate is `kernel/test-aarch64.sh`: one boot of the bring-up on `virt`
with four cores and 25 deterministic assertions on the verdict lines it prints (the tree,
the allocator, the tables, the dirty bit, the GIC, the tick, the secondaries, the IPIs,
the EL0 probe, the vDSO from EL0, and no trap anywhere) — CI runs it under TCG. When the
shared modes come over, it folds into `test.sh` as below.

`kernel/test.sh` gains `TAL_ARCH=x86_64|aarch64` (default: the host's), which
selects the kernel path, the QEMU binary, the machine type (`microvm`/`q35` vs
`virt,gic-version=3,its=on`), the firmware for the UEFI block (OVMF vs
`QEMU_EFI.fd`), the user-binary directory for the disk images, and the gate
list. A gate is one of three kinds, declared in the table `test.sh` already
keeps:

* **arch-neutral** (the vast majority: the FS, the network, processes, signals,
  the scheduler's policy, the console, `filed`, the observability's format);
* **x86-only**: `wc` (PAT), `cat`, `x2apic`, `pveoi`, `waitpkg`/`mwaitx`
  tiers, `smtpack`, `identhole`'s CR3 half, `tboot`/multiboot2 boots, the
  BIOS block — `SKIP (x86-only)` on arm64, counted separately so the total does
  not drift silently (the lesson of the stale uefi-stub: a silently smaller count
  looks like a regression);
* **arm64-only**: `its`, `psci`, `icache`, `asidwrap`, `bbm` (break-before-make
  under a COW storm), `wfe`, `cntfrq`, `align` (one boot with `SCTLR.A = 1`),
  `dtb` (a host unit test) — `SKIP (arm64-only)` on x86.

Under TCG the timing gates (`slice`, `futexlat`, `tick`, `mbench`'s thresholds,
`sysns`) are marked `SKIP (needs KVM)`; the deterministic subset (`TAL_CI=1`)
runs under TCG on the x86 dev box for the fast loop, the full set runs under
KVM on the arm64 metal. `validate.sh` follows the same switch.

---

## 7. Risks, ranked

1. **The weak memory model finds latent bugs in shared code** (§4.13) — likely,
   and the cure is reading plus hours of soak on real hardware. This is the one
   item that can move the estimate by weeks.
2. **The ITS** (§4.8) — the largest new driver, with a specification that is
   easy to get subtly wrong (the `SYNC`/`INV` discipline, the pending table's
   cache attributes, the collection re-targeting on migration). Mitigation: QEMU
   models it; Linux's `irq-gic-v3-its.c` is the reference for the command
   sequences.
3. **The ABI struct layouts are silent** (§4.4): nothing crashes, a database
   opens the wrong file or misreads a size. Mitigation: host tests per struct
   and the whole `kernel/user` set with `strace` before any service.
4. **Break-before-make** (§4.3): correct under QEMU whether or not it is done;
   wrong on Neoverse in a way that shows once an hour. Mitigation: the `bbm`
   gate under KVM, the fault-retry path already exists.
5. **The icache** (§4.12): same character — silent, hardware-only.
6. **The wall-clock reference under KVM on arm64** (§4.9): the servo has nothing
   cheap to lean on; the metal problem (ROADMAP §2.3) becomes every instance's.
7. **The value proposition must be re-measured from zero**: the tick-cost
   programme (session 19: 35.5 → 2.3 µs through PV EOI, x2APIC, TSC-deadline)
   used x86 levers; arm64's tick starts cheap (a system-register EOI, no MMIO,
   no PV needed) but this is a hypothesis until `talaria.test=tick` says so. The
   scoreboard's 295 points are an x86 result and say nothing about arm64.
8. **The stand's cost and availability**: `c7g.metal` on demand for every
   regression run; if the quota or the price does not allow it, the audit and
   the timing gates have no home. An alternative is any arm64 box with KVM
   (an Ampere workstation, an Apple-silicon Mac with HVF for the logic part).
9. **Toolchain drift**: the pinned nightly must build `aarch64-unknown-none`
   with `build-std` and `aarch64-unknown-uefi`; a bump of the pin is a deliberate
   commit with both regressions green (the existing rule).

---

## 8. Deliberately not done in the first version

| Item | Why not |
|---|---|
| SVE/SVE2 for the workload | The state's size and the per-thread vector length; no target workload needs it (§4.7). Its own item later. |
| A BIOS-like path, `tboot`, multiboot2 | arm64 EC2 is UEFI-only; the stub IS the metal path. |
| `TTBR1` (a split kernel/user address space), a high-half kernel | The product's model is one table with the kernel identity-mapped and user pages marked user; changing it is a different kernel, not a port. |
| The 64K or 16K translation granule | 4K keeps the page-table code shared with x86; Linux binaries with 64K `p_align` load fine on 4K. |
| Big-endian, AArch32 EL0 compatibility | No workload. |
| MPAM (the CAT analogue) | Not exposed on Graviton, not modelled by QEMU; `llc=` refuses by name. |
| An SMMU (the IOMMU) | The same "one trusted workload" trade-off as x86 (ROADMAP §3). |
| KVM PV features (PV EOI, PV IPI, kvmclock, `KVM_FEATURE_*`) | They do not exist on arm64 KVM; the levers are the architecture's own. |
| Lazy FP/SIMD save | A measured optimisation for phase 9, not a correctness item. |
| Cross-building the twelve services from the x86 box | Days of emulated compile; an arm64 builder is cheaper than the wait. |

---

## 9. The next session: the KVM stand on `c7g.metal`

The first session did what §9 used to list — the toolchains, the `arch/` split (to the
last `asm!`), first light and phases 1-3(a) under TCG, the gate in CI. What TCG cannot
show is now the whole of what is left to learn, and it needs an arm64 host with KVM.
The choice is **`c7g.metal`** (Graviton3, Neoverse V1, ARMv8.4-A, 64 cores, no SMT,
`ap-northeast-1` where the vCPU quota fits): the generation the first production
target (`c7g.*`) will be, with every feature the target asks for (LSE, the Crypto
Extensions, FEAT_HAFDBS for the dirty bit, PMUv3). `c6g.metal` (Graviton2, N1) would do
for the logic and the audit at a slightly lower price; `a1.metal` (Cortex-A72) would not
— no LSE, no HAFDBS. A metal instance takes 10-20 minutes to come up and costs about
$2.3 an hour: the session is planned as ONE bring-up of a few hours, with everything
that can be prepared in advance prepared in advance — and it has been:
`infra/aws/kvm-stand-aarch64.sh` is the whole of §9.2 as one command.

### 9.1. Before the stand is paid for (the dev box, no clock running) — DONE 2026-09-30

Items 1-3 are in the tree; item 4 is the open design work.

1. **The stand's recipe in `infra/aws/`** — done: `main.tf` takes `arch` (the AMI
   `al2023-ami-2023*-${arch}`), and `kvm-stand-aarch64.sh` runs §9.2 end to end:
   apply, ssh, provision, upload, the stand image built natively, the kernel, the gate
   under KVM, the four probes, fifty boots, destroy (`KEEP=1` to keep). The original
   text of the item: `main.tf` looks the AMI up by
   `al2023-ami-2023*-x86_64`: the arm64 twin (`*-arm64`) selected by a variable, and
   `instance_type = c6g.metal`. A user-data or an `ansible` play that installs docker,
   git, `binutils` (a native `objcopy`), the pinned nightly with
   `aarch64-unknown-none`, builds `services/docker/Dockerfile.qemu-aarch64` NATIVELY
   (the `fedora:40` base pulls its arm64 variant) and clones the `arm64-port` branch.
   Check `/dev/kvm` exists and is group-readable in the play, not by hand.
2. **The probes TCG forgives, written now, judged there** — done, in
   `arch/aarch64/probes.rs`, each a `talaria.test=` value; under TCG all four run and
   their machinery is proved (a job on a secondary core, the guarded load with its
   EL1 fixup, the EL0 state machines), which is what `test-aarch64.sh` asserts:
   * `ring` — the §4.13 fences under a weak machine: a producer core and a consumer
     core over an SPSC ring in the `fastring` layout (the exact code path of
     `fastpath.rs`, minus the card), a checksum per slot, a few seconds and millions of
     slots, the count of corrupted slots printed — zero or the fences are wrong;
   * `icache` — §4.12: write a probe into a frame, run it at EL0, rewrite the SAME frame
     with a probe that answers differently, `icache::sync`, run again — the second
     answer or a stale line executed (then repeat WITHOUT the sync to see the failure
     the sync prevents: a gate that cannot fail proves nothing);
   * `bbm` — §4.3 break-before-make: one core flips a page between `protect` and
     `protect_none` in a loop while another reads it and takes the expected fault or
     the expected value — a TLB conflict abort or a stale translation is the failure;
   * `svccost` — the first arm64 number for §4.5: a hundred thousand `svc` round trips
     timed by the counter from EL0, printed as ns per call, with and without the
     528-byte FP/SIMD save in the vector (the lazy-save decision needs both).
3. **The timing lines already printed are made judgeable under KVM** — done:
   `TAL_KVM=1 kernel/test-aarch64.sh` requires 49-51 ticks in 50 ms (TCG gives 48-49),
   and the gate runs the four probes as four more boots (36 assertions in all).
4. **The dev-box work that does not need the stand goes on in parallel** — done
   while the stand came up: the `arch/x86_64/signal.rs` extraction (the x86
   `ucontext`/`gregs`, `fork_regs`, `exit_uregs`, `sigreturn`, the fault-kind decode)
   and the neutral `Frame` (§4.5), each with its own x86 regression.

### 9.2. On the stand (one bring-up, 3-4 hours)

| # | Step | The gate / what is recorded |
|---|---|---|
| A | `kvm-stand-aarch64.sh` brings `c7g.metal` up and provisions it; `kernel/build-aarch64.sh`; `kernel/test-aarch64.sh` under `-enable-kvm -cpu host` | DONE 2026-09-30: 36/36 in 5.1 s; MIDR 0x41/0xd40 r1p1 (Neoverse V1), `CNTFRQ` 1.05 GHz, HAFDBS DBM works, PSCI 1.1, 256 SPIs, 49 ticks in 50 ms — `docs/MEASUREMENTS.md` §10 |
| B | `talaria.test=ring` at `-smp 2/4/8`, several minutes each | DONE: 200 M slots each, 0 corrupted at 2, 4 and 8 |
| C | `talaria.test=icache` | DONE: the synced rewrite answers anew; the unsynced one was coherent too, every repeat — the failure needs a harder shape (§4.13) |
| D | `talaria.test=bbm` | DONE: 49 M reads, 0 wrong values, 0 conflict aborts |
| E | `talaria.test=svccost` | DONE: 55 ns with the save, 41 without |
| F | 50 runs of `test-aarch64.sh` in a loop (`-smp 4`, 250 boots) | DONE: 0 of 50 runs failed, 250 boots |
| G | `lab-down`; the hours and the bring-up time go to `docs/ENGINEERING-NOTES.md` §5 | DONE: §5.1 — a Graviton metal is up in a minute, the first bring-up cost under an hour of `c7g.metal` |

Not in this session, on purpose: the UEFI path (phase 6 needs the aarch64 stub, not
written), the workloads (phase 7 needs `syscall.rs` over — phase 3 first), and the
x86 regression on the metal (`test.sh` is x86 QEMU; the arm64 set is
`test-aarch64.sh` until the shared modes come over, §6).

### 9.3. What the session must leave behind

* `docs/MEASUREMENTS.md`: a new section with the stand's first facts (A) and numbers
  (E) — the arm64 twin of the x86 syscall cost, so that phase 9 has a baseline;
* `docs/ENGINEERING-NOTES.md` §5: the stand's recipe, the bring-up time, the price, and
  every pitfall met on the way (the x86 sections there began the same way);
* this plan: §4.13 updated with what B/C/D found, and the status line at the top.

## 10. Phase 3 as it is being executed: the census and the stages

The stand is done; what is left of the port is the kernel above `arch/` compiling and
running on arm64. Phase 3's exit gate (§5) needs `syscall.rs`, and `syscall.rs` names
almost every module there is — so the phase is not "port syscall.rs", it is "make the
whole crate compile for aarch64", judged by two numbers that only go down:

* **the gates**: the `cfg(target_arch = "x86_64")` lines in `main.rs` (84 on 2026-09-30);
* **the census**: `kernel/census-aarch64.sh` copies the tree, drops every gate in front of
  a `mod X;` and counts what `cargo check` for aarch64 says, per file and per missing
  name (1 096 errors on 2026-09-30, the first pass — the deeper type errors come once the
  names resolve). The real tree keeps its gates until a module's whole dependency closure
  compiles, so the CI arm64 build stays green while the count falls.

The first census, by missing name: `println!` 444 (the arm64 UART's macro is exported but
not in textual scope — one `#[macro_use]`), `crate::smp` 230, the clock functions of
`crate::tsc` about 90 (`mono_ns`, `cycles_to_ns`, `wall_ns`, the servo), `crate::idt` 48,
`crate::pmu` 42, `crate::serial` 27, `arch::signal` 20, `crate::cat` 11, `crate::gdb` 10,
`crate::ioapic` 8, `percpu` fields 20, `arch::cpu` port I/O and MSRs 15, `user_elf!` 44.

### 10.1. The stages, each with its gate

| # | Stage | What is built | Gate |
|---|---|---|---|
| 3.1 | **The names.** Every module name the shared code reaches on x86 exists on arm64 with the same surface: `arch::serial` (the PL011 under the 16550's names, the macro in scope), `arch::smp` (the GIC + timer + PSCI under the LAPIC's names: `MAXCPU`, `nr_cpus`, `start_cpu`, `apic_of` = the affinity id, `send_ipi` = an SGI, `lapic_timer_init` = the deadline, the slice knobs), `arch::idt` (the vectors as INTIDs, `TICKS`, `set_vector` as a handler table the IRQ vector dispatches on, `ExcFrame` = the trap frame), `arch::tsc` (the counter under the TSC's names; the servo and the wall clock, which are arithmetic over a counter, move out of `arch/x86_64/tsc.rs` into a shared `clocksrc.rs` with the calibration left behind), `arch::pmu`/`cat`/`gdb`/`vga`/`crypto` as honest stubs that say so (PMUv3, the gdb stub and the Crypto Extensions are phases 8 and later), `arch::ioapic::route` = a GIC SPI, the `percpu` fields (`cur_task`, the sched TTBR0, `off_stack`, `sig_newrsp`) | the census drops below 300 with no new gate in `main.rs`; x86 486/0 after the `clocksrc` split |
| 3.2 | **The x86-isms inside the shared modules**, each moved behind `arch` or gated where it is a device that does not exist on arm64: the legacy PCI config space (`pci.rs` on `0xCF8` — arm64 has the ECAM from the tree), `acpi` (arm64 boots from the tree until phase 6: a `bootinfo` that both feed), the port I/O callers, `rdrand`, `map_mmio_wc`/PAT (the arm64 memory attributes), `cpuid` users, the `timer_stub_sched` frame layout that `sched.rs` builds by hand (→ `arch::switch::build_ctx`), `coredump`/`elfnote` register order (→ `arch::signal`), the TLS base (`fsbase` → `arch::cpu::set_tls`), `user.rs` (`EM_AARCH64`, 64K `p_align`, the auxv) | the census reaches zero on every module but `selftest/*` and `runmode`; x86 486/0 |
| 3.3 | **The arm64 entry and switch** — the real code behind the names: `syscall_entry.rs` (the SVC stub storing the neutral `Frame` order of §4.5, the FP/SIMD save, `sig_newrsp` as a write into `sp_el0`), `switch.rs` (`fpu_save/restore` of the 528 bytes, `enter_saved_ctx` = restore the trap frame and `eret`, `stack_switch_call`, `soft_yield_kick`, the tick vector into the scheduler), `gdt::enter_user` = `eret` to EL0 with `SP_EL0`/`ELR`/`SPSR`, `setjmp`, `signal.rs` (the AAPCS64 `sigcontext`, `pc-4`/`x8` restart, the vDSO's `__kernel_rt_sigreturn`), the sync-exception dispatch on `ESR.EC` into the page-fault path with `FAR` as the address | DONE 2026-09-30 (§10.2): 6 gates left, `boot_shared` runs to its end on arm64, the scheduler switches on the tick (`ipi`), `slice` green. Open inside it: the EL0 fault path into `deliver_exception_signal` (the sync-EL0 stub still treats a fault as fatal) |
| 3.4 | **The binaries.** `kernel/user/` cross-built with `aarch64-linux-musl` into `user/aarch64/`, `user_elf!` picking by target; the musl loader and its `%fs` → `TPIDR_EL0` | DONE 2026-09-30 (§10.2): `hello`, `mhello`, `mprintf`, `pthreads` under TCG |
| 3.5 | **The set.** `hello|mhello|mprintf|pthreads|sigtest|futexlat|mbench|forktls|efault|cpptest*|fputest|memrace|shmrace` under TCG, then under KVM on the stand; `test-aarch64.sh` grows the shared modes; `talaria.strace=1` lists zero `ENOSYS` | DONE 2026-09-30 (§10.4) but `cpptest*` (no libstdc++ against the cross musl yet): 119 checks under TCG, the set under KVM on the stand, zero unknown syscall numbers over it; `mbench`'s numbers in MEASUREMENTS §10 |

The order inside 3.1 and 3.2 is the census's order — the largest count first — and each
step lands as its own commit with the count in the message.

### 10.2. Where it stands (2026-09-30, evening)

**Stages 3.1–3.3 are done in one day: the census is 0, `main.rs` has 6 gates left (the
x86 boot entry, AVX, the `user_elf!` macro), and the whole kernel compiles, links and
BOOTS on arm64.** `rust_main` was split: its x86 early part (multiboot/PVH, the IDT, the
PIC silence, AVX, the TSC) and `boot_shared` — everything after, which arm64's `kmain`
enters once the tree, the GIC and the timer are up. Under TCG on QEMU `virt`:

* `talaria.mode=none` runs `boot_shared` to its end (the wait tiers, the wall clock, the
  knobs, the virtio-mmio scan over the tree's windows, `runmode`) and powers off through
  PSCI;
* `talaria.test=slice` — the first shared self-test green on arm64: `slice-test: ALL OK`;
* `talaria.test=ipi` — the SHARED SCHEDULER switches two kernel tasks on the tick through
  the new vector stubs, a secondary comes up through PSCI under it, IPIs reach the
  secondary. The arm64 gate asserts both (`kernel/test-aarch64.sh`: 7 boots, 43 checks).
* `talaria.test=threads` reaches the first ring-3 task and dies at the first instruction
  — the test embeds x86 machine code; that is stage 3.4's work (`user/aarch64/`), not a
  defect of the switch.

What was decided on the way, and why:

* **One frame for every entry.** `trap::Frame` (36 words: x0..x30, sp, elr, spsr, esr,
  far) plays `syscall::Frame` (the neutral names `nr`, `a1..a6`, `pc`, `flags`, `usp` are
  its fields), `idt::ExcFrame` and the scheduler's saved context. The `svc` path calls
  `rust_syscall` on the frame in place and the stub's restore returns x0. Every stub that
  can switch tasks (an IRQ from either EL, a sync exception at EL1) calls a handler that
  returns THE FRAME TO ENTER; the tail (`SCHED_TAIL`) frees the stack a switch left,
  loads the address space the scheduler chose and restores by the FRAME's origin
  (SPSR.M), not the stub's — a tick from EL1 may enter a frame an EL0 entry saved.
* **The FP/SIMD state travels below the frame.** Every stub saves the 528 bytes under its
  frame and records the area in the per-core block; `switch::fpu_save` copies from THERE,
  so the kernel's own use of the V registers between the entry and the scheduler cannot
  leak into a task's saved state (x86 lives with that hazard because rustc rarely emits
  SSE early in `rust_schedule`; on arm64 LLVM uses q registers for any memcpy).
* **The scheduler builds no frame by hand any more.** `arch::switch::{build_user_ctx,
  build_kernel_ctx, ctx_pc, ctx_sp, ctx_fp, ctx_flags, ctx_is_user, ctx_plausible,
  ctx_exit_uregs, ctx_park, Gp, gp_with_arg, gp_with_ret, user_flags, USER_FLAGS_*}` is
  the whole of what `sched.rs` says about a saved context; the fourteen raw slot reads it
  had are gone. The PSTATE literals differ (x86 `0x202`/`0x2`, arm64 `0`/`0x80`) and so
  they had to.
* **The soft yield is `svc #1` at EL1** (x86: `int 0x41`): the sync-EL1 stub is a
  scheduling stub, so the frame it returns can be another task's.
* **The signal frame is `arch::signal`'s in full**, including two x86 habits that had
  leaked into the shared code: the return slot below the frame (`RET_SLOT`: 8 on x86,
  where `ret` pops the restorer; 0 on arm64, where it travels in x30) and the shape of
  `rt_sigreturn` (an `iretq` from a register array on x86; a rewrite of the `svc` frame
  on arm64, which the stub then restores). `elfnote`'s `NT_PRSTATUS` takes its register
  count from the machine (27 / 34).
* **Policy left the arch modules where it was hiding**: the slice and its knobs
  (`slice.rs`), the wall clock and the servo (`wallclock.rs`), the counters' arithmetic
  (`perfsample.rs`), the TLS base, the wait tiers' detection and the deadline timer
  (`arch::cpu::wait_caps`, `arch::smp::deadline_arm`), the reset (`arch::cpu::reset_machine`),
  the virtio-mmio slot list (`arch::virtio_mmio_slot`: the microvm grid on x86, the tree's
  windows on arm64 — the grid's addresses are not RAM on `virt`, and the first shared boot
  faulted on them).
* **What is honestly absent** and says so: the PMU, CAT, the gdb stub, the VGA window, the
  AES on the Crypto Extensions, an external clock reference (`have_ref` is false; the
  clock drifts by the quartz), the console capture of `talaria.stat=`, ACPI (the tree is
  the description until phase 6; the FADT ports are x86's).

**Stage 3.4, the same evening: ring 3 runs.** `hello.elf` (a `write` and an `exit` through
`svc`), `mhello` and `mprintf` (static musl: `set_tid_address`, the TLS in `TPIDR_EL0`, the
auxv, `brk`/`mmap`, stdio) and `pthreads` (four threads through `clone` and `futex`,
preempted by the tick — `counter=200000 -> OK`, the process and its threads reaped) all pass
under TCG, and the arm64 gate asserts them where the binaries are present. What it took:

* a cross musl — Fedora's musl is x86-only, so musl 1.2.5 is built with an `aarch64-linux-gnu-gcc`
  (`services/docker/Dockerfile.musl-aarch64`; the dev box's mirrors refused the image twice,
  so the same build by hand on the host stands in — `make -C kernel/user aarch64 MUSLA64=`);
* `user_elf!` on arm64 is GENERATED by `build.rs` from `user/aarch64/` — a name that is there
  is embedded, the rest are empty slices — so the kernel builds whatever the set is;
* **the link address is the load address.** The loader keeps a static binary's identity
  (`vaddr == phys`) and reserves the range from the allocator; x86's `0x1000_0000` is a PCIe
  window on `virt`, which reads as zeros, and the first arm64 `hello` died on `udf #0` at
  its own entry. arm64 binaries are linked at `0x5000_0000` (`LDFA64`, `user-aarch64.ld`);
* `e_machine`, `AT_HWCAP` and `AT_PLATFORM` are the arch's (`ELF_MACHINE`, `AT_HWCAP`,
  `PLATFORM`);
* the EL0 fault path mirrors x86's: COW copy, demand-zero, the signal to the process's handler,
  the supervisor's recovery, else the process dies as under Linux; the EL1 path serves the
  kernel's first touch of a reserved user page and the profiler's guarded load.

Eight of the sixteen sources build for arm64; `efault`, `fputest`, `futexlat`, `memrace`,
`shmrace`, `smp2`, `smp4`, `forktls` carry x86 inline asm (`pause`, `%fs:0`, `rcx`) or
x86-only syscall names (`SYS_pipe`, `SYS_open`, `REG_RIP`) and are ported next.
`sigtest`, `mbench`, `schedapi`, `mapshared` start FROM DISK — the virtio-blk over
virtio-mmio of phase 4 comes before them.

**The disk, the same evening.** virtio-blk over the tree's virtio-mmio windows mounts the
TFS1 image of the arm64 set (`mkfs.py <img> user/aarch64`), `talaria.test=fs` reads a file
from it, and `procdisk` runs two `mhello` processes from the disk in separate address
spaces to a clean end — after one real bug: the bring-up had mapped the vDSO into the
KERNEL's tables for its EL0 probe, which put a PDPT into the kernel PML4's slot 0xFF;
`copy_kernel_pml4` shares every present kernel slot with every process, the processes'
stacks (`0x7fff_e...`) live in that slot, and two processes from disk ended up on ONE stack
— returns into 0 and into the bytes of the string they had just printed. x86 never maps the
vDSO into the kernel tables, so the slot is empty there. The vDSO now goes into the kernel
tables only on the probes' branch. The gate asserts the disk (`fs`, `procdisk`) when the
binaries are present.

### 10.3. The next session (as it was planned on 2026-09-30; executed on 2026-10-01 — §10.5)

Phase 3 is closed but for the C++ set; what comes next is phase 4 (§5) — the devices the
regression's other half needs — with two loose ends of phase 3 taken on the way. In
order, each its own commit with the count:

1. **virtio-net over the tree's virtio-mmio windows**, the arm64 twin of the microvm's
   NIC: `open_nic` on arm64 finds the window whose device id is 1, the SPI of that window
   (§4.8: `virt` wires them at INTIDs 48..79) goes on `NIC_VECTOR`, and the network block
   of `test.sh` that needs no PCIe comes over: `epsrv`, `mtsrv`, `connect`, `netshoot`,
   `mode=proc` WITH a NIC, `forksrv` (a fork per connection — the PostgreSQL path).
   Gate: those modes under TCG with a tap; `tcp_bench`/`udp_ping` numbers on the stand.
2. **PCIe ECAM + the ITS** (§4.8): the config space through the ECAM window the tree
   names, MSI-X through the ITS (`its=on` is already on the gate's command line), NVMe
   (`-device nvme`) and virtio-pci. Gate: `talaria.test=its`; the ext2/jbd2/fs block on
   NVMe; `nvme` in the UEFI block's place until phase 6.
3. **The two loose ends of phase 3.** `cpptest*` need a libstdc++ built against the cross
   musl (an arm64 `talaria-alpine-build`: Alpine's aarch64 g++ is exactly that toolchain);
   `launcher`/`fastapp`/`fastflood` are x86 machine code by design and wait for the
   zero-syscall datapath's arm64 form. And the kernel's OWN crash on arm64 prints the
   registers and halts (`trap`): the x86 `CRASH DUMP` block — the last syscall, the stack
   hex, the x29 chain, `fr::dump`, `coredump::dump_kernel` — belongs there too (phase 8's
   list, but the diagnosis is wanted earlier than that).
4. **The ordering audit starts** (§4.13): the soak of §10.4 is the meter — `shmrace`,
   `memrace`, `fputest`, `futexlat`, `ring` for hours under KVM with the downgrades in.

Not yet, on purpose: the UEFI boot path (phase 6), the real workloads (phase 7). The gate
stays `test-aarch64.sh` until the shared modes are all over.

### 10.4. Where it stands (2026-09-30, night): the set runs

**Stages 3.4-3.5 are done but for the C++ binaries: the whole ring-3 regression that has
an arm64 binary passes under TCG, `test-aarch64.sh` is 119/0** (61 checks new — the
ported sources, the ext2 modes, the process model, the crashes) **and the KVM stand ran
the set** (`docs/MEASUREMENTS.md` §10.1). x86 stayed at `test.sh` 486/0 + 22/0,
`validate.sh` 14/0, `kernel-unit` 285/0 (five layout tests new), clippy clean on both
targets.

**The set.** `kernel/user/tal_arch.h` holds the four machine-dependent things a test
ever needed — the cycle counter, the spin hint, the thread pointer, the pc in a signal's
`ucontext` — and eight sources that carried x86 asm or x86-only numbers became one
source each for both machines; what is the machine's own stayed under
`#if defined(__aarch64__)` (`fputest` fills v16..v31 and checks x0/x1/x8/x16/x17/x30
across a signal — arm64's rcx/r11; `efault` names the arm64 kernel's addresses and goes
through `openat`/`pipe2`; `crashme peek` reads 128 MB into the RAM of either machine).
The Makefile's arm64 set is every C source but the three that are x86 machine code by
design: 42 binaries. Every mode of §10.3's steps 1-3 is green: `efault` (with its
`uacheck=0` A/B), `shmrace` (with its `shmlock=0` A/B), `smp2`, `smp4`, `futexlat`,
`sigtest` (also as `mode=proc` from `/cell.manifest`), `mbench`, `schedapi`, `mapshared`,
`fputest`, `memrace`, `execmt`, `forktls` (100 forks of 1.5 GB in a 4 GB guest),
`forkap`, `crashme`'s core dump (opened by the host's gdb: `main () at crashme.c`, the
registers read), the crash and the restart of a multithreaded cell, the isolation of
the identity window in `cell` and `proc`. Not one unknown syscall number over the set
(a number the arm64 table lacks is now REPORTED once, as one the dispatcher lacks is).

**Thirteen defects stood between "ring 3 runs" and the set.** Four were in the shared
code, and x86 had been lucky with each:

* `install(); irq_enable(); start()` in the `ipi` and `threads` modes: a tick between
  the unmask and `start` saved the BOOT context into task 0's slot and "entered task 0"
  on the boot stack — TCG lost 1 boot in 10 (the timer's period had long passed, so the
  tick was pending at the unmask); under KVM the window is microseconds. The unmask is
  gone; the task's own frame turns interrupts on at the entry.
* an ignored `SIGCHLD` interrupted the `wait4` that was waiting for it: the child's
  signal is sent from `exit_cleanup`, a moment before the child has LEFT its core;
  `sched::reap` still says "dying", the parent sees a pending signal and returns
  `EINTR` — for a signal nobody handles. `crashme forkap` lost 30 children of 40 under
  TCG. Linux discards an ignored, unblocked signal when it is SENT (`sig_ignored`);
  `proc::send`/`send_proc` do the same for `SIG_IGN` and for `SIG_DFL` of the three
  default-ignore signals.
* `fork_cow_space` kept a `MAP_SHARED` page for the child only below 1 GB — GB0, x86's
  identity gigabyte; the RAM of `virt` starts AT 1 GB. The child's first write to the
  shared page was a rights fault on the kernel's own 2M leaf (`sigtest` died after
  `fork`). The test is `user_gb()` now.
* the `SA_RESTART` list was matched against the process's RAW number: arm64's 61 is
  `getdents64`, its `wait4` is 260 — a `waitpid` interrupted by a handled `SIGCHLD` came
  back `EINTR` under `SA_RESTART` (`sigtest` under SMP timing).

The other nine were the port's:

* the pickup loop's stack was never set on arm64 (`sched::set_ap_stack`): the first
  thread to EXIT on a secondary moved the core onto `sp = 0`; the fault's own entry
  faulted, forever and in silence, and every task queued on that core waited for it
  (`futexlat` stopped after its first phase — the `IT HAS HUNG` snapshot named a dead
  task as "executing" on core 1);
* `struct stat` written in x86's 144-byte layout into a 128-byte arm64 structure
  overwrote the caller's saved x29/x30 — `sigtest` returned from `main` INTO ADDRESS 0
  after printing its verdict. `arch::abi::{enter, leave}` bounce `fstat`/`newfstatat`
  through a kernel buffer and write the arm64 layout, translate `epoll_event` (16 bytes,
  `data` at 8) both ways, swap the `O_DIRECTORY`/`O_NOFOLLOW`/`O_DIRECT`/`O_LARGEFILE`
  pairs on `openat`/`pipe2`/`fcntl`, and `uname` says `aarch64`; `uaccess` judges the
  PROCESS's sizes; five host tests pin the layouts (§4.4's table, now code);
* `CNTKCTL_EL1.EL0VCTEN` was set on the boot core only: the vDSO's clock read TRAPPED
  on the first thread that ran on a secondary (`schedapi`, `fputest`'s threads —
  `MSR/MRS trapped` at the vDSO's pc). Every core now grants what Linux grants EL0
  (`cpu::enable_el0_access`: the counter, `CTR_EL0` and the cache maintenance for a JIT's
  `__clear_cache`, `DC ZVA` for a libc's `memset`);
* the x86 sanity check of a saved FPU image (`mxcsr`'s reserved bits at offset 24) ran on
  arm64 over the middle of q1 and SKIPPED the restore of a thread whose double had
  those bits ("a broken fxsave image" in an arm64 log) — `arch::switch::fpu_image_check`;
* the FP/SIMD restore loaded the live registers while the scheduler still had Rust code
  to run before the `eret` — the same hazard the save side avoids (§10.2): on this
  machine LLVM copies structures through q0/q1. `fpu_restore` now records the image
  (`percpu::fp_pending`) and the entering tail loads it as its last act
  (`FP_TAKE_PENDING` in `SCHED_TAIL` and `enter_frame_tail`). The gate's positive control:
  with the restore disabled `fputest` fails at once ("v16 is spoilt at the iteration
  810") — the test catches what it claims;
* an IPI on the timer's vector — what the shared code sends to make another core enter
  its scheduler — was masked to SGI 11, which nobody handled: `execve` waited a second
  for sisters that never left their cores ("2 threads of the process did not stop"), and
  a dedicated core could not be kicked at all. `RESCHED_SGI` (4) is that request; the
  IRQ vector takes it as a tick that re-arms nothing;
* the EL0 crash path killed the process and nothing more: it now has what x86's has —
  the thread pointer in the crash line (mapped or not, read through the tables; there is
  no self-pointer at `[tpidr]` on arm64, the TLS lies above it), the x29 chain in the
  log (`obs::unwind::print_backtrace`, shared: an x29 frame record is the same
  `{caller's fp, return address}` pair as RBP's), the supervisor's notification, the
  sisters stopped before the dump, then the ELF core — with THIS machine's `e_machine`
  (it said x86-64, and a host gdb refused it);
* `forktls`/`idleshoot` fell through to the manifest on a machine without a NIC (the
  branch they lived in was the "NIC found" one);
* a syscall number outside the arm64 table returned `ENOSYS` in silence.

**The stand, the same night (`c7g.metal` under KVM, `docs/MEASUREMENTS.md` §10.1).** The
first boot of the set died at the first `alloc_frame`, silently — four things only a real
machine under KVM could show, each fixed and the gate then **120/0** (the speedups
judged too): `TPIDR_EL1` and the other UNKNOWN-at-reset registers start as the poison
`0x1de7ec7edbadc0de` under KVM (the stubs stored into it; `boot.rs` zeroes them);
the PSCI `hvc` declared no clobbers and KVM poisons x1..x17 as SMCCC allows; the
instruction cache was never told about loaded code (`icache::sync` after the four
loaders, the vDSO, a COW copy of an executable page, `mprotect(PROT_EXEC)`); and a
process's first fetch used a stale translation of the kernel's regime one boot in three
(every space shares ASID 0 — the root switch invalidates on both sides now). The
numbers: `mbench syscall` 152 ns (x86 95), `svccost` 130/112 ns (the FP/SIMD save 18 ns;
the morning's 55/41 was the bring-up stub without the dispatcher), the cross-core futex
RTT p50 ~520 ns, the speedups 2.00 and 4.00, the soak 0 failed boots of 450 in the
clean run.

**What TCG says about the numbers** (all for the stand to judge): `mbench syscall`
10.8 µs under TCG against 55 ns for the bare `svc` under KVM — the emulator's exception
cost, not the kernel's; `futexlat` p50 9.6 µs; `smp2` 1.90, `smp4` 3.52 (real cores under
MTTCG). The gate: 119 checks in about five minutes under TCG.

**Gates and meters.** `kernel/test-aarch64.sh` builds its ext2 images from
`kernel/user/aarch64` on the spot, reads the core dump back on the host with `debugfs`
and an aarch64-aware gdb, and judges the speedups only under `TAL_KVM=1`;
`kernel/soak-aarch64.sh` is the ordering soak (N boots of every mode whose verdict
depends on what two cores see of each other's writes, the failing consoles kept);
`infra/aws/kvm-stand-aarch64.sh` uploads the binaries with the tree (the cross musl is
the dev box's; `services/docker/Dockerfile.musl-aarch64` is the reproducible route, the
dev box's mirrors refused it three times — the hand build of musl 1.2.5 with
`aarch64-linux-gnu-gcc` stands in), runs the gate, keeps the consoles of `mbench`,
`futexlat`, `smp2`, `smp4`, then the soak and the gate in a loop.

**Where to start reading:** this section (§10.1–10.6) is the execution log; `kernel/census-aarch64.sh`
is the progress meter (0 now — it turns useful again only if a new x86-gated module appears);
`kernel/test-aarch64.sh` is the gate (229 checks; the ring-3 part needs
`kernel/user/aarch64`, the ext2 part `mkfs.ext2` on the host, the network part the stand image
with `/dev/net/tun`, the UEFI part a fresh `kernel/uefi-stub/build.sh aarch64`). The pitfalls that cost time are in
`docs/ENGINEERING-NOTES.md` and the session memory: do NOT rebuild x86 while `test.sh` runs
(two runs collide over the shared disk images — a whole invalid regression), the arm64 link
address is `0x5000_0000` (RAM on `virt`), and the cross musl is `talaria-musl-aarch64` (or
built by hand on the dev box, `make -C kernel/user aarch64 MUSLA64=`).

### 10.5. Where it stands (2026-10-01): the devices, the firmware path, what a loaded host found

**§10.3's four steps are done, and the first half of phase 6 with them.** The gate went
119 → 154 → 203 → 217 → 229, each step its own commit; x86 stayed at `test.sh` 486/0 +
22/0 at every one of them, clippy clean on both targets in both configurations.

**Step 1, the network (119 → 154).** The NIC came up through the shared driver table on
the first boot — `virtnet` already asked `arch::virtio_mmio_slot` — and the datapath in
polling mode worked as it was. What the regression's network block needed on top:

* the card's line: the tree NAMES the SPI of every virtio-mmio window (`virt`: 16..47), so
  `arch::virtio_mmio_gsi` answers from the tree on arm64 (x86 keeps its "anchored at the
  top of the GSI space" derivation) and `ioapic::route` installs the handler under the
  INTID before it enables the line;
* **the acknowledge must not ask for a lock** (ENGINEERING-NOTES §6.1 item 22): a level
  SPI whose status the handler could not clear — the interrupted pass held the queue — was
  re-entered 600 thousand times a second; `diskserver` with `talaria.netirq=1` never
  answered one boot in four. The counters read through the QEMU monitor (`xp` on the
  kernel's statics and on the device's status register) named it in one run;
* the console capture on the PL011: `serial::capture_*` was a stub that answered "no", so
  every `talaria.stat=` block and every `netprof` summary was SKIPPED on arm64 — the gates
  that judge by those counters had nothing to read;
* the wall clock's base: the PL031 the tree names (`arch::tsc::rtc_read`); there is no
  kvmclock here and `CLOCK_REALTIME` said 2023.

The block itself is `kernel/test-aarch64-net.sh`, run INSIDE the stand image with a tap
(the guest and its clients share it): `userver` in polling mode and with the line and the
parking, `diskserver` over TCP, `mtsrv` with `netsolo` (`ppoll` on arm64), `forksrv` from
ext2 (the PostgreSQL path: `mode=proc` with a NIC), `netshoot` on four cores, the outgoing
set (DNS, connect, ECONNREFUSED, close with a dead and a slow peer, the wall clock). Under
TCG the counts of the parking gates are judged for their outcome, under KVM for the count.

**Step 2, PCIe, NVMe, the ITS (154 → 203).** The config space is the ECAM window of the
tree's `pci-host-ecam-generic` (above 4 GB on `virt`, mapped on demand exactly as the
MCFG's is on x86); no firmware ran before the kernel under `-kernel`, so the BARs nobody
assigned come from the host bridge's 32-bit range of the same node (`pci::set_mmio_window`
— x86's `0xC000_0000` is RAM here). NVMe then came up on the first boot. The ITS is
`arch/aarch64/its.rs` — the LPI configuration and pending tables, the device and
collection tables, the command queue; a collection per core mapped on its first vector, a
device on its first vector (`MAPD` + a translation table sized for its count),
`MAPTI`/`INV` per vector, `MOVI` for a re-route — and `arch::msi` on BOTH machines says
what a device's MSI/MSI-X entry holds: the LAPIC's address and the IDT vector on x86 (bit
for bit what `pci.rs` wrote before), the translation register and the event number on
arm64. `talaria.test=its`: an NVMe admin completion arrives as an LPI on core 0 and
reaches the vector's handler; after a re-route it arrives on core 1 and not once on core 0
(an LPI is one pending bit: 8 completions were 6 interrupts). One defect of the port's
own on the way: a `GITS_BASER`'s bits 9:8 are the PAGE SIZE, and the first version put the
redistributor-style cacheability bits there. The gate got the ext2/jbd2 block on
virtio-blk AND on NVMe — `ext2`, `sigtest` from the volume, `ext2v` across reboots,
`fsbench` with a queue depth, `journal1/2` in both modes with the machine KILLED at the
commit and `e2fsck` on the host — plus symlinks, `nojournal`, a transaction larger than
the journal, `htree` with 3000 names, `its`, `msix`.

**Step 3, the two loose ends of phase 3 (203 → 217).** The C++ set builds with the
`aarch64-linux-musl-cross` toolchain (its libstdc++ is built against musl; `make -C
kernel/user aarch64` builds the four when that g++ is on the PATH) and passes. A fatal
exception of the kernel prints, after the registers, what x86's `CRASH DUMP` block does —
the task and its last syscall, the abort decoded, the stack's words (only where the sp is
a mapped, aligned address), the x29 chain, the flight recorder — and writes the kernel's
core dump: the host's gdb opens it at `core_fault_l1` and unwinds to `_start`. Since this
session it also prints whose kernel stack the core is on, which task every core believes
it runs, and the last decisions of every core's scheduler — the three lines that named
each of the four defects below from ONE failing console. Not ported, on purpose:
`launcher`/`fastapp`/`fastflood` (x86 machine code by design) and libtsync's fibers.

**Step 4, the audit's device half.** `arch::barrier::dma_wmb` before the NVMe and ENA
doorbells (`dmb oshst`; the same `mfence` as before on x86) and `dma_rmb` behind an NVMe
completion (`dmb oshld`; a compiler fence on x86) — §4.11's item, which waited for the
drivers to come over. `NW_IN`, `IO_YIELD` and `READY_CTX` were read again: a task's own
hints, read by a neighbour as one word — a stale read is a worse decision, not a crash;
they stay `Relaxed` and the soak judges them.

**The firmware path (217 → 229) — phase 6's first half, under `edk2`.** `kernel/uefi-stub`
is one crate for two machines now (`efi.rs` the firmware's interface, `x86_64.rs`,
`aarch64.rs`). The arm64 half: the command line from `LoadOptions`, the RSDP (and the
device tree, when a firmware publishes one) from the configuration tables, the firmware's
clock (`GetTime` — a machine described by ACPI has no RTC the kernel could read), a bounce
copy of the kernel's one PT_LOAD, `ExitBootServices`, then — with the caches cleaned over
everything the next step reads or writes — the MMU OFF at the level the firmware left,
the copy to the segment's physical address, and the Image entered with `x0` = the stub's
boot block (`kmain::BootBlock`). A range that is not RAM, or that holds ACPI tables, is
REFUSED with a message. On the kernel's side `arch/aarch64/acpiboot.rs` reads what
`dtb.rs` reads from a tree — the MADT's GICC/GICD/GICR/ITS entries, the GTDT's timer
interrupts, the SPCR's console, the FADT's PSCI flags — into the same `dtb::Summary`
(four host tests on tables built in the test), and the memory map is the firmware's (11
entries under `edk2`, the PVH format `mem::init` reads). `edk2` under QEMU publishes NO
device tree next to its ACPI tables, so the gate's UEFI block is the EC2 shape: four cores
through PSCI as the FADT names it, the ECAM from the MCFG, an NVMe whose 64-bit BAR the
FIRMWARE assigned above 4 GB (`0x80_0000_4000`), `ext2` on it, the ITS the MADT names. It
worked on the first boot. What it does not have yet: a 16550 console (a Nitro guest's
SPCR names one — the kernel says so and prints nothing rather than guess an address), and
a load address other than `0x4008_0000`.

**What a loaded host found (the SHARED code; ENGINEERING-NOTES §6.1 items 23-25).** The
gate failed once in `forkap` — a trap at pc `0x10` — while eight other QEMUs ran on the
dev box's eight cores. Eight copies of that one boot in parallel reproduced it in a
hundred boots, and three different defects came out one after another, each from one
console:

* `sched::stack_wait` gave the wait for a released kernel stack up after 10 ms and entered
  anyway: a vCPU the host had taken away for longer than that was still on the stack. A
  wait is now given up only when NO core says it is leaving that stack
  (`percpu::off_stack`);
* a thief stole the boot core's CURRENT task in the window where the boot core's switch
  had announced `CURRENT = next` and then rolled back: `sched::bsp_on` asks the core's
  committed word too;
* a kernel stack's guard page stayed a hole in the address space that was active when the
  stack was allocated, after the stack was freed: `sched::free_kstack` takes it back in
  every live space.

Before: a failure in 60-160 boots of `forkap` under that load. After the three: **0 of 480.**
None of them is arm64's; a quiet KVM host never lost these races (the stand's soak was
0/450 the day before), a shared EC2 host with its 8 ms steals can.

**Also the stand's, not the kernel's:** the dev box's `talaria-qemu-aarch64` image was
older than its Dockerfile (no `ip`, no `socat`; the Fedora mirrors refused a rebuild, the
two tools were copied in from the x86 image of the same Fedora); a `socat` client closes
0.5 s after its input ends, and under TCG a forking server answers at 0.8 s.

### 10.6. The next session

Phase 4 is closed under TCG; what is next is the stand and EC2 itself. In order:

1. **The KVM stand on the new gate — NOT RUN YET.** Two launches on 2026-10-02 were lost
   to the dev box's network (an ssh broken a minute into the kernel build, then one timed
   out in provisioning; the instance was destroyed both times, nothing was measured). The
   script's long steps now run detached on the stand and are polled for (`rrun`), so a
   lost connection no longer costs the run. What it must do (`infra/aws/kvm-stand-aarch64.sh` — it builds the
   stub and loads `tun`/`vhost_net` now): `TAL_KVM=1 test-aarch64.sh` (the timing lines
   of the network block and of UEFI's `smp4` are judged only there), the network numbers
   (`kernel/netnum-aarch64.sh`: `udp_ping` against `userver` in three configurations,
   `tcp_bench` against `diskserver`), the soak with the barriers in, the gate in a loop.
   The parallel-load loop of §10.5 belongs on the stand too: `forkap` × N copies with
   more vCPUs than cores is the cheapest race detector the port has.
2. **EC2's own boot** (phase 6 proper): the 16550 console the SPCR of a Nitro guest names
   (`uart.rs` is a PL011 only), `mk-ami.sh --architecture arm64` with the arm64 stub as
   `BOOTAA64.EFI`, the first `c7g.large` to the mode prompt, then ENA on Graviton (the
   driver is shared; its doorbells already have `dma_wmb`, its LLQ push needs the
   Normal-NC mapping and `dsb st`), the PHC as the wall clock's reference (there is no
   kvmclock; the base comes from the firmware now). The load address: the kernel is
   linked at `0x4008_0000`, and the stub refuses a machine whose RAM is elsewhere — read
   what a Graviton's map says before deciding whether that is enough.
3. **The ordering audit proper** (phase 5): §4.13 file by file with the soak as the
   meter, the hot `SeqCst` downgrades as measured x86 commits.
4. **What phase 4 left open:** a virtio-pci transport (the tree has no driver for it on
   either machine; under UEFI the gate has no NIC), bridges on arm64 without firmware
   (bus numbers are nobody's there), the guard page of a FORKED task's kernel stack (it
   is installed in the parent's space — §6.1 item 25 fixed the leak, not the coverage).

### 10.7. The KVM stand run and EC2's first facts (2026-10-03/04)

**The stand ran, at last, end to end on a `c7g.metal`** (`infra/aws/kvm-stand-aarch64.sh`,
~52 min, self-destructed). The gate under KVM: **229/1** — the one FAIL was `htree (host)`,
and it was the gate lying about the host, not the kernel (below). Everything the stand
alone can judge came out clean: `mbench` (alu 0.77 ns/step, syscall 153 ns, chase-64m 94 ns),
`futexlat` (same- and cross-core futex p50 ~512 ns, cond ~1900 ns), `smp2`/`smp4` speedup
2.00/4.00, the four probes TCG cannot judge (`ring` 0 corrupted at 9.25 M slots/s, `icache`,
`bbm`, `svccost` all "as expected"), the soak **0 of 50** on all nine modes, and the
parallel race detector **0 of 96** on `forkap`/`shmrace`/`memrace` 24-at-once on 64 cores —
the three shared-code races of §10.5 stay fixed under a real weakly-ordered machine.

**The htree defect (ENGINEERING-NOTES §6.1 item 26), found because the host was finally
an arm64 one.** ext2's directory hash has a version (`s_def_hash_version`) and a signedness
(the superblock flag set by `mkfs` where `char` is unsigned — every arm64 host); the
kernel folded the two into the effective version (+3 when unsigned) and wrote THAT into the
`dx_root` of every index it built. On an x86-made volume the two coincide and three gates
said "clean" for a year; the arm64 stand's `mkfs` flagged the hash unsigned, the kernel put
a 4 in the root, and e2fsck refused the tree (Linux would have read those directories as
signed half_md4 and found no name). `ext2.rs` now writes the raw version; the gate builds
the unsigned volume on purpose on both machines (`mkext2.sh HASHU=1`, the arm64 gate's new
`htree (unsigned hash)` block) — a check whose verdict depends on its host is not a check.

**EC2's own facts, read off a live `c7g.metal` and a `c7g.large`** (both terminated;
`infra/aws/logs/aarch64-{graviton,nitro}-acpi-*.log`). A Nitro guest (`c7g.large`) is an
`edk2` machine: the console its SPCR names is a **16550, MMIO at `0x8000_0000`, `mmio32`**
(byte registers a word apart) — so `uart.rs` grew a 16550 register set beside the PL011,
chosen by the SPCR's interface type and access size (`UartKind`), and the kernel now talks
on EC2's console instead of going silent. The memory map starts at `0x4000_0000` (the
kernel's `0x4008_0000` load address is RAM), the GIC's redistributors and ITS sit at
`0x1000_0000` (GB0, with the ECAM at `0x2000_0000`), PSCI is `SMC`, the ENA/NVMe are
`1d0f:ec20`/`1d0f:8061` on bus 0, and the counter runs at 1.05 GHz. The bare `c7g.metal`
is different again — its 16550 is at `0xE2F0_0000` (GB3, a gigabyte with no RAM) and its
GIC is above 512 GiB — which is why `mmu::early_enable` now maps a Device block for every
GICD/GICR/ITS/UART/ECAM the description names that falls in a gigabyte with no RAM, and
`gb_kind` asks the firmware's memory map which gigabytes are RAM instead of assuming
"below the kernel is devices". `tools/mk-ami.sh ARCH=arm64` writes the aarch64 stub as
`BOOTAA64.EFI`; `ec2-boot-talaria.sh ARCH=arm64` registers the AMI `--architecture arm64`
on a `c7g.large` — the next step is that first boot to the mode prompt.

**Still open — `talaria.netirq=1` on `diskserver` faults under KVM** (3 of 4 on the stand;
the `tcp [netirq=1]` line of `netnum` is blank). A data abort at EL1 during the EL0-sync
return (`RESTORE_FRAME` in `vec_8`) on a page in the middle of the parked task's kernel
stack — i.e. a kstack page that is mapped when the syscall parks is not mapped in the
address space the wake switches back to. `udp [netirq=1]` (userver, also a single parked
waiter) is clean and `tcp [polling]` is clean, so the trigger is the park/wake + address-
space switch on the TCP+disk path, not the line itself. `netirq` is a non-default knob
(polling is the default and passes); the crash wants its own session with the stand or a
TCG repro harness that drives the guest from the host.

### 10.8. The netirq fault closed, and the first own boot on EC2 Graviton (2026-10-04)

**The `netirq` diskserver fault was an interrupt inside the restore, not a page-table
hole.** The svc path comes back from a parked syscall with interrupts open (the parking
in `wait_on_ex` enables them to be woken). `RESTORE_FRAME` writes ELR/SPSR and then reloads
the registers; an IRQ taken in between rewrites ELR/SPSR with ITS return — a pc inside the
macro — and the `eret` re-runs the tail of the restore with sp already past the frame. Both
stand dumps fit it to the byte: SPSR with I clear, ELR at two different points of the
restore sequence, sp equal to the kernel stack's top (the frame's 0x120 too high), the
faulting page the user stack's guard right above it. TCG takes interrupts only at a
translation block's end and the restore is one block — a TCG repro harness ran clean 4/4
before the fix too, which is what pointed away from the tables. `RESTORE_FRAME` now masks
IRQ/FIQ first (`trap.rs`). The x86 return path had the same window in a narrower form —
`rust_syscall` may return with IF=1, and an interrupt between `pop %rsp` and `sysretq`
lands at CPL0 on the user stack — and now executes `cli` after the call. The KVM
confirmation of the arm64 fix waits for the next stand run.

**Talaria boots on a `c7g.large`** (a Nitro guest, edk2; `infra/aws/logs/aarch64-ec2-
c7g.large-*.log`): the stub, the kernel from ACPI, GICv3+ITS, PSCI, our own tables with
hardware DBM, a musl process in ring 3; `talaria.pci=1 talaria.test=ext2` — ext2+JBD2 RW on
the EBS NVMe, ALL OK; `talaria.test=smp2` — the second vCPU through PSCI, speedup 2.00;
`talaria.mode=userver talaria.ip=dhcp` — **the ENA came up unchanged** (LLQ in BAR2 as
Normal-NC, the descriptor writes behind `dma_wmb`), DHCP, and a UDP echo answered 20 000
round trips from an x86 `c6i.large` neighbour with no loss, p50 40.3 us, p99 48.6 us (a first
number, not a comparison: the neighbour is another type and there is no Linux baseline on
`c7g` yet). What it took:

* **The SPCR's address is a placeholder on EC2.** The table is revision 2 and its PCI fields
  name `1d0f:8250` at 00:01.0; its address structure says `0x8000_0000`, which is the
  ENA's BAR, and the function's own BAR 0 (`0x8004_8000`) has memory decoding OFF — the
  firmware does not print through it at all. Writing the placeholder printed nothing (and
  would write into the ENA). `acpiboot` reads the PCI fields and the MCFG's segment-0
  window, takes BAR 0 through the ECAM and switches decoding on; no BAR — no console,
  never the placeholder. Session 56's "16550, mmio32" was wrong twice: the access size is
  a byte, and the address is not the console's.
* **The early map got one span of RAM** (1 GiB .. 18.9 GiB on a Nitro guest: RAM at 1-2 GiB
  and from 16 GiB), so the PCI window between was a Normal block. `kmain` passes the
  firmware's RAM entries, joined only where no whole gigabyte lies between them.
* Two hazards no gate can show, closed on the way (TCG has no caches; KVM on a Graviton
  forces the guest's RAM cacheable through S2FWB): the stub cleans the ACPI tables, the
  RSDP and the device tree to PoC — the kernel reads them with its MMU off — and
  `uart::lock` takes no atomic while the MMU is off (an RMW on Device memory is the
  implementation's choice).

The gate: **232/0** under TCG (the htree-unsigned block of §10.7 included). On a bash 5.1
host the gate script itself had stopped parsing — an apostrophe inside `${JM:+...}`,
literal for bash 5.2, a quote for 5.1 — so the stand ran it whole and this box did not.

Still open, in order: the allocator sees only the RAM below 4 GiB (850 MB of a
`c7g.large`'s 3995 — the shared identity window, not an arm64 matter; closed in §10.9);
the ENA's PHC is not declared on `c7g.large` (a clock reference wants a type that has
it); the netirq fix's KVM run; the ordering audit (phase 5); the tails of phase 4.

### 10.9. The RAM above 4 GiB (2026-10-04)

**The workload now gets the machine's memory, not the identity window's.** The main
allocator still stops at 4 GiB — what it hands out (page tables, kernel stacks, the FS
cache, DMA rings) is guarded, unguarded and split at 4K, which a 1 GiB identity leaf does
not allow. What lies above is now the main node's **high area** (`mem::high_init`, after
the NUMA areas, before the first address space): the largest piece of RAM above 4 GiB
that no other node owns, mapped by 1 GiB identity leaves at once (an AS copies the
kernel's PDPT at birth), with its frames returned into the areas' free list. The
workload's own pages — heaps, stacks, lazy and COW pages, everything that comes through
`alloc_contig_node` for the main node — take it first; the low memory is left to the
kernel. On a `c7g.large` that is 850 MB -> ~3.8 GB for the workload; an x86 `c6i.xlarge`
gets its five gigabytes above 4 back too. The ENA rings' `alloc_high` takes from the same
bump (a CAS now). The COW refcount table grows to the top of RAM (`rc_grow`, from the high
area): a `c7g.large`'s RAM ends at 18.9 GiB, past the static table's 16.

Two holes the old limit hid, closed with it (ENGINEERING-NOTES §6.1 item 29): a
`MAP_FIXED` over a gigabyte that is a 1 GiB identity leaf (`paging::ident_leaf_overlap`
— the kernel would write a frame there into the process's page) is refused like one
below 4 GiB, and the high area and the NUMA areas stop below the process heap's gigabyte
(`PROC_HEAP_BASE`, 192 GiB). `lazy_fault`/`map_fresh_pages` check every frame against the
active AS, above 4 GiB too. The FS cache's default is a sixteenth of all free memory but
not more than half of the low.

The gate: `talaria.test=highmem` (the frames are in the area and identity, a returned
block comes back from the area's list, the low allocator untouched, the refcount reaches
the top, a process inherits the leaf and its teardown leaves the kernel's, the MAP_FIXED
refusal) on x86 `microvm` 4 GB and on arm64 `virt` 4 and 17 GB (the second grows the
table); `forktls` on both runs its 1.5 GB over the high area; the NUMA block checks that
node 0's area is not node 1's memory. arm64 **237/0** under TCG; x86 `test.sh` **489/0** + host 22/0; clippy clean on both.

**On the live `c7g.large`** the area is `0x4_0000_0000..0x4_ba00_0000` (2976 MB), the
refcount table grew to cover it, and `highmem` passed — and `forktls` PANICKED one syscall
in: `split_2m_lazy: there is no frame`. Its 1.5 GB mapping takes 2 MB pages (on by default
from 32 MB), and those came from the LOW allocator only — ~600 MB after the FS cache — so
the workload ate the low memory and the next page table had none. Under QEMU the low
memory was 3 GB and nothing showed. Now the workload's huge pages come from the high area
first (`mem::alloc_workload_aligned`), and no workload allocation falls back to the low
memory below a 64 MB reserve when a high area exists. The machine's shape is a gate now:
`talaria.lowmem=850` cuts the low allocator, and `forktls` on 5 GB with it reproduced the
EC2 panic byte for byte on the old code and passes on the new — on both architectures.
Back on the `c7g.large` with the fix: `forktls` 100 of 100, ALL OK; `sigtest` ALL OK.

**The guard of a forked task's kernel stack** (a phase-4 tail, §10.6 item 4). `fork`
allocated the child's stack with the PARENT's space active, and the guard went into the
parent's private directory of the kernel's gigabyte; the child, the only one running on
the stack, had none. `sched::alloc_kstack_in` lays it in the child's space and in the
kernel's reference one (which later spaces copy); a refused fork gives the stack back with
the guard taken off everywhere (`release_kstack` — the bare `free_contig` left the hole of
ENGINEERING-NOTES item 25). Gate `kstackguard` on both machines; `forkap` 24 of 24 with
eight and sixteen copies in parallel under TCG.

The gates after both: arm64 **240/0** under TCG; x86 `test.sh` 490/2 + host 22/0, the
two FAILs the knob registry's order (`talaria.lowmem` out of alphabetical place — the
`cmdline` self-test caught it), fixed and re-run ALL OK on both machines; clippy clean.

Still open: a single high area (the largest piece — a machine with several holes above
4 GiB uses one of them); the real fix of the identity window is a kernel direct map in
the higher half.

### 10.10. The ordering audit by reading, and phase 4's tails (2026-10-04)

**The audit (phase 5, §4.13), first pass.** Four readers went through the shared code by
subsystem — the scheduler; the network stack and the rings; the file system, memory and
page tables; the wait/wake protocols, the cores' bring-up, the syscalls and the
observability counters — with one question per shared word: is this a publish/consume pair,
a Dekker pair, or a read-change-store from two cores? Every finding was re-read on both
sides before a line changed. What changed:

* **One word, two writers, no atomic** (all architectures — `docs/ENGINEERING-NOTES.md`
  §6.1 item 30). The pipe (one count both ends rewrote: now an SPSC ring, the reader owns the
  head, the writer the tail, a lock per END for several readers or writers); the eventfd's
  counter (a CAS take, an atomic add); the pending-signal sets of a task and of a process
  (`fetch_or`/`fetch_and`, a take that checks the bit it took); `FUTEX_WAKE_OP`'s op on the
  user word (atomic, as Linux's); the COW refcount (`mem::rc_release`: the check and the
  decrement one RMW — and `cow_fault` installs its copy by a CAS over the entry it decided
  on, so two threads faulting on one page no longer both install and both drop); the task
  slot of `fork`/`clone` (a claim lock: two cores took one row); the dead-address-space
  queue (a CAS claim); `mem::reserve`'s pair under the list's lock.
* **The publish/consume pairs.** `wait4`'s exit code against the death marks (a release
  after the code, an acquire before reading it — the slot's previous tenant's status could
  be read); `NOTIFIED` (an `AtomicBool`, release/acquire); the core-dump registers;
  ext2's deferred-write claim released without the FS lock (a fence before the `-1`);
  the wall clock's seqlock writer (the fence after the odd count); the ITS's LPI vector
  (set before the mapping, release/acquire).
* **Dekker pairs.** The NIC's "a pass is owed" flag against the stack lock's release (both
  sides `SeqCst`, and the interrupt retries the lock once after raising the flag — the window
  existed with no reordering at all); the flag's clear is a swap that the ring reads follow;
  `netsys::wake_owner`'s load of the sleeper's registration is `SeqCst` after its `SeqCst` bump.
* **arm64's own barriers.** `send_sgi` has the `dsb ishst` Linux's `gic_ipi_send_mask` has
  (a `dmb` does not order a system-register write, and `nomem` let the compiler move the
  caller's stores past the `msr`); `flush_page`/`flush_all` have the `dsb ishst` before the
  `tlbi`; a flush of more than 64 pages or of "everything" was LOCAL (`tlbi vmalle1`) — it is
  `vmalle1is` now; `fork` flushes the parent's other threads' TLBs (on both machines: a
  sibling on another core kept writing into the frame the child now shares); `munmap` and
  `madvise(DONTNEED)` give the frames back AFTER the flush (`paging::Deferred`), not during
  the walk; the switch stub frees the old kernel stack with `stlrb`; the ITS's command queue
  is behind a `dsb st`; `dma_wmb`/`dma_rmb` lost `nomem` (a compiler barrier as well).
* **One with no ordering in it.** arm64's `membarrier` waited for `WAKE_ACKS`, which only
  x86's wake stub increments: every call with a second core online spun its 1 ms insurance
  and ordered nothing. The wake SGI now acknowledges after a `dmb ish` (`trap.rs`).
* **The secondary's boot data.** A core started by `CPU_ON` runs with its caches off until
  `enable_here`: the kernel's root word and the top of its stack are cleaned to the PoC
  before the call (Linux does the same for `secondary_data`; under KVM S2FWB hides it).

The gate for the first class is new: **`piperace`** (both machines) — a 4 MB stream through
a pipe between threads on two cores, checked byte by byte, and an eventfd written from three
cores, summed exactly. On the old kernel under TCG it failed 2 runs of 3 (4.1 million wrong
bytes, a sum of 304 930 for 300 000) and hung the third; it passes on the new one under TCG
and KVM.

**What phase 5 still owes.** The KVM run of all this (the stand's soak, the parallel race
detector, `piperace` in a loop — with the netirq confirmation of §10.8 on the same bill);
the hot `SeqCst` downgrades, each a measured x86 commit (the readers listed candidates: the
scheduler's redundant `fence(SeqCst)` next to `SeqCst` accesses, the candidate filters of
the pickup, `NET_WAITERS`/`NET_POLLER`); three small ones left as noted — `netprof`'s dump
slot claim (diagnostics), `l1::arm_intr`'s `try_take` that can skip an unmask, the 1 GB
identity leaves of a high area that cover a hole beside it (Normal memory over what may be
a device on a bare machine).

**Phase 4's tails.**

* **virtio-pci** (`virtnet.rs`): the vendored crate's `PciTransport` over `pci.rs`'s config
  access, its BARs assigned or respected (`pci::assign_mem_bars`), a driver of its own
  (`virtio-pci` — the enumeration counts per driver). Polled; its interrupt would be an MSI-X
  through the ITS. The gates: arm64 `virt` with no firmware (our BARs), arm64 under edk2 and
  x86 q35 under OVMF (the firmware's BARs), a UDP echo each — the UEFI paths have a NIC.
* **Bridges with no firmware** (`pci::number_bridge`): depth first, the next bus number as
  the secondary, the subordinate opened while the buses below are numbered and closed after,
  the devices behind given BARs during the scan and the bridge's memory window set around
  them. The tree's ECAM now names as many buses as its window holds. Gate: a card behind a
  root port, a switch's upstream and downstream ports (bus 3), echoing.
* **The zero-syscall cell on arm64**: `fastapp`/`fastflood` take the call and the spin hint
  from `tal_arch.h` (`fastflood`'s publish had a compiler barrier only — a release fence
  now); the core started for the cell runs the datapath instead of the bring-up's idle loop
  (`runmode::CELL_AP`) and reports through `AP_UP`, not `ONLINE`. Gate: 100 000 frames
  through the TX ring with the kick amortised, and the echo with zero syscalls and faults per
  packet.
* **The RAM above 4 GB in up to four pieces** (`memspan::pick_high`, host-tested in
  `kernel-unit`): a machine with holes between banks, or another node's area in the middle
  of the main node's RAM, gets every piece, the largest first.

* **The last ring-3 sources**: `launcher` (freestanding: raw `svc`, `clone(SIGCHLD)` for the
  fork arm64 does not have; its child runs `/mhello` — there is no arm64 busybox yet) and
  libtsync (the fibers' switch saves x19-x30 and d8-d15; the counter and the spin hint are the
  machine's; there is no user-mode monitor wait on arm64, so the tier 1 stays off). Gates:
  `procdemo`, `fiber`, `syncbench`. `kernel/user/aarch64/tsync/` is embedded by `build.rs`
  as `tsync/<name>`, as on x86.

**Found by the port of libtsync, fixed: the generic timer's interrupt storm.** With every
wait in the kernel (arm64 has no user-mode monitor tier) the `tsync` set stopped in its MPSC
phase, 4 runs of 4 under TCG, while x86 with the tier 1 compiled out passed 12 of 12. A copy
of the phase with a watchdog printed the queue at the stall: no position skipped (the tail
equalled the pushes), one slot "free for h" whose producer slept with its predicate met —
and the core's timer state: core 0 had taken **1 141 858 timer interrupts** at a 500 us
period. The generic timer is level-sensitive: when a waiter's borrowed deadline fired,
`rearm_period_ns()` said "nothing to re-arm" (the timer is borrowed) and the vector neither
moved the compare nor masked the timer, so the line stayed raised and the vector was
re-entered the moment it ended — until the borrower ran again and gave the timer back.
x86's TSC-deadline fires once and never showed it. The vector now silences the timer when
it has nothing to re-arm (`trap.rs`), and after the scheduler owns the timer a zero period
is a dedicated core, not a cue to revive the bring-up tick (`smp::rearm_period_ns`). It hit
every timed wait on arm64 (futex timeouts, `parkNanos`), not libtsync alone; `tsync` is in
the arm64 gate now.

**Graviton like for like** (`docs/MEASUREMENTS.md` §10.3). The UDP RTT from a Linux
`c7g.large` neighbour: to a second Linux `c7g.large` p50 34.1 us, to Talaria's `userver` on a
`c7g.large` p50 34.5 us — a tie (8 alternating pairs of 20 000), with Talaria's p99 the
narrower (39.9–42.1 against 39.8–49.4 us). The ENA's PHC is declared on none of `c7g`,
`c7gn`, `c8g`, `m7g`: the wall clock's reference on Graviton is not the PHC (the Amazon Time
Sync service over the network is the candidate — a design item, not done).

**The KVM stand on the audit** (`c7g.metal`, `infra/aws/kvm-stand-aarch64.sh` at `bd0e701`, the
instance destroyed; `docs/MEASUREMENTS.md` §10.4): the gate under KVM **268/0**; `netnum`'s
`tcp [netirq=1]` 39 509 req/s, 25.3 us a request — the `diskserver` fault of §10.7 is gone on
hardware, §10.8's fix confirmed; the soak **0 of 50** on all ten modes with `piperace` among
them; the parallel race detector **0 of 96** on `forkap`/`shmrace`/`memrace`/`piperace`, 24
at once on 64 cores; one whole-gate loop clean. What it also showed: the wake path is slower
than on the previous stand — futex cross-core p50 513 -> 558–598 ns, condvar 1900 -> 2450–2590
ns — the price of the audit's barriers there (the SGI's `dsb`, the acknowledged membarrier,
the `SeqCst` loads), to be attributed and paid back by measured commits. The stand ran
before the timer fix above.

The gates: arm64 `test-aarch64.sh` **269/0** under TCG (240 before: `piperace`, virtio-pci
with and without firmware, the bridges, the cell's two modes, `tsync`, `fiber`, `syncbench`,
`procdemo`); x86 `test.sh` **496/0** + host 22/0 (`piperace`, the virtio-pci echo under OVMF);
`kernel-unit` 296/0; clippy clean on both.

### 10.11. The wake path's step bisected; phase 5's small ones (2026-10-04)

**The stand on the timer fix and an A/B in one run** (`docs/MEASUREMENTS.md` §10.5). The gate
under KVM 270/0 with `tsync` in it — the generic timer's storm fix of §10.10 holds on
hardware; the soak, the race detector and the loop clean. The stand script has an A/B step
now: `BISECT=` commits and `VARIANTS=` patches over HEAD, each built in its own tree, then
`futexlat` and `svccost` in interleaved rounds; `ABONLY=1` runs it alone.

**What it found about §10.10's wake-path price.** The step is the audit commit's and
reproducible (+50–60 ns futex RTT, +400–500 ns condvar; the RAM-above-4-GiB and
masked-restore commits before it are flat). It is NOT the three barriers it was blamed on
(the SGI's `dsb ishst`, the membarrier ack, the dead-space scan's Acquire — all three reverted
together leave it whole), not Acquire as `ldar` (`+rcpc` does nothing), not code alignment
(both sides 64-byte aligned keep it). No single file's revert takes it back beyond the
spread. Still owed: the combined reverts and a hunk-level split (`ABONLY=1`, ~20 min of the
instance). The barriers stay — they are correct and they are not the price.

**x86's SeqCst downgrades, measured.** The waiter's registration (`sync::wait_on_ex`: eight
SeqCst stores before a fence — eight `xchg` per wait) made Relaxed + a Release WADDR:
futex RTT 446–471 ns against 438–463 under KVM. Nothing to pay back; not committed. The
scheduler's `fence(SeqCst)` after a SeqCst `swap` in `ap_claim` is redundant by the model
but sits off the mwait wake path; left.

**Phase 5's small ones.** `l1::arm_intr` writes the ENA's receive unmask directly
(`NetDev::arm_reg`) instead of skipping it when a neighbour holds the card; `netprof`'s
dump slot is claimed by a swap. The 1 GB identity leaves of a high area that cover a hole
beside it stay: the right fix is 2 MB leaves for a partial gigabyte, which touches how every
address space inherits the kernel's PDPT, and no machine seen puts a device in such a
gigabyte (Graviton's PCI window is in GB2 — the 16550 at 0x8004_8000 — x86's high BARs are far above RAM).

**The wall clock on Graviton** (no PHC, §10.10): the servo takes any source
(`wallclock::feed_ref`), but nothing feeds it in production on either machine — the PHC is
read by a self-test only. A reference over the network (SNTP to the Amazon Time Sync
service, 169.254.169.123, from a thread with the network stack, the round trip's midpoint as
the sample, a source `SRC_NTP`) is a new feature for both architectures, not a port item.

Gates: arm64 `test-aarch64.sh` 269/0 (TCG), 270/0 (KVM); x86 `test.sh` `TAL_CI=1` 182/0 + host
16/0; clippy clean on both.

### 10.12. Phase 8 under TCG: PMUv3, the profile with stacks, the gdb stub (2026-10-04)

**PMUv3** (`arch/aarch64/pmu.rs`). The surface is x86's — the cycles, the retired
instructions, the last-level cache, the predictor, on every core — and the per-core
snapshots, the tick's 20 ms step, `perf poke` through an IPI and the refresh before printing
moved out of x86's module into the shared `perfsample.rs`, so `perf`, `top`, `kstat` and
`metrics` read both machines the same way. The events are chosen by `PMCEID0/1` and checked
against the deed (a counter that does not move over a short loop is dropped, not printed as
zero); the cycles are `PMCCNTR_EL0` with `PMCR.LC`. The difference from x86 is the width:
before PMUv3p5 an event counter is 32 bits and wraps in about a second at a workload's
rate. The module takes the first the machine allows — 64-bit counters (`PMCR.LP`), CHAIN
pairs (half the events), or 32 bits widened by the reader (correct while the core is read
once per wrap: the tick's snapshot does it; a dedicated core unread for longer loses the
wraps, and the boot line says which width it is). QEMU's `max` is PMUv3p5 and models
`INST_RETIRED` only under `-icount`; the gate runs `-icount shift=0`, where the IPC is
exactly 1.00, and `talaria.pmunarrow=1` forces the reader's widening and sets the counter
4096 events below 2^32 before a million-iteration loop — the positive control (the widening
switched off) reads a difference of 2^64 - 4 288 967 251. EL0 may read the counters
(`PMUSERENR.CR|ER`), not write them.

**The profile with stacks.** The tick's sampler read the interrupted frame pointer as the
frame's ninth word and the privilege as `cs & 3` (x86's RBP slot and CS selector): on arm64
every ring-3 sample was filed as the kernel's at depth 1 (`docs/ENGINEERING-NOTES.md` item
32). Through the frame's accessors: `profdeep` has 248 ring-3 samples, the most frequent
stack 237 times, the average depth 5.8 — the same picture as x86.

**The gdb stub** (`arch/aarch64/gdb.rs` over the shared `gdbproto.rs`). arm64 has one serial
line — `virt` models one PL011, a Graviton guest's only port is the console's — so the stub
shares the console's, as Linux's kgdboc does: while the machine is stopped every other core
is parked (an SGI; its frame is what `g` shows for its thread) and the line carries packets
alone; while it runs, gdb skips the console's text; gdb's ^C reaches the console's poll and
stops the machine. The machine's part: `brk #0x401` for the kernel's own stop, `brk #0x400`
written into the text for `Z0`, the breakpoint and watchpoint registers for `Z1`-`Z4`
(published as the machine's state and applied by every core at its scheduler entry, the OS
lock released first), the software step with the stepped frame's D cleared and I set. Two
things arm64 does not give: a resume flag — a resume from a hardware breakpoint or a
watchpoint is a silent step over the instruction with this core's registers off, then the
registers back — and debug exceptions in the kernel, which runs with `PSTATE.D` set, so the
registers catch the workload and the kernel is debugged with `break` and `stepi`. The stub
waits for `?` until gdb is attached: an unsolicited stop reply crossed gdb's `qSupported` on
the shared line and the attach failed.

Gates (TCG, new in `test-aarch64.sh`): the counters with the exact IPC, the widening across a
wrap, `obs` with kstat naming the counters and the `wfe` tier, `perf` in the console,
`profdeep`, `obslive`; the stub — `g` of 536 hex digits, two steps, `hbreak`, detach — and a
host gdb attaching over the console's line (`bt`, `cpsr` decoded, `info threads`, detach); on
SMP a stop before the workload's first instruction, an `hbreak` on a thread spinning on a
secondary caught there, a write watchpoint stopping at three consecutive stores (0, 1, 2 —
the silent step-over works), ^C, the thread list, detach, and the workload's counter moving
under a re-attach. x86's stub moved onto `gdbproto.rs` (its `gdb-stub` gate passes); its `m`
no longer faults on an unmapped address and its `qAttached` answers. arm64
`test-aarch64.sh` **289/0** under TCG (269 before); clippy clean on both.

**On the stand** (`docs/MEASUREMENTS.md` §10.6): the gate under KVM 305/0 with all of the above
on a Neoverse V1 through KVM (its debug registers, its PMU), the soak 0/200, the race
detector 0/192. The A/B of §10.11's wake-path step closed one door: reverting the audit
commit's whole `kernel/src` over HEAD leaves the step in place — it is not the audit's code;
the next suspect is what else that commit changed (the `tsync` set embedded by `build.rs`,
a larger image and everything after it moved).

**Phase 7, the first step: the small services** (`services/build-aarch64.sh`). Lua, SQLite,
QuickJS, BusyBox, mongoose and Redis — the sources each README fetches, cross-built with the
aarch64 musl at 0x5000_0000, unmodified — run on arm64: the four interpreters to their
verdicts from a TFS1 disk, mongoose answers HTTP and Redis RESP over virtio-net, zero unknown
syscalls. Redis runs as a process (`mode=proc`, ext2): on arm64 it forks at start-up to test
the kernel for Linux's ff1712f953e2 (the dirty bit lost across a write-protect) and reads the
child's `/proc/self/smaps`. It refused, and was right — `paging::protect` dropped D on both
machines and arm64's read-only encoding had nowhere to keep it (`docs/ENGINEERING-NOTES.md`
item 33). D is carried now; `/proc/self/smaps` is generated from the tables
(`paging::as_runs`: a run of resident private pages with the same rights is a mapping, Rss and
the shared/private clean/dirty split by the COW refcount and D); `talaria.test=dirtyprot`
(both machines) fails on each half of the old code. The gate runs the services when
`kernel/target/a64-services` is built (12 checks; the network two in `test-aarch64-net.sh`).
The databases, the JVM and ClickHouse still need the arm64 builder (`c7g` + docker).

Gates after both steps: arm64 `test-aarch64.sh` **307/0** under TCG (the services and
`dirtyprot` included), x86 `test.sh` **497/0** + host 22/0; clippy clean on both.

### 10.13. Phase 7, the second step: the databases and the JVM without a builder (2026-10-05)

**No arm64 builder was needed.** The plan's builder (a `c7g` with docker) was for compiling
the services; but MongoDB, the JVM and ClickHouse are ready-made binaries on x86 too, and the
distributions ship aarch64 builds of PostgreSQL and MariaDB as well. `apk --arch aarch64
--root DIR --no-scripts` in an x86 Alpine container unpacks the aarch64 packages with their
dependency closure without executing anything (`services/build-aarch64-db.sh`): PostgreSQL
16.14 and MariaDB 10.11.18 from Alpine 3.20, MongoDB 4.0.5 from Alpine 3.9 (x86's release),
Temurin 21.0.12.1 for alpine-linux aarch64 (x86's build). The data directories PostgreSQL and
MariaDB need an `initdb` the host cannot run; they are the x86 builds' — the same bytes on
both machines (little-endian, MAXALIGN 8; `pg_control` checks exactly that). The clients are
Python (`pgprobe.py` is new), so the arm64 stand runs the gate unchanged.

`kernel/test-aarch64-db.sh` (the arm64 twin of `validate.sh` B10-B11) under TCG: **12/0** —
PostgreSQL (a table written by one forked backend, read by a second), MariaDB (InnoDB through
a connection's thread), MongoDB (a dynamic ELF with 18 shared objects, WiredTiger, OP_MSG
insert/find/count), the JVM (start, interpreter and JIT, a cross-thread park); zero unknown
syscalls. The dynamic loader needed nothing: `talaria.test=dyn` (a PIE through ld-musl-aarch64,
dlopen of libz) passed on its first boot.

**What the real programs found** (`docs/ENGINEERING-NOTES.md` items 34-37):
* `AT_HWCAP`'s CPUID bit promised an emulation of EL0 `mrs` of the ID registers that did not
  exist — OpenSSL reads `MIDR_EL1` at load and PostgreSQL died with a SIGILL.
  `trap::emulate_id_read` answers as Linux's `emulate_mrs` (MIDR, MPIDR as 0x80000000, the
  ID_AA64 registers without SVE/SME/MTE/PAC); `talaria.test=idregs`.
* `MAP_FIXED_NOREPLACE` was honoured only beside `MAP_FIXED`: HotSpot's 64 probes for an
  encodable compressed-class-space base all came back elsewhere and Temurin refused to start
  (both machines; on x86 the heap's fallback hid it).
* Without a NIC every `epoll_wait`/`poll` timeout was zero (`netsys::HZ`'s placeholder).
* Under TCG an idle PostgreSQL is six processes spinning `epoll_pwait` on sets with a
  signalfd: `select 1` took 0-147 s. `talaria.netmix=<ns>` (off by default; the TCG gate turns
  it on) parks a non-polling waiter of a mixed set; a pipe's bytes, an eventfd's write and a
  signal made pending kick the waiters (`netsys::mix_kick`, Dekker's order on `NW_IN`).
  `talaria.test=mixwait` (both machines): the parked one is woken by each producer within
  2 ms; with the kicks removed it wakes at its 2 s ceiling (1970 ms) and the gate fails.

**ClickHouse: the address space, then a defect of both machines.** Its official arm64 binary
(x86's release, 25.8.33.6, under Ubuntu 22.04's arm64 glibc) is an ET_EXEC at
0x200000..0x1CA9_7310 (458 MB). Every process of this kernel shares the kernel's identity map,
and there it was the devices on `virt` (the GIC at 0x0800_0000, the UART, virtio-mmio, the
32-bit PCIe window) — and, the plan believed, RAM on a Graviton (it is not: §10.14). x86 had the same collision
with RAM and moved the kernel above the binary (`services/clickhouse/build-kernel.sh`).
1. DONE: the kernel reaches GB0's devices through an alias — the same 1 GiB Device block
   once more in L0 slot 384 (`mmu::DEV_SLOT`, VA 0xC000_0000_0000 + PA), in the early map
   and in the kernel's root before the first address space copies it. `arch::mmio_va` is
   applied where a base is first read (the GIC frames, the ITS, the UART after the MMU is
   on, the RTC, the virtio windows, a low ECAM, `pci::bar`, the virtio HAL); what a DEVICE is
   told stays physical (`mmio_pa`: the ITS's translater, a redistributor in a MAPC). x86's
   `mmio_va` is the identity. The prints keep the physical addresses the gates match.
2. DONE: GB0 of devices is no longer mapped by its identity at all on arm64, so it is free in
   every process; the loader lays the ET_EXEC there with fresh frames. Unmapping it found a
   null dereference the flash had been answering (`docs/ENGINEERING-NOTES.md` item 40).
3. DONE without code (§10.14): on a Graviton GB0 is devices too (the GIC, the ECAM; RAM from
   1 GiB), so step 2 already covers it; the knob `talaria.lowres=` this item planned was for a
   memory map nobody had read — the machine's own boot log of §10.8 said otherwise.

On `virt` under TCG ClickHouse then answered — and crashed about every third run, a thread
returning to address 0 right after starting. The hunt built three instruments that stay: a
frame ledger (`mem::fc_*`: a bit per frame, a hand-out of a handed-out frame or a free of a
free one is said with the caller), the page tables' journal (`talaria.ptlog=1`: every entry
write with the line that made it; a killed process prints the history of its pc's and fault's
pages) and the kernel stacks' quarantine (`talaria.kq=1`: a freed stack is poisoned and
checked before reuse). They excluded a double-allocated frame, a replaced entry and a stack
written after its free; the crashed thread's register file named it: x0..x18 and lr zero.
A syscall that moves its thread to another core rebuilt the return context from the
callee-saved registers only (both machines; item 38, `talaria.test=migregs`). With the whole
register file carried, ClickHouse's runs are clean (6 of 6, four with the push off, against 6 crashes in 6 with the
stealing push off). Also: `get_mempolicy`/`set_mempolicy`/`mbind` (one node: the default
policy, accepted) — arm64's ClickHouse asks at start-up.

**On the metal** (`docs/MEASUREMENTS.md` §10.7-§10.8): the five servers under KVM 15/0 with the
product's spinning wait and 15/0 with `netmix` (the JVM's speedup4 3.98, park 0.40 us;
ClickHouse's B12 in 5 s). The stand found one more defect of both machines: the secondary
cores ran `syscall::init`, whose global half allocated and re-zeroed the workload's brk heap —
a test's static image died on its first instruction in 10-17 % of the parallel detector's
boots; `init_core` closes it (0 of 288 after, 31 of 288 before; ENGINEERING-NOTES item 41).
The wake path's "step" of §10.11-§10.12 was the layout's lottery: with and without the
embedded `tsync`, today's tree is at `ea24e3f`'s level.

Gates after the session: `kernel/test-aarch64-db.sh` **14/0** under TCG (the five servers,
ClickHouse's B12 set in 20 s), arm64 `test-aarch64.sh` **316/0** (`dyn`, `idregs`, `mixwait`,
`migregs` new), x86 `test.sh` **501/0** + host 22/0 (`mixwait`, `migregs` new), kernel-unit
296/0; clippy clean on both. Phase 7's list stands at: everything on `virt` (the gate) — the
KVM stand's numbers in `docs/MEASUREMENTS.md` §10.7; ClickHouse on a Graviton — §10.14.

### 10.14. A Graviton's own boot runs ClickHouse; the wall clock gets a reference; the plug pulled (2026-10-05)

**ClickHouse on `c7g.large`, no change needed.** §10.13's step 3 planned a knob for "GB0 is RAM
on a Graviton". The boot log of §10.8 had said otherwise all along: the firmware's map is RAM
at 0x4000_0000..0x8000_0000 and from 0x4_0000_0000, the GIC at 0x1000_0000, the ECAM at
0x2000_0000, the PCI window (the console's 16550, the NVMe and ENA BARs) at 0x8000_0000 — GB0
is devices, exactly as on `virt`, and the devices' alias already frees it. The official arm64
binary ran validate.sh B12's set to the right sums and `exit_group(0)` on its first boot there
(`docs/MEASUREMENTS.md` §10.9). The comment in `mmu::gb_kind` that carried the wrong map is
corrected. `docs/ENGINEERING-NOTES.md` item 42.

**The wall clock's reference: SNTP** (`talaria.ntp=aws|<a.b.c.d>[,<poll s>]`,
`kernel/src/ntp.rs`, both machines). §10.11 left the servo with nothing to feed it on a Graviton
(no PHC on Nitro's ENA, no paravirtual clock on arm64 KVM) and on any metal. The client is a
kernel UDP socket on the first interface, run in the network stack's pass like the DHCP client
(so wherever the stack is driven, workload or idle). A query carries our counter in its
transmit field and the server echoes it, so a reply names its query and nothing is kept per
query; the server's `(T2 + T3) / 2` is paired with our `(t1 + t4) / 2`. A poll is a burst of
four and only the shortest round trip is fed (the pairing's error is bounded by half the round
trip; a queue lengthens it). The network reference owns the servo (`wallclock::set_net_ref`:
kvmclock's tick feed stands aside), feeds it with its own interval (`feed_ref_iv`), and its first
sample before the workload starts SETS the base (`wallclock::set_base`) instead of stepping it —
EC2's UEFI clock was a second ahead and the first version counted a "step back" at boot
(`docs/ENGINEERING-NOTES.md` item 43). The boot waits up to 3 s for that first sample.

Gates (`kernel/test.sh` and `test-aarch64-net.sh`): `kernel/tools/minintp.py` serves the host's
clock +3 s running 150 ppm fast; the first sample sets the base (+3.0 s under kvmclock, +3-4 s
from arm64's PL031 whole seconds), eight samples without a step, and the judge
(`kernel/tools/ntpcmp.py`: the kernel echo's wall-clock stamp against an SNTP answer, both
referred to the host's clock) puts the guest within 1 ms (3 ms under TCG) — without the
feature it is 3 s off. A silent server is said and the boot goes on. On `c7g.large` against the
Amazon Time Sync Service: ±21 us judged from a Linux neighbour, the servo at the crystal's
+15 ppm. Host tests for the packet, the parse (kiss-of-death, unsynchronised, reversed
timestamps), the midpoint pair and the burst.

**The pulled plug on EBS** — phase 7's second exit gate: `ec2-crash-test.sh` on `c7g.large`,
ordered and `journal=data`, both `RECOVERED OK` after `stop-instances --force`.

**The guide on a Graviton, and what it found.** `talaria-deploy.sh` takes arm64 (`--arch`, or
the binary's `e_machine`, or a Graviton type; the UEFI image through `mk-ami.sh`, the AMI
`--architecture arm64 --boot-mode uefi`, no disk trace — the console works), `examples/mdpipe`
builds for aarch64 (`make a64`), and HANDS-ON-GUIDE §11.1 is the run on a `c7g.large` — phase 8's
last exit gate. The run found three defects, each reproduced under TCG with the old binary and
now gated (`docs/ENGINEERING-NOTES.md` items 44-45):
* the supervisor's reset at a workload's end closed the SNTP socket with the workload's UDP
  sockets, and the next pass panicked in smoltcp on the dangling handle;
* a workload that does not use the network does not drive the stack, so the client in the pass
  sampled once in 30 s — it now runs from core 0's tick when a poll is due and the stack has
  been idle 2 ms, with a bounded wait (≤ 500 us a query, ≤ 2 ms a tick) so the round trip does
  not carry a tick;
* `top`'s IPC of 6.8e7: a `c7g.large` declares CHAIN and its odd counter never carries (a raw
  probe: five wraps of the low half in ten seconds, the high half 0; a carry check at boot passed —
  right after a write it carries); CHAIN is opt-in now (`talaria.pmuchain=1`), below PMUv3p5 the
  reader widens 32-bit counters, and `top` refuses (and prints raw) a difference no core can
  produce. On the way, the per-core snapshot slot became a seqlock
  with the writer's interrupts masked.

**The KVM stand on `e95abf4`** (the TODO's "the whole stand on HEAD"): the gate 313/0 under KVM,
the five servers 15/0 in both wait modes, the soak 0 of 650, the parallel detector 0 of 576, two
more gate runs clean (`docs/MEASUREMENTS.md` §10.10). `netmix` stays off by default — under KVM
it changes nothing measurable.

Phase 7 stands complete against its table: the services on `virt` and under KVM (§10.13),
ClickHouse on the Graviton itself, the power cut on a `c7g` with EBS. Phase 6's
"wall-clock reference decision" is made: SNTP everywhere, the PHC where it exists.

Gates after the session: arm64 `test-aarch64.sh` **338/0** under TCG (six `ntp` checks new, no
SKIP), x86 `test.sh` **507/0** + host 22/0 (six `ntp` checks new), kernel-unit 302/0 (six `ntp`
tests new), `talaria.test=obs` with the PMU difference's plausibility on both, clippy clean on
both.

### 10.15. Phase 9 begins: the scoreboard's path on a Graviton; the cache counter; the mdpipe tail named (2026-10-05)

**The scoreboard's arm64 path** (`bench/suite`, `ARCH=arm64`, `lib/arch.sh` — one switch for the
whole EC2 path). The workload trees take the arm64 builds: Redis and mongoose cross-built
static (`services/build-aarch64.sh`); nginx, PostgreSQL 16, MariaDB 10.11 and MongoDB 4.0.5 are
Alpine's own aarch64 packages (`services/apkroot.sh`, factored out of `build-aarch64-db.sh`)
put at x86's paths, so the manifests, the specs and their measured settings stay ONE file for
both machines (nginx's configure runs its test programs and does not cross-build; the Alpine
binary is the same on both sides of the stand). `tlgen` reads `CNTVCT_EL0` (`make a64`). The
AMI is the aarch64 stub (`mk-ami.sh` refuses a stub older than the kernel), terraform takes
AL2023 arm64 and `c7g.xlarge` (4 vCPU as x86's `c6i.xlarge`, four physical cores — no SMT on
either side), the playbook leaves out the x86-only kernel parameters. `a64-smoke.sh` boots every
arm64 image under TCG in both roles (6 workloads, 15 cases, the generator role on Talaria arm64
with `netidle`: all PASS); `ARCH=arm64 infra/smoke.sh redis` on a `c7g.large` 11/0 with the
inventory matching. The scoreboard itself: §10.17.

**The cache counter on a Graviton VM** (the next-session list's item 4). The boot dropped a
working counter: LL_CACHE_MISS_RD is not declared there, the fallback L2D_CACHE_REFILL is and
counts (0.99 per line over a random 64 MB walk), but the "does it move" check is an ALU loop.
Now only the instructions and the branches are judged by moving (as x86 judges its fixed
counters), and an event with no counter prints "- (no counter)" (`top`, `perf`) instead of a
zero. `talaria.test=pmu` gained `pmu-ev:` — every candidate cache event on the last counter
(borrowed and given back where there is no spare, a `c7g.large` has two) across the walk.
`docs/ENGINEERING-NOTES.md` item 46, `docs/MEASUREMENTS.md` §10.11.

**The mdpipe tail on a Graviton** (item 2). Ten runs: p99 33.5-36 us in the default wait tier
`wfe(monitor)`, 6.8 us with `talaria.sync=hlt`, 4.6-5.7 us on Linux on the same type. The
theory "Nitro wakes a `wfe` sleeper late" was measured and refuted (`talaria.test=wakeprobe`:
a store 140 ns, an SGI 2.3-2.5 us under `wfe` and `wfi` alike, after 30 us .. 1 ms of sleep); a
first probe that switched the tier on the store's wake alone was built and dropped the same
hour. The tier's traffic differs (2.4x the futex waits, 2.3x the reschedule IPIs), and the knobs
move it: `talaria.wakepreempt=0` takes the p99 to 8.2 us. The wake latency itself is mostly
under 20 us in both tiers, so what that knob changes is open; `talaria.sync=hlt` (6.8) and
`wakepreempt=0` (8.2) are the measured levers meanwhile. Item 47.

**PostgreSQL on a Graviton's memory map** (found by the scoreboard): the FS cache took half of a
`c7g.xlarge`'s 844 MB below 4 GB, where a process's `MAP_SHARED` memory must come from, and the
postmaster's 256 MB `shared_buffers` failed; in the same shape under TCG (`virt` 10 GB,
`talaria.lowmem=850`) the old kernel panicked on a page table. The cache comes from the high area
now. Item 48. And mongodb's launch panicked on a kernel stack right after its manifest's 512 MB
cache resize was refused: the bump allocator moved its pointer before checking the bound, so a
refused request took the rest of the bump (both machines; `talaria.test=allocfail`). Item 49.

**The direct map**: the plan is §10.16.

### 10.16. The identity window's real fix: a kernel direct map (the plan, 2026-10-05)

§10.9 ended with "the real fix of the identity window is a kernel direct map in the higher
half", and the GB0 alias of §10.13 was its first, narrow piece. This is the plan; no code yet.
The line numbers are `bb198d8`'s.

**What leans on virt == phys today.** The contract is written down three times — P1
(`paging.rs:12`), `IDENT_HI` (`paging.rs:284`) and L3 (`user.rs:23`) — and the kernel has no
heap: every buffer is a frame from `mem::alloc_*`, used as a pointer as it is. Five kinds:

| kind | where | about |
|---|---|---|
| (a) a frame used as a kernel pointer | the page-table walks (`paging.rs`: ~56 entry dereferences, ~49 table casts), fresh-frame zeroing and the COW copy, the ELF loader and the stack builder (`user.rs`), the FS cache and its buffers (`ext2.rs`), the refcount table (`mem.rs`), the vDSO, the ITS tables, the kernel-stack poison, probes and self-tests | ~150 sites |
| (b) a kernel pointer given to a device | the NVMe PRPs (`nvme.rs`, `fs.rs`), the ENA rings and buffers in `.bss` (`ena.rs:126` says "phys == virt"), the ITS tables, x86's PV-EOI word; virtio's arena already asks `paging::virt_to_phys` | ~20 sites |
| (c) page-table entries | they store physical addresses already; every walker dereferences them through the identity | the walkers |
| (d) MMIO | arm64's GB0 goes through `mmu::DEV_SLOT`/`mmio_va`; every other device gigabyte (x86's GB3, `map_mmio_1g` for BARs, Graviton's PCI window) is identity | `pci.rs`, `paging.rs` |
| (e) code before any new map exists | x86's 32-bit entry with its static 4 GiB of 2M pages, the AP trampoline at 0x8000, arm64's `early_enable`, the DTB/ACPI parsing, the UEFI stub's hand-off | the boot paths |

The user-visible part is the hard one: a `MAP_SHARED`/shm mapping returns the frame's
identity address to ring 3 and opens it in the process's private copy of the kernel gigabyte
(`syscall.rs` around 2250), and the identity modes (`cell`, `run_disk`) run ring 3 in the
kernel's own space by design.

**The design.** `mem::kva(pa)`/`kptr::<T>(pa)`/`kpa(va)`; today's `paging::virt_to_phys` (a walk
of the current tables) becomes `walk_phys`. The allocator, the refcount and the entries stay
keyed by the physical address and are translated where they are dereferenced; the drivers'
entry points take a `Pa` newtype, so a kernel pointer handed to a device does not compile. The
map lives in top-level slot 256 on both machines — x86 `0xFFFF_8000_0000_0000`, arm64 TTBR0's
L0[256] (`0x0000_8000_0000_0000`, above `USER_TOP`, which `user_range_ok` already enforces):
1 GiB leaves, supervisor, never executable, Normal WB, ONLY the firmware map's RAM (a device
or a hole must never be cacheable — on arm64 that is an architectural aliasing fault). The
slots exist before the first address space, so `copy_kernel_pml4` (`paging.rs:2164`, slots
1..511) gives every process the map as it gives the kernel today. **TTBR1 is not needed for
this**: a high TTBR0 slot keeps §4.3's one table and is the same code as x86 — §8's row stays
true until the image itself moves. Kernel stacks move to a window below the map with their
own 4K tables and a guard page there, which retires the per-space guard dance
(`sched.rs:2105`) and means no direct-map leaf is ever split at run time (on arm64 that would
be break-before-make on memory other cores use).

**The stages**, each with both regressions green:

1. The helpers as the identity, every site converted (paging; the drivers with `Pa`; the FS,
   the loader and COW; the rest), a census meter like `census-aarch64.sh` counting what is
   left; gate `dmapid` (`kpa(kva(x)) == x` for low, high, `.bss` and stack frames). No
   behaviour change.
2. The map beside the identity, `kva` switched to it; gate `dmap` (written through the map,
   read through the identity: the low allocator, the high area, the 17 GB refcount top, a NUMA
   node; a fresh process and a fork child have the slot).
3. The kernel-stack window; `kstackguard` adapted, a new `kstackwin` (an overflow faults on the
   named guard in a process, a fork child, after a refused fork), `forkap` x16.
4. (a) Shared mappings get a chosen user address instead of the identity one; (b) RAM's
   identity leaves leave process spaces, first behind `talaria.dmap=strict`. Gate
   `dmapstrict`: no identity RAM leaf in a process; a `MAP_FIXED` over a former high-area
   gigabyte succeeds and the kernel's writes to those frames do not show there; an x86 ET_EXEC
   at 0x4000_0000 loads; `highmem`/`identhole` rewritten; PostgreSQL's shared memory on both.
   **The risky stage**: a missed conversion is a ring-0 fault.
5. The MMIO window on x86 and every device gigabyte on arm64 behind `mmio_va`.
6. The 4 GiB cut lifted: one allocator over all RAM, several high areas, a DMA32 pool only for
   what needs it (the trampoline, the AP's 32-bit CR3, 32-bit devices).
7. Later: the image in the higher half (x86 −2 GiB, `code-model: kernel`; arm64 linked high),
   where TTBR1 is reconsidered; ClickHouse's high-base kernel retires.

**Stage 1 is done (2026-10-06).** The meter is the compiler: `kernel/census-dmap.sh` counts
the nightly lint `implicit_provenance_casts` (integer->pointer and pointer->integer) on both
machines — 609 sites on x86 and 639 on arm64 at the start, four times the estimate above,
because the 190 ring-3 addresses of `syscall.rs` and its kin are sites too. Each one now names
its kind through `kernel/src/kaddr.rs` (re-exported as `mem::`): `kptr`/`kva` (RAM by its
physical address, ~290 sites), `kpa_of`/`kpa` (to a device, an entry, CR3/TTBR, ~40),
`vptr`/`vaddr` (already virtual, ~140), `uaccess::uptr` (ring 3, ~220), `devptr` (MMIO, ~65).
At 0 the lint is `deny` in `main.rs`. The drivers' pass found three ENA sites that gave the
device the buffer's virtual address as a plain integer (no cast to see) — they `kpa()` it now,
and the `NetDev` contract is "the kernel's virtual address, the driver translates". Gate
`talaria.test=dmapid` on both machines. Left for later stages, marked `// DMAP:` in the code
(`grep -rn 'DMAP:' kernel/src`): the identity modes' dual addresses (a frame that is also ring 3's
address — the fast ring, `load_elf` in `run_disk`, the shm/`MAP_SHARED` zeroing, stage 4); the
PRP buffers on a kernel stack (`fs::read_raw`/`write_raw`, `nvme::read_blocks_boot`: stage 3
moves the stacks out of RAM's map, so those need a bounce frame); the probes that test the
identity leaf itself (stage 4 rewrites them); `mem::init` reading the boot hand-off before any
map exists (kind (e): it must stay the identity); `kva` must stay the identity until the map is
built in `paging::init` (stage 2's ordering). The `Pa` newtype for the drivers is not done yet.

**Stage 2 is done (2026-10-06).** `paging::dmap_init` builds the map right after
`paging::init`, before any address space: slot 256 of the reference table (x86
`0xFFFF_8000_0000_0000`, arm64 TTBR0 `0x0000_8000_0000_0000` = `USER_TOP`), the firmware map's
RAM rounded inward to 2 MiB, a 1G leaf for a gigabyte that is RAM end to end and 2M leaves for
the rest, supervisor, NX, pre-dirtied (arm64's DBM), one implementation through `mmu::enc`. A
bit per 2 MiB (`kaddr.rs`, 32 KiB) says what it covers; `kva` translates only that, and every
other address keeps its identity and is counted (`DMAP_MISS`) — under QEMU x86 22 k such calls
per boot, the last one `0xbfffffc6`: the ACPI tables in a reserved region, the first thing stage 4
must give a kind of its own. `talaria.dmap=0` is the identity everywhere (the A/B and the
fallback). The two regressions on the map: one failure each, the same probe (`dirtyprot` wrote
through `kptr` and read the identity leaf's dirty bit — a `// DMAP:` site; it writes through
the identity now); then x86 511/0, arm64 348/0, with gates for the map ON by default and OFF
by the knob. On the machines (`infra/aws/variants.sh`, mdpipe, the map ON): a `c7g.xlarge` maps
8026 MB (6 1G + 941 2M leaves) and gives the morning's numbers (p99 34.0 on two cores, 2.1 on
four, against 32.9-34.0 and 2.6 without the map); a `c6i.xlarge` (tboot) maps 8044 MB (5 1G +
1462 2M) and gives p99 7.6 / 6.3 against Linux's 10.7 in the same hour. Not done from the stage's gate list: a NUMA node's area and the refcount top are
covered by the same RAM walk, not tested separately.

**Stage 3 is done (2026-10-06).** The kernel stacks have a window of their own: top-level slot
257 (`mem::KWIN_BASE`, x86 `0xFFFF_8080_0000_0000`, arm64 `0x0000_8080_0000_0000`), its table
built with the map, so every space shares every table under it. Slot i is 64 KB of addresses:
page 0 the guard, never mapped — in every space at once — and pages 1..15 mapped one by one
from single frames the first time the slot is used (`paging::kwin_map`; arm64 publishes with
`dsb ishst`). A freed stack's slot goes back to `sched`'s pool WITH its frames, so the window is
never unmapped and never needs a shootdown, and no leaf of it changes while in use. This
retires both the per-space guard dance (item 25) and a stack's need for 64 KB of contiguous
memory (item 49's panic). `migrate_current_to` takes a window stack too. `talaria.kwin=0` keeps
the identity stacks (and the old gate still runs on them). A crash dump names a fault in the
window: "A KERNEL STACK OVERFLOW: CR2/far is the GUARD page, slot N". Not done: a push
overflow still double-faults on x86 (no IST for #PF/#DF) — as with the identity stacks.

The regression found what stage 1 had marked: a buffer on a kernel stack handed to a device by
`kpa_of` of its first byte. A window stack's pages are separate frames, so the physical address
was wrong — the power-button gates lost an unsynced file and the second boot saw no ext2 (a
journal commit block built on the stack went to the disk from another frame). `fs::Bounce`
copies such a buffer through contiguous frames at the three doors (`read_raw`/`write_raw`,
`io_one`, `blk_write_durable`) and `nvme::read_blocks_boot`. x86 513/0, arm64 350/0. On the
machines (mdpipe, threads by `clone`, ext2 on EBS with fsync): `c7g.xlarge` p99 33.5 / 2.7 on two
/ four cores, `c6i.xlarge` 7.3 / 5.4 — the stage-2 numbers.

**Stage 4a is done (2026-10-06).** An anonymous `MAP_SHARED` in a process takes an address of
its own — the process's mmap region, or the `MAP_FIXED` one — and its frames are mapped as
non-private user leaves (`paging::map_leaf_shared`), zeroed through the direct map;
`fork_cow_space` hands any such leaf below `USER_TOP` to the child as it is (it used to share
only the identity ones in the user gigabyte). The frames' lifetime is the identity one's:
`unmap_proc` leaves a non-private frame alone and `mreg` frees it at a restart. On the way a
second defect closed: a `MAP_SHARED|MAP_FIXED` took the private branch and turned COW at the
first fork. `sigtest` checks both (the address `0x6…`, the fixed one shared after a fork); with
`talaria.sharedid=1` (the identity again) the same binary fails the fixed check — the gate
catches. x86 514/0, arm64 351/0, x86 `validate.sh` 14/0 (PostgreSQL, MariaDB, MongoDB, the JVM,
ClickHouse), arm64's databases (PostgreSQL's shared buffers, MariaDB, MongoDB) 10/0.

The named objects followed the same day: a SysV segment and a POSIX `/dev/shm` object take an
address in the NAMED WINDOW (`0x7000_0000_0000`..`0x7800_0000_0000`, below the dynamic loader's
`0x7f00…`), one per object, never reused and the SAME in every process — the identity's one
property worth keeping, because the futex registry keys a waiter by its address and a futex in
named memory must wake across unrelated processes. The frames are mapped as shared leaves into
a space when it attaches (`shwin_attach` at `shmat` and at the object's `mmap`); ivshmem (the
host's memory) keeps its identity; `talaria.sharedid=1` covers these too. `sigtest` prints both
addresses (`0x70…`) and checks the POSIX object after a fork as well. x86 515/0, arm64 352/0,
`validate.sh` 14/0, arm64 databases 10/0.

**Stage 4b is done behind its knob (2026-10-06).** `talaria.dmap=strict`: a process space
(`new_process_space`, so every fork child too) keeps the identity only where the kernel still
needs it — its image and what lies below it in its gigabyte, and every 2 MiB the map does NOT
cover (devices, the firmware's reserved tables such as QEMU's ACPI at `0xbfffffc6`, holes: `kva`
gives those their identity, so they stay reachable). Every identity leaf over the map's RAM is
gone (`paging::strict_strip`; a 1G leaf the map covers in part is split in the process's own
PDPT copy). The kernel's space keeps the identity — the identity modes run ring 3 there. A
`munmap` leaves no leaf at all instead of the old supervisor identity one (`proc_ident_leaf`), a
`MAP_FIXED` over a former high-area gigabyte is allowed (`ident_leaf_overlap`), the zero-syscall
region is mapped at its address explicitly, `talaria.sharedid` is ignored, and a ring-0 fault on
such an address names itself in the dump ("RAM BY ITS IDENTITY in a process space"). Strict
refuses to start without the map or the stacks' window. A build with `TALARIA_STRICT` set makes
it the default — that is how both regressions ran over it. Gate `talaria.test=dmapstrict` (no
such leaf in a process space and its fork child, the kernel's write through the map does not
show in the process's own page at the frame's former identity address, a low and a high frame);
`identhole` and `highmem` know strict; `sigtest` runs once more under strict on both machines.

The whole x86 set over a strict-by-default build found what the gate could not: x86's AP boot
stack and pickup-loop stack were identity frames, and an AP that switched into a process space
on one triple-faulted silently (every four-core gate failed, QEMU simply exited). Both come from
the stacks' window now (ENGINEERING-NOTES 52). Then, strict by default: x86 525/0, arm64 339/0
(its one failure was `identhole` counting arm64's never-mapped device gigabyte as a hole —
fixed), x86 `validate.sh` 14/0 (PostgreSQL, MariaDB, MongoDB, the JVM, ClickHouse), arm64's
databases 10/0. The default build after it: x86 520/0 + host 22/0, arm64 356/0 (UEFI blocks
included), kernel-unit 302/0. Left: strict as the default (after the machines: `variants.sh`, the databases on
EC2), a `MAP_FIXED` below 4 GiB (still refused: the image and the low devices are there).

Then the plan's last gate item: `sigtest` linked at `0x4000_0000` (`kernel/user/sigtest-hi`,
x86's GB1 of RAM) runs ALL OK under strict — the image, its COW forks and its shared memory
where the kernel's identity was (`talaria.test=sigtesthi`; it passes without strict too, because
the loader still reserves an image's identity range below 4 GiB). x86 522/0 + host 22/0, arm64
356/0. On the machines (`variants.sh`, mdpipe, `smp=4`, the knob in user-data): `c6i.xlarge`
p99 5.5 us strict / 6.0 without / Linux 8.2; `c7g.xlarge` 2.45 strict / 2.26 without / Linux 7.1
— the same within a run's spread. Two things the machines taught: a user-data knob arrives after
`paging::init`, so strict is now set by the FINAL cmdline (it builds nothing at boot; it refuses
if an address space already exists); and an arm64 AMI carries the kernel inside the UEFI stub —
a stub built before the last kernel build shipped the old kernel without a word, so
`tools/mk-ami.sh` now rebuilds a stub older than the kernel.

**Stage 5 is done (2026-10-06).** The devices' window: top-level slot 384 on both machines (x86
`0xFFFF_C000_0000_0000`, arm64 TTBR0 `0x0000_C000_0000_0000` — arm64's GB0 alias of §10.13 was its
first gigabyte). `mmio_va(pa)` gives a device's gigabyte a window gigabyte the first time it meets
it (`paging::mwin_map`, the registry `mem::mwin_*`): a gigabyte with no RAM is one 1G leaf, a
gigabyte that also holds RAM gets only its RAM-free 2M pieces (a device mapping never aliases RAM),
uncached, supervisor, never executable; a leaf once written stays, so no shootdown. x86 moved its
LAPIC (`smp::LAPIC_VA`, which the EOI stub reads too), the IOAPICs, the microvm's virtio-mmio grid
and the default ECAM onto it; arm64 its console on a Graviton's GB3; both machines the PCI BARs and
ECAM windows above 4 GiB — which had bypassed `mmio_va` altogether: `pci::bar` returned the physical
address on that path, and the identity leaf `map_mmio_1g` laid made it work. With the window on,
no identity leaf is laid for a device above 4 GiB any more; ivshmem keeps one (ring 3 is handed it
by its identity). `talaria.mwin=0` is the identity again (arm64's GB0 stays in the window). Gate
`talaria.test=mwin`: every device pointer the machine keeps (x86 the LAPIC and the IOAPIC; arm64 the
GIC's two frames, the ITS, the UART and the ECAM at 256 GiB) is a window address that round-trips,
a process space shares the window's table and reads the same ID register, and no `mmio_va` fell
back to the identity (`mem::MWIN_MISS`, the meter like `DMAP_MISS`).

Under strict a process space then loses the identity of every gigabyte with no RAM in it
(`paging::strict_gb_drops`: its devices are in the window, a hole is nobody's; ivshmem's gigabyte
stays) — x86's GB3, and GB1-2 on a small machine; `dmapstrict` counts them (3 in the kernel's
space, 0 in a process's under QEMU x86). And a `MAP_FIXED` below 4 GiB is granted wherever the
space has no identity left (`paging::fixed_low_ok`: the map's RAM above the image, those
gigabytes); the image and whatever the kernel still reaches by its identity stay refused.
`sigtest` maps `0xA000_0000` privately, forks, checks the copy and unmaps. A kernel fault on
such an address names itself in the dump ("A DEVICE BY ITS IDENTITY in a process space").

Stage 3's leftover closed with it: an overflow by a push. x86's #DF runs on IST1 (a window stack
per core, given in `gdt::setup_gdt_cpu`; the boot core's TSS is loaded at boot now, not at the
first ring-3 entry), and arm64's EL1 entries ask, before the stub saves its frame, whether
sp − 1 KiB is a window stack's guard page (`TPIDRRO_EL0` is the one scratch register there) and
dump on an emergency stack. Gate `talaria.test=kstackpush` on both: a window stack driven into its
guard by pushes ends in a dump that names the guard. Before, x86 triple-faulted and arm64 faulted
on its own frame forever — both without a word, the shape ENGINEERING-NOTES 52 had to find by
elimination.

The regressions: the default build x86 526/0 + host 22/0, arm64 362/0 (UEFI with the NVMe BAR above
4 GiB included); a strict-by-default build (`TALARIA_STRICT=1`) x86 528/0 + 22/0, arm64 362/0,
x86 `validate.sh` 14/0 (PostgreSQL, MariaDB, MongoDB, the JVM, ClickHouse), arm64's databases
10/0; kernel-unit 302/0. On the machines (`variants.sh`, mdpipe, `smp=4`): a `c7g.xlarge` maps one
device gigabyte besides GB0 (`0x8000_0000`, the ENA and the NVMe) and gives p99 2.27 us (strict
2.58, Linux 4.8 in the same hour); a `c6i.xlarge` maps GB3, its ENA's LLQ is write-combining in the
window (`0xffff_c000_3e80_0000`) and gives p99 5.49 us (strict 5.55, Linux 8.3-9.3). The A/B
variant `talaria.mwin=0` taught the lesson of strict once more: from user-data it arrives after
the window is built and did nothing — such a `=0` (for `dmap`, `kwin`, `mwin`) now says it is
IGNORED instead of passing in silence. Next: strict as the default (the databases on the machines
under it), then stage 6.

**Stage 6, its first part, is done (2026-10-07): the kernel's memory from any RAM.** The low
allocator still stops at 4 GiB, and it stays what it is: the memory a frame's identity can reach —
the identity modes' dual frames (`// DMAP: dual`), x86's AP boot (a 32-bit CR3, the trampoline),
everything with `talaria.dmap=0`, all the early structures. Beside it, `mem::alloc_any` (and
`_frame`, `_aligned`): with the map on, the high areas first (their free list, then their bumps),
the low allocator after them; with the map off it IS the low allocator. Moved onto it: every page
table after the boot (`new_level*`, the splits, a process space's PML4/PDPT/PD and its PT copies,
the windows' tables), the stacks' window frames, the FS's buffers (read-ahead, write-back, the
journal's stage and copies, the cache's fallback), NVMe's queues and PRP lists, ENA's admin
queues, its host-info and PHC pages, the DMA bounce, arm64's ITS tables, the PMU walk and the
unwinder's buffer. The workload's own memory moved with it: the ELF image's chunks and pages
(ClickHouse's 525 MB came from below 4 GiB) and an anonymous `MAP_SHARED` (PostgreSQL's
`shared_buffers`) are the workload allocator's now — the high area first, the low one above its
reserve. On a `c7g.large` (850 MB below 4 GiB) that leaves the low memory to what needs it.
`mem::ANY_FROM_HIGH` counts the frames taken from above (`kstat mem`, the `cell:` status line);
gate `talaria.test=anyram` on both machines (4 GiB guests): a block above 4 GiB through the map,
back to the areas' free list, a process space whose tables lie above 4 GiB switched into and read
through, the counter moving — and with `talaria.dmap=0` all of it below 4 GiB. The three
databases boot and answer with 5 GiB (a 2 GiB high area) by default and under strict
(`MEM=` is a knob of their `run.sh` now), ClickHouse and the JVM with 6 GiB. The regressions: x86
530/0 + host 22/0, `validate.sh` 14/0; arm64 364/0, its databases 14/0 (PostgreSQL, MariaDB,
MongoDB, the JVM, ClickHouse).

Moving the loader found a stage-1 leftover the census could not see (ENGINEERING-NOTES 54): arm64's
`icache::sync` cleans by virtual address, and five callers gave it the frame's identity — the
loaders, a COW copy of an executable page, a 2 MiB `mprotect(PROT_EXEC)`. Under strict a process
space has no such address; TCG does not translate cache maintenance, so only hardware would have
faulted, and only on a code page written in a forked child. They pass `kva(frame)`.

RAM beyond the identity followed the same day. The high areas stop at `user::PROC_HEAP_BASE`
(192 GiB: the process heap's slot overlays any identity above), so a `c6i.metal` (256 GiB) never
saw its last 64. With the map on, the RAM above that ceiling and inside the map's slot (512 GiB)
becomes up to four more areas WITHOUT any identity leaf (`mem::XHIGH_*`); only `alloc_any` hands
them out — the kernel's structures, reached through `kptr` — and it takes them first, which keeps
the identity-reachable areas for the workload (whose frames the identity modes still touch by
address). `anyram` on a 200 GB guest (`memory-backend-ram,reserve=off`, so the host needs none of
it), on both machines: `0x30_0000_0000..0x32_4000_0000` (9 GB) is seen, `alloc_any` takes it
first, the workload's frames stay below; PostgreSQL boots and answers on that guest.

**Strict is the default (2026-10-07).** The last evidence it waited for came from real arm64
hardware: the KVM stand (`c7g.metal`, `CUSTOM=` with a strict-by-default build of `4801645`) ran the
gate 348/0 (its open-source and dynamic-ELF blocks have no images there) and the databases 15/0 —
PostgreSQL, MariaDB, MongoDB, the JVM, ClickHouse — with real cache maintenance, which TCG skips.
x86's evidence is local KVM on the dev box's own hardware (`validate.sh` over strict since stage
4b) and mdpipe under strict on a `c6i.xlarge`. The EC2 scoreboard A/B of the databases did not
give judgeable points this time (the stand's own seams, not the kernel: an image without its
server, two queued runs on one terraform state); it is the next scoreboard's to repeat.
`talaria.dmap=1` is the map with RAM's identity kept in process spaces (the A/B), `talaria.dmap=0`
no map at all; the `TALARIA_STRICT` build switch is gone. The regressions assert that a default boot
is strict and keep the `dmap=1` path: x86 535/0 + host 22/0, `validate.sh` 14/0; arm64 369/0, its
databases 14/0.

Left of stage 6: the low pieces other than the largest one are still unused (the low allocator
takes one region; on x86 the other one is the 640 KB below 1 MB); the workload cannot use RAM
beyond the identity yet (the identity modes would need a check of their own); and the remaining
`alloc_contig` callers that are not dual (the vDSO, `syscall.rs`'s identity-mode mmaps and shm)
keep the low.

**Stage 7, the plan (2026-10-07).** The image in the higher half: x86 at −2 GiB (PML4[511]/PDPT[510];
`code-model: kernel` is already the target's, so the Rust side links there as it is), arm64 at
`0x0000_FFFF_8000_0000` — TTBR0's L0[511]/L1[510], the SAME indices, above `USER_TOP`, so §4.3's one
table and the shared paging code stay and `copy_kernel_pml4` (slots 1..511) hands the image to every
space; TTBR1 would buy a kernel half never copied and later ASID/KPTI options, and is left for then.
What stage 1 already did: every image-address-to-physical conversion goes through `kaddr::kpa`
(37 `kpa_of`, 4 `kpa`) — the image's bounds (`kernel_lo/hi`), the boot records, kvmclock, PV-EOI,
steal time, the virtio arena and the ENA/NVMe buffers in `.bss` — so one branch there (the image's
range minus its offset) keeps them all. The work is where code runs at the physical address:

1. **7.0, the vocabulary, no behaviour change.** `AT()` in both linker scripts with an offset `KOFF`
   (0 for now) and physical entry symbols for `ENTRY`, the PVH note, the mb2 tag and `tools/mk-tboot.sh`;
   the stubs check "the entry lies in the PT_LOAD by its physical address", not `entry == paddr`;
   `kpa`'s image branch; the four Rust sites that use an image address as physical without it
   (arm64's AP stack as its MMU-off `sp`, the unwinder's probe at `kernel_lo() + 0x1000`, `early_enable`'s
   assumption). Gate `kimg`: `.text`/`.data`/`.bss` and the PV/DMA words round-trip.
2. **7.1, a high alias while linked low**: the image mapped a second time at slot 511 (x86's boot
   tables, arm64's `early_enable`, `paging::init` on both — before the root switch and before any
   space). Gate: a word read through both; a process space and a fork child have it.
3. **7.2, x86 linked at −2 GiB**: the 32-bit entry with `sym - KOFF` (its absolutes are R_X86_64_32 and
   fail to link — loud), a `movabs`+`jmp` into the high half, the AP trampoline handed `kernel_cr3()`
   explicitly (it must map 0x8000 and the image, below 4 GiB — ENGINEERING-NOTES 52's class); all four
   loaders (PVH, the UEFI stub, tboot, GRUB).
4. **7.3, arm64 linked high**: today Rust runs with the MMU off up to `early_enable` (the DTB, the
   console, `println!`) and on every secondary up to `enable_here` — an absolute pointer there (a
   format string's pieces, a vtable) is a high address with no MMU. Linux's way: an asm early map in
   `_start` (the image's identity and its alias, MAIR/TCR/SCTLR, a branch high), the DTB parsed after
   it; `ap_start` turns the MMU on in asm from the root's and the stack's physical addresses; VBAR
   set once high. The riskiest step: a fault there is silent.
5. **7.4, the low image identity retired**: `STRICT_KEEP` shrinks to leaf 0 (the trampoline, the
   BIOS area), the image's leaves leave process spaces, `fixed_low_ok` and `mprotect` stop refusing
   its range, `user_gb()` is renamed for what it is, W^X on the image (`.text` RX, `.rodata` R,
   `.data`/`.bss` RW+NX). Gate: an x86 ET_EXEC at 0x200000 in a process space.
6. **7.5, ClickHouse's high-base kernel retires** (`services/clickhouse/build-kernel.sh`, `target-hi`,
   `validate.sh` B12 on the default kernel); the identity modes keep the limit (the image is still
   there physically) and say so.

**Stage 7.0-7.3 are done (2026-10-07): the image runs in the higher half on both machines.**
7.0: both linker scripts give every section `AT(ADDR - KOFF)` and the loaders physical symbols
(`pvh_start_pa`, `mb2_start_pa`, `_head_pa` — the PVH note, the mb2 tag, `mk-tboot.sh`'s `nm`, arm64's
ELF entry). x86's ELF entry stays VIRTUAL: GRUB 2.06's multiboot2 ELF loader refuses an entry
outside the segment's `p_vaddr` range ("entry point isn't in a segment") even with the mb2 entry tag
— the full regression's BIOS block found it — so the x86 UEFI stub translates `entry - p_vaddr +
p_paddr`. And cargo did not know the linker scripts are inputs: an edit to one relinked nothing (a
build "finished in 0.44 s" with the old entry); `build.rs` names them now; `kaddr::KIMG_OFF`/`KIMG_VBASE` and `kpa`'s image branch; arm64's CPU_ON context is the stack's
`kpa`; `mem::kernel_va_lo` for the unwinder's probe. 7.1: `paging::init` maps the image's 2M leaves a
second time at slot 511 before the root switch (`paging::kimg_alias`). 7.2: x86 links at −2 GiB
(`KOFF = 0xFFFF_FFFF_8000_0000`, the image at `0xffffffff80200000`): the 32-bit entry writes every
absolute as `(sym - KOFF_A)` (a `const` operand), the boot tables carry PML4[511]/PDPT[510] → GB0's
directory, and long mode jumps high by `movabs`; `code-model: kernel` was already the target's, and no
relocation overflowed. 7.3: arm64 links at `0xffff_8008_0000` (`KOFF = 0xFFFF_4000_0000`), and Rust
no longer runs with the MMU off: `_start` builds a boot map in asm (every gigabyte of the first
512 GiB by its identity as Device — what the MMU-off code saw — except the image's, Normal, plus the
same block at L0[511]/L1[510]), invalidates its lines, turns the MMU on and branches high;
`early_enable` became a root switch with the MMU on (`dsb ishst` before, `isb` before the TLB
invalidate) and keeps L0[511]; a secondary turns its MMU on in `ap_start` from `AP_ROOT`/`AP_TCR`,
which `start_secondary` publishes and cleans to the point of coherency. The linker scripts ASSERT
that `KOFF` puts the image's gigabyte at `KIMG_VBASE` (a `KBASE` elsewhere fails at link time).
Gate `talaria.test=kimg` on both: `KIMG_OFF` against the running map, `kpa` of a `.text`/`.rodata`/
`.data`/`.bss` word against a table walk, the alias reads, writes, executes and is in a process space.

On the machines (`variants.sh`, mdpipe, `smp=4`, the image's alias in the console at the
stage-7 addresses): a `c7g.xlarge` booted through the UEFI stub gives p99 2.24 us strict / 2.49 with
`talaria.dmap=1` / Linux 4.77 in the same hour; a `c6i.xlarge` 6.25 / 5.39 / 10.0 — stage 5's
numbers.

Two lessons on the way. The gate itself took `addr_of!(__kernel_pstart)` — an absolute low symbol
from high code is a PC-relative `adrp` out of its ±4 GiB, and the link failed: exactly the class the
stage removes, so the gate reads the load address from the tables instead. And the secondaries
came up silent with the boot map's TCR: the kernel's leaves leave the access flag to the hardware
(TCR.HA), a core without HA faults on its first access — before it has vectors. They take the boot
core's own TCR now (ENGINEERING-NOTES 55).

**Stage 7.4 and 7.5 are done (2026-10-07).** With the image at its high alias, strict keeps only
leaf 0 of RAM's identity in a process space (`STRICT_KEEP` = 2 MiB: x86's AP trampoline at 0x8000,
the BIOS area) instead of everything below the image's end; `mprotect` may change the image's former
range in a strict process space (`paging::proc_low_ok`, never in the kernel's own); x86's trampoline is
handed `kernel_cr3()` explicitly. Gate `talaria.test=sigtestlo`: `sigtest` linked at 0x200000 — the
kernel image's own load address — passes (its image, forks, shared memory); the pre-stage-7 kernel
(9220575) hangs silently on the same binary, its process overlaying the kernel's code. 7.5: the official
ClickHouse ET_EXEC (2 MB..528 MB) runs on the normal kernel — `validate.sh` B12 passed on it — and
`services/clickhouse/build-kernel.sh` (KBASE 528 MB, `target-hi`) is gone. x86 538/0 + 22/0,
`validate.sh` 14/0, arm64 370/0, arm64 databases 14/0. Then W^X on the image: the linker scripts start `.rodata` and `.data` on
2 MiB (`__text_end`, `__rodata_end`), so the alias's 2M leaves carry one section's rights each — the
code read-only and executable, the constants read-only, the data and `.bss` writable and never
executable; `kimg` checks all four. The one writer of the kernel's code, a gdb breakpoint (x86's
`0xCC`, arm64's `brk`), writes through the direct map (`kaddr::kimg_write_va`: the same RAM, writable,
not executable) and arm64 syncs both addresses. The ELF loader no longer reserves an image's identity
range from the frame allocator under strict (nothing can collide there; ClickHouse's would have
fenced off half a gigabyte of the low memory). `user_gb()` keeps its name; the image's LOW identity
in the kernel's own space stays RWX (the identity modes run there). On the machines (`variants.sh`, mdpipe, `smp=4`): the W^X
kernel with strict keeping only leaf 0 gives p99 2.59 us on a `c7g.xlarge` (Linux 4.81 in the same
hour) and 6.00 on a `c6i.xlarge` (Linux 8.67). The identity modes (`talaria.cell=identity`, `run_disk`) still share the kernel's space and
cannot take an ET_EXEC at 2 MB.

**The risks, ranked:** a missed conversion stays silent until 4b (the meter, the strict knob,
a fault message that names the map); a device given a kernel address (the `Pa` type, a bound
check on every queued address); arm64 attribute aliasing (map RAM only, never split a leaf);
the semantics of shared and zero-syscall regions; every top slot existing before the first
space (keep the `AS_LIVE` alarm). Open: whether structures should store kernel addresses
(the cache, the rings) or translate on use — translate first; whether the reference space
ever drops the identity (the identity modes need it).

### 10.17. The first arm64 scoreboard (2026-10-05/06)

Phase 9's exit gate — "a first arm64 scoreboard in `docs/MEASUREMENTS.md` with the same judging
rules" — is met: `docs/MEASUREMENTS.md` §10.13, six workloads on four `c7g.xlarge` per bring-up,
Talaria against AL2023 arm64 with the same trees and the same `tlgen`. In one line: **the tail
is Talaria's** (p99 won on redis 19/1/0, mongoose 10/0/0, nginx 8/1/0, mysql 8/2/0; MariaDB's
ceiling +41-44 %), **the median and the closed-loop ceiling of the network-only servers are
Linux's** (redis p50 0/4/16, ceilings -30..-38 % on redis, nginx /nop, PostgreSQL pk), and
PostgreSQL is mixed; mongodb is Talaria's on 9 of 15 medians and 13 of 15 p99.99, Linux's on
`insert`'s p99 at the middle and top rates. 291 points in all.

The scoreboard was also the port's largest test so far: three defects only a Graviton's own
memory map and a root-run Linux side could show (§10.15; ENGINEERING-NOTES 48-49), each now
reproduced under TCG in that shape and gated.

What phase 9 leaves open, in the order the numbers rank it: the median and the ceilings on the
network path — split on 2026-10-06 (`docs/MEASUREMENTS.md` §10.14): under the Linux generator the
Talaria redis server has a flat 38-40 us median and a ceiling 14-17 % above Linux's, so the gap is
the Talaria GENERATOR's client path, the next thing to measure; redis's rare 0.3-0.9 ms outliers at low rates;
PostgreSQL `pk`'s ceiling; the wfe tier's mdpipe tail (§10.15); the lazy FP save (the svc round
trip is 130 ns with the save, 113 without — 17 ns, under the noise of any row above).

## 11. After the migration: what is left, in order (2026-10-07)

Phases 0-9 and the direct map's stages 1-7 are closed: both machines boot, run ring 3, the
devices, the databases, the JVM and ClickHouse, with the kernel image in the higher half, W^X on
it, no RAM identity in a process space by default, and the kernel's memory from any RAM; each
step was gated on both regressions and booted on a `c7g.xlarge` and a `c6i.xlarge`. The branch
`arm64-port` stays a branch until the owner says otherwise. What is left, the order being the
order of work:

0. **PostgreSQL's processes are corrupted on a `c6i.xlarge` (found 2026-10-08, CLOSED 2026-10-09:
   a stale root reloaded by the switch stub — see the end of this item).** The x86 board's PostgreSQL stand never got its role; a probe (`talaria-deploy.sh --data`
   the board's image, `--keep-disk`, the kernel's log read off the volume with `read-disk-trace.sh`,
   because EC2's 64 KB console window is overwritten by the end-of-run dump loop) shows the shape every
   time: the startup process exits on core 1, core 1 idles and "has left the tables of the finished
   process onto the kernel's CR3", the space's freeing "is deferred — its tables are held by an AP",
   and at once the postmaster faults ON CORE 1 in ring 3 at `rip=0x0`/`0x1` (an instruction fetch; a
   plausible `rsp`, the right `fs`) — its saved user context or its stack returned zero. Every probe
   whose log has that "left the tables" right after the startup's exit failed; the ones where it
   came later did not. NOT this migration's: the kernel before stage 6 (`9220575`) fails the same way
   (1 of 3 strict, 3 of 3 in its non-strict default); today's kernel 5 of 8 strict, 3 of 3 with
   `dmap=1`. Not reproduced locally in 11 runs (q35, 8 GiB, 2 and 4 cores, NVMe, virtio-net): the
   deferred path is never taken there. Then: a `kernel stack overflow` in `metrics::dump` on window
   slot 0 when all the tasks are gone (the dump's 2 KB frame on a stack ~59 KB deep — the caller's
   path), repeating every ~6 s. Next: make the local run take the deferred path (an AP left idle on a
   dead space while the parent migrates onto it), then the frame that goes to zero. Leads read so far:
   `rip` 0 and 1 are SIG_DFL and SIG_IGN, and the startup child resets its SIGCHLD disposition — but
   both delivery paths (`deliver_signals`, `deliver_exception_signal`) skip a handler of 0/1, so the
   zero is in the postmaster's saved context or stack, not a disposition; the per-core deferred rsp
   substitution (`percpu::sig_newrsp`, `SIGRSP[t]`, carried by `ap_loop` and the switch) is the
   next to read; the pickup loop's own CR3 handling compares against the loaded CR3 and is right.

   **Found since (2026-10-08).** Reproduced LOCALLY: PostgreSQL under TCG (`-accel tcg`, two cores,
   q35 or microvm with virtio-mmio), about 1 boot in 5-6 — the same sequence (the startup child exits,
   the deferred free, the postmaster on core 1 at `rip=0`), or the machine dying silently there (a
   triple fault: QEMU exits under `-no-reboot`). **It is older than the migration and than the
   arm64 port**: `main` (`3a29de1`, before the port) fails the same way, 2 of 10 in the microvm
   shape. Instruments built for it (opt-in): the ring-3 fault report prints the faulting stack page's
   frame, whether the ledger counts it handed out, its refcount; `talaria.freetrace=1` keeps the last
   8192 frees with caller and task and the report lists every free of that frame (it also puts
   dropped DMA bounces in a canary quarantine); `talaria.watchframe=<phys>` prints every hand-out,
   free and refcount change of one frame. What they showed: the postmaster's stack frame is
   legitimately its own — its history is "a private page of another backend, munmapped
   (`unmap_proc`, refcount released to 0), twice a disk-read bounce (`fs::Bounce`), then the
   postmaster's COW copy of its stack page" — the same in a clean run and in a crashing one; no
   double hand-out, no double free; the fork's `PROT_NONE` 2 MB branch is not hit; no device writes
   into a bounce after its I/O completes (the canary). Yet the page's contents read zero where a
   return address was. Every instrument that slows the frames' reuse (the watch's prints, the
   quarantine) makes the crash disappear (0 of 25 each; at the base rate that is a ~1 % chance).
   Read and found right on the way: the COW fault (an atomic choice of copy or in place), the
   frees in `relazy_pages`/`unmap_proc`/`free_pt`/`free_pd` (all through `rc_release`), the deferred
   space free's guard, the HAL (`share` is a table walk, `unshare` copies nothing), the CR3 switch
   (a real `mov cr3`, no PCID, no global user leaves). A real bug found on the way: `rt_sigsuspend`
   ignored its mask (75bd3be, gate `chldrace`). Next: the COW copy's source and destination around a
   just-freed bounce frame (a stale translation of the destination through the map? a copy from
   the wrong source?), and the triple-fault variant (a QEMU `-d int` trace of the last exceptions).

   **Closed (2026-10-08 locally, 2026-10-09 on a `c6i.xlarge`).** The `-d int` trace caught
   the triple fault: a kernel `#PF` in `netsys::yield_window` on a root whose kernel half was gone —
   a freed, reused top-level table still loaded. A ring of the last 64 `unmap_proc` calls (with the
   task, the core, the loaded root and a fork marker) then named the whole thing on a staged
   reproducer (`kernel/user/cowstale.c`: the parent pinned to core 1, then 0, a child exiting
   between): **the parent t0, on core 1, ran its `mmap`s with its dead child's root loaded.** The
   switch stub takes the root to load from a per-core word (`gs:40`; arm64's `sched_root`) and never
   cleared it; `ap_loop` enters a task with a direct root switch that leaves the word alone; the next
   tick on which the scheduler STAYED (it writes nothing) reloaded the previous task's root. The
   postmaster after its startup child's exit is exactly that: it lands on the core the child left,
   then lives in the child's space until the child's space is freed under it — `rip=0`, or a triple
   fault. Fix: the stub spends the decision (`movq $0, %gs:40`). arm64's tail loads `sched_root` only
   when it enters ANOTHER frame — a scheduler that stays returns the same frame and skips the load —
   so arm64 was not exposed (its negative control passes, 3 of 3); it clears the word too
   (`str xzr, [x9, #40]`), a decision being spent where it is carried out. The same chase
   opened two holes real on their own: the COW copy freed the original before any other core
   forgot it (a sibling thread kept reading the original — now the page is shot down first, as
   `munmap` does), and a write fault from a stale read-only translation of a page another core had
   already made writable was a SIGSEGV (now a spurious fault: flush and retry, a run of eight at one
   address within a millisecond still faults). A belt over the brace: the dead-space reaper also
   checks the root each core really holds (`REAL_CR3`, read by the core at its scheduler entry and
   its every `paging::switch`); since the stub fix it never fires. Before → after under TCG:
   `cowstale` failed in the first run of every series → 12 of 12; PostgreSQL ~1 boot in 5 → 30 of
   30 ready with the deferred path taken 7 times. Gates `cowstale`/`cowstalet` on both machines.
   The instruments stay, opt-in or one entry per call: the `unmap_proc` ring and the walk of the
   faulting stack's tables in the ring-3 fault report, the roots `free_address_space` freed. On EC2
   (`ec2ab-postgresql-20261008-223129`, `6962a3b`): the server's console shows THE sequence that
   failed every probe before — "core 1 ... has left the tables of the finished process", "the
   freeing of the AS ... is deferred" — followed by "ready to accept connections", no fault, and
   phase A ran the whole ladder (item 1 has the numbers). Phase B was cut by the session's
   two-hour limit for a background job, after its server had booted with `dmap=1` and served `pk`;
   the stand was torn down by hand.
1. **The databases under strict on the EC2 scoreboard — done 2026-10-08, x86 PostgreSQL
   2026-10-09 (after item 0).** x86 PostgreSQL under strict against AL2023 in the same hour, every
   point Talaria's (p50/p99/p99.9 us): `pk` at 2309 req/s 78/92/98 vs 96/119/154, at 20781 75/96/163
   vs 96/939/2875; `select1` at 3779 53/68/73 vs 58/76/88, at 34011 52/72/77 vs 52/1527/5788.
   The earlier runs: one run per stand (`stand_lock`), B = A plus `talaria.dmap=1` (`APPEND=1`), the
   consoles kept, mk-ami's silent x86 death fixed. Under strict, against AL2023 in the same hour:
   arm64 PostgreSQL `select1` p99 75-100 us vs 78-9249, `pk` p50 96-101 vs 114-133; arm64 MariaDB
   p99.9 70-93 vs 131-10068; x86 MariaDB `pk` p50 55-60 vs 69-114, p99.9 76-88 vs 150-10469;
   mongodb on both mostly Talaria's below the ceiling. Strict against `dmap=1` (B after a stop/start,
   which moves the instance to another host): arm64 PostgreSQL and MongoDB p50/p99 within ±11 %
   (one ceiling point apart, with B's generator then still missing `netidle=1`), x86 MariaDB B 4-25 %
   SLOWER on every point (the host move's size, not the tables'), x86 MongoDB median B/A 0.96. Strict
   costs nothing measurable. arm64 MariaDB's B gave no point — repeated 2026-10-09
   (`ec2ab-mysql-20261009-014542`) with the consoles kept: not `dmap=1` at all. B begins with a
   stop/start, and after it InnoDB found no checkpoint in `ib_logfile0` ("created before MariaDB
   10.2.2", the server exits) — the symptom x86 had before its power button: the arm64 kernel
   did not listen to the stop (a hardware-reduced FADT has no PM1 block), so every Graviton stop
   was a power cut a few minutes later, and A's first start had just rebuilt the redo log
   (`Resizing redo log from 96 MiB to 32 MiB`). Fixed: the button on arm64 is the ACPI Generic
   Event Device (`ACPI0013` in the DSDT/an SSDT, its `_CRS` interrupt found without an AML
   interpreter), its SPI made edge-latched and its pending bit polled from the tick as x86 polls
   PM1a; gate `uefi-pwrbtn` under edk2 (`kernel/test-aarch64-pwr.sh`: both paths, the unsynced
   file read back). ON A GRAVITON THE GED IS NOT THE BUTTON: the first EC2 B phase with it failed
   the same way, and a one-instance probe (ready -> a regular stop -> start) showed the stop
   taking 254 s — EC2's timeout, then the power cut. `talaria.acpidump=1` (the tables' button
   devices as hex) read a `c7g.xlarge`'s DSDT: its `GED1` carries 32 interrupts (INTID 48-79)
   whose `_EVT` notifies the PCI slots (hot-plug), and the button is `GPO0`, a PL061 (`ARMH0061`
   at 0x0903_0000) whose `_AEI` has `_E03` -> `Notify(PWRB)` and `_E04` -> `Notify(SLPB)`. The
   choice is now by the AML's meaning, not the first match: a GED counts only if its body
   notifies PWRB (QEMU's does), a PL061's pin is the hex of the `_Exx`/`_Lxx` method that notifies
   PWRB, its base the Memory32Fixed inside the same Device (the scopes from the PkgLengths);
   checked in Python on the probe's bytes, then on EC2: "a PL061 GPIO at 0x9030000, pin 3", the
   stop in 19 s, MariaDB ready on the next boot; the whole A/B then ran its B phase
   (`ec2ab-mysql-20261009-083319`). THE HARD CUT ITSELF was a real data-loss bug, on both
   machines: a probe (`--keep-disk`, `talaria.pwrbtn=0`, `stop --force`, the volume read on a helper
   — raw by `debugfs`, then replayed by `e2fsck`) showed the journal had committed the new
   `ib_logfile0`'s inode and block map while its 32 MiB were zeros on the disk — the same on a
   `c6i.xlarge`. `jrn_commit(ino)` wrote only the fsync'd file's data before journaling ALL the
   pending metadata, which held other files' allocations, and the kernel has no background flush.
   Fixed as ext4's `ordered`: a cache slot holding a freshly allocated data block is marked
   (`CNEW`), and before any metadata transaction those blocks are written, whoever's file (an
   overwrite of an existing block is not marked — its old content is a consistent file). Gate
   `orderedcut`/`orderedv` on both machines (a new file without an fsync, another file's fsync,
   `kill -9`, the second boot reads it back): without the fix 16 of 16 pages lost, with it intact.
   The arm64 MariaDB A/B on both fixes (`ec2ab-mysql-20261009-083319`, `c7g.xlarge`): A against
   AL2023 (p50/p99/p99.9 us) `pk` at 3343 req/s 62/75/87 vs 66/87/112, at 30087 65/84/95 vs
   58/3060/9962; `select1` at 4481 50/62/73 vs 48/63/74, at 40329 54/71/83 vs 43/1737/6804 —
   Linux's median lower at the top rates, every tail Talaria's. B (`dmap=1`, after the stop/start
   that moves the instance) ran every point: `pk` p50 66-67 against A's 62-65, p99 within ±2 % —
   the host move's size again (x86 MariaDB's B was 4-25 % slower the same way). Item 1 is closed. What the item said before:
   **The databases under strict on the EC2 scoreboard.** The A/B of 2026-10-07 (`run-remote-ab.sh
   <wl> "talaria.dmap=strict"`) gave no judgeable point, and the stand, not the kernel, was why:
   an AMI built before the worktree had `services/*/build` (fixed: `mk-ami.sh` refuses a tree that
   did not build); a `mongodb` stand whose Talaria server ran MariaDB and whose generator refused
   connections in phase A (not explained — the stand's state crossed between consecutive runs in
   the worktree `my_os-s64`); two queued chains on one terraform state (the lock). First the
   harness: one stand per state directory, a guard that refuses a second run on the same state, the
   server's console checked for the workload's own banner before phase A. Then PostgreSQL, MariaDB
   and MongoDB, A (`talaria.dmap=1`) against B (the default, strict), on `c7g.xlarge` and
   `c6i.xlarge`. Exit: judgeable points on both sides, B within A's spread or explained.
2. **Phase 9's performance, as the numbers rank it** (§10.17, `docs/MEASUREMENTS.md` §10.13-10.14):
   (a) the Talaria generator's client path — the whole redis median/ceiling gap on the board is the
   generator's (under the Linux generator the Talaria server's ceiling is 14-17 % above Linux's); a
   LOCAL try (2026-10-09, one Talaria redis server under KVM, the closed-loop ping ceiling at 16
   connections; the script is kept as `bench/suite/genpath.sh`): tlgen in Talaria spends 95 % of its
   time inside `recvfrom` (64 %) and `sendto` (32 %), 3.6 and 4.0 us per call (`sysprof`), the stack's
   lock held ~1.4 us per take, 3.3 takes per request, ~7.5 us of the 12.8 us request outside the lock
   — the next instrument is a per-stage split of that outside part. But the local ORDER is not EC2's:
   tlgen in the container (no VM) 123 k/s, in Talaria 78-85 k/s with `vhost=off` and 53 k/s with
   `vhost=on`, in a Linux VM 21 k/s (p50 425 us) — the local virtual NIC (virtio kicks, vhost,
   interrupts) reorders the contestants, where on EC2 the Linux generator was the faster. The client
   path is measured on EC2 only: phase B with `AB_ROLES=loadgen` and `talaria.sysprof=1
   talaria.stat=<ms>` on the generator (the periodic counter block needs no console input).
   Done 2026-10-09 (`ec2ab-redis-20261009-202439`, `c7g.xlarge`, the generator's console kept):
   `recvfrom` 704.5 M calls at 4275 ns each (3012 s of kernel time over the run), `sendto`
   188.4 M at 1997 ns — 3.7 `recvfrom` per `sendto`, so most receives are polls of an empty
   socket (EAGAIN), each paying the full path; the next step is that ratio (where tlgen polls,
   what an empty receive costs, whether a readiness wait replaces it), not the per-call price.
   B itself is not a performance point: the profile doubled the Talaria p99 (55.8 -> 127 us at
   21 k/s). **Read again 2026-10-21: the per-call price IS the ceiling, and it was the pointer
   check.** The reports' `syscalls` block (the same tlgen, both generators) says the ratio is
   the rate's: at 21 k/s the Talaria generator polled 8.4 times per request and the Linux one
   72 — Linux's empty receive is cheap, so it spins faster; at the closed-loop ceiling the
   Talaria generator was down to ONE empty receive per request (3.0 syscalls, 85-88 k/s) while
   the Linux one made 8 (9.8 syscalls, 124-139 k/s). Three syscalls at 4.3 + 2.0 + 4.3 us are
   the 11.7 us per request of the Talaria ceiling. And the generator's console says where the
   4.3 us went: the stack's lock was held 671 s over the run against 3012 s of `recvfrom`
   kernel time (0.63 us per take), and 638 M idle passes were cancelled by `pollgap` — the
   empty receive barely touched the stack. The rest was `uaccess::gate`: tlgen's `recv` asks
   for `TL_RBUF` = 1 MiB on every poll, and `user_range_ok` walked the four levels for every
   4K page of it — 256 walks per call. A new gate, `kernel/user/gatecost` (an empty
   non-blocking pipe read 200 000 times with 4 KiB and 1 MiB buffers, the kernel writing
   nothing; then the same at a file's end), under KVM on the dev box: 4 KiB 153 ns, 1 MiB
   2997 ns — the 3 us the Graviton was missing; Linux with the same binary 516 and 530
   (it checks nothing up front and faults on the bytes it writes). Two fixes: (1) the check
   descends once per 4K TABLE and reads its consecutive entries in place up to the 2 MiB
   boundary (a 2M/1G leaf or a missing table still answers for its whole span at once) —
   `walk_leaf_tbl`, `leaf_ok`; (2) `read`/`recvfrom` from a pipe or a socket are checked only
   as far as the descriptor can deliver (`Len::ArgCap`, `Mem::read_cap`: the pipe's ring,
   the TCP receive buffer, one UDP datagram — the handlers copy through those same bounded
   rings and never past them; a file keeps the whole length). After: pipe 4 KiB 158 ns,
   1 MiB 160 (1.0x); a file's end 4 KiB 277, 1 MiB 571 (2.1x — the walk's own price, 256
   entries read in place). Found on the way and fixed: `read(fd, buf, 0)` on an empty
   non-blocking pipe answered EAGAIN where Linux answers 0. Gate `gatecost` on both machines
   (the ratios: under 3x on the pipe, under 8x at a file's end; every answer EAGAIN, the 0-byte
   one 0), a host unit test of the cap (`uaccess::tests`). On a `c7g.large`
   (`variants-gatecost-c7g`, the same binary on AL2023 in the same hour; ns per call): the pipe
   4 KiB / 1 MiB Talaria 207 / 207, Linux 217 / 216 — the empty receive of a Graviton is now a
   200-ns syscall; a file's end Talaria 327 / 870 (the 256 entries read and decoded in place),
   Linux 206 / 205. The same redis A/B on a `c7g.xlarge` with this kernel
   (`results/ec2ab-redis-20261021-143739`, 2026-10-21; A = the scoreboard's phases on both sides,
   B = `sysprof=1 stat=5000` on the Talaria generator only): the generator's `recvfrom` fell from
   4274 ns per call to **455** (5.02 G calls in the hour, 2287 s of the kernel; `sendto` 2280 ns,
   242 M calls — the sends did not change, the empty polls got cheap and so got more frequent:
   20.7 receives per send against 3.7, 4.85 G idle passes of the stack cancelled). The
   closed-loop ceiling of the Talaria generator (three probes, median, req/s): ping 85.7-88.8 k
   at a 0 % spread (the hard per-call wall) → 102.9 k at a 35 % spread (89.7 / 102.9 / 138.9);
   get 85.6 → 114.4, set 85.6 → 105.3, mixed 85.4 → 115.7; under the profile (B) 124.4 / 108.7 /
   113.9 / 103.3. Linux's generator in the same hour 141.1 / 128.2 / 124.6 / 126.6 — the gap
   from 0.62-0.65x to 0.73-0.91x, and a scattered ceiling now, not a wall: what is left is no
   longer the price of a call. The ladder itself (A, service latency vs Linux at the same rate):
   every Talaria median and p99 under Linux's up to the fourth rung (ping 77 k: p50 42.5 vs 53.8,
   p99 56.0 vs 96.2; get 86 k: 41.9 vs 45.0, 56.0 vs 96.9), the top rung 92.5-104 k req/s against
   77-80 k before. Left on the generator, pre-existing (the 2026-10-09 run showed it at 66.6 k):
   from the fourth rung up the Talaria generator stalls once per run for a whole second (max
   1.000 s, 100-350 k requests behind it at a debt p99 of 1.6 us; `the ring lied 1` in its
   stat) — the open loop's debt, not the service latency, and the next thing to name on the
   client path (`talaria.sysprof` is not the instrument: B doubles the p99, 56 → 115 us).
   **Named and fixed 2026-10-22: the stall was the TCP retransmission floor.** The max column
   said it: 1 000 067..1 000 124 us at every rung from 77 k up — one second plus one RTT, the
   same at every rate, while the Linux generator's max stayed at 280-380 us. A number that exact
   is a protocol timer, not a scheduler: smoltcp 0.14 keeps RFC 6298's floor, `RTTE_MIN_RTO =
   1000` ms (and `RTTE_INITIAL_RTO` 1000), and a closed-loop request/response connection has no
   second segment in flight to earn a duplicate ACK, so every lost segment is recovered by that
   timer alone — one drop at the ceiling cost the generator 1.000 s and the 100-350 k requests of
   debt behind it. The stat block's `the ring lied 1` was a red herring (one event in the hour
   against four stalls per run, and a lie costs the 100 us of `pollhard`, not a second). Fix:
   smoltcp is vendored (`kernel/vendor/smoltcp`, its `TALARIA.md`) with the floor at Linux's
   `TCP_RTO_MIN`, 200 ms. Gate `txloss` on both machines: `talaria.txdrop=N` drops every N-th
   TRANSMITTED frame on the floor (a loss gate, not a feature — the only way to see what a loss
   costs on a stand that never loses one; a receive-side drop would measure the PEER's timer,
   since the peer's reply carries the ACK), 60 echo rounds against the harness's echo server
   with every 7th frame lost: under KVM every slow round is 200-201 ms (10 of 60), none over
   600 ms; before, 1 000 ms each. What is left is the DROP itself — why the generator (or the
   server) loses a frame once per rung at 77 k+ req/s where Linux loses none; the ENA receive
   ring under a burst is the first suspect, to be measured on EC2 with the drop counters. A drop
   costs 200 ms meanwhile, as it does Linux;
   (b) the wfe tier's mdpipe tail at `smp=2` — **closed 2026-10-09: it was `nanosleep`.** The
   per-tick instrument was built (`kernel/src/umark.rs`): the workload names a slow interval itself,
   `prctl(PR_TALARIA_SLOW, t0_ns, t1_ns)` (mdpipe's `--mark-us N`; Linux answers EINVAL), and the
   `stat:` block folds every core's scheduler decisions inside the windows into "who held the cores",
   with the part each holder spent ASLEEP as the core's current task (the cores' sleep intervals
   kept from the first mark on); gate `umarkt` on both machines (a hog spinning on core 1 holds it
   awake, the marking thread its own core asleep). Its first run on a `c7g.large`
   (`variants-umark-c7g`, the default `wfe(monitor)`, p99 36.0 us against Linux's 5.7): during the
   20 283 slow ticks the cores were held by `md_gen` 36 % and `md_main` 13 % AWAKE — and `md_main`,
   which sleeps a second at a time, had run 85 339 times in 30 s (113 452 and 6 s of a core with
   `wakepreempt=0`). `nanosleep` was a loop of 200-us coarse `wait_window`s: a long sleep woke 5000
   times a second to read the clock, and a short one overslept to the next tick
   (`kernel/user/sleeplat`: `nanosleep(50 us)` overshoot p50 1947 us under KVM), so mdpipe's
   generator, paced by such sleeps, pushed its ticks in bursts. Fix: the whole term is one EXACT
   deadline of the wait tier (the tick serves a far one, the core's timer a near one, a core with
   other work parks the sleeper; overshoot p50 25 us, p99 32, with or without a neighbour); a
   coarse whole term was tried and kept the 2-ms oversleep (its `sync=hlt` p99 6.7 -> 20.5, the
   generator's shape changed). Gates `sleeplat`/`sleeplath` on both machines. On the `c7g.large`
   (`variants-exactsleep-c7g`, p50/p99/p99.9 us, max): default 4.4/5.1/7.9, 7.1 ms (before
   1.7/36.0/41.8, 1.8 ms); `sync=hlt` 5.2/6.2/8.9, 0.26 ms; `wakepreempt=0` 4.3/5.1/7.4, 44 us;
   Linux in the same hour 3.7/4.9/5.4, 0.40 ms. The median rose because the generator now paces
   like Linux's (no bursts). What is left of it: the default's maximum — `ready->cpu` has ~150 waits
   of 0.5-1 ms and a few over 1 ms (the worst 7.07 ms: a ready worker on core 1 behind its sibling,
   the core's current task), gone with `wakepreempt=0` (max 44 us): under the 150-us threshold of
   preempting on a wakeup a busy core is only kicked, and the woken one waits for that core's next
   tick — the next instrument is that wait, not the threshold's value. **Named and fixed
   2026-10-21.** The instrument first: `umark` kept the FIRST six marks, so the 7-ms stage was
   among 295 marks and the six printed were 16-60 us; it now keeps the six LONGEST since the
   previous block, each core's decisions inside the window (the first and the last when more than
   fit) and each core's SLEEPS overlapping it (`slept [+31..+93]`), and the worst `ready->cpu`'s
   decision tail prints from 0.5 ms (was 10). Its first run (`variants-rqwait-c7g`, the default
   kernel, max 492 us): every one of the six longest stages (425-491 us) had the same shape on core
   1 — `[-4 yield t3->t1 rotation
   from:futex] [+408 yield t1->t2 rotation from:the fs]` or `[+490 tick t1->t2 rotation]`: a worker
   gave up the core, the rotation gave it to md_sink, and the other worker, woken 4 us later, stood
   ready until md_sink's slice ENDED (a tick or the FS), 400-490 us; md_sink had run under the
   150-us threshold at the wake, so the waker only kicked, and nobody looked again when it passed
   the threshold. The knob's promise ("a woken one waits at most the threshold") did not hold: it
   waited up to the slice. Fix: a wakeup against a RUNNING occupant younger than the threshold sends
   the hint with a resched IPI (as the over-threshold case does) and the core's scheduler, seeing a
   young occupant, STAYS but pulls its own tick to the moment the occupant reaches the threshold
   (`smp::tick_sooner`: a one-shot, the period returns with that tick; `stay:young` in the
   decisions, `deferred preemptions:` in `stat:`); a core without a tick (dedicated, the timer lent
   out) switches at once. Gate `wakedef` on both machines (a cell on two cores: a spinning hog and a
   futex waiter on core 1, the main thread wakes it 1000 times at a random phase of the slice;
   under KVM p99 414 us, max 499 before → p99 87-99, max 201-472; under TCG the mechanism's counters
   and no lost wakeup — the emulation's latencies are its own). On the `c7g.large`
   (`variants-wakedef-c7g`, three instances of the default kernel, mdpipe 30 s): max 297 / 155 /
   121 us against 492 and 7070 before, p99 4.84-5.00, p99.9 7.04-7.24 (Linux in the earlier hour
   4.9 / 5.4, max 400); the longest stages are now the threshold itself — `[+18 tick t2->t2
   stay:young h:t3] [+133 tick t2->t3 hint h:t3]`, 112-134 us. Two things the instrument named and
   this item leaves: (1) the one 296-us stage of the three runs is the OTHER class — core 1's
   occupant was a worker ASLEEP in its futex wait and the core slept in four 62-us pieces
   (`slept [+31..+93] [+94..+156] [+156..+218] [+218..+281]`) while the woken sibling stood ready,
   until the sleeper's own wake rotated (`[+295 yield t2->t3 rotation from:futex]`): the
   doorbell/word sleeper woke four times and went back to sleep over a ready neighbour; the 7-ms
   maximum of 2026-10-09 (the occupant t3 asleep as the core's task, one switch in 7 ms) is the same
   shape with a longer period, rare (once in 90 s of runs here) and now caught whole by the kept
   intervals when it happens — the next step on the mdpipe tail; (2) the ~150 `ready->cpu` waits
   of 0.5-1 ms per run are md_main's: a parked sleeper with an exact deadline on a core whose tick
   went LONG (2 ms, `idleslice`) wakes at the long tick (`[-14005 tick t4->t4 stay:nobody] …
   [-1999 …] [-0 tick t4->t0 rotation]`, every 2 ms) — a timed sleep behind a running neighbour is
   late by up to the long tick; harmless for mdpipe (a 1-s sleep), to be weighed for `nanosleep`
   under load. Also met, not chased: in `talaria.test=` mode the APs are the legacy dedicated ones
   (no tick), and a thread that moves itself there with `sched_setaffinity` from the BSP starves a
   second thread placed on that core; the gates boot such tests as a cell. **2026-10-22, (2)
   closed:** `sync::pull_tick_to_sleeper` — at every hardware tick, after the period is retuned,
   the core looks for the nearest deadline among its own sleepers (a parked one or one asleep
   under the tick; `TIMED_WAITS` keeps the scan off for a workload without timed waits) and, when
   it lies inside the current period, pulls the next tick to it (`smp::tick_sooner`, a one-shot
   — the long tick stays long for the runner). Gate `sleeplong` on both machines (300 sleeps of
   5 ms behind a spinning sibling): the overshoot's median under KVM 36-52 us against ~1 ms
   before (the 2-ms long tick served the deadline), 300 pulls in `stat:`; TCG 122 us. **(1), the
   "occupant asleep" class, measured in isolation:** gate `wakesleep` on both machines (the
   core's current task asleep in its own futex wait, a parked sibling woken 1000 times at a
   random phase of the slice): under KVM x86 p99 61-72 us, max 92-111, lost 0 — the path there
   is the resched IPI to a core busy with a sleeper (`WK_RESCHED_IDLE`); on a `c7g.large`
   (`variants-wakesleep-c7g`, two instances of this kernel, 20 000 wakes each, the path there the
   doorbell write into the occupant's `wfe` — `by a call 19 873 / 19 352` in the stat block) p50
   0 us, p99 1, max 5-6, lost 0; Linux on the same type in the same hour p50 3, p99 4, max 199.
   So the class AS MODELLED — one sleeper as the core's current task, one parked sibling woken
   from another core — has no tail on a Graviton: the 296-us stage of `variants-wakedef-c7g`
   (once in 90 s of mdpipe) needs something the model lacks (a third party on the core, a
   deadline, the pickup loop) and stays open with its instrument, the kept intervals of `umark`,
   to be read off the next mdpipe run that shows it. Gate `wakesleep` stays as the class's
   regression on both machines (KVM p99 under 250 us and lost 0; TCG lost 0).
   (c) redis's rare
   0.3-0.9 ms outliers at low rates; (d) PostgreSQL `pk`'s ceiling, measured under the Linux
   generator too; (e) the lazy FP save (17 ns of a 130 ns svc — last).
3. **Stage 6's tails.** The low allocator takes one RAM region (the others below 4 GiB stay
   unused — on x86 the 640 KB under 1 MB); the workload cannot use RAM beyond the identity ceiling
   yet (the identity modes need a check of their own first); the `alloc_contig` callers that are not
   dual (the vDSO, `syscall.rs`'s identity-mode mmaps, shm's identity fallback) keep the low memory.
   **Weighed 2026-10-22, left as written on purpose:** the first tail costs nothing on either
   target — a `c7g.large` has ONE RAM region below 4 GiB (`0x4000_0000..0x7863_0000`, the
   allocator's 844 MB after the image; the rest is the high area above 4 GiB) and on x86 the
   unused region is the 640 KB under 1 MB; the second matters only above 192 GiB of RAM, which no
   instance of the scoreboard has; the third keeps kernel-sized allocations (the vDSO's pages, a
   shm fallback) in the low memory by design. None of the three changes a number on any machine
   the port runs on.
4. **The direct map's tails.** ~~The `Pa` newtype for the drivers' entry points~~ — done 2026-10-09:
   `kaddr::Pa`, made only by `frame` (an allocator's frame), `of` (a kernel pointer) and `of_kva`;
   each refuses a value at or above 2^47 (every kernel virtual address on both machines) by
   stopping the kernel. NVMe's `push_rw`/`push_rw_ex` and the whole ext2 → fs → nvme request chain
   carry it, ENA's descriptors are written from it, and so are the addresses the hypervisor or the
   firmware write into (KVM's pvclock, steal time, PV EOI; virtio's `dma_alloc`; PSCI's secondary
   stack). `NetDev` keeps taking the kernel's virtual address by design (virtnet/nicloop slice it);
   the driver makes the `Pa`. Gate `parefuse` on both machines (a stacks' window address stops the
   kernel with its message); ~~`user_gb()` renamed~~ — done
   2026-10-09: `ident_gb()`, the identity gigabyte with 2M leaves (the identity modes' user region,
   a process's private PD); ~~arm64's Image header claims "placement
   anywhere"~~ — done 2026-10-09: the flags no longer set bit 3, and `_start` compares where it was
   put with `_head_pa` before writing anything, stopping in a `wfi` loop otherwise (checked: an image
   loaded 2 MiB higher with `-device loader` sits at `_start+0x1c`, the loop); TTBR1 reconsidered only if ASIDs per process or a KPTI-like split are wanted.
5. **Housekeeping.** ~~A `c6i.large` billed $2.57 a day~~ — answered 2026-10-09: Cost Explorer has it
   at 24 h a day from 2026-06-12 (before the port, before CloudTrail's 90 days) to 2026-10-04 16:59
   UTC — the account's `hermes-canary-sampler` (`Project=hermes`, another project; the cleanup filters
   by `Project=talaria*` and leaves it alone by design). It stopped with no EC2 API call in CloudTrail
   (from inside the instance), nothing of ours ended it; the worktree `my_os-s64` carries symlinked
   `services/*/build` and its own terraform state — retire it or make it the scoreboard's only home.
   **`talaria.test=` mode's secondary cores** (met 2026-10-21 by `wakedef`): without a layout
   (`affinity::CONFIGURED` false) `role()` calls every AP dedicated, so the test modes run their
   APs without a tick, and `sched_setaffinity` moves a thread only from the BSP (a PoC) — a thread
   that moved itself onto such a core starves a second thread placed there for good (under TCG,
   1000 wakeups of 1000 lost). The gates boot such tests as a cell meanwhile; the fix is either
   the common role for the test modes' APs (the tick on) or `set_affinity` refusing a second
   thread on a dedicated core — and the self-move from an AP, so a pin means what it says.
   **Done 2026-10-22, three pieces.** (1) `affinity::wants_tick`: without a layout plan an AP
   that hosts MORE THAN ONE task wants the tick (`sched::cpu_task_count`); `ap_loop` arms the
   timer by it at a task's entry, and `ap_schedule` starts it when it switches on a core that
   never went back through the loop (the preempted sleeper's borrowed deadline is returned a line
   above); a legacy AP with one task keeps run-to-completion, as the timing self-tests expect,
   and `role()`, stealing and pushing are untouched. (2) A self-move onto a core whose current
   task is RUNNING without a tick sets the newcomer as the hint and sends the resched IPI
   (`migrate_current_to`): the ring alone reached only an idle core or a bell sleeper, and the
   newcomer never ran. (3) `sched_setaffinity` moves the thread FROM ANY core and TO ANY core of
   the pool, the BSP included (the `cur_cpu != 0` PoC guard is gone; a target outside the pool
   stays a no-op). Two things the real move surfaced, both fixed: the BSP's LIMBO (a context left
   on the old stack after a self-move, `CURRENT` still naming the task) did not end when the
   same task moved BACK onto the BSP — the entry into its fresh frame was taken as "the same
   task stays", the next tick did not save its context and re-entered the consumed frame
   ("AN ENTRY INTO A USED FRAME t0 by core 0", a #GP; `cowstale` 4 runs of 6 — whose pin(0) from
   core 1 had been a no-op until now, so the item-0 scenario runs as written for the first time
   and passes 6 of 6); and the old kernel stack of a task that left an AP was queued for freeing
   while the AP still stood on it (the BSP reaps between the queueing and `ap_exit_to_loop`) —
   the AP now hands it to its pickup loop, which queues it from its own stack (`AP_DEAD_KSTK`).
   Gates on both machines: `wakedef` in `talaria.test=` mode (KVM p99 96-97 us, lost 0 — 1000 of
   1000 lost before; TCG judges lost 0) and `pinmove` (a thread walks 0 → 1 → 2 → 1 → 0 → 2 → 0
   by `sched_setaffinity` and runs on every core it asked for, on both).
6. **The merge of `arm64-port` into `main`** — the owner's call; before it, both regressions and
   `validate.sh` on the HEAD to be merged. **The pre-merge run, 2026-10-22, on the HEAD that closes
   items 2a, 2b(2) and 5:** `kernel/test.sh` 570/0 plus the host's 22/0, `kernel-unit` 303/0,
   clippy `-D warnings` clean in both configurations, `kernel/test-aarch64.sh` (with `-net` and
   `-pwr`) 409/0 under TCG, `kernel/validate.sh` 9/0 with PostgreSQL, MariaDB, MongoDB, OpenJDK 21
   and ClickHouse green (14 PASS, 0 FAIL); the same kernel booted on EC2
   as the default variant of `variants-wakesleep-c7g` (two `c7g.large`) and
   `variants-wakesleep-c6i` (two `c6i.large`), both to the workload's verdict and a clean
   `supervise: code 0`. `main` has not moved since the branch point (`3a29de1`), so the merge is a
   fast-forward of 130 commits.

## 12. The next session: beating Linux at every point of the board (plan, 2026-10-22)

The goal in one sentence: a Graviton3 scoreboard (§10.13) where no point of the service latency —
p50, p99, p99.9, p99.99 at every rung of every workload — and no closed-loop ceiling is Linux's,
under BOTH generators (ours and Linux's, `ab-phase.sh GEN=linux`), in the same hour. The rules
that got the board this far stay (ENGINEERING-NOTES 58, 61; §11's method): a lost point is first
an INSTRUMENT that names the mechanism, then a fix, then a gate that cannot lie; no knob is turned
by taste; every number is taken next to Linux's in the same hour; the generator's own share of a
point is split off before the server is blamed (§10.14). Costs are EC2 hours of `c7g.xlarge` x 4
(one workload pair per bring-up, two or three hours) — about a dozen bring-ups for the session.

**Where Linux wins today** (§10.13, re-read through §10.14-10.15 and §11): the Talaria generator's
ceiling (103-116 k against 125-141 k req/s) and, through it, redis's medians at the two top rungs;
redis's rare outliers at low rates (p99.99 285-558 us against 116-130, max 0.5-0.9 ms against
0.2-0.4 — §11 item 2c); PostgreSQL `pk`'s ceiling (15.5 against 25.2 k req/s) and its p99/p99.9
(143-149 against 131-144 us — item 2d); mongodb `insert`'s p99 at the middle and top rates
(235-350 against 184-293 us, the write path) and `ping`'s median at the top two rungs; nginx's
`nop` ceiling (98 against 146 k) and `file` (79 against 94 k); three p50 points of mysql, one of
mongoose. Everything else is ours or parity. The order below is the order of work; 0 comes first
because the board is sixteen days and six kernel changes old.

0. **Re-take the whole board on this HEAD** (`ARCH=arm64 bench/suite/run-remote-all.sh`, the six
   workloads, three repeats) and, for every network workload, the Linux-generator phase over the
   same ladder. Since the board was taken the kernel changed what the board measures: the
   syscall gate's price (recvfrom 4.3 → 0.45 us), `nanosleep`, the woken thread's wait, the
   sleeper's deadline, the TCP retransmission floor, and `strict` by default. The re-take is the
   REAL worklist for items 1-6 — a point that moved to parity by itself is not worked on. Keep
   both reports under `results/` with the date; the acceptance of every later item is read
   against them. Cost: two bring-ups (redis+mongoose+mysql, nginx+PostgreSQL+mongodb) plus the
   generator phases.

1. **The generator's ceiling and the lost frame** (redis's top rungs, nginx `nop`). Three
   instruments, then the fix the instruments name: (a) the ENA receive ring's drop counters and
   the ring's occupancy at the ceiling into the generator's `stat:` block (ENA keeps `rx_drops`
   in its admin statistics; the completion queue's phase tells a full ring) — the one drop per
   rung at 77 k+ that costs 200 ms now and that Linux never takes; (b) `sendto`'s price on the
   generator (2.3 us per call at 2.0 sends per request; `gatecost`'s method: the same call with
   and without the stack's work) — what of it is the doorbell per packet (LLQ), the stack's lock,
   the copy; Linux's `sendto` on the same instance is the reference; (c) the generator's
   syscalls per request (3.0 against Linux's 9.8 at the ceiling: Linux polls the empty socket
   more and still wins — its receive is cheaper or its send batches). The hypotheses, in the
   order the numbers will rank them: a doorbell per `sendto` → one doorbell per pass (the batch
   mode §7.2 exists for the server; the generator's path takes it too); the receive copy through
   the socket ring → a read straight from the frame; the ring's size against a burst → a deeper
   ring or an earlier pass. Gate: `txloss`'s twin for drops — a counter that must read 0 on a
   10-minute run at the ceiling; `gatecost` extended to `sendto`. Acceptance: the Talaria
   generator's ceiling ≥ Linux's on the same type in the same hour, every redis rung's median
   ours or parity under BOTH generators.

2. **redis's rare outliers at low rates** (§11 item 2c; p99.99 and max at 10-25 k req/s). The
   instrument is `umark` in `tlgen`: mark every request over 200 us
   (`prctl(PR_TALARIA_SLOW, t0, t1)`, the server's and the generator's kernels both keep the six
   longest with the cores' decisions and sleeps) — the stat block then names who held the core
   and whether it slept (the method that found `nanosleep` and the slice's wait). The suspects
   the board left: a long tick of 2 ms meeting a waiter that is not a timed sleeper, the kick path
   (`WK_KICK`, 999 of 1000 in `wakesleep`: a wakeup that only kicks a core and waits for the
   kicked sleeper to notice), the stack's `pollhard` pass colliding with a request. Gate: a
   `wakesleep`-like test at a low rate with the marks on and a p99.99 ceiling; acceptance: redis
   p99.99 and max ≤ Linux's at the 10 % and 25 % rungs.

3. **PostgreSQL `pk`: the ceiling and the p99** (§11 item 2d). First the split: the ceiling under
   the Linux generator (the board's 15.5 k is the Talaria pair's); then `talaria.sysprof=1` on the
   server at the ceiling — `pk` is a B-tree descent plus heap reads through `pread` on the FS
   cache, so the suspects are the cache's lookup (the page cache behind `ext2`, `fscnt`, the
   htree), the process model's syscall price under `strict` (every `pread` checks its buffer: the
   gate's table walk is now once per 4K table, but a 8 KB page read still walks), the per-backend
   wakeup from the stack (a fork per connection: the socket's owner is a process, the deliver
   path goes through `netpark`). Linux's `perf` on the same query is the reference for where the
   time SHOULD go. Gate: a `pk`-shaped microbenchmark (a backend's `pread` of a cached 8 KB page
   plus a `recv`/`send`) in `kernel/user` with its price against Linux's `gatecost` numbers.
   Acceptance: `pk` ceiling ≥ Linux's under the Linux generator, p99 ≤ Linux's at every rung.

4. **mongodb `insert`'s p99: the write path on EBS.** The instrument exists (session 38's fsync
   split: data 1056 us against copies 888, `jrnfree`, the FUA killed by `talaria.fua`); what the
   board says is that Linux's ext4 commits an `insert` in 184-293 us p99 where ours takes
   235-350. The three suspects, each a measurement before a fix: the data write before the commit
   in ordered mode (one NVMe command per extent — are our extents contiguous, and does the
   journal write wait for the data's completion or go in parallel on a second queue?); the
   commit's own latency on EBS (a 4 KB journal block with the barrier: Linux issues the data and
   the commit back to back and waits once); the wake of the fsync-er after the completion (the
   NVMe completion's interrupt path to a parked process — `netpark`'s cousin on the disk side,
   with a ceiling that may be the tail). Gate: `fsyncconc`'s latency histogram against a Linux
   guest on the same EBS volume in the same hour (the powercut harness already puts both on one
   volume). Acceptance: `insert` p99 ≤ Linux's at the middle and top rungs with the
   `powercut-ec2.sh` promise intact (the data survive).

5. **nginx and mongoose: the HTTP path.** After 0 and 1, what is left of nginx's `nop` and
   `file` ceilings is the server's: an HTTP response is several hundred bytes in one `writev`
   and a `sendfile` for `file` — the instruments are `sysprof` on the server at the ceiling and
   `netprof`'s stages; the suspects are the per-byte copy into the socket ring (a response of
   250 B against redis's 5), `sendfile` through the FS cache, and the 100-us `pollhard` pass.
   Acceptance: both ceilings ≥ Linux's under the Linux generator, the medians parity or ours.

6. **The small ones** (mysql's three p50 points, mongodb `ping`'s top-rung median, mongoose's
   one p50): taken from the re-take's report, each with the same question — the generator's or
   the server's (`GEN=linux`) — and only then an instrument. They are listed so that the
   acceptance is "no point", not "no big point".

7. **The acceptance run and the x86 check.** The board again on the session's HEAD, both
   generators, three repeats: the acceptance is a table with no Linux cell at any point of any
   workload and every ceiling ours or parity, plus `docs/MEASUREMENTS.md` §10.16 with the numbers
   and the mechanism named for every point that moved. Then the x86 board on `c6i.xlarge` once
   (the kernel is shared; sessions 33-43 left it ours — a regression there would be ours too).
   Both regressions and `validate.sh` before every EC2 run, as in §11 item 6.

### 12.1. Progress (2026-10-23)

Item 0 is done (MEASUREMENTS §10.16, both generators, `summary-genlinux.md` next to
`summary.md`); the PostgreSQL phase under the Linux generator needed the manifest's
`PROBE_ARGS` (`ebe4dae`) and was retaken. Item 1: the lost frame is measured to the frame and
bounded to "Talaria at both ends, between the two ENAs", not named — the next instrument is a
VPC traffic mirror of both ENIs; a sendto's price is the stack's pass, nothing hidden. Item 3:
the AENQ/admin wrap (`35b2bcd`) and the arm64 range flush (`5e97998`) are fixed and gated; the
×2 of the server's ceiling is bimodal within a boot, its per-core accounting is written down,
its mechanism is open — the waiter knobs do not move it. Item 5: the nginx server's profile is
taken (`writev` 3.1 us is the gap). Items 2, 4, 6, 7 are not started. The instruments live on
`arm64-inst` (`lossprobe-ec2.sh`, `kernprobe-ec2.sh`, `genprobe-ec2.sh`, the event ring, the
port counters, the frame classifier, the quiet-point report, the five-leg sendto split); what
is correct by the spec is in `arm64-port` (`dma_rmb` after the phase bit, the monotonic ring
counters, the batched flush). ENGINEERING-NOTES 62-66 hold the lessons, 66 in particular.

### 12.2. Progress (2026-10-25)

Item 3: PostgreSQL's ×2 is named and halved — its postmaster and background processes spun
their mixed `epoll` sets (socket + signalfd/self-pipe + the death pipe) on the backends' two
cores; the manifest key `netmix=<ns>` (`8610cda`) parks them: pk 9.9 → 18.9 k req/s, select1
26.7 → 31.5-35.4 k, the medians at Linux's (MEASUREMENTS §10.17). ASIDs on arm64 (`5efa961`)
were the first suspect and measured nothing; they stay because they are correct, gated on a
`c7g.metal` under KVM (`infra/aws/logs/aarch64-20261025-131835`): the gate 389/1 then 0 of 1
in the loop, the databases 15/0 twice, the soak 13 modes × 50 boots and the race detector
6 × 96 with no failure; the one FAIL was `shmrace A/B`'s negative control breaking the other
way (a half-published row faults instead of a second row), and its assertion now takes both. The stat block counts who spins a mixed set beside another
waiter (`444cd35`): only PostgreSQL and MongoDB do, and MongoDB under the knob loses its top
rungs — the knob is not a default. Left of item 3: the ceiling (pk 18.9 against 25.5 k, select1
33 against 44 k). Item 1: Nitro's connection tracking and the MTU are excluded, the VPC mirror
is not an instrument at this rate; the next stand is a `c7gn`/`.metal` pair.

### 12.3. Progress (2026-10-25, later)

Item 3: under `netmix` the polling slot belonged to the postmaster (first in, never out,
spinning) and the backends parked; `netmixsteal=<ns>` gives the slot to the waiter with
traffic (MEASUREMENTS §10.18). PostgreSQL's manifest takes 1 ms: the select1 ceiling
36.0 → 58.0 k (Linux 44.3 k), pk 18.4 → 21.2 k (Linux 24.6 k), the top rungs' p99 ÷2..÷6.
Left of item 3: pk's ceiling (21.2 against 24.6 k).

### 12.4. Progress (2026-10-26)

Item 3, pk's ceiling (MEASUREMENTS §10.19): the arm64 double TLB invalidation is gone
(munmap 3.3 → 2.0 us; KVM gate clean), and nine knob phases did not move the ceiling —
netmix's ceiling, the card line with and without parking, netyield, wakeidle, wakepull,
wakepreempt. The tick profile booked the kernel's masked syscall time to ring 3; counted
apart, PostgreSQL's ring 3 is ~29 us per request (Linux's user time 37), a quarter of the
cores sleep at the ceiling: it is the round trip (183 against 159 us with 4 connections),
not the processor. Left of item 3: those 24 us — the backend's runnable-but-not-running
time between its read and its write is the next instrument.

### 12.5. Progress (2026-10-26, later)

Item 3: the instrument split the read->write — 67 us on a core, not in a queue — and the
PMU at EL0 showed ring 3 at IPC 0.6 with ~800 L2-TLB refills a request: every mmap
reservation ended with the broadcast full flush (MEASUREMENTS §10.20). It flushes now only
over a present entry it overwrites: pk's ceiling 25.6-26.2 k against Linux's 26.8 k on the
same rise (was 20.7 against 25.2). Left of item 3: pk's ring 3 is still ~120 k cycles a
request with ~120-230 L2-TLB refills; whether Linux's is less is not measured (its guest
has no PMU access in the harness).

### 12.6. Progress (2026-10-26, the board)

Item 3's ceiling is accepted (MEASUREMENTS §10.21): on the PostgreSQL board under both
generators pk's ceiling is 25.5 / 25.4 k against Linux's 24.9 k, select1's 55.0 / 55.0 k
against 43.0 k. Item 3's p99 is not: the lowest rungs' p99-p99.9 are Linux's by 9-16 % (the
wakeup of an idle server), and select1's top rung p99 (104 against 85 us). The top rungs'
medians are Linux's because its cores stop sleeping there while its tail goes to milliseconds.
Followed on three rises: the low points change hands from rise to rise (a boot is not the
variable — five boots agree to a microsecond); netmixsteal's age, netmix=0 and idlepoll do not
move them. The steady one is select1's lowest rung (p99 79-84 against 74-75): its next
instrument is Linux's own side (`perf` in the Linux guest), which the harness does not have.

### 12.7. Progress (2026-10-27)

Item 3's low rungs: the instrument on Linux's side exists now (statd's `perf stat -a` at EL0
and EL1 in the middle of a point, `DURING=1`) and named the difference at once — the same
instructions per request took 4-10x the L2-TLB refills here, because Linux maps PostgreSQL's
text from 64 KB page-cache folios with the Contiguous bit (MEASUREMENTS §10.22). The hint is
ours by default on arm64 (`c5aade8`, `talaria.cont=0` — off): select1's lowest rung p50/p99
58.8/74.4 us against Linux's 62.7/72.3 (was 63.2/78.1), pk's 107.7/134.0 against 115.3/131.5,
every median of the low and middle rungs ours; a second rise moved the same way, with its
Linux faster at select1's bottom (p99 70.3 against our 76-77 while our ring 3 is the cheaper,
38.4 k cycles against 42.8 k). KVM `c7g.metal` gate clean with the hint on. Left of item 3:
select1's lowest p99 on a fast-Linux rise (the wakeup, not ring 3), the top rungs' medians
(Linux's cores stop sleeping), select1's ring 3 at the top rung (IPC 1.1 against 1.9 — the
next instrument is L1I/L1D-TLB and the cache events per request on both sides).

### 12.8. Progress (2026-10-27, later)

Item 3's select1 bottom: the 3.4 % of frames that wait 10-50 us in the ring while the slot's
holder is in ring 3 were named, and a parked waiter that drives the stack meanwhile
(`talaria.parkpoll`) was built, measured and reverted. Its A/B did not rise above the drift
between phases (MEASUREMENTS §10.23). The top rungs then turned out to be partly a matter of
the stand. The Linux server runs PostgreSQL on cores 2-3 with its interrupts on 0-1, and given
all four (`LINUX_SERVER_CORES=0-3`) its ceilings double: select1 91 k, pk 50.6 k. Against that,
Talaria on four cores keeps the low and middle rungs and loses the top of both ladders (select1's
ceiling 75.7 k, the top-rung median 48-51 us against 41). The owner chose the whole instance
for both (`SMP_ARM64=4`, `LINUX_CORES_ARM64=0-3` in the passport), so the next item is the
four-core ceiling and the top rungs on it. That ceiling is a latency (4 connections, one
request each), and ring 3 is the cheaper on four cores (select1 16.6 k cycles a request against
18.1 k, which closes item 2). A Linux generator takes 4.5-5.5 us off every rung of the same
Talaria server. Measured that way the server keeps all of the ladder but select1's last rung
(46.4 against 44.6 us) and pk's top-rung p99. The next targets are our generator's ~5 us (the
client path of the same kernel) and those ~2 us at the server's last rung.

What is NOT in this plan, on purpose: the lazy FP save (§11 item 2e, 17 ns of a 130 ns svc —
last, and only if a point needs exactly that), a new slice or threshold value without an
instrument that asks for it, and any change to how the board judges (the 0.95/1.05 rule and
"within a workload" stay — a board that moves its own goalposts proves nothing).

Related: `docs/ROADMAP.md` (where this sits among the other open items),
`docs/DEVELOPMENT-GUIDE.md` §3–§7 (the x86 mechanisms this plan translates),
`docs/BARE-METAL.md` (what QEMU hides — the arm64 list in §7 above is its
counterpart), `docs/ENGINEERING-NOTES.md` (how a gate is written so that it
cannot lie).
