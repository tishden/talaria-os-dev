//! The context switch on AArch64: the FP/SIMD state, entering a saved frame, the stack
//! switch of a core leaving a task, the soft-yield kick — and the saved-context API
//! `sched.rs` builds and reads its frames through (the x86 twin is `arch/x86_64/switch.rs`).
//! A saved context IS the entry frame (`trap::Frame`, 36 words), so a fresh task is a
//! frame written at the top of its kernel stack, and entering it is the stubs' restore.
//!
//! The tick's stub (`trap.rs`, `STUB_SCHED` + `SCHED_TAIL`) plays x86's `timer_stub_sched`:
//! it calls `rust_schedule(frame)` and enters whatever frame comes back.
//!
//! # Safety
//! Every function here changes THIS core's machine state (C4 of `sched.rs`); the contracts
//! are the caller's, named at the sites.

use super::trap::Frame;
use core::sync::atomic::Ordering;

/// x86's XSAVE mask; here a constant zero the shared code may read.
pub static XSAVE_MASK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The FP/SIMD state: q0..q31 (512), fpcr, fpsr.
pub const FPU_BYTES: usize = 528;

/// Save the current task's FP/SIMD state into `area`. The source is the state the ENTRY
/// stub saved below its frame (`percpu::fp_area`): what the task had when it entered the
/// kernel, untouched by the kernel's own use of the V registers since. With no entry area
/// (a kernel context that never entered through a stub) the live registers are taken.
/// # Safety
/// C3 + C4: `area` is the CURRENT task's, at least `FPU_BYTES` long, 16-byte aligned.
#[inline]
pub unsafe fn fpu_save(area: *mut u8) {
    let src = super::percpu::fp_area();
    if src != 0 {
        core::ptr::copy_nonoverlapping(src as *const u8, area, FPU_BYTES);
    } else {
        fpu_save_live(area);
    }
}
unsafe fn fpu_save_live(area: *mut u8) {
    core::arch::asm!(
        "stp q0, q1, [{a}, #0]",     "stp q2, q3, [{a}, #32]",    "stp q4, q5, [{a}, #64]",    "stp q6, q7, [{a}, #96]",
        "stp q8, q9, [{a}, #128]",   "stp q10, q11, [{a}, #160]", "stp q12, q13, [{a}, #192]", "stp q14, q15, [{a}, #224]",
        "stp q16, q17, [{a}, #256]", "stp q18, q19, [{a}, #288]", "stp q20, q21, [{a}, #320]", "stp q22, q23, [{a}, #352]",
        "stp q24, q25, [{a}, #384]", "stp q26, q27, [{a}, #416]", "stp q28, q29, [{a}, #448]", "stp q30, q31, [{a}, #480]",
        "mrs {t}, fpcr", "str {t}, [{a}, #512]", "mrs {t}, fpsr", "str {t}, [{a}, #520]",
        a = in(reg) area, t = out(reg) _, options(nostack));
}
/// Restore the FP/SIMD state from `area` into the live registers (the target of a switch:
/// the stub's tail does not touch them when it enters another task's frame).
/// # Safety
/// C3 + C4, as `fpu_save`, and the area must hold a state saved by it.
#[inline]
pub unsafe fn fpu_restore(area: *const u8) {
    core::arch::asm!(
        "ldp q0, q1, [{a}, #0]",     "ldp q2, q3, [{a}, #32]",    "ldp q4, q5, [{a}, #64]",    "ldp q6, q7, [{a}, #96]",
        "ldp q8, q9, [{a}, #128]",   "ldp q10, q11, [{a}, #160]", "ldp q12, q13, [{a}, #192]", "ldp q14, q15, [{a}, #224]",
        "ldp q16, q17, [{a}, #256]", "ldp q18, q19, [{a}, #288]", "ldp q20, q21, [{a}, #320]", "ldp q22, q23, [{a}, #352]",
        "ldp q24, q25, [{a}, #384]", "ldp q26, q27, [{a}, #416]", "ldp q28, q29, [{a}, #448]", "ldp q30, q31, [{a}, #480]",
        "ldr {t}, [{a}, #512]", "msr fpcr, {t}", "ldr {t}, [{a}, #520]", "msr fpsr, {t}",
        a = in(reg) area, t = out(reg) _, options(nostack));
}

/// Enter a saved frame: interrupts masked, sp = the frame, the stubs' restore, `eret`.
/// # Safety
/// C2 + C3: `sp` is a frame this core SAVED (or built) for a task it has claimed. It does
/// not return — the stack it was called on is abandoned.
pub unsafe fn enter_saved_ctx(sp: u64) -> ! {
    core::arch::asm!("msr daifset, #2", "mov sp, {}", "b enter_frame_tail", in(reg) sp, options(noreturn));
}
core::arch::global_asm!(r#"
.section .text
.global enter_frame_tail
enter_frame_tail:
    ldp x2, x3, [sp, #256]
    msr elr_el1, x2
    msr spsr_el1, x3
    and x4, x3, #0xF
    cbnz x4, 1f
    ldr x1, [sp, #248]
    msr sp_el0, x1
1:
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    add sp, sp, #288
    eret
"#);

/// Move onto the stack `top` and call `f` there; it never returns.
/// # Safety
/// `top` must be the top of a live stack nobody else runs on; `f` must not return.
pub unsafe fn stack_switch_call(top: u64, f: extern "C" fn() -> !) -> ! {
    core::arch::asm!("mov sp, {}", "blr {}", in(reg) top, in(reg) f, options(noreturn));
}

/// Raise this core's soft-yield flag and take the tick's path by hand: an `svc` at EL1
/// lands in `sync_el1_entry`, which hands the frame to `rust_schedule` when the scheduler
/// owns the tick (x86: `int 0x41`). The core's number is read with interrupts masked so
/// that a tick cannot move the task between the read and the flag.
/// # Safety
/// C4: this core's registers; `flags` must have `MAXCPU` cache-line entries.
pub unsafe fn soft_yield_kick(flags: *const u64) {
    core::arch::asm!(
        "mrs {d}, daif",
        "msr daifset, #2",
        "mrs {c}, tpidr_el1",
        "ldr {c}, [{c}, #24]",
        "lsl {c}, {c}, #6",
        "add {c}, {c}, {f}",
        "mov {o}, #1",
        "str {o}, [{c}]",
        "svc #1",
        "msr daif, {d}",
        f = in(reg) flags, c = out(reg) _, o = out(reg) _, d = out(reg) _,
    );
}

// ── The saved context as `sched.rs` builds and reads it ──────────────────────────

pub const CTX_WORDS: usize = 36;
/// The general-purpose registers a context is built from: x0..x30.
pub type Gp = [u64; 31];
pub const GP_ZERO: Gp = [0; 31];
/// A register file with the first argument set (x0).
pub fn gp_with_arg(arg: u64) -> Gp { let mut g = GP_ZERO; g[0] = arg; g }
/// A register file with the syscall's return value set (x0) — the same register here.
pub fn gp_with_ret(v: u64) -> Gp { gp_with_arg(v) }
/// PSTATE for a fresh EL0 context: EL0t with everything unmasked / with IRQs masked.
pub const USER_FLAGS_IRQ_ON: u64 = 0x0;
pub const USER_FLAGS_IRQ_OFF: u64 = 0x80;
/// PSTATE for a kernel context: EL1h, unmasked.
const KERNEL_FLAGS: u64 = 0x5;
/// The flags word of an interrupted EL0 context, fit for a new one: the condition flags
/// survive, the mode is EL0t and nothing is masked.
pub fn user_flags(f: u64) -> u64 { f & 0xF000_0000 }
/// Are IRQs enabled in this PSTATE (the I bit clear)?
pub fn flags_irq_on(f: u64) -> bool { f & 0x80 == 0 }

#[inline(always)]
fn frame_at(sp: u64) -> *mut Frame { sp as *mut Frame }

/// Build an EL0 context at the top of `kstack_top`; the sp (the frame) to enter it with.
/// # Safety
/// C3: `kstack_top` is the top of a kernel stack nobody runs on.
pub unsafe fn build_user_ctx(kstack_top: u64, pc: u64, usp: u64, flags: u64, gp: &Gp) -> u64 {
    let sp = (kstack_top & !0xF) - CTX_WORDS as u64 * 8;
    let f = frame_at(sp);
    core::ptr::write_bytes(f as *mut u8, 0, CTX_WORDS * 8);
    for (i, &v) in gp.iter().enumerate() { (*f).set_x(i, v); }
    (*f).usp = usp; (*f).pc = pc; (*f).flags = flags;
    sp
}
/// Build an EL1 context: `entry` on `stack_top` (16-aligned), interrupts on.
/// # Safety
/// As `build_user_ctx`.
pub unsafe fn build_kernel_ctx(stack_top: u64, entry: extern "C" fn() -> !) -> u64 {
    let sp = (stack_top & !0xF) - CTX_WORDS as u64 * 8;
    let f = frame_at(sp);
    core::ptr::write_bytes(f as *mut u8, 0, CTX_WORDS * 8);
    (*f).usp = stack_top & !0xF; (*f).pc = entry as usize as u64; (*f).flags = KERNEL_FLAGS;
    sp
}
#[inline(always)]
fn rd(sp: u64, off: usize) -> u64 { unsafe { core::ptr::read_volatile((sp as *const u64).add(off)) } }
pub fn ctx_pc(sp: u64) -> u64 { rd(sp, 32) }
pub fn ctx_sp(sp: u64) -> u64 { rd(sp, 31) }
pub fn ctx_fp(sp: u64) -> u64 { rd(sp, 29) }
pub fn ctx_flags(sp: u64) -> u64 { rd(sp, 33) }
/// The mode word is the SPSR; `mode_is_user` reads M[3:0] == 0 (EL0t).
pub fn ctx_mode(sp: u64) -> u64 { rd(sp, 33) }
pub fn mode_is_user(mode: u64) -> bool { mode & 0xF == 0 }
pub fn ctx_is_user(sp: u64) -> bool { mode_is_user(ctx_mode(sp)) }
/// Does what lies at `sp` look like a frame: EL0t or EL1h, AArch64 (M[4] clear), no
/// reserved PSTATE bits above the condition flags.
pub fn ctx_plausible(sp: u64) -> bool {
    let m = ctx_mode(sp);
    (m & 0xF == 0 || m & 0xF == 5) && m & 0x10 == 0 && m & 0x0FFF_F000 == 0
}
pub fn ctx_words(sp: u64) -> [u64; CTX_WORDS] {
    let mut c = [0u64; CTX_WORDS];
    for (k, w) in c.iter_mut().enumerate() { *w = rd(sp, k); }
    c
}
/// Where a thread stood in user space, from its frame — for a core dump
/// (pc, sp, the frame pointer, x19..x23 in the callee-saved slots, pstate, no syscall number).
pub fn ctx_exit_uregs(sp: u64) -> [u64; 10] {
    [rd(sp, 32), rd(sp, 31), rd(sp, 29), rd(sp, 19), rd(sp, 20), rd(sp, 21), rd(sp, 22), rd(sp, 23), rd(sp, 33), u64::MAX]
}
/// Rewrite a user context so that its next entry lands in the kernel at `f`, IRQs masked,
/// at EL1h: the stack is SP_EL1 as the restore leaves it (the frame's own top), so `_ksp`
/// is not needed here.
/// # Safety
/// C3: `sp` is a saved frame of a task this core owns.
pub unsafe fn ctx_park(sp: u64, f: extern "C" fn() -> !, _ksp: u64) {
    let fr = frame_at(sp);
    (*fr).pc = f as usize as u64;
    (*fr).flags = 0x3C5;
}
#[allow(dead_code)]
fn _ordering_used() -> Ordering { Ordering::Relaxed }

/// The address `sched::install` puts on the tick's vector (x86: the asm stub of that name).
/// Here it is a marker: `idt::SCHED_TIMER != 0` is what the IRQ vector asks before it hands
/// the tick to `rust_schedule`; the stubs of `trap.rs` are the tick's code.
#[unsafe(no_mangle)]
pub extern "C" fn timer_stub_sched() {}
