//! `tal_setjmp`/`tal_longjmp` for AArch64: the callee-saved x19..x28, the frame pointer,
//! the link register, the stack pointer and the callee-saved halves of d8..d15 — 21 words
//! in a `supervise::JmpBuf` (`JMPBUF_WORDS`). The declarations Rust calls through are in
//! `supervise.rs`, as on x86.

/// The words a jump buffer holds on this machine.
pub const JMPBUF_WORDS: usize = 21;

core::arch::global_asm!(r#"
.section .text
.global tal_setjmp
.global tal_longjmp
tal_setjmp:
    stp x19, x20, [x0, #0]
    stp x21, x22, [x0, #16]
    stp x23, x24, [x0, #32]
    stp x25, x26, [x0, #48]
    stp x27, x28, [x0, #64]
    stp x29, x30, [x0, #80]
    mov x9, sp
    str x9, [x0, #96]
    stp d8, d9, [x0, #104]
    stp d10, d11, [x0, #120]
    stp d12, d13, [x0, #136]
    stp d14, d15, [x0, #152]
    mov x0, #0
    ret
tal_longjmp:
    ldp x19, x20, [x0, #0]
    ldp x21, x22, [x0, #16]
    ldp x23, x24, [x0, #32]
    ldp x25, x26, [x0, #48]
    ldp x27, x28, [x0, #64]
    ldp x29, x30, [x0, #80]
    ldr x9, [x0, #96]
    mov sp, x9
    ldp d8, d9, [x0, #104]
    ldp d10, d11, [x0, #120]
    ldp d12, d13, [x0, #136]
    ldp d14, d15, [x0, #152]
    cmp x1, #0
    csinc x0, x1, xzr, ne
    ret
"#);
