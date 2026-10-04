//! The instruction cache is NOT coherent with data writes on AArch64 (docs/ARM64-PLAN.md
//! §4.12): after the kernel writes code the workload will execute — an ELF's `.text`,
//! a COW copy of an executable page, an `mprotect(PROT_EXEC)`, the signal trampoline —
//! the data cache must be cleaned to the point of unification and the instruction
//! cache invalidated for that range, or a stale line executes the previous contents.
//! x86 needs none of this, which is exactly why it is easy to forget: the failure is
//! silent, intermittent and invisible under TCG (QEMU keeps the caches coherent).

/// Clean and invalidate [addr, addr+len) to the point of coherency: what a core with its
/// caches OFF will read (or write past the caches) must be in memory, not in a line of
/// this core's cache — the secondary's boot data (`kmain::start_secondary`).
pub fn clean_inval_poc(addr: u64, len: u64) {
    if len == 0 { return; }
    let ctr: u64;
    unsafe { core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)); }
    let dline = 4u64 << ((ctr >> 16) & 0xF);
    let mut a = addr & !(dline - 1);
    unsafe {
        while a < addr + len { core::arch::asm!("dc civac, {}", in(reg) a, options(nostack)); a += dline; }
        core::arch::asm!("dsb sy", options(nostack));
    }
}

/// Make the bytes at `[addr, addr + len)` visible to instruction fetch on every core.
/// `addr` is the address the KERNEL wrote through (the frame's identity address for a
/// loader, the process's own for an `mprotect`): the clean is by address and reaches the
/// same lines either way; the invalidate is by address only on a PIPT instruction cache
/// (`CTR_EL0.L1Ip` = 0b11 — Neoverse and every recent core), and on a VIPT one it is the
/// whole cache of every core (`ic ialluis`), as Linux's `sync_icache_aliases` does, since
/// the process fetches through another virtual address than the kernel wrote.
pub fn sync(addr: u64, len: u64) {
    if len == 0 { return; }
    // The cache line sizes come from CTR_EL0: DminLine (bits 19:16) and IminLine (3:0),
    // in words.
    let ctr: u64;
    unsafe { core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)); }
    let dline = 4u64 << ((ctr >> 16) & 0xF);
    let iline = 4u64 << (ctr & 0xF);
    let pipt = (ctr >> 14) & 0x3 == 0b11;
    let end = addr + len;
    unsafe {
        let mut a = addr & !(dline - 1);
        while a < end { core::arch::asm!("dc cvau, {}", in(reg) a, options(nostack)); a += dline; }
        core::arch::asm!("dsb ish", options(nostack));
        if pipt {
            let mut a = addr & !(iline - 1);
            while a < end { core::arch::asm!("ic ivau, {}", in(reg) a, options(nostack)); a += iline; }
        } else {
            core::arch::asm!("ic ialluis", options(nostack));
        }
        core::arch::asm!("dsb ish", "isb", options(nostack));
    }
}
