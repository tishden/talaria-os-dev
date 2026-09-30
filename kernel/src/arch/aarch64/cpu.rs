//! The privileged instructions of AArch64, one wrapper each — the counterpart of
//! `arch::x86_64::cpu`. The same vocabulary where the two machines have the same
//! notion (interrupt masking, halting, the counter), an arm64-only name where they
//! do not (`isb`, `dsb_ish`, the system registers by name).
//!
//! # Safety
//!
//! As on x86: every `unsafe fn` changes machine state the caller must own at that
//! moment; the wrappers add nothing to the caller's contract.

/// Enable interrupts (IRQ and FIQ) on this core.
#[inline(always)]
pub unsafe fn irq_enable() { core::arch::asm!("msr daifclr, #3", options(nomem, nostack)); }

/// Disable interrupts (IRQ and FIQ) on this core.
#[inline(always)]
pub unsafe fn irq_disable() { core::arch::asm!("msr daifset, #3", options(nomem, nostack)); }

/// The DAIF mask bits of this core (for `irq_enabled`/`irq_restore`).
#[inline(always)]
pub fn irq_flags() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, daif", out(reg) v, options(nomem, nostack)); }
    v
}

/// Are IRQs open on this core (DAIF.I clear)?
#[inline(always)]
pub fn irq_enabled() -> bool { irq_flags() & (1 << 7) == 0 }

/// Restore the DAIF mask saved by `irq_flags`.
#[inline(always)]
pub unsafe fn irq_restore(flags: u64) { core::arch::asm!("msr daif, {}", in(reg) flags, options(nomem, nostack)); }

/// Wait for an interrupt (`wfi`). Unlike `hlt`, a PENDING but masked interrupt wakes it too.
#[inline(always)]
pub unsafe fn halt() { core::arch::asm!("wfi", options(nomem, nostack)); }

/// Halt for good.
#[inline(always)]
pub fn halt_forever() -> ! { loop { unsafe { halt(); } } }

/// Halt for good with interrupts masked.
#[inline(always)]
pub fn halt_irq_off_forever() -> ! { loop { unsafe { irq_disable(); halt(); } } }

/// The one-instruction interrupt window of `sync.rs` (W4): open, sleep, close.
#[inline(always)]
pub unsafe fn irq_enable_halt_disable() { core::arch::asm!("msr daifclr, #3", "wfi", "msr daifset, #3", options(nomem, nostack)); }

/// Instruction synchronisation barrier: the system-register writes before it take effect.
#[inline(always)]
pub fn isb() { unsafe { core::arch::asm!("isb", options(nomem, nostack)); } }

/// Data synchronisation barrier, inner shareable: table writes are complete before TLBI/enable.
#[inline(always)]
pub fn dsb_ish() { unsafe { core::arch::asm!("dsb ish", options(nostack)); } }

/// Data synchronisation barrier, full system.
#[inline(always)]
pub fn dsb_sy() { unsafe { core::arch::asm!("dsb sy", options(nostack)); } }

/// The counter that plays the TSC: `CNTVCT_EL0`, read after an `isb` so that it is
/// not read speculatively early (the architecture allows that without the barrier).
#[inline(always)]
pub fn counter() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack)); }
    v
}

/// The counter's frequency in Hz (`CNTFRQ_EL0`), set by the firmware.
#[inline(always)]
pub fn counter_hz() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack)); }
    v
}

/// The current exception level (0..3) from `CurrentEL`.
#[inline(always)]
pub fn current_el() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) v, options(nomem, nostack)); }
    (v >> 2) & 3
}

/// The affinity id of this core (`MPIDR_EL1`, Aff3..Aff0) — the arm64 "APIC id".
#[inline(always)]
pub fn mpidr() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack)); }
    v & 0xFF00_FFFFFF
}

/// This core's index for the per-core tables. Until `smp.rs` comes across with its
/// dense map of affinities, it is `Aff0` — which is what QEMU `virt` and a Nitro
/// guest number their cores by (one cluster). A machine with several clusters gets
/// the dense map, not a bigger table.
#[inline(always)]
pub fn cpu_index() -> usize { (mpidr() & 0xFF) as usize }

/// The main id register (`MIDR_EL1`): implementer, part number, revision.
#[inline(always)]
pub fn midr() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, midr_el1", out(reg) v, options(nomem, nostack)); }
    v
}

/// `ID_AA64MMFR0_EL1`: the physical address range, the granules supported.
#[inline(always)]
pub fn id_aa64mmfr0() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, id_aa64mmfr0_el1", out(reg) v, options(nomem, nostack)); }
    v
}

/// The syndrome and the faulting address of the last exception taken to EL1.
#[inline(always)]
pub fn esr_el1() -> u64 { let v: u64; unsafe { core::arch::asm!("mrs {}, esr_el1", out(reg) v, options(nomem, nostack)); } v }
#[inline(always)]
pub fn far_el1() -> u64 { let v: u64; unsafe { core::arch::asm!("mrs {}, far_el1", out(reg) v, options(nomem, nostack)); } v }
#[inline(always)]
pub fn elr_el1() -> u64 { let v: u64; unsafe { core::arch::asm!("mrs {}, elr_el1", out(reg) v, options(nomem, nostack)); } v }
#[inline(always)]
pub fn sctlr_el1() -> u64 { let v: u64; unsafe { core::arch::asm!("mrs {}, sctlr_el1", out(reg) v, options(nomem, nostack)); } v }
#[inline(always)]
pub fn ttbr0_el1() -> u64 { let v: u64; unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) v, options(nomem, nostack)); } v }

/// What EL0 may touch without a trap, as Linux grants it — ON EVERY CORE (these are
/// per-core registers; a secondary that was not given them trapped the vDSO's clock read
/// of the first thread that ran on it: `MSR/MRS trapped` at the vDSO's pc):
/// * `CNTVCT_EL0`/`CNTFRQ_EL0` (`CNTKCTL_EL1.EL0VCTEN`): the vDSO's clock;
/// * `CTR_EL0` (`SCTLR_EL1.UCT`) and the cache maintenance by address (`UCI`): a JIT's
///   `__clear_cache` reads the line size and issues `dc cvau`/`ic ivau` itself;
/// * `DC ZVA` (`DZE`): a libc's `memset` uses it when `DCZID_EL0` says it may.
pub unsafe fn enable_el0_access() {
    // The whole word, not a read-modify-write: CNTKCTL_EL1's reset value is UNKNOWN
    // (KVM's poison), and the other bits (the event stream, EL0 access to the physical
    // timer) are not wanted.
    core::arch::asm!("msr cntkctl_el1, {}", "isb", in(reg) 1u64 << 1, options(nomem, nostack));
    let s = sctlr_el1() | (1 << 14) | (1 << 15) | (1 << 26);
    core::arch::asm!("msr sctlr_el1, {}", "isb", in(reg) s, options(nomem, nostack));
}

/// Does this core manage the access flag and the dirty state in hardware (FEAT_HAFDBS:
/// `ID_AA64MMFR1_EL1.HAFDBS` >= 2 — the dirty state, not only the access flag)?
/// `sti; hlt` — wait for an interrupt with them enabled, and leave them enabled.
/// # Safety
/// The caller expects to be interrupted here.
#[inline(always)]
pub unsafe fn irq_enable_halt() { core::arch::asm!("msr daifclr, #2", "wfi", options(nomem, nostack)); }
/// `sti; pause; cli` — a one-instruction interrupt window in a spin.
/// # Safety
/// As `irq_enable_halt`; interrupts are off again on return.
#[inline(always)]
pub unsafe fn irq_enable_pause_disable() { core::arch::asm!("msr daifclr, #2", "yield", "msr daifset, #2", options(nomem, nostack)); }
/// Arm this core's exclusive monitor on the word at `addr`: a store to it by another core
/// clears the monitor and raises the event `wfe` waits for — MONITOR's job on x86.
/// # Safety
/// `addr` must be mapped and readable (invariant W3 of sync.rs).
#[inline(always)]
pub unsafe fn monitor(addr: *const u32) {
    core::arch::asm!("ldxr {t:w}, [{a}]", t = out(reg) _, a = in(reg) addr, options(nostack));
}
/// `sti; mwait; cli` — sleep until the monitored word is written or an interrupt arrives.
/// # Safety
/// After `monitor`, with the re-read between them, as sync.rs orders it.
#[inline(always)]
pub unsafe fn irq_enable_mwait_disable() { core::arch::asm!("msr daifclr, #2", "wfe", "msr daifset, #2", options(nomem, nostack)); }
/// The full barrier the x86 code calls `mfence`.
#[inline(always)]
pub fn mfence() { dsb_sy(); }
/// What the core offers a waiter: the exclusive monitor plays MONITOR/MWAIT, the generic timer
/// is a deadline timer; there is no user-mode timed wait (WFET is FEAT_WFxT, not read yet).
pub fn wait_caps() -> (bool, bool, bool, bool) { (true, false, false, true) }
/// No `umwait` ceiling to lift on this machine.
/// # Safety
/// Nothing to do.
pub unsafe fn umwait_unlimit() {}
/// The thread-pointer of the workload's current thread (`TPIDR_EL0`; musl's `%fs` on x86).
/// # Safety
/// As on x86: this core's register, on a context switch.
pub unsafe fn set_tls_base(v: u64) { core::arch::asm!("msr tpidr_el0, {}", in(reg) v, options(nomem, nostack)); }
/// # Safety
/// As `set_tls_base`.
pub unsafe fn tls_base() -> u64 { let v: u64; core::arch::asm!("mrs {}, tpidr_el0", out(reg) v, options(nomem, nostack)); v }
/// The faulting address of the last abort on this core.
pub fn fault_addr() -> u64 { far_el1() }
/// Power the machine off through the firmware (PSCI SYSTEM_OFF); returns only if it refused.
pub fn power_off() { super::psci::system_off(); }
/// Reset the machine through the firmware (PSCI SYSTEM_RESET) and never return.
pub fn reset_machine() -> ! { super::psci::system_reset(); halt_forever() }

pub fn has_hw_dirty() -> bool {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, id_aa64mmfr1_el1", out(reg) v, options(nomem, nostack)); }
    (v & 0xF) >= 2
}
