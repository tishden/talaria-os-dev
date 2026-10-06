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
/// Is the saved image fit to be restored? `mxcsr` with a reserved bit set makes
/// `fxrstor`/`xrstor` raise #GP in ring 0, and a misaligned area likewise — `Err` carries
/// the word for the diagnosis.
#[inline]
pub fn fpu_image_check(area: *const u8) -> Result<(), u32> {
    // SAFETY: the caller hands a task's FPU slot — at least 512 bytes of it.
    let mxcsr = unsafe { core::ptr::read_volatile(area.add(24) as *const u32) };
    if mxcsr & 0xFFFF_0000 != 0 || crate::mem::vaddr(area) & 0xF != 0 { Err(mxcsr) } else { Ok(()) }
}
/// How much of an entry's FP area a forked task inherits: the legacy fxsave area.
pub const FPU_INHERIT: usize = 512;

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

// ── The saved context as `sched.rs` builds and reads it ──────────────────────────
// A task off a core is a frame on its kernel stack: the fifteen general-purpose registers
// `timer_stub_sched` pushed (r15 at the lowest address, rax at +14*8) and the hardware
// iretq frame above them (rip, cs, rflags, rsp, ss). The scheduler never spells the
// layout: it asks this API, and the arm64 twin answers the same questions about the
// trap frame (docs/ARM64-PLAN.md §10.1, stage 3.2).

/// The words a saved context occupies.
pub const CTX_WORDS: usize = 20;
/// The general-purpose registers a context is built from, in the `fork_regs` order:
/// rax, rbx, rcx, rdx, rsi, rdi, rbp, r8..r15.
pub type Gp = [u64; 15];
pub const GP_ZERO: Gp = [0; 15];
/// A register file with the first argument set (rdi): a fresh ring-3 thread's `arg`.
pub fn gp_with_arg(arg: u64) -> Gp { let mut g = GP_ZERO; g[5] = arg; g }
/// A register file with the syscall's return value set (rax): a fork child's 0.
pub fn gp_with_ret(v: u64) -> Gp { let mut g = GP_ZERO; g[0] = v; g }
/// The flags a fresh ring-3 context starts with: interrupts on (the tick preempts) / off.
pub const USER_FLAGS_IRQ_ON: u64 = 0x202;
pub const USER_FLAGS_IRQ_OFF: u64 = 0x2;
/// The flags word of an interrupted user context, made fit for a new context of it.
pub fn user_flags(f: u64) -> u64 { f | 0x2 }
/// Does the flags word have interrupts enabled (IF)?
pub fn flags_irq_on(f: u64) -> bool { f & 0x200 != 0 }

/// Build a ring-3 context at the top of `kstack_top`; the sp to enter it with.
/// # Safety
/// C3: `kstack_top` is the top of a kernel stack nobody runs on.
pub unsafe fn build_user_ctx(kstack_top: u64, pc: u64, usp: u64, flags: u64, gp: &Gp) -> u64 {
    let mut sp = kstack_top & !0xF;
    let push = |sp: &mut u64, v: u64| { *sp -= 8; core::ptr::write(crate::mem::vptr::<u64>(*sp), v); };
    push(&mut sp, 0x23); push(&mut sp, usp); push(&mut sp, flags); push(&mut sp, 0x2B); push(&mut sp, pc);
    for k in 0..15 { push(&mut sp, gp[k]); } // rax is pushed first (popped last)
    sp
}
/// Build a ring-0 context: `entry` on `stack_top`, as after a call (rsp = 8 mod 16, or the
/// compiler's aligned SSE spills fault), interrupts on.
/// # Safety
/// As `build_user_ctx`.
pub unsafe fn build_kernel_ctx(stack_top: u64, entry: extern "C" fn() -> !) -> u64 {
    let mut sp = stack_top & !0xF;
    let push = |sp: &mut u64, v: u64| { *sp -= 8; core::ptr::write(crate::mem::vptr::<u64>(*sp), v); };
    push(&mut sp, 0x10); push(&mut sp, (stack_top & !0xF) - 8); push(&mut sp, 0x202); push(&mut sp, 0x08);
    push(&mut sp, entry as usize as u64);
    for _ in 0..15 { push(&mut sp, 0); }
    sp
}
#[inline(always)]
fn slot(sp: u64, k: u64) -> u64 { unsafe { core::ptr::read_volatile(crate::mem::vptr::<u64>(sp + k * 8)) } }
/// The program counter, the user stack pointer, the frame pointer and the flags of a context.
pub fn ctx_pc(sp: u64) -> u64 { slot(sp, 15) }
pub fn ctx_sp(sp: u64) -> u64 { slot(sp, 18) }
pub fn ctx_fp(sp: u64) -> u64 { slot(sp, 8) }
pub fn ctx_flags(sp: u64) -> u64 { slot(sp, 17) }
/// The mode word (cs here, spsr on arm64) — for the log lines; `mode_is_user` decodes it.
pub fn ctx_mode(sp: u64) -> u64 { slot(sp, 16) }
pub fn mode_is_user(mode: u64) -> bool { mode & 3 == 3 }
pub fn ctx_is_user(sp: u64) -> bool { mode_is_user(ctx_mode(sp)) }
/// Does what lies at `sp` look like a frame at all (the sanity check `ctx_enter` makes)?
pub fn ctx_plausible(sp: u64) -> bool {
    let (cs, rfl) = (ctx_mode(sp), ctx_flags(sp));
    (cs == 0x8 || cs == 0x2B) && rfl & 2 != 0 && rfl >> 22 == 0
}
/// The whole frame, word by word (the debugger's view).
pub fn ctx_words(sp: u64) -> [u64; CTX_WORDS] {
    let mut c = [0u64; CTX_WORDS];
    for (k, w) in c.iter_mut().enumerate() { *w = slot(sp, k as u64); }
    c
}
/// Where a thread stood in user space, from its frame — for a core dump
/// (`sched::set_exit_uregs`: rip, rsp, rbp, rbx, r12..r15, rflags, no syscall number).
pub fn ctx_exit_uregs(sp: u64) -> [u64; 10] {
    [slot(sp, 15), slot(sp, 18), slot(sp, 8), slot(sp, 13), slot(sp, 3), slot(sp, 2), slot(sp, 1), slot(sp, 0), slot(sp, 17), u64::MAX]
}
/// Rewrite a user context so that its next entry lands in the kernel at `f` on the stack
/// `ksp`, interrupts off (a dead thread parked out of its own code).
/// # Safety
/// C3: `sp` is a saved frame of a task this core owns.
pub unsafe fn ctx_park(sp: u64, f: extern "C" fn() -> !, ksp: u64) {
    let w = |k: u64, v: u64| core::ptr::write_volatile(crate::mem::vptr::<u64>(sp + k * 8), v);
    w(15, f as usize as u64); w(16, 0x08); w(17, 0x2); w(18, ksp); w(19, 0x10);
}
