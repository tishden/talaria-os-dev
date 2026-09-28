/* fputest.c — the regression of the saving of the FPU/AVX STATE (talaria.test=fputest).
 *
 * The kernel enables AVX for the workload, which means it must save the YMM registers in
 * full: at a context switch (XSAVE in sched) and at the delivery of a signal (the whole
 * area in the frame). A mistake here does not crash on the spot — it spoils the data
 * where a JIT or a garbage collector counts it, and surfaces minutes later as a
 * "random" crash. So we check it directly: we fill the YMMs, go through an obstacle and
 * compare byte by byte.
 *
 * Three obstacles:
 *   1) a signal (a SIGSEGV over PROT_NONE), the handler CLOBBERS the ymm;
 *   2) a syscall (the ordinary path ring3->ring0->ring3);
 *   3) the preemption: twice as many threads as cores, each one with ITS OWN pattern in
 *      the ymm, and every core has a neighbour — the kernel's tick is not a "lonely" one.
 * musl static, -Wl,-Ttext-segment=0x10000000.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <pthread.h>
#include <sys/mman.h>
#include <time.h>

static volatile int fails = 0;
static void chk(int ok, const char *what) {
    if (ok) printf("fputest: ok %s\n", what);
    else { printf("fputest: FAIL %s\n", what); fails++; }
    fflush(stdout);
}

/* Fill ymm0..ymm7 with the pattern from buf (8 registers of 32 bytes). */
static inline void ymm_load(const unsigned char *b) {
    __asm__ volatile(
        "vmovups   0(%0), %%ymm0\n vmovups  32(%0), %%ymm1\n"
        "vmovups  64(%0), %%ymm2\n vmovups  96(%0), %%ymm3\n"
        "vmovups 128(%0), %%ymm4\n vmovups 160(%0), %%ymm5\n"
        "vmovups 192(%0), %%ymm6\n vmovups 224(%0), %%ymm7\n"
        :: "r"(b) : "memory");
}
static inline void ymm_store(unsigned char *b) {
    __asm__ volatile(
        "vmovups %%ymm0,   0(%0)\n vmovups %%ymm1,  32(%0)\n"
        "vmovups %%ymm2,  64(%0)\n vmovups %%ymm3,  96(%0)\n"
        "vmovups %%ymm4, 128(%0)\n vmovups %%ymm5, 160(%0)\n"
        "vmovups %%ymm6, 192(%0)\n vmovups %%ymm7, 224(%0)\n"
        :: "r"(b) : "memory");
}

static unsigned char *guard;                   /* a PROT_NONE page — the source of the SIGSEGV */
static volatile int handled;

static void handler(int sig, siginfo_t *si, void *uc) {
    (void)sig; (void)si; (void)uc;
    unsigned char junk[256];
    for (int i = 0; i < 256; i++) junk[i] = (unsigned char)(0xA5 ^ i);
    ymm_load(junk);                            /* the handler CLOBBERS the ymm entirely */
    handled = 1;
    mprotect(guard, 4096, PROT_READ | PROT_WRITE); /* let the instruction repeat */
}

static int spin_ms = 300;                     /* the duration of the threads' run (ms) */
static void *spinner(void *arg) {
    unsigned long id = (unsigned long)arg;
    unsigned char pat[256], out[256];
    for (int i = 0; i < 256; i++) pat[i] = (unsigned char)(id * 37 + i);
    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    for (long k = 0; ; k++) {
        ymm_load(pat);
        /* the preemption may happen at any moment of this interval */
        for (volatile int j = 0; j < 50; j++) { }
        ymm_store(out);
        if (memcmp(pat, out, 256) != 0) {
            int r = 0; while (r < 8 && pat[r * 32] == out[r * 32]) r++;
            printf("fputest: the thread %lu: ymm%d is spoilt at the iteration %ld (we expected %02x, we got %02x)\n",
                   id, r, k, pat[r * 32], out[r * 32]);
            fails++; return (void *)1;
        }
        if ((k & 1023) == 0) {
            clock_gettime(CLOCK_MONOTONIC, &t1);
            if ((t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000 >= spin_ms) break;
        }
    }
    return 0;
}

int main(void) {
    unsigned char pat[256], out[256];
    for (int i = 0; i < 256; i++) pat[i] = (unsigned char)(i * 7 + 1);

    /* 1) a syscall must not touch the ymm */
    ymm_load(pat);
    (void)getpid();
    ymm_store(out);
    chk(memcmp(pat, out, 256) == 0, "the ymm survived a syscall");

    /* 2) the signal: the handler clobbers the ymm, after the return the state must come back */
    guard = mmap(0, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (guard == MAP_FAILED) { printf("fputest: FAIL mmap\n"); return 1; }
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, 0);
    ymm_load(pat);
    handled = 0;
    *(volatile unsigned char *)guard = 1;      /* SIGSEGV -> the handler -> a repeat of the write */
    ymm_store(out);
    chk(handled && memcmp(pat, out, 256) == 0, "the ymm survived a signal handler");

    /* 3) THE GENERAL-PURPOSE REGISTERS survive the handler — including rcx and r11.
     *
     * They are easy to lose unnoticed: a return from a handler through `sysret`
     * PHYSICALLY cannot bring them back — that instruction requires the return address to
     * be in rcx and the flags in r11. For an ordinary syscall that is lawful (`syscall`
     * destroys them anyway), for a signal it is not: an arbitrary piece of code has been
     * interrupted. The JVM catches a SIGSEGV at every safepoint poll, that is, in the
     * middle of JIT code where all the registers are taken, and the spoiling of two of
     * them looks like a "random" crash minutes later. So we check it directly. */
    {
        unsigned long got_rcx = 0, got_r11 = 0;
        volatile unsigned char *g2 = guard;
        if (mprotect(guard, 4096, PROT_NONE) != 0) { printf("fputest: FAIL mprotect\n"); fails++; }
        handled = 0;
        __asm__ volatile(
            "movabs $0x1122334455667788, %%rcx\n"
            "movabs $0x99aabbccddeeff00, %%r11\n"
            "movb   $1, (%2)\n"
            "mov    %%rcx, %0\n"
            "mov    %%r11, %1\n"
            : "=r"(got_rcx), "=r"(got_r11)
            : "r"(g2)
            : "rcx", "r11", "memory");
        chk(handled && got_rcx == 0x1122334455667788UL && got_r11 == 0x99aabbccddeeff00UL,
            "rcx and r11 survived a signal handler");
    }

    /* 4) the preemption: every thread has its own pattern, the kernel must not mix them up.
     *
     * There are TWICE as many threads as cores — so that every core has a neighbour and
     * the tick is not a "lonely" one (in solitude the core skips part of the scheduler's
     * work). It is exactly that part that is dangerous: it goes BEFORE the workload's FPU
     * is saved, and any xmm the kernel touches there (zeroing an array — `xorps/movaps`)
     * reaches the workload spoilt — WITHOUT a task switch, "it is us working".
     * It was caught by a crash of SObjectizer on `talaria.stealpush=1` with zero
     * migrations. That is why the regression runs this same test with the knobs that turn
     * such work on inside the tick (test.sh). Time rather than a number of iterations — so
     * that there are guaranteed to be many ticks (the quantum is 0.5 ms). */
    int ncpu = (int)sysconf(_SC_NPROCESSORS_ONLN);
    if (ncpu < 1) ncpu = 1;
    int nthr = ncpu * 2; if (nthr > 32) nthr = 32;
    pthread_t t[32];
    for (unsigned long i = 0; i < (unsigned long)nthr; i++) pthread_create(&t[i], 0, spinner, (void *)(i + 1));
    for (int i = 0; i < nthr; i++) pthread_join(t[i], 0);
    printf("fputest: %d threads on %d cores, %d ms\n", nthr, ncpu, spin_ms);
    chk(fails == 0, "the threads' ymm are not mixed up at preemption");

    printf("fputest: %s\n", fails ? "FAILED" : "ALL OK");
    fflush(stdout);
    return fails ? 1 : 0;
}
