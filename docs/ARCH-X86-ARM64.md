# x86-64 vs AArch64 — the implementation differences

How the same kernel is built on two machines. One source for the shared modules
(scheduler, FS, network stack, drivers' logic, process model, observability),
two `arch/` surfaces (`kernel/src/arch/x86_64/`, `kernel/src/arch/aarch64/`) that
translate the machine to one vocabulary. This page is the working reference for
what differs, what is the same, and what to account for when touching either side.

The plan and the decisions behind it: `docs/ARM64-PLAN.md` §3-§4. The module map:
`docs/DEVELOPMENT-GUIDE.md` §4.

---

## 0. The one-sentence summary

x86-64 is TSO, PCID-less, IDT + LAPIC + IOAPIC + MSI, calibrated TSC, XSAVE,
real-mode SMP, and Intel CAT. AArch64 is a weak memory model, ASIDs, a vector
table + GICv3 + ITS, a read-from-register generic timer, a 528-byte FP/SIMD
state, PSCI SMP, and no cache-allocation. Every other difference below is a
consequence of one of these.

---

## 1. Boot and firmware

| | x86-64 | AArch64 |
|---|---|---|
| The image | a PVH ELF (`-kernel`), plus a multiboot2 header for BIOS/GRUB | a Linux-format `Image`: the `ARMd` magic at offset 56, loaded at RAM base + `text_offset` (`0x4008_0000` on `virt`) |
| The entry | `pvh_start` / `mb2_start` → long mode | EL2 → EL1, the early identity map, `.bss`, FP/SIMD on, then Rust |
| The machine description | ACPI always (EC2, bare metal); BIOS via `tboot`/`mk-bios.sh` | A **device tree** (`-kernel` under QEMU) **or** ACPI (UEFI / EC2 Graviton) — two parsers: `dtb.rs`, `acpiboot.rs` |
| Firmware | OVMF (UEFI) or BIOS | **UEFI only** (edk2 `QEMU_EFI.fd`) — no BIOS path exists on arm64 EC2 |
| SMP bring-up | a real-mode trampoline + the LAPIC | one **PSCI** call per core (`CPU_ON` through `hvc`/`smc`) |

**What to account for:** the arm64 kernel must tolerate *both* a DTB and ACPI for
the same information (RAM, cores, GIC/ITS, timer PPIs, console, PCIe ECAM); the
`talaria.pci=1`/`talaria.dtb=` surface decides. `mk-bios.sh`, `mk-tboot.sh` and the
multiboot2 path stay x86-only by design (`ARM64-PLAN.md` §0).

---

## 2. Exceptions and the syscall entry

| | x86-64 | AArch64 |
|---|---|---|
| The table | the **IDT**, 256 vectors, one handler each | a **vector table** at `VBAR_EL1`: 16 entries, 0x80 B apart, four groups (current/lower EL × Sync/IRQ/FIQ/SError) |
| The syscall | `syscall`/`sysret` (the `int 0x80` path is legacy) | `svc #0` → the Sync-from-EL0 entry |
| The fault data | the error code on the stack, `CR2` | **`ESR_EL1`** (the syndrome, class `EC` + fault status `DFSC`), **`FAR_EL1`** (the address), **`ELR_EL1`**/`SPSR_EL1` (the return) |
| The saved frame | per-ABI regs (the syscall frame) + the XSAVE area | ONE **36-word `Frame`** (x0..x30, sp, elr, spsr, esr, far) + a 528-byte FP/SIMD block below it |

**What to account for:** the arm64 frame is *the* dispatcher frame (`syscall::Frame`),
*the* fault frame (`idt::ExcFrame`), and *the* scheduler's saved context — one shape
for all three, so the shared code sees the neutral names (`nr`, `a1..a6`, `pc`,
`flags`, `usp`) on both machines. The IRQ/FIQ masking in `RESTORE_FRAME` is
**before** the `msr elr_el1` on purpose: an IRQ taken between them would rewrite
ELR/SPSR and re-run the restore tail — a bug TCG never shows and KVM did
(`trap.rs` `RESTORE_FRAME`).

---

## 3. The MMU

| | x86-64 | AArch64 |
|---|---|---|
| The descriptor | the **canonical format** the shared `paging.rs` computes in (P/RW/US/PWT/PCD/PS/NX + the OS bits) | the same canonical format, translated at the boundary by `mmu::enc`/`dec` to the AArch64 stage-1 descriptor |
| The root | `CR3` | `TTBR0_EL1` (the kernel's own tables) |
| Address spaces | no PCID — every root switch drops the core's TLB | **ASIDs** (16-bit, in `TTBR0[63:48]`): a switch between two ASIDs is `msr ttbr0_el1; isb`, no invalidation |
| The dirty bit | the hardware **D bit** (bit 6), `take_dirty` clears it | no dirty bit; **DBM** (`TCR.HD`) makes a page read-only-until-first-write, and `AP[2]` carries D; without the feature the fallback is "every writable page is dirty". A read-only page's old dirty state survives in **bit 59** (the `H_SWD` trick, Linux `ff1712f953e2`) |
| TLB shootdown | IPIs to the *holders* of an address space (`NO_SHOOTDOWN`, `ask_host_flush`, the identity-hole shootdowns) | broadcast **`tlbi ...is`** — the architecture does it; `paging::shootdown_impl` is a plain flush |
| Large pages | 1 GB / 2 MB / 4 KB | 1 GB blocks (level 1), 2 MB, 4 KB, plus the **Contiguous hint** (16 aligned 4 KB pages share one TLB entry, `cont_fold`) |
| Memory attributes | PAT (`PWT`/`PCD` → a PAT index) | **MAIR** (Attr0 Normal WB, Attr1 Device-nGnRE, Attr2 Normal-NC for the ENA LLQ) |
| Kernel image | identity GB0, the kernel at the low MBs | the kernel in the **higher half** (TTBR0 slot 256, above `USER_TOP`) with W^X, and a **direct map** over any RAM (`talaria.dmap=strict`); the devices' alias lives at slot 384 |

**What to account for:** `enc`/`dec` is the only place the two descriptor
encodings touch — but the address mask differs (`ADDR_MASK` stops at bit 47 on
arm64, bit 51 is DBM, and a mask that swallowed it once turned a frame number
into a data abort). `set_root` has the break-before-make invalidation on *both*
sides of the register write for ASID-0 switches (a speculative walk under the
old regime read zeros one boot in three under KVM).

---

## 4. The Linux ABI

The dispatcher and the pointer gate match on x86-64's syscall numbers, which are
the **internal** numbering; the arm64 entry translates before dispatch
(`abi::to_internal`, 163 rows) and re-translates at the door (`abi::enter`/`leave`).

| | x86-64 | AArch64 |
|---|---|---|
| The numbers | `unistd_64.h` | the **asm-generic** table; 29 x86 calls have no arm64 number (`open`, `stat`, `poll`, `fork`, `pipe`, `epoll_wait`, `arch_prctl` …) — an arm64 libc never issues them |
| `struct stat` | 144 bytes, 64-bit `st_nlink` first | **128 bytes**, `st_mode` before a 32-bit `st_nlink` — an untranslated `fstat` writes 16 bytes *past* the caller's structure |
| `struct epoll_event` | 12 bytes, `data` at 4 | **16 bytes**, `data` at 8 |
| `clone` | `(flags, stack, parent_tid, child_tid, tls)` | `(flags, stack, parent_tid, **tls, child_tid**)` — the last two swap (`abi::normalize`) |
| `O_*` flags | `O_DIRECT`/`O_LARGEFILE` at bits 14-15, `O_DIRECTORY`/`O_NOFOLLOW` at 16-17 | the two pairs **change places** (`abi::oflags`, its own inverse) |
| `rt_sigreturn` | the kernel supplies the restorer | **no restorer in `sigaction`** — the kernel must supply the `__kernel_rt_sigreturn` trampoline in the vDSO |

**What to account for:** a layout difference is a memory-safety bug at the
process's side, so the translation lives at the dispatcher's door in `abi.rs` —
not in the dozen places that fill the structures. The `clone` swap, missed, gives
a new thread a garbage TLS and a `child_tid` written into it.

---

## 5. Interrupts

| | x86-64 | AArch64 |
|---|---|---|
| The controller | the local **LAPIC** + the **IOAPIC** for lines | **GICv3**: a distributor, a redistributor per core, the `ICC_*` CPU interface |
| MSI / MSI-X | the device writes **one MSI address** into the message | the **ITS** — an in-memory translation service the kernel must program (device tables, a collection table, commands) — translates a device's event into an **LPI** |
| The ID space | vectors | INTIDs: **SGI** 0-15 (IPIs), **PPI** 16-31 (the timer), **SPI** 32+ (devices), **LPI** 8192+ (MSI-X) |
| The edge/level semantics | edge on MSI, level on INTx | a GIC **SPI is level** — a handler that clears a line through a `try_take` of a lock it does not hold re-enters 600 k×/s (the `diskserver` fault); what silences a line must not depend on anything the interrupted code holds (`NetDev::intr_regs`) |

**What to account for:** there is no small version of the ITS — it is one of the
three genuinely new pieces of the port (`ARM64-PLAN.md` §0). The IRQ path
(`irq_entry`) dispatches by INTID: the timer PPI, `RESCHED_SGI`, LPIs, the
`WAKE_VECTOR` SGI (which does a `dmb ish` for `membarrier` — an exception on arm64
orders nothing by itself), and the gdb park SGI.

---

## 6. Time and the tick

| | x86-64 | AArch64 |
|---|---|---|
| The clock | **TSC**, calibrated against a PIT or asked from the hypervisor | the **generic timer**, `CNTV_CTL/CMP_EL0`, its frequency **read from `CNTFRQ_EL0`** — no calibration |
| The tick | the LAPIC **TSC-deadline**, one-shot | the timer's PPI 27, deadline-only, **level-sensitive** — with the compare in the past the line stays raised, so a borrowed/one-shot deadline must be **silenced** (`timer::stop`) or the core spins in a million interrupts |
| The wall clock | kvmclock / PHC | SNTP (`talaria.ntp=aws`, ±21 µs) on Graviton; the ENA PHC is the better reference where it exists (`ROADMAP.md` §2.3) |

---

## 7. The wait tiers

| | x86-64 | AArch64 |
|---|---|---|
| Sleep in the kernel | `monitor`/`mwait` on the user's futex word (the wake is a store, no IPI); `umwait` as a ring-3 tier | **WFE** on the exclusive monitor, `SEV` to wake — **one tier** for ring 0 and ring 3 both |
| The futex word | the waker's store itself wakes the sleeper | the same semantics; the `WAKE_VECTOR` SGI + `dmb ish` gives the ordering `iretq` used to give |

**What to account for:** there is no `umwait`/`mwait`-tier ladder on arm64 — the
tick-cost programme of the x86 sessions has to be redone with the WFE levers
(`ARM64-PLAN.md` §4.9).

---

## 8. FP / SIMD

| | x86-64 | AArch64 |
|---|---|---|
| The state | the **XSAVE** area (XMM/YMM/…, `mxcsr`) | a fixed **528-byte** block: q0..q31 + `fpcr` + `fpsr` |
| Save/restore | `xsave`/`xrstor` | `stp qN`/`ldp qN` in the entry macros; the save obeys `FPSAVE_ON` (the `svccost` probe measures it off) |
| What is not given | — | **SVE** is not saved (the `mrs` ID-register emulation clears the SVE bits, `trap.rs::emulate_id_read`) |

**What to account for:** the eager FP/SIMD save is a measured 17 ns of a 130 ns
`svc`; the lazy save is deliberately left out unless a point needs exactly that
(`ARM64-PLAN.md` §12.11).

---

## 9. The memory-ordering model

The single biggest behavioural difference, and the reason phase 5 of the port was
an audit, not a port.

| | x86-64 | AArch64 |
|---|---|---|
| The model | **TSO** — a load may not be reordered after a load, a store after a store, a load after a store; only store→load can reorder | **weak** — the compiler and the core may reorder freely unless a barrier orders them |
| The barriers | `mfence`/`lfence`/`sfence`, `lock`ed ops | **`dmb`/`dsb`/`isb`**, in three shareability domains: `ish` (inner — the other cores), `osh` (outer — PCIe devices behind the host bridge), `nsh` |
| DMA ordering | stores to a ring before the doorbell are ordered by TSO | `dma_wmb` (`dmb oshst`) before the doorbell — a PCIe device is in the *outer* domain, and a plain `dmb ish` does not order the stores as the device sees them |
| The audit | 1 444 `Relaxed`, 877 `SeqCst`, 367 `read_volatile`/`write_volatile` sites were *correct only because* x86 is TSO | the same sites re-examined one by one; ten defects found that a quiet KVM host never lost (`ENGINEERING-NOTES.md` §6.1) |

**What to account for:** a release store is `stlrb`, an acquire load `ldarb` — and
the `SCHED_TAIL` stack-free uses a **store-release** because a plain `strb` may
become visible before the handler's last spills onto the stack being freed.
Anything a device reads or writes needs `osh*`, not `ish`.

---

## 10. Devices

| | x86-64 | AArch64 |
|---|---|---|
| The interconnect | PCI + MSI-X, virtio-pci | **virtio-mmio** windows from the DTB (32 × 0x200 on `virt`) *and* PCIe through **ECAM** + MSI-X through the ITS (NVMe, virtio-pci) |
| The NIC | ENA with LLQ (the same on both) | ENA with LLQ over **Normal-NC** (MAIR index 2): an LLQ record is written as whole 64-byte blocks (`barrier::wc_copy64`, `__iowrite64_copy`'s shape) + `dgh`, ordered by the doorbell's `dmb oshst` |
| The disk | virtio-blk / NVMe | the same, plus virtio-blk over virtio-mmio |

**What to account for:** the `talaria.test=ena`/`phc` gates have no card under
QEMU, so they exist only on EC2 (`CONTRIBUTING.md`). The devices' alias window
(`DEV_SLOT` 384) exists because on `virt` the workload's ELF may want the low
gigabyte the devices live in.

---

## 11. Crypto, cache allocation, the instruction cache

| | x86-64 | AArch64 |
|---|---|---|
| AES | AES-NI (`aese`/`aesenc`), the in-place TLS path | the Crypto Extensions (`aese`/`pmull`) are **not written yet** — `Aes128::new` refuses and says so; `ct_eq` is arithmetic and shared |
| Cache allocation | Intel **CAT** (`llc=` → the L3 masks, `cat.rs`) | **none** — Graviton exposes no MPAM; the surface answers "no" (`cat.rs`) |
| The I-cache | coherent (no explicit sync for written code) | **`dc cvau`/`ic ivau`** after the kernel writes code the workload executes — an ELF's `.text`, a COW copy of an executable page, `mprotect(PROT_EXEC)`, the signal trampoline (`icache.rs`) |

---

## 12. Observability

| | x86-64 | AArch64 |
|---|---|---|
| The PMU | the core PMU (`rdpmc`) | **PMUv3** (`PMCCNTR_EL0` + event counters; 32-bit counters widened by CHAIN or by the reader counting wraps — no reference-cycles, the generic timer is the reference) |
| Unwinding | frame pointers / CFI | the **x29 frame chain** (`unwind.rs`) |
| The gdb stub | serial | the same, over the console (`gdb.rs`); `brk`-based, W^X-aware (a software breakpoint is written through the direct map) |

---

## 13. What is the same (and must stay that way)

The shared modules do not know which CPU they run on — the scheduler's policy
(`rqpick`, `affinity`, `placement`), the process model (`proc`), the FS
(`ext2*`, `jbd2`, `fs`, `tfs`), the network stack (`netsys`, `l1`, `fastpath`,
`fastring`, smoltcp), the drivers' logic (`ena`, `nvme`, `virtnet`), the
observability (`obs/`, `metrics`, `telemetry`) and the operator's side
(`console`, `shell`, `edit`, `filed`). They need one thing each: the
memory-ordering audit of §9. When a mechanism is added to one architecture, the
other gets it through the shared vocabulary, not by translation.

---

Related: `docs/ARM64-PLAN.md` (the plan and the decisions), `docs/DEVELOPMENT-GUIDE.md`
§3-§7 (the x86 mechanisms this translates), `docs/ENGINEERING-NOTES.md` §6.1 (what
the second architecture found in the shared code), `docs/BARE-METAL.md` (what QEMU
hides — its §7 is the arm64 list).
