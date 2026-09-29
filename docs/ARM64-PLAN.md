# Talaria OS — the arm64 port: the plan

What it takes to run the kernel on AArch64 (EC2 Graviton, QEMU `virt`) with the
same product contract as on x86-64: unmodified Linux binaries in ring 3 (EL0),
the cores owned by the workload, the OS as a control plane. This is a WORKING
PLAN, written before a line of arm64 code exists: it names every x86 mechanism
the kernel leans on, what replaces it, in which order it is built, and by which
gate each step is judged. The numbers here are estimates and are marked as such;
the measurements come later and go to `docs/MEASUREMENTS.md`.

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
`__kernel_rt_sigreturn` trampoline arm64 needs because it has no `SA_RESTORER`). The
gates in `main.rs` went 92 → 85: that count is the meter. The x86-64 tree is the release (`README.md`); the
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
`syscall::Frame`, and `gdb::Frame` that mirrors them). The sync-exception
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

## 9. The first session's checklist

1. Install: `rustup target add aarch64-unknown-none aarch64-unknown-uefi
   --toolchain nightly-2026-08-20`; `dnf install qemu-system-aarch64-core
   edk2-aarch64`; `aarch64-linux-musl-cross` into `services/docker/` as a third
   toolchain image (`Dockerfile.musl-aarch64`).
2. Create `kernel/src/arch/{mod.rs,x86_64/}` and MOVE (not rewrite) the `asm!`
   holders listed in §4.1 there, one module per commit, running `test.sh` after
   each: the count must not move. This is a week of mechanical work with a
   sharp gate, and it is the part that cannot be wrong without being noticed.
3. Write `aarch64-talaria.json`, `linker-aarch64.ld` with the `Image` header
   section first, and the empty `arch/aarch64/` that compiles; make
   `cargo build --target aarch64-talaria.json` a CI job next to the x86 one.
4. Boot to `talaria: boot` on `qemu-system-aarch64 -M virt -cpu max -kernel`
   with the PL011 and the DTB parser (phase 1's first half). From here the work
   is the table in §3, top to bottom.
5. Bring up a `c7g.metal` once, install docker + `qemu-system-aarch64` + KVM
   there, run the x86-independent subset of `test.sh` under `TAL_ARCH=aarch64`,
   and record the price of an hour and the time to bring it up in
   `docs/ENGINEERING-NOTES.md` — the stand's economics decide how often the
   full set runs.

Related: `docs/ROADMAP.md` (where this sits among the other open items),
`docs/DEVELOPMENT-GUIDE.md` §3–§7 (the x86 mechanisms this plan translates),
`docs/BARE-METAL.md` (what QEMU hides — the arm64 list in §7 above is its
counterpart), `docs/ENGINEERING-NOTES.md` (how a gate is written so that it
cannot lie).
