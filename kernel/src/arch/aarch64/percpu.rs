//! Per-CPU data through `TPIDR_EL1` (docs/ARM64-PLAN.md §4.6): the register holds the
//! address of this core's `PerCpu`, set once when the core comes up and never changed —
//! the arm64 twin of x86's GS.base. The block's field OFFSETS are the same as on x86,
//! because the exception stubs address them by number.
//!
//! What the shared modules use today: `cpu_id`, `MAXCPU`, `ONLINE`, `cur_task_of`. The
//! rest of the block (the kernel stack top for an EL0 entry, the current task) is filled
//! in as the syscall entry and the scheduler come across.

use core::sync::atomic::{AtomicU64, Ordering};

/// The ceiling on logical cores — the width of the `ONLINE` mask, as on x86.
pub const MAXCPU: usize = 64;

/// The field offsets are used verbatim by the stubs (trap.rs):
///   +0  ursp        the user sp on an EL0 entry (scratch)
///   +8  kstack_top  the kernel stack of this core's current task
///   +16 sig_newrsp  a user-sp substitution on the way out (signals)
///   +24 cpu         the logical number
///   +32 cur_task    the scheduler task on this core; u64::MAX — idle
///   +40 sched_root  the root table the core is switching to
///   +48 (unused on arm64: x86's PV EOI word)
///   +56 off_stack   the task whose kernel stack the core is leaving
///   +64 fp_area     the FP/SIMD area of the current entry (the stubs write it)
///   +72 fp_pending  the FP/SIMD image to load at the next entry into a frame (0 — none)
#[repr(C, align(64))]
pub struct PerCpu {
    pub ursp: u64,
    pub kstack_top: u64,
    pub sig_newrsp: u64,
    pub cpu: u64,
    pub cur_task: u64,
    pub sched_root: u64,
    pub _pv_eoi: u64,
    pub off_stack: u64,
    /// +64: the 528-byte FP/SIMD area the current entry's stub saved below its frame
    /// (`switch::fpu_save` copies a task's state from there, not from the live registers).
    pub fp_area: u64,
    /// +72: the saved FP/SIMD image of the task the scheduler is about to enter
    /// (`switch::fpu_restore` sets it, the entering tail loads the registers and clears it).
    pub fp_pending: u64,
}

static mut CPUS: [PerCpu; MAXCPU] = {
    const Z: PerCpu = PerCpu { ursp: 0, kstack_top: 0, sig_newrsp: 0, cpu: 0, cur_task: u64::MAX, sched_root: 0, _pv_eoi: 0, off_stack: u64::MAX, fp_area: 0, fp_pending: 0 };
    [Z; MAXCPU]
};

/// The live cores (a bitmask, bit0 = the boot core).
pub static ONLINE: AtomicU64 = AtomicU64::new(1);

/// Point `TPIDR_EL1` at this core's block. Before it, `cpu_id` falls back to the affinity.
pub fn init_tpidr(cpu: usize) {
    if cpu >= MAXCPU { return; }
    unsafe {
        CPUS[cpu].cpu = cpu as u64;
        let base = crate::mem::vaddr(core::ptr::addr_of!(CPUS[cpu]));
        core::arch::asm!("msr tpidr_el1, {}", "isb", in(reg) base, options(nomem, nostack));
    }
}

/// Initialise the current core's per-CPU area and enter it into `ONLINE`.
pub fn init(cpu: usize) {
    init_tpidr(cpu);
    ONLINE.fetch_or(1 << cpu, Ordering::SeqCst);
}

/// This core's block (the diagnostics).
pub fn block_ptr() -> *mut PerCpu { block() }
#[inline(always)]
fn block() -> *mut PerCpu {
    let p: u64;
    unsafe { core::arch::asm!("mrs {}, tpidr_el1", out(reg) p, options(nomem, nostack)); }
    crate::mem::vptr::<PerCpu>(p)
}

/// This core's logical number: from the block when it is set, else the affinity.
#[inline]
pub fn cpu_id() -> usize {
    let b = block();
    if b.is_null() { return super::cpu::cpu_index(); }
    unsafe { (*b).cpu as usize }
}

/// Which task core `cpu` is executing; u64::MAX — idle.
pub fn cur_task_of(cpu: usize) -> u64 {
    if cpu >= MAXCPU { return u64::MAX; }
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(CPUS[cpu].cur_task)) }
}
/// The task whose kernel stack core `cpu` is leaving in the switch stub (u64::MAX — none):
/// what a core that waits for that stack asks before it gives the wait up (`sched::stack_wait`).
pub fn off_stack_of(cpu: usize) -> u64 {
    if cpu >= MAXCPU { return u64::MAX; }
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(CPUS[cpu].off_stack)) }
}

/// The x86 name of `init_tpidr`: only the register, not the `ONLINE` mask.
pub fn init_gs(cpu: usize) { init_tpidr(cpu); }
/// No PV EOI word on arm64.
pub fn pv_eoi_addr(_cpu: usize) -> u64 { 0 }
/// The user-sp substitution on the way out to EL0 (+16), the current task (+32), the root
/// table the core is switching to (+40, the x86 name kept), the stack being left (+56).
///
/// BEFORE `init` the register is 0 and a field is a read of physical 0x20 — which on `virt`
/// answered from the flash through GB0's identity map and went unnoticed until GB0 stopped
/// being mapped (the devices' alias, `mmu::DEV_SLOT`): the frame allocator's per-task count
/// asked `cur_task` at the first allocation. Without a block a read is "none" and a write
/// goes nowhere.
#[inline] pub fn sig_newrsp() -> u64 { let b = block(); if b.is_null() { 0 } else { unsafe { (*b).sig_newrsp } } }
#[inline] pub fn set_sig_newrsp(v: u64) { let b = block(); if !b.is_null() { unsafe { (*b).sig_newrsp = v; } } }
#[inline] pub fn set_cur_task(t: u64) { let b = block(); if !b.is_null() { unsafe { (*b).cur_task = t; } } }
#[inline] pub fn cur_task() -> u64 { let b = block(); if b.is_null() { u64::MAX } else { unsafe { (*b).cur_task } } }
/// The root word carries the space's ASID (`mmu::ttbr_of`): the exit path writes it into
/// `TTBR0_EL1` as it is and invalidates only when ASID 0 is on either side.
#[inline] pub fn set_sched_cr3(v: u64) { let b = block(); if !b.is_null() { unsafe { (*b).sched_root = if v == 0 { 0 } else { super::mmu::ttbr_of(v) }; } } }
#[inline] pub fn set_off_stack(t: u64) { let b = block(); if !b.is_null() { unsafe { (*b).off_stack = t; } } }

/// The kernel stack for an EL0 -> EL1 entry on the current core (+8).
#[inline]
pub fn set_kstack_top(v: u64) { let b = block(); if !b.is_null() { unsafe { (*b).kstack_top = v; } } }
#[inline]
pub fn kstack_top() -> u64 { let b = block(); if b.is_null() { 0 } else { unsafe { (*b).kstack_top } } }

/// The FP/SIMD area the current entry saved (0 — none: a kernel context that never entered).
#[inline] pub fn fp_area() -> u64 { let b = block(); if b.is_null() { 0 } else { unsafe { (*b).fp_area } } }
/// The image the next entry into a frame loads (`switch::fpu_restore`).
#[inline] pub fn set_fp_pending(a: u64) { let b = block(); if !b.is_null() { unsafe { (*b).fp_pending = a; } } }
