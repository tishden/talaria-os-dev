//! The context switch of x86-64, as `sched.rs` used to carry it inline: the FPU state
//! (`fxsave`/`xsave` by the mask AVX left), entering a saved context (fifteen registers
//! and an `iretq` frame), the tick's entry stub `timer_stub_sched`, the soft-yield kick
//! and the stack switch of an AP leaving a task (docs/ARM64-PLAN.md §4.1). The POLICY —
//! whom a core takes next, what a migration costs — stays in `sched.rs`; this file is
//! the mechanics of registers.
//!
//! # Safety
//!
//! Every function here changes THIS core's machine state (C4 of `sched.rs`) and most do
//! not return; the contracts are the caller's, named at the sites.

use core::sync::atomic::Ordering;

/// The XCR0 mask for XSAVE/XRSTOR (0 — XSAVE is unavailable, we work with fxsave).
pub static XSAVE_MASK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Save the current core's FPU state into the task's area.
///
/// # Safety
/// C3 + C4: `area` must be the CURRENT task's FPU area — at least the size the active
/// `XSAVE_MASK` requires and 64-byte aligned, as `fxsave`/`xsave` demand. A wrong area
/// silently mixes two threads' floating point.
#[inline]
pub unsafe fn fpu_save(area: *mut u8) {
    let m = XSAVE_MASK.load(Ordering::Relaxed);
    if m != 0 {
        core::arch::asm!("xsave [{a}]", a = in(reg) area, in("eax") m as u32, in("edx") (m >> 32) as u32, options(nostack));
    } else {
        core::arch::asm!("fxsave [{}]", in(reg) area, options(nostack));
    }
}
/// Restore the FPU state from the task's area.
///
/// # Safety
/// C3 + C4, as `fpu_save`, and the area must hold a state SAVED by it (a zeroed area
/// restores a legal but foreign state).
#[inline]
pub unsafe fn fpu_restore(area: *const u8) {
    let m = XSAVE_MASK.load(Ordering::Relaxed);
    if m != 0 {
        core::arch::asm!("xrstor [{a}]", a = in(reg) area, in("eax") m as u32, in("edx") (m >> 32) as u32, options(nostack));
    } else {
        core::arch::asm!("fxrstor [{}]", in(reg) area, options(nostack));
    }
}

/// Enter a saved context (like the tail of `timer_stub_sched`): 15 GP + an iretq.
/// `cli` first — the entry must be atomic with respect to the ticks/IPIs; the task's IF is
/// restored by the iretq from the frame.
///
/// # Safety
/// C2 + C3: `sp` must point at a frame this core SAVED for a task it has claimed (15
/// general-purpose registers plus the iretq frame). Entering an unclaimed context means
/// two cores in one thread; entering a frame that is not one means an iretq into
/// garbage. It does not return — the stack it was called on is abandoned.
pub unsafe fn enter_saved_ctx(sp: u64) -> ! {
    core::arch::asm!(
        "cli",
        "mov rsp, {}",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx", "pop rax",
        "iretq",
        in(reg) sp, options(noreturn)
    );
}

/// Move onto the stack `top` and call `f` there; it never returns.
///
/// # Safety
/// `top` must be the top of a live stack nobody else runs on; `f` must not return.
pub unsafe fn stack_switch_call(top: u64, f: extern "C" fn() -> !) -> ! {
    core::arch::asm!("mov rsp, {}", "call {}", in(reg) top, in(reg) f, options(noreturn));
}

/// Raise this core's soft-yield flag and take the timer's vector by hand (`int 0x41`).
/// The core's number is taken INSIDE the same block (gs:24) — between a read from the
/// outside and the `cli` a tick would manage to preempt the task, and the flag would
/// land on the wrong core. `flags` is the table of per-core flags, 64 bytes apart.
///
/// # Safety
/// C4: this core's registers; `flags` must have `MAXCPU` cache-line entries.
pub unsafe fn soft_yield_kick(flags: *const u64) {
    core::arch::asm!(
        "pushfq",
        "cli",
        "mov {c}, gs:[24]",      // the core's logical number
        "shl {c}, 6",            // the step of a SOFT_YIELD element is 64 bytes (a cache line)
        "add {c}, {f}",
        "mov qword ptr [{c}], 1",
        "int 0x41",              // TIMER_VECTOR
        "popfq",
        f = in(reg) flags,
        c = out(reg) _,
    );
}

core::arch::global_asm!(r#"
.section .text
.global timer_stub_sched
timer_stub_sched:
    push %rax
    push %rbx
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %rbp
    push %r8
    push %r9
    push %r10
    push %r11
    push %r12
    push %r13
    push %r14
    push %r15
    mov %rsp, %rdi          /* cur_rsp -> rust_schedule */
    call rust_schedule      /* rax = next rsp */
    mov %rax, %rsp          /* we have moved onto the target's kernel stack (in the common GB0) */
    mov %gs:56, %rcx        /* the task whose stack we have just left (MAX = none) */
    cmp $-1, %rcx
    je 3f
    lea STACK_BUSY(%rip), %rdx
    movb $0, (%rdx,%rcx)    /* the stack is free — the taking core may enter (see stack_wait) */
    movq $-1, %gs:56
3:
    mov %gs:40, %rcx        /* the CR3 of THIS core's target (0 = do not switch) */
    test %rcx, %rcx
    jz 2f
    mov %rcx, %cr3          /* switch the address space here — the stack is already a common one */
2:
    pop %r15
    pop %r14
    pop %r13
    pop %r12
    pop %r11
    pop %r10
    pop %r9
    pop %r8
    pop %rbp
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rcx
    pop %rbx
    pop %rax
    iretq
"#, options(att_syntax));
