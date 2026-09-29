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
