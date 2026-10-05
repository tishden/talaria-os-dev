/* migregs: A SYSCALL THAT MOVES THE THREAD TO ANOTHER CORE KEEPS EVERY REGISTER.
 *
 * What it is for. A syscall preserves every register but its return value (x86 also rcx
 * and r11, which syscall/sysret take), and compiled code relies on it: an inline svc keeps
 * its temporaries in x9..x18 and lr across the call. When `sched_setaffinity` (or a
 * `prctl(PR_SET_NAME)` a layout rule acts on) moves the calling thread to another core, the
 * kernel builds the thread's return context anew there — and it carried only the
 * callee-saved registers: x0..x18 and lr came back zero (ClickHouse's threads returned to
 * address 0 on arm64), rdi..r10 on x86.
 *
 * The thread starts on the boot core, fills the registers with patterns, asks to be moved
 * to core 1 with a raw syscall whose every register is an in-out operand, and compares. */
#define _GNU_SOURCE
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>
#include <sys/syscall.h>

static int cpu_now(void) { unsigned c = 0, n = 0; syscall(SYS_getcpu, &c, &n, 0); return (int)c; }

int main(void) {
    uint64_t mask = 2;  /* core 1 */
    int before = cpu_now(), bad = 0;
#if defined(__aarch64__)
    register uint64_t x0 __asm__("x0") = 0, x1 __asm__("x1") = 8, x2 __asm__("x2") = (uint64_t)&mask;
    register uint64_t x8 __asm__("x8") = SYS_sched_setaffinity,
        x9 __asm__("x9") = 0x9999, x10 __asm__("x10") = 0xaaaa, x11 __asm__("x11") = 0xbbbb,
        x12 __asm__("x12") = 0xcccc, x13 __asm__("x13") = 0xdddd, x14 __asm__("x14") = 0xeeee,
        x15 __asm__("x15") = 0xffff, x16 __asm__("x16") = 0x1616, x17 __asm__("x17") = 0x1717;
    uint64_t lr_in = 0x3030, lr_out;
    __asm__ volatile("mov x30, %[lin]\n\tsvc #0\n\tmov %[lout], x30"
        : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x8),
          "+r"(x9), "+r"(x10), "+r"(x11), "+r"(x12), "+r"(x13), "+r"(x14), "+r"(x15), "+r"(x16), "+r"(x17),
          [lout] "=&r"(lr_out)
        : [lin] "r"(lr_in) : "x30", "memory");
    uint64_t got[] = {x1, x2, x8, x9, x10, x11, x12, x13, x14, x15, x16, x17, lr_out};
    uint64_t want[] = {8, (uint64_t)&mask, SYS_sched_setaffinity,
        0x9999, 0xaaaa, 0xbbbb, 0xcccc, 0xdddd, 0xeeee, 0xffff, 0x1616, 0x1717, 0x3030};
    const char *name[] = {"x1","x2","x8","x9","x10","x11","x12","x13","x14","x15","x16","x17","lr"};
    long ret = (long)x0;
#else
    register long rax __asm__("rax") = SYS_sched_setaffinity, rdi __asm__("rdi") = 0, rsi __asm__("rsi") = 8,
        rdx __asm__("rdx") = (long)&mask, r10 __asm__("r10") = 0x1010, r8 __asm__("r8") = 0x0808, r9 __asm__("r9") = 0x0909;
    register long rbx __asm__("rbx") = 0xbbbb, r12 __asm__("r12") = 0x1212;
    __asm__ volatile("syscall" : "+r"(rax), "+r"(rdi), "+r"(rsi), "+r"(rdx), "+r"(r10), "+r"(r8), "+r"(r9), "+r"(rbx), "+r"(r12)
                     : : "rcx", "r11", "memory");
    uint64_t got[] = {rdi, rsi, rdx, r10, r8, r9, rbx, r12};
    uint64_t want[] = {0, 8, (uint64_t)&mask, 0x1010, 0x0808, 0x0909, 0xbbbb, 0x1212};
    const char *name[] = {"rdi","rsi","rdx","r10","r8","r9","rbx","r12"};
    long ret = rax;
#endif
    int after = cpu_now();
    for (unsigned i = 0; i < sizeof got / sizeof got[0]; i++)
        if (got[i] != want[i]) { printf("migregs: %s came back %#llx (it was %#llx)\n", name[i], (unsigned long long)got[i], (unsigned long long)want[i]); bad++; }
    printf("migregs: sched_setaffinity -> %ld, the core %d -> %d\n", ret, before, after);
    if (before == after) { puts("migregs: the thread did not move — nothing was tested"); bad++; }
    puts(bad ? "migregs: FAILURES" : "migregs: ALL OK (every register survived the move)");
    return 0;
}
