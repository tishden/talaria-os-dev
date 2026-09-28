.section .text
.global _start
_start:
    # p = mmap(NULL, 4096, PROT_READ|WRITE=3, MAP_PRIVATE|ANON=0x22, -1, 0)
    mov $9, %rax
    xor %rdi, %rdi
    mov $4096, %rsi
    mov $3, %rdx
    mov $0x22, %r10
    mov $-1, %r8
    xor %r9, %r9
    syscall
    mov %rax, %r12          # p
    movq $0x1234, (%r12)    # a write (ok, it is mapped)
    # munmap(p, 4096)
    mov $11, %rax
    mov %r12, %rdi
    mov $4096, %rsi
    syscall
    mov $1, %rax            # write(1, msg1) — "before the access"
    mov $1, %rdi
    lea msg1(%rip), %rsi
    mov $len1, %rdx
    syscall
    movq (%r12), %rax       # A READ AFTER munmap -> there must be a #PF (no UAF)
    mov $1, %rax            # if we got here — the UAF is NOT prevented
    mov $1, %rdi
    lea msg2(%rip), %rsi
    mov $len2, %rdx
    syscall
    mov $60, %rax
    xor %rdi, %rdi
    syscall
msg1:
    .ascii "uaf: the access after the munmap...\n"
len1 = . - msg1
msg2:
    .ascii "uaf: FAIL — the access succeeded (a UAF)\n"
len2 = . - msg2
