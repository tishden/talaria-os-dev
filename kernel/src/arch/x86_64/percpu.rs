//! Per-CPU data through GS_BASE (SMP threads per core).
//! GS.base on every core ALWAYS points to its PerCpu (set when the core is initialised
//! and never changed) — without swapgs: interrupts and syscalls read %gs:off
//! at any moment. The invariant: user code does not touch GS (arch_prctl ARCH_SET_GS ->
//! EINVAL; a `mov %gs` of a selector in ring 3 will break the current thread — documented).
use core::sync::atomic::{AtomicU64, Ordering};

/// The ceiling on LOGICAL cores. 64 is not "with a margin" but exactly the width of the
/// `ONLINE` mask (u64) and of the `sched_getaffinity` mask the workload sees. The cost in
/// .bss: the AP pickup loops' stacks (16 KB × MAXCPU) — 1 MB, the rest is pennies.
/// It used to be 4: on EC2 guests nothing larger came up anyway, while on `c6i.metal` MADT reports
/// 64 cores, and hitting the ceiling in the middle of a bring-up means silently losing the machine.
pub const MAXCPU: usize = 64;

/// The field offsets are used verbatim in the asm stubs (syscall_entry):
///   gs:0  = ursp (scratch: the user rsp on syscall entry)
///   gs:8  = kstack_top (the kernel stack of this core's current task)
///   gs:16 = sig_newrsp (substituting the user rsp when delivering a signal / on sigreturn)
///   gs:24 = cpu (the logical number, dense 0..N-1 — NOT the APIC id, see smp.rs)
///   gs:32 = cur_task (the index of this core's scheduler task; u64::MAX — the core is idle)
///   gs:40 = sched_cr3 (the CR3 of the task THIS core is switching to)
///   gs:48 = pv_eoi (the KVM PV EOI word: the host sets bit 0 if the EOI need NOT be written)
///   gs:56 = off_stack (the task whose kernel stack this core is leaving in the switch stub; u64::MAX — none)
#[repr(C, align(64))]
pub struct PerCpu {
    pub ursp: u64,
    pub kstack_top: u64,
    pub sig_newrsp: u64,
    pub cpu: u64,
    pub cur_task: u64,
    pub sched_cr3: u64,
    /// The KVM PV EOI protocol word (`smp::pveoi_*`). The host raises bit 0 before
    /// entering the guest if the interrupt acknowledgement need not be written to the LAPIC;
    /// the handler clears the bit and avoids the single expensive access to
    /// the APIC window. It lives HERE rather than in a separate table so that the handler
    /// fetches it with one gs-relative instruction — without the core number.
    pub pv_eoi: u64,
    /// The task released by this core's scheduler but on whose kernel stack
    /// the core still stands: the tail of `timer_stub_sched` clears its `sched::STACK_BUSY`
    /// right AFTER `mov %rax,%rsp` and puts u64::MAX here (see `sched::stack_wait`).
    pub off_stack: u64,
}

static mut CPUS: [PerCpu; MAXCPU] = {
    const Z: PerCpu = PerCpu { ursp: 0, kstack_top: 0, sig_newrsp: 0, cpu: 0, cur_task: u64::MAX, sched_cr3: 0, pv_eoi: 0, off_stack: u64::MAX };
    [Z; MAXCPU]
};

/// Which task CORE cpu is executing (u64::MAX — idle). For diagnosing
/// hangs: "everyone is waiting" and "everyone is waiting except one that does not get
/// the CPU" are different illnesses, and it is exactly this table that tells them apart.
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

/// The live cores under the sync scheduler (a bitmask, bit0 = BSP).
pub static ONLINE: AtomicU64 = AtomicU64::new(1);

const MSR_GS_BASE: u32 = 0xC000_0101;

/// Only GS.base -> this core's PerCpu. Separate from `init`, because it is needed
/// EARLIER: interrupt handlers read per-CPU data gs-relative (the core
/// number is gs:24, the PV EOI word is gs:48), and before this call GS.base=0, i.e.
/// the very first interrupt would go to a physical address in the zero page. Whereas
/// entering the ONLINE mask is a decision of ANOTHER level: the cell's core (the datapath)
/// deliberately does not enter it, nobody schedules or wakes it.
pub fn init_gs(cpu: usize) {
    if cpu >= MAXCPU { return; }
    unsafe {
        CPUS[cpu].cpu = cpu as u64;
        let base = crate::mem::vaddr(core::ptr::addr_of!(CPUS[cpu]));
        core::arch::asm!("wrmsr", in("ecx") MSR_GS_BASE, in("eax") base as u32, in("edx") (base >> 32) as u32, options(nostack, preserves_flags));
    }
}

/// Initialise the current core's per-CPU area (the BSP in kmain, an AP in ap_entry).
pub fn init(cpu: usize) {
    init_gs(cpu);
    ONLINE.fetch_or(1 << cpu, Ordering::SeqCst);
}

/// The physical address of core `cpu`'s PV EOI word (the kernel is identity-mapped).
pub fn pv_eoi_addr(cpu: usize) -> u64 {
    if cpu >= MAXCPU { return 0; }
    unsafe { crate::mem::Pa::of(core::ptr::addr_of!(CPUS[cpu].pv_eoi)).get() }
}

/// The current core's number (gs:24). Before init() GS.base=0 — so calling it early is inadmissible.
#[inline]
pub fn cpu_id() -> usize {
    let c: u64;
    unsafe { core::arch::asm!("mov {}, gs:[24]", out(reg) c, options(nostack, preserves_flags, readonly)); }
    c as usize
}

/// The kernel stack for a ring3->ring0 entry on the current core (the syscall stub: gs:8).
#[inline]
pub fn set_kstack_top(v: u64) {
    unsafe { core::arch::asm!("mov gs:[8], {}", in(reg) v, options(nostack, preserves_flags)); }
}

/// Substituting the user rsp on syscall exit (delivering a signal / sigreturn): gs:16.
/// Read the pending user-rsp substitution (gs:16). The value belongs to THE TASK
/// rather than to the core: the scheduler must save and restore it on a
/// switch (see sched::SIGRSP) — otherwise a neighbouring task would take it.
#[inline]
pub fn sig_newrsp() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mov {}, gs:[16]", out(reg) v, options(nostack, preserves_flags, readonly)); }
    v
}
#[inline]
pub fn set_sig_newrsp(v: u64) {
    unsafe { core::arch::asm!("mov gs:[16], {}", in(reg) v, options(nostack, preserves_flags)); }
}

/// This core's current scheduler task (gs:32); u64::MAX — none.
#[inline]
pub fn set_cur_task(t: u64) {
    unsafe { core::arch::asm!("mov gs:[32], {}", in(reg) t, options(nostack, preserves_flags)); }
}
#[inline]
pub fn cur_task() -> u64 {
    let t: u64;
    unsafe { core::arch::asm!("mov {}, gs:[32]", out(reg) t, options(nostack, preserves_flags, readonly)); }
    t
}

/// The CR3 of the task THIS core is switching into (gs:40). It is read by the tail of
/// `timer_stub_sched` already AFTER the move onto the target's kernel stack. It used to be the global
/// variable `SCHED_NEXT_CR3` — while the scheduler lived only on the BSP that
/// worked; with a scheduler on EVERY core of the shared pool two cores switching
/// at once would overwrite each other's target address space. The same
/// class as the logical number against the APIC id: one place for everybody lives
/// exactly until the second owner appears.
#[inline]
pub fn set_sched_cr3(v: u64) {
    unsafe { core::arch::asm!("mov gs:[40], {}", in(reg) v, options(nostack, preserves_flags)); }
}
/// The task whose kernel stack THIS core will leave in the switch stub (gs:56).
#[inline]
pub fn set_off_stack(t: u64) {
    unsafe { core::arch::asm!("mov gs:[56], {}", in(reg) t, options(nostack, preserves_flags)); }
}
