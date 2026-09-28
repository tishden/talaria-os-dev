.section .text
.global _start
_start:
    # write(1, msg_start) — ALLOWED
    mov $1, %rax
    mov $1, %rdi
    lea msg_start(%rip), %rsi
    mov $len_start, %rdx
    syscall
    # getuid (102) — FORBIDDEN by the filter -> we expect rax = -1 (EPERM)
    mov $102, %rax
    syscall
    cmp $-1, %rax
    jne fail
pass:
    mov $1, %rax
    mov $1, %rdi
    lea msg_pass(%rip), %rsi
    mov $len_pass, %rdx
    syscall
    jmp done
fail:
    mov $1, %rax
    mov $1, %rdi
    lea msg_fail(%rip), %rsi
    mov $len_fail, %rdx
    syscall
done:
    mov $60, %rax          # exit(0) — ALLOWED
    xor %rdi, %rdi
    syscall
msg_start:
    .ascii "sfilter: the start (getuid must be forbidden)\n"
len_start = . - msg_start
msg_pass:
    .ascii "sfilter: PASS (a forbidden syscall -> EPERM)\n"
len_pass = . - msg_pass
msg_fail:
    .ascii "sfilter: FAIL (a forbidden syscall went through)\n"
len_fail = . - msg_fail
