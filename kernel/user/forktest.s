.section .text
.global _start
_start:
    mov $57, %rax          # fork
    syscall
    test %rax, %rax
    jz child
parent:
    pushq $0x1111          # a write into the stack -> a COW fault (a private copy of the parent's stack)
    mov $1, %rax
    mov $1, %rdi
    lea pmsg(%rip), %rsi
    mov $plen, %rdx
    syscall
    jmp done
child:
    pushq $0x2222          # a write into the stack -> a COW fault (a private copy of the child's stack)
    mov $1, %rax
    mov $1, %rdi
    lea cmsg(%rip), %rsi
    mov $clen, %rdx
    syscall
done:
    mov $60, %rax
    xor %rdi, %rdi
    syscall
pmsg:
    .ascii "FORKTEST: the parent\n"
plen = . - pmsg
cmsg:
    .ascii "FORKTEST: the child\n"
clen = . - cmsg
