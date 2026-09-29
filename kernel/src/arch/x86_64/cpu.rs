//! The privileged instructions, one wrapper each. This is the whole vocabulary
//! the shared modules use to touch THIS core's machine state: interrupt masking,
//! the halt/pause family, MSRs, CPUID, the control registers, TLB invalidation,
//! port I/O, the TSC, MONITOR/MWAIT. Each wrapper is the one `asm!` block that
//! used to sit inline at its call sites, with the same `options(...)`: the
//! compiler is promised exactly what it was promised before.
//!
//! # Safety
//!
//! Every `unsafe fn` here changes or depends on machine state that the caller
//! must own at that moment: interrupts on the current core, a control register,
//! an MSR, a port. The contract is the CALLER's (named at the sites as P5, W3,
//! W4, S4 in the modules' `# Safety` sections); the wrappers add nothing to it
//! and take nothing away. The safe functions read state without side effects.

/// Enable interrupts on this core (`sti`).
#[inline(always)]
pub unsafe fn irq_enable() { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)); }

/// Disable interrupts on this core (`cli`).
#[inline(always)]
pub unsafe fn irq_disable() { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)); }

/// The RFLAGS of this core (for `irq_enabled`/`irq_restore`).
#[inline(always)]
pub fn irq_flags() -> u64 {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags)); }
    flags
}

/// Are interrupts open on this core (RFLAGS.IF)?
#[inline(always)]
pub fn irq_enabled() -> bool { irq_flags() & 0x200 != 0 }

/// Re-open interrupts if `flags` (from `irq_flags`) says they were open.
#[inline(always)]
pub unsafe fn irq_restore(flags: u64) { if flags & 0x200 != 0 { irq_enable(); } }

/// Wait for the next interrupt (`hlt`). With interrupts masked it never returns.
#[inline(always)]
pub unsafe fn halt() { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)); }

/// Halt for good: the tail of a panic, a refused boot, a finished suite.
#[inline(always)]
pub fn halt_forever() -> ! { loop { unsafe { halt(); } } }

/// Halt for good with interrupts masked first: nothing may wake this core again.
#[inline(always)]
pub fn halt_irq_off_forever() -> ! { loop { unsafe { irq_disable(); halt(); } } }

/// `sti; hlt` — open the window and sleep in one step (the self-check loops).
#[inline(always)]
pub unsafe fn irq_enable_halt() { core::arch::asm!("sti; hlt", options(nostack, preserves_flags)); }

/// `sti; hlt; cli` — a ONE-INSTRUCTION interrupt window (invariant W4 of `sync.rs`):
/// the only interrupt that can arrive is the one that wakes the `hlt`.
#[inline(always)]
pub unsafe fn irq_enable_halt_disable() { core::arch::asm!("sti; hlt; cli", options(nostack)); }

/// `sti; pause; cli` — the same one-instruction window without sleeping.
#[inline(always)]
pub unsafe fn irq_enable_pause_disable() { core::arch::asm!("sti; pause; cli", options(nostack)); }

/// Arm the MONITOR on the cache line of `addr` (rax = the line, ecx/edx = 0).
#[inline(always)]
pub unsafe fn monitor(addr: *const u32) {
    core::arch::asm!("monitor", in("rax") addr, in("rcx") 0u64, in("rdx") 0u64, options(nostack, preserves_flags));
}

/// `sti; mwait; cli` — sleep until the monitored line is written or an interrupt
/// arrives; the window is exactly the `mwait` (W3 + W4).
#[inline(always)]
pub unsafe fn irq_enable_mwait_disable() {
    core::arch::asm!("sti; mwait; cli", in("rax") 0u64, in("rcx") 0u64, options(nostack));
}

/// Read an MSR.
#[inline(always)]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nostack, preserves_flags));
    ((hi as u64) << 32) | lo as u64
}

/// Write an MSR.
#[inline(always)]
pub unsafe fn wrmsr(msr: u32, v: u64) {
    core::arch::asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack, preserves_flags));
}

/// CPUID leaf `leaf`, sub-leaf 0: (eax, ebx, ecx, edx).
#[inline(always)]
pub fn cpuid(leaf: u32) -> (u32, u32, u32, u32) { cpuid_count(leaf, 0) }

/// CPUID leaf `leaf`, sub-leaf `sub`: (eax, ebx, ecx, edx). The intrinsic saves
/// and restores rbx, which LLVM reserves.
#[inline(always)]
pub fn cpuid_count(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    let r = core::arch::x86_64::__cpuid_count(leaf, sub);
    (r.eax, r.ebx, r.ecx, r.edx)
}

/// `mfence` — order every prior load and store before the next instruction.
#[inline(always)]
pub fn mfence() { unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)); } }

#[inline(always)] pub unsafe fn read_cr0() -> u64 { let v: u64; core::arch::asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)); v }
#[inline(always)] pub unsafe fn write_cr0(v: u64) { core::arch::asm!("mov cr0, {}", in(reg) v, options(nostack, preserves_flags)); }
/// The faulting address of the last page fault on this core.
#[inline(always)] pub unsafe fn read_cr2() -> u64 { let v: u64; core::arch::asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)); v }
#[inline(always)] pub unsafe fn read_cr3() -> u64 { let v: u64; core::arch::asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)); v }
/// A CR3 write is never `nomem`: it publishes the table writes before it (P5 of `paging.rs`).
#[inline(always)] pub unsafe fn write_cr3(v: u64) { core::arch::asm!("mov cr3, {}", in(reg) v, options(nostack, preserves_flags)); }
#[inline(always)] pub unsafe fn read_cr4() -> u64 { let v: u64; core::arch::asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)); v }
#[inline(always)] pub unsafe fn write_cr4(v: u64) { core::arch::asm!("mov cr4, {}", in(reg) v, options(nostack, preserves_flags)); }

/// Invalidate this core's TLB entry for the page of `vaddr`.
#[inline(always)]
pub unsafe fn invlpg(vaddr: u64) { core::arch::asm!("invlpg [{}]", in(reg) vaddr, options(nostack, preserves_flags)); }

/// Write back and invalidate the caches (the PAT reprogramming sequence).
#[inline(always)]
pub unsafe fn wbinvd() { core::arch::asm!("wbinvd", options(nostack)); }

/// Write the extended control register `xcr` (XCR0: which state XSAVE covers).
#[inline(always)]
pub unsafe fn xsetbv(xcr: u32, v: u64) {
    core::arch::asm!("xsetbv", in("ecx") xcr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nomem, nostack));
}

/// Read the extended control register `xcr`.
#[inline(always)]
pub unsafe fn xgetbv(xcr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    core::arch::asm!("xgetbv", in("ecx") xcr, out("eax") lo, out("edx") hi, options(nomem, nostack));
    ((hi as u64) << 32) | lo as u64
}

// ── Port I/O ─────────────────────────────────────────────────────────────────
#[inline(always)] pub unsafe fn outb(port: u16, val: u8) { core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags)); }
#[inline(always)] pub unsafe fn inb(port: u16) -> u8 { let v: u8; core::arch::asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack, preserves_flags)); v }
#[inline(always)] pub unsafe fn outw(port: u16, val: u16) { core::arch::asm!("out dx, ax", in("dx") port, in("ax") val, options(nomem, nostack, preserves_flags)); }
#[inline(always)] pub unsafe fn inw(port: u16) -> u16 { let v: u16; core::arch::asm!("in ax, dx", in("dx") port, out("ax") v, options(nomem, nostack, preserves_flags)); v }
#[inline(always)] pub unsafe fn outl(port: u16, val: u32) { core::arch::asm!("out dx, eax", in("dx") port, in("eax") val, options(nomem, nostack, preserves_flags)); }
#[inline(always)] pub unsafe fn inl(port: u16) -> u32 { let v: u32; core::arch::asm!("in eax, dx", in("dx") port, out("eax") v, options(nomem, nostack, preserves_flags)); v }

/// The time-stamp counter.
#[inline(always)]
pub fn rdtsc() -> u64 { unsafe { core::arch::x86_64::_rdtsc() } }

/// Reboot by a triple fault: an empty IDT and a `#BP`. The last resort when the
/// reset ports did nothing (`suite::reboot`).
#[inline(always)]
pub unsafe fn triple_fault() -> ! {
    let idt: [u8; 10] = [0; 10];
    core::arch::asm!("lidt [{}]", in(reg) &idt, options(nostack));
    core::arch::asm!("int3", options(nostack));
    halt_forever()
}
