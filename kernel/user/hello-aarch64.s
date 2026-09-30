// The arm64 twin of hello.s: a write(1, msg) and an exit(7) through the Linux AArch64
// syscall ABI — the number in x8, the arguments in x0..x2, `svc #0` (docs/ARM64-PLAN.md §4.4).
.section .text
.global _start
_start:
    mov x8, #64                 // write
    mov x0, #1
    adr x1, msg
    mov x2, #msglen
    svc #0
    mov x8, #93                 // exit
    mov x0, #7
    svc #0
msg:
    .ascii "Hello from Talaria's userspace (EL0, a Linux syscall)!\n"
msglen = . - msg
