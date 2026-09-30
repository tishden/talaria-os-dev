/* tal_arch.h — the few machine-dependent lines of the test programs in one place, so that
 * one source builds for x86-64 and for AArch64 (docs/ARM64-PLAN.md §10.3). Everything else
 * in kernel/user/ is plain C over musl and needs nothing from here. */
#ifndef TAL_ARCH_H
#define TAL_ARCH_H
#include <stdint.h>

/* The cycle counter a test times itself with: the TSC / the virtual counter (CNTVCT_EL0).
 * The unit differs (cycles / counter ticks) — every user calibrates it against the clock
 * or only takes ratios. */
static inline uint64_t tal_ticks(void) {
#if defined(__x86_64__)
    unsigned lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return (uint64_t)hi << 32 | lo;
#elif defined(__aarch64__)
    uint64_t v;
    __asm__ volatile("isb\n mrs %0, cntvct_el0" : "=r"(v));
    return v;
#else
#error "tal_arch.h: an unknown machine"
#endif
}

/* The spin-wait hint. */
static inline void tal_relax(void) {
#if defined(__x86_64__)
    __asm__ volatile("pause");
#elif defined(__aarch64__)
    __asm__ volatile("yield");
#endif
}

/* The thread pointer: the TLS self-pointer at %fs:0 / TPIDR_EL0. */
static inline unsigned long tal_tp(void) {
    unsigned long v;
#if defined(__x86_64__)
    __asm__ volatile("mov %%fs:0, %0" : "=r"(v));
#elif defined(__aarch64__)
    __asm__ volatile("mrs %0, tpidr_el0" : "=r"(v));
#endif
    return v;
}

/* The interrupted pc in a signal handler's ucontext. */
#if defined(__x86_64__)
#define TAL_UC_PC(uc) ((unsigned long)((ucontext_t *)(uc))->uc_mcontext.gregs[REG_RIP])
#elif defined(__aarch64__)
#define TAL_UC_PC(uc) ((unsigned long)((ucontext_t *)(uc))->uc_mcontext.pc)
#endif

#endif
