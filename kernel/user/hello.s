.section .text
.global _start
_start:
    mov $1, %rax
    mov $1, %rdi
    lea msg(%rip), %rsi
    mov $msglen, %rdx
    syscall
    mov $60, %rax
    mov $7, %rdi
    syscall
msg:
    .ascii "Hello from Talaria's userspace (ring 3, a Linux syscall)!\n"
msglen = . - msg
