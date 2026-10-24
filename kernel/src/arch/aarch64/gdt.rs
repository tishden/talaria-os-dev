//! No descriptor tables on arm64. What the x86 `gdt` module gave the kernel — a kernel
//! stack for the next ring-3 entry and the way INTO ring 3 — is `SP_EL1` (the hardware
//! keeps it across an `eret` to EL0, so a task's kernel stack is wherever its last frame
//! was restored from) and an `eret` with `ELR`/`SPSR`/`SP_EL0` loaded.

/// The kernel stack of this core's current task: recorded per core (the x86 TSS.rsp0);
/// `SP_EL1` itself is set by the entry that restored the task's frame.
pub fn set_rsp0(v: u64) { super::percpu::set_kstack_top(v); }
/// # Safety
/// Nothing to set up; the x86 name.
pub unsafe fn setup_gdt_cpu(_cpu: usize, kstack_top: u64) { super::percpu::set_kstack_top(kstack_top); }

/// Enter EL0 at `entry` with the stack `ustack` and the PSTATE `flags`
/// (`switch::USER_FLAGS_*`). The kernel stack the next `svc` lands on is this core's
/// recorded kstack top when there is one, else the current stack. It does not return.
/// # Safety
/// `entry` and `ustack` must be mapped in the CURRENT address space with user rights.
pub unsafe fn enter_user(entry: u64, ustack: u64, flags: u64) -> ! {
    let top = super::percpu::kstack_top();
    core::arch::asm!(
        "msr daifset, #2",
        "cbz {top}, 1f",
        "mov sp, {top}",
        "1:",
        "msr sp_el0, {usp}",
        "msr elr_el1, {pc}",
        "msr spsr_el1, {fl}",
        "eret",
        top = in(reg) top, usp = in(reg) ustack, pc = in(reg) entry, fl = in(reg) flags, options(noreturn)
    );
}
/// Switch to the address space `root` (TTBR0) and enter EL0 as one sequence.
/// # Safety
/// As `enter_user`, in the space `root`, whose tables must map this kernel stack as well.
pub unsafe fn switch_and_enter_user(root: u64, entry: u64, ustack: u64, flags: u64) -> ! {
    let top = super::percpu::kstack_top();
    // the space's ASID goes with it (the full invalidation stays: a process's first entry is rare)
    let root = super::mmu::ttbr_of(root);
    core::arch::asm!(
        "msr daifset, #2",
        "msr ttbr0_el1, {root}",
        "isb",
        "tlbi vmalle1",
        "dsb ish",
        "isb",
        "cbz {top}, 1f",
        "mov sp, {top}",
        "1:",
        "msr sp_el0, {usp}",
        "msr elr_el1, {pc}",
        "msr spsr_el1, {fl}",
        "eret",
        root = in(reg) root, top = in(reg) top, usp = in(reg) ustack, pc = in(reg) entry, fl = in(reg) flags, options(noreturn)
    );
}
