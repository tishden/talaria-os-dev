//! The SYSCALL entry of x86-64 (docs/ARM64-PLAN.md §4.5): the MSRs that arm it, the
//! stub that saves the user's registers on the task's kernel stack and calls
//! `rust_syscall(&Frame)`, and the `iretq` that a signal's return needs where `sysretq`
//! cannot restore rcx/r11. The frame's layout is `syscall::Frame`; the per-CPU
//! offsets are `percpu.rs`'s.
//!
//! # Safety
//!
//! S4 of `syscall.rs`: THIS core's MSRs and registers, armed before it ever enters ring 3.

use super::cpu::{rdmsr, wrmsr};

const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_FMASK: u32 = 0xC000_0084;
const MSR_EFER: u32 = 0xC000_0080;

/// Set up SYSCALL on this core: STAR (the selectors), LSTAR (the handler), FMASK, EFER.SCE.
///
/// # Safety
/// S4: once per core, before it enters ring 3.
pub unsafe fn init_msrs() {
    unsafe extern "C" { fn syscall_entry(); }
    // STAR: [47:32]=the kernel CS (SS=+8); [63:48]=the base for the sysret user (CS=+16, SS=+8)
    // GDT (gdt.rs): 0x08 kcode, 0x10 kdata, 0x18 ucode32, 0x20 udata, 0x28 ucode64
    // sysret: user CS = 0x18+16=0x28 (|3), SS = 0x18+8=0x20 (|3). STAR[63:48]=0x18|3=0x1B
    let star: u64 = (0x1Bu64 << 48) | (0x08u64 << 32);
    wrmsr(MSR_STAR, star);
    wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);
    wrmsr(MSR_FMASK, 0x200); // clear IF at the entry into a syscall
    wrmsr(MSR_EFER, rdmsr(MSR_EFER) | 1); // SCE
}

/// Leave the kernel with a FULL register set and a hardware iret frame taken from `regs`
/// (rax..r15 in the order below, then the iretq frame) — exactly as Linux does on
/// `rt_sigreturn`, where `sysretq` cannot restore rcx/r11.
///
/// # Safety
/// `regs` must point at 15 registers followed by a valid iretq frame for ring 3.
pub unsafe extern "C" fn sig_iret(regs: *const u64) -> ! {
    core::arch::asm!(
        "mov rsp, rdi",
        "pop rax", "pop rbx", "pop rcx", "pop rdx", "pop rsi", "pop rdi", "pop rbp",
        "pop r8", "pop r9", "pop r10", "pop r11", "pop r12", "pop r13", "pop r14", "pop r15",
        "iretq",
        in("rdi") regs, options(noreturn)
    );
}

core::arch::global_asm!(r#"
.section .text
.global syscall_entry
syscall_entry:
    /* syscall: nr=rax, args=rdi,rsi,rdx,r10,r8,r9; rcx=ret rip, r11=ret rflags. IF=0 (FMASK).
       Per-CPU through GS.base (percpu.rs, always the kernel's): gs:0=the ursp scratch, gs:8=kstack_top
       (= this core's TSS.rsp0), gs:16=sig_newrsp. We switch to the task's kernel stack. */
    mov %rsp, %gs:0
    mov %gs:8, %rsp
    pushq %gs:0                 /* Frame.ursp */
    push %r15           /* callee-saved: it is inherited by a fork child / a signal's frame */
    push %r14
    push %r13
    push %r12
    push %rbp
    push %rbx           /* save the callee-saved rbx (we use it for the align) */
    push %r11           /* ret rflags */
    push %rcx           /* ret rip */
    push %r9            /* a6 */
    push %r8            /* a5 */
    push %r10           /* a4 */
    push %rdx           /* a3 */
    push %rsi           /* a2 */
    push %rdi           /* a1 */
    push %rax           /* nr  -> the top = &Frame */
    mov %rsp, %rdi      /* rust_syscall(&Frame) */
    mov %rsp, %rbx      /* save the frame's pointer */
    sub $512, %rsp
    and $-16, %rsp      /* the fxsave area (16-aligned) = (rbx-512)&~15 */
    fxsave (%rsp)       /* the user's SSE/x87 state: the kernel is built with SSE and clobbers xmm */
    mov %rsp, %r12
    call rust_syscall
    fxrstor (%r12)      /* restore (or the state substituted by sigreturn/the signal delivery) */
    mov %rbx, %rsp      /* restore the frame; rax = the return value */
    add $8, %rsp        /* discard the nr */
    pop %rdi
    pop %rsi
    pop %rdx
    pop %r10
    pop %r8
    pop %r9
    pop %rcx            /* ret rip */
    pop %r11            /* ret rflags */
    pop %rbx
    pop %rbp
    pop %r12
    pop %r13
    pop %r14
    pop %r15
    cmpq $0, %gs:16             /* the signal's delivery / sigreturn: substitute the user rsp */
    je 1f
    mov %gs:16, %rsp
    movq $0, %gs:16
    jmp 2f
1:  pop %rsp            /* back onto the user stack (Frame.ursp) */
2:  sysretq
"#, options(att_syntax));
