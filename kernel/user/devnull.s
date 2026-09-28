.section .text
.global _start
_start:
    # fd = open("/dev/null", O_RDWR=2)
    mov $2, %rax
    lea path(%rip), %rdi
    mov $2, %rsi
    xor %rdx, %rdx
    syscall
    mov %rax, %r12          # fd
    test %rax, %rax
    js fail                 # fd < 0 -> fail
    # write(fd, wmsg, wlen) -> we expect wlen (discarded)
    mov $1, %rax
    mov %r12, %rdi
    lea wmsg(%rip), %rsi
    mov $wlen, %rdx
    syscall
    cmp $wlen, %rax
    jne fail
    # read(fd, buf, 16) -> we expect 0 (EOF)
    mov $0, %rax
    mov %r12, %rdi
    lea buf(%rip), %rsi
    mov $16, %rdx
    syscall
    test %rax, %rax
    jnz fail
    # close(fd)
    mov $3, %rax
    mov %r12, %rdi
    syscall
    mov $1, %rax
    mov $1, %rdi
    lea okmsg(%rip), %rsi
    mov $oklen, %rdx
    syscall
    jmp done
fail:
    mov $1, %rax
    mov $1, %rdi
    lea failmsg(%rip), %rsi
    mov $faillen, %rdx
    syscall
done:
    mov $60, %rax
    xor %rdi, %rdi
    syscall
path:
    .asciz "/dev/null"
wmsg:
    .ascii "discard-me"
wlen = . - wmsg
okmsg:
    .ascii "DEVNULL: /dev/null OK (the write is discarded, read=EOF, close)\n"
oklen = . - okmsg
failmsg:
    .ascii "DEVNULL: FAIL\n"
faillen = . - failmsg
.section .bss
buf:
    .skip 16
