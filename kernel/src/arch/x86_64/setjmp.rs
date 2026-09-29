//! `tal_setjmp`/`tal_longjmp` for x86-64: the callee-saved registers, the stack pointer
//! and the return address in a `supervise::JmpBuf` (64 bytes). The recovery of a
//! crashed workload (`supervise.rs`) and the restart of a thread scheduler
//! (`sched.rs`) land here; the declarations that Rust calls through are in
//! `supervise.rs`.

core::arch::global_asm!(r#"
.section .text
.global tal_setjmp
.global tal_longjmp
tal_setjmp:
    mov %rbx, 0(%rdi)
    mov %rbp, 8(%rdi)
    mov %r12, 16(%rdi)
    mov %r13, 24(%rdi)
    mov %r14, 32(%rdi)
    mov %r15, 40(%rdi)
    lea 8(%rsp), %rax
    mov %rax, 48(%rdi)
    mov (%rsp), %rax
    mov %rax, 56(%rdi)
    xor %eax, %eax
    ret
tal_longjmp:
    mov 0(%rdi), %rbx
    mov 8(%rdi), %rbp
    mov 16(%rdi), %r12
    mov 24(%rdi), %r13
    mov 32(%rdi), %r14
    mov 40(%rdi), %r15
    mov 48(%rdi), %rsp
    mov %rsi, %rax
    test %rax, %rax
    jnz 1f
    mov $1, %rax
1:  jmp *56(%rdi)
"#, options(att_syntax));
