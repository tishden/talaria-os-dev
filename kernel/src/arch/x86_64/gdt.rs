//! The GDT, the per-core TSS and the entry into ring 3 — the x86-64 machinery `user.rs`
//! used to carry inline (docs/ARM64-PLAN.md §4.1). The tables are `static mut` ARRAYS
//! INDEXED BY CORE: a core touches only its own entry, which is what makes them
//! race-free without a lock (a TSS descriptor cannot be shared anyway — `ltr` sets its
//! busy bit).

use core::mem::size_of;

#[repr(C, packed)]
struct Tss {
    _r0: u32, rsp0: u64, rsp1: u64, rsp2: u64, _r1: u64,
    ist: [u64; 7], _r2: u64, _r3: u16, iomap: u16,
}
// Per-CPU: every core has its own TSS (rsp0 = the kernel stack of its current task) and its own
// GDT (a TSS descriptor cannot be shared — the busy bit at ltr). BSP = 0, AP = the APIC ID.
const MAXCPU: usize = super::percpu::MAXCPU;
const TSS0: Tss = Tss { _r0:0, rsp0:0, rsp1:0, rsp2:0, _r1:0, ist:[0;7], _r2:0, _r3:0, iomap: size_of::<Tss>() as u16 };
static mut TSS: [Tss; MAXCPU] = [TSS0; MAXCPU];
static mut GDT: [[u64; 8]; MAXCPU] = [[0; 8]; MAXCPU];

/// Update TSS.rsp0 of THE CURRENT core (the kernel stack for the next ring3->ring0 entry)
/// + gs:8 for the syscall stub. For the thread scheduler (BSP) and pinned AP tasks.
pub fn set_rsp0(v: u64) {
    let cpu = super::percpu::cpu_id();
    // SAFETY: the entry of THIS core in a per-core array (see the module's note), so no
    // other core writes it. `write_unaligned` through `addr_of_mut!` because `Tss` is
    // `repr(packed)`: a reference to `rsp0` would be a misaligned reference.
    unsafe { core::ptr::write_unaligned(core::ptr::addr_of_mut!(TSS[cpu].rsp0), v); }
    super::percpu::set_kstack_top(v);
}

#[repr(C, packed)]
struct Gdtr { limit: u16, base: u64 }

/// Build the GDT (kcode/kdata/ucode32/udata/ucode64/TSS) and load GDT+TR
/// on THE CURRENT core cpu (a per-CPU TSS: the busy bit does not allow sharing the descriptor).
///
/// # Safety
/// `cpu` must be THIS core's number (`percpu::cpu_id()`) and below `MAXCPU`: the
/// function writes `GDT[cpu]`/`TSS[cpu]` and then loads them into the CURRENT
/// core's GDTR/TR, so passing a foreign number both races with that core and
/// points this one at a table it does not own. `kstack_top` must be the top of a
/// live kernel stack (rsp0 for the next ring3 -> ring0 entry).
pub unsafe fn setup_gdt_cpu(cpu: usize, kstack_top: u64) {
    let g = &mut GDT[cpu];
    g[0] = 0;
    g[1] = 0x00209A0000000000; // 0x08 kernel code64
    g[2] = 0x0000920000000000; // 0x10 kernel data
    g[3] = 0x00CFFA000000FFFF; // 0x18 user code32 (a placeholder for STAR)
    g[4] = 0x00CFF2000000FFFF; // 0x20 user data (DPL3) -> SS 0x23
    g[5] = 0x0020FA0000000000; // 0x28 user code64 (DPL3) -> CS 0x2B
    TSS[cpu].rsp0 = kstack_top;
    super::percpu::set_kstack_top(kstack_top);
    // IST1: the double fault's own stack. A kernel stack overflowed by a PUSH faults on the
    // guard, the #PF cannot push its frame there either, and the #DF that follows needs a stack
    // that is not the dead one — without it the machine triple-faults with no word (ARM64-PLAN
    // §10.16 stage 3's leftover). A window stack (its own guard) when there is one.
    let ist = core::ptr::addr_of_mut!(TSS[cpu].ist).cast::<u64>();
    if core::ptr::read_unaligned(ist) == 0 {
        let top = crate::sched::kstack_from_window()
            .or_else(|| crate::mem::alloc_contig(4).map(|b| crate::mem::kva(b) + 4 * 4096)).unwrap_or(0);
        core::ptr::write_unaligned(ist, top);
        if top != 0 { super::idt::df_on_ist(); }
    }
    let base = crate::mem::vaddr(core::ptr::addr_of!(TSS[cpu]));
    let limit = (size_of::<Tss>() - 1) as u64;
    let low = limit & 0xFFFF
        | (base & 0xFF_FFFF) << 16
        | 0x9u64 << 40           // type: available 64-bit TSS
        | 0x1u64 << 47           // present
        | ((limit >> 16) & 0xF) << 48
        | ((base >> 24) & 0xFF) << 56;
    g[6] = low;                  // the TSS descriptor (16 bytes) in slots 6..8
    g[7] = base >> 32;
    let gdtr = Gdtr { limit: (8 * 8 - 1) as u16, base: crate::mem::vaddr(core::ptr::addr_of!(GDT[cpu])) };
    core::arch::asm!("lgdt [{}]", in(reg) &gdtr, options(readonly, nostack, preserves_flags));
    core::arch::asm!("ltr {0:x}", in(reg) 0x30u16, options(nostack, preserves_flags));
}

/// Into ring 3: an `iretq` with SS = the user data segment, CS = the user code64 segment,
/// the given RSP and RFLAGS (0x2: interrupts off in ring 3 — the kernel's default; 0x202:
/// on). It does not return; the kernel stack it was called on is abandoned (L4 of `user.rs`).
///
/// # Safety
/// `entry` and `ustack` must be mapped in the CURRENT address space with user rights.
pub unsafe fn enter_user(entry: u64, ustack: u64, rflags: u64) -> ! {
    core::arch::asm!(
        "push {ss}", "push {rsp}", "push {flags}", "push {cs}", "push {rip}", "iretq",
        ss = in(reg) 0x23u64, rsp = in(reg) ustack, flags = in(reg) rflags,
        cs = in(reg) 0x2Bu64, rip = in(reg) entry, options(noreturn)
    );
}

/// Switch to the address space `pml4` and enter ring 3 as ONE asm: entry/ustack/pml4 are
/// in registers (taken before the switch from the old stack), and the push of the iret
/// frame goes into the current rsp — mapped in the new AS too (the same stack region).
/// No access to locals after `mov cr3`.
///
/// # Safety
/// As `enter_user`, in the space `pml4`, whose tables must map this kernel stack as well.
pub unsafe fn switch_and_enter_user(pml4: u64, entry: u64, ustack: u64, rflags: u64) -> ! {
    core::arch::asm!(
        "mov cr3, {pml4}",
        "push {ss}", "push {rsp}", "push {flags}", "push {cs}", "push {rip}", "iretq",
        pml4 = in(reg) pml4, ss = in(reg) 0x23u64, rsp = in(reg) ustack, flags = in(reg) rflags,
        cs = in(reg) 0x2Bu64, rip = in(reg) entry, options(noreturn)
    );
}
